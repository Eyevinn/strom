//! DPDFNet streaming speech enhancement, one 10 ms hop at a time.
//!
//! Framing follows DPDFNet's own streaming wrapper: vorbis window of 960
//! samples, hop 480, rfft -> model (one frame plus a recurrent state vector)
//! -> irfft * window -> overlap-add. Output lags input by `MODEL_DELAY`.

use std::collections::HashMap;
use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use tract_onnx::prelude::*;

pub(super) const SAMPLE_RATE: i32 = 48_000;
pub(super) const HOP: usize = 480;
const WIN: usize = 2 * HOP;
const BINS: usize = WIN / 2 + 1;
/// Frames between an input spectrum and the model output that enhances it.
const MODEL_DELAY_FRAMES: usize = 4;
/// Input-to-output delay in samples: one hop of overlap-add plus the model's frames.
pub(super) const MODEL_DELAY: usize = HOP + MODEL_DELAY_FRAMES * HOP;

static MODEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dpdfnet2_48khz_hr.onnx"));

pub(super) struct Engine {
    /// Reused across hops so a run allocates no plan state of its own.
    runner: TypedSimpleState,
    init_state: Tensor,
    /// The model's recurrent state, fed back in on every hop.
    state: Tensor,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    /// The last WIN input samples.
    hist: Vec<f32>,
    ola: Vec<f32>,
    time_buf: Vec<f32>,
    freq_buf: Vec<Complex32>,
    scratch_fwd: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    /// Interleaved re/im, [BINS][2], the model's input and output layout.
    spec: Vec<f32>,
    spec_out: Vec<f32>,
    /// Input spectra of the last MODEL_DELAY_FRAMES hops, for the attenuation limit.
    noisy: Vec<Vec<f32>>,
    noisy_pos: usize,
    /// Share of the input spectrum mixed back in; 0 removes everything the model removes.
    alpha: f32,
}

impl Engine {
    /// Parse and optimise the embedded model. Takes most of a second, so
    /// callers run it off the streaming thread.
    pub(super) fn load() -> Result<Self, String> {
        let err = |e: TractError| format!("{e:#}");
        let onnx = tract_onnx::onnx();
        let proto = onnx.proto_model_for_read(&mut &MODEL[..]).map_err(err)?;
        let metadata: HashMap<&str, &str> = proto
            .metadata_props
            .iter()
            .map(|p| (p.key.as_str(), p.value.as_str()))
            .collect();
        let init_state = tensor1(&initial_state(&metadata)?);
        let runner = onnx
            .model_for_proto_model(&proto)
            .map_err(err)?
            .with_input_fact(0, f32::fact([1, 1, BINS, 2]).into())
            .map_err(err)?
            .with_input_fact(1, f32::fact([init_state.len()]).into())
            .map_err(err)?
            .into_optimized()
            .map_err(err)?
            .into_runnable()
            .map_err(err)?
            .spawn()
            .map_err(err)?;

        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(WIN);
        let inv = planner.plan_fft_inverse(WIN);
        let window = (0..WIN)
            .map(|i| {
                let s = (0.5 * std::f32::consts::PI * (i as f32 + 0.5) / HOP as f32).sin();
                (0.5 * std::f32::consts::PI * s * s).sin()
            })
            .collect();
        Ok(Self {
            state: init_state.clone(),
            init_state,
            runner,
            time_buf: fwd.make_input_vec(),
            freq_buf: fwd.make_output_vec(),
            scratch_fwd: fwd.make_scratch_vec(),
            scratch_inv: inv.make_scratch_vec(),
            fwd,
            inv,
            window,
            hist: vec![0.0; WIN],
            ola: vec![0.0; WIN],
            spec: vec![0.0; BINS * 2],
            spec_out: vec![0.0; BINS * 2],
            noisy: vec![vec![0.0; BINS * 2]; MODEL_DELAY_FRAMES],
            noisy_pos: 0,
            alpha: 0.0,
        })
    }

    /// Forget everything heard so far.
    pub(super) fn reset(&mut self) {
        self.state = self.init_state.clone();
        self.hist.fill(0.0);
        self.ola.fill(0.0);
        for n in &mut self.noisy {
            n.fill(0.0);
        }
    }

    /// Cap how far the model may pull any bin down; 100 dB or more means no cap.
    pub(super) fn set_attenuation_limit(&mut self, db: f64) {
        self.alpha = if db >= 100.0 {
            0.0
        } else {
            10f64.powf(-db / 20.0) as f32
        };
    }

    /// Enhance one hop: `input` and `output` are HOP samples each.
    pub(super) fn process_hop(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String> {
        self.hist.copy_within(HOP.., 0);
        self.hist[WIN - HOP..].copy_from_slice(input);
        for ((t, h), w) in self.time_buf.iter_mut().zip(&self.hist).zip(&self.window) {
            *t = h * w;
        }
        self.fwd
            .process_with_scratch(
                &mut self.time_buf,
                &mut self.freq_buf,
                &mut self.scratch_fwd,
            )
            .map_err(|e| e.to_string())?;
        for (k, c) in self.freq_buf.iter().enumerate() {
            self.spec[2 * k] = c.re;
            self.spec[2 * k + 1] = c.im;
        }

        let err = |e: TractError| format!("{e:#}");
        let spec = Tensor::from_shape(&[1, 1, BINS, 2], &self.spec).map_err(err)?;
        let state = std::mem::take(&mut self.state);
        let mut outputs = self
            .runner
            .run(tvec!(spec.into(), state.into()))
            .map_err(err)?;
        self.state = outputs.remove(1).into_tensor();
        let spec_e = outputs[0].try_as_plain_ram().map_err(err)?;
        self.spec_out
            .copy_from_slice(spec_e.as_slice::<f32>().map_err(err)?);

        // The model's output lags its input by MODEL_DELAY_FRAMES, so the
        // limit blends in the input spectrum from that many hops ago.
        let noisy = &mut self.noisy[self.noisy_pos];
        if self.alpha > 0.0 {
            for (e, n) in self.spec_out.iter_mut().zip(noisy.iter()) {
                *e = self.alpha * n + (1.0 - self.alpha) * *e;
            }
        }
        noisy.copy_from_slice(&self.spec);
        self.noisy_pos = (self.noisy_pos + 1) % MODEL_DELAY_FRAMES;

        for (k, c) in self.freq_buf.iter_mut().enumerate() {
            *c = Complex32::new(self.spec_out[2 * k], self.spec_out[2 * k + 1]);
        }
        // A real signal has no imaginary DC or Nyquist term; realfft refuses one.
        self.freq_buf[0].im = 0.0;
        self.freq_buf[BINS - 1].im = 0.0;
        self.inv
            .process_with_scratch(
                &mut self.freq_buf,
                &mut self.time_buf,
                &mut self.scratch_inv,
            )
            .map_err(|e| e.to_string())?;
        // realfft's inverse is unnormalised.
        let norm = 1.0 / WIN as f32;
        for ((o, t), w) in self.ola.iter_mut().zip(&self.time_buf).zip(&self.window) {
            *o += t * w * norm;
        }
        output.copy_from_slice(&self.ola[..HOP]);
        self.ola.copy_within(HOP.., 0);
        self.ola[WIN - HOP..].fill(0.0);
        Ok(())
    }
}

/// The recurrent state vector starts at zero except for two normalisation
/// blocks whose starting values the export stores in the model metadata.
fn initial_state(metadata: &HashMap<&str, &str>) -> Result<Vec<f32>, String> {
    let get = |k: &str| {
        metadata
            .get(k)
            .copied()
            .ok_or_else(|| format!("model metadata lacks {k}"))
    };
    let floats = |k: &str| -> Result<Vec<f32>, String> {
        get(k)?
            .split(',')
            .map(|v| v.trim().parse::<f32>().map_err(|e| format!("{k}: {e}")))
            .collect()
    };
    for (k, want) in [
        ("n_fft", WIN),
        ("hop_length", HOP),
        ("sample_rate", SAMPLE_RATE as usize),
    ] {
        let got: usize = get(k)?.parse().map_err(|e| format!("{k}: {e}"))?;
        if got != want {
            return Err(format!("model {k} is {got}, expected {want}"));
        }
    }
    let size: usize = get("state_size")?
        .parse()
        .map_err(|e| format!("state_size: {e}"))?;
    let (erb, spec) = (floats("erb_norm_init")?, floats("spec_norm_init")?);
    if erb.len() + spec.len() > size {
        return Err("normalisation init larger than the state".into());
    }
    let mut state = vec![0.0; size];
    state[..erb.len()].copy_from_slice(&erb);
    state[erb.len()..erb.len() + spec.len()].copy_from_slice(&spec);
    Ok(state)
}
