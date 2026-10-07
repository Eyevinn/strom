use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::{register, ELEMENT_NAME, SAMPLE_RATE};

const CHANNELS: usize = 2;
const BUF: usize = 480;

struct Harness {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    sink: gst_app::AppSink,
    element: gst::Element,
    pts: gst::ClockTime,
}

impl Harness {
    fn new(enabled: bool) -> Self {
        let _ = gst::init();
        let _ = register();
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("rate", SAMPLE_RATE as i32)
            .field("channels", CHANNELS as i32)
            .field("layout", "interleaved")
            .build();
        let src = gst_app::AppSrc::builder()
            .caps(&caps)
            .format(gst::Format::Time)
            .build();
        let element = gst::ElementFactory::make(ELEMENT_NAME)
            .property("enabled", enabled)
            .build()
            .unwrap();
        let sink = gst_app::AppSink::builder().sync(false).build();
        let pipeline = gst::Pipeline::new();
        pipeline
            .add_many([src.upcast_ref(), &element, sink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &element, sink.upcast_ref()]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        Self {
            pipeline,
            src,
            sink,
            element,
            pts: gst::ClockTime::ZERO,
        }
    }

    /// Push one buffer of interleaved samples and return what comes out.
    fn process(&mut self, samples: &[f32]) -> (Vec<f32>, Option<gst::ClockTime>) {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let mut buffer = gst::Buffer::from_mut_slice(bytes);
        let duration = gst::ClockTime::from_nseconds(
            (samples.len() / CHANNELS) as u64 * 1_000_000_000 / SAMPLE_RATE as u64,
        );
        {
            let b = buffer.get_mut().unwrap();
            b.set_pts(self.pts);
            b.set_duration(duration);
        }
        self.pts += duration;
        self.src.push_buffer(buffer).unwrap();
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("element returned no buffer");
        let out = sample.buffer().unwrap();
        let map = out.map_readable().unwrap();
        let samples = map
            .as_slice()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        (samples, out.pts())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Deterministic white noise at about -20 dBFS, the same in every channel.
fn noise(state: &mut u32, frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * CHANNELS);
    for _ in 0..frames {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let v = ((*state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.35;
        out.extend(std::iter::repeat_n(v, CHANNELS));
    }
    out
}

fn energy(samples: &[f32]) -> f64 {
    samples.iter().map(|s| (*s as f64) * (*s as f64)).sum()
}

#[test]
fn disabled_passes_audio_through_unchanged() {
    let mut h = Harness::new(false);
    let mut rng = 1;
    for i in 0..50 {
        let input = noise(&mut rng, BUF);
        let (output, pts) = h.process(&input);
        assert_eq!(output, input, "buffer {i} changed while disabled");
        assert_eq!(pts, Some(gst::ClockTime::from_mseconds(10 * i)));
    }
}

/// Noise is what the model removes. Once the model has loaded the output
/// must fall at least 20 dB below the input, buffers keep their size and
/// timestamps, and switching off returns the dry signal exactly.
#[test]
fn enabled_suppresses_noise_and_switches_back_cleanly() {
    let mut h = Harness::new(true);
    let mut rng = 7;
    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut in_e, mut out_e, mut window) = (0.0, 0.0, 0);
    let mut i = 0u64;
    loop {
        let input = noise(&mut rng, BUF);
        let (output, pts) = h.process(&input);
        assert_eq!(output.len(), input.len(), "buffer size changed");
        assert_eq!(
            pts,
            Some(gst::ClockTime::from_mseconds(10 * i)),
            "timestamp changed"
        );
        i += 1;
        in_e += energy(&input);
        out_e += energy(&output);
        window += 1;
        // Judge 0.5 s at a time, so the dry buffers before the model is
        // loaded and the crossfade in do not count.
        if window == 50 {
            let reduction_db = 10.0 * (in_e / out_e.max(1e-20)).log10();
            if reduction_db >= 20.0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "noise only reduced {reduction_db:.1} dB after {i} buffers"
            );
            (in_e, out_e, window) = (0.0, 0.0, 0);
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    h.element.set_property("enabled", false);
    // One buffer covers the crossfade back to dry; after that, exact.
    let _ = h.process(&noise(&mut rng, BUF));
    for j in 0..20 {
        let input = noise(&mut rng, BUF);
        let (output, _) = h.process(&input);
        assert_eq!(
            output, input,
            "buffer {j} after disabling is not the dry signal"
        );
    }
}

/// A buffer size that is not a whole hop must still come back the same size,
/// so the element never runs short of wet samples.
#[test]
fn odd_buffer_sizes_keep_their_length() {
    let mut h = Harness::new(true);
    let mut rng = 3;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut processed = false;
    while !processed {
        for frames in [441, 960, 17, 1024, 480] {
            let input = noise(&mut rng, frames);
            let (output, _) = h.process(&input);
            assert_eq!(output.len(), input.len());
            // Wet output differs from the dry input; that shows the model ran.
            processed |= output != input;
        }
        assert!(Instant::now() < deadline, "the model never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    for frames in [441, 960, 17, 1024, 480, 1, 479, 481] {
        let input = noise(&mut rng, frames);
        assert_eq!(h.process(&input).0.len(), input.len());
    }
}

/// With the model loaded and running, dropping the pipeline must finalize the
/// element: nothing (the loader thread included) may keep it alive.
#[test]
fn element_is_finalized_after_running() {
    let mut h = Harness::new(true);
    let mut rng = 11;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let input = noise(&mut rng, BUF);
        if h.process(&input).0 != input {
            break;
        }
        assert!(Instant::now() < deadline, "the model never ran");
        std::thread::sleep(Duration::from_millis(1));
    }
    let element = h.element.downgrade();
    let pipeline = h.pipeline.downgrade();
    drop(h);
    assert!(
        pipeline.upgrade().is_none(),
        "pipeline still alive after drop"
    );
    assert!(
        element.upgrade().is_none(),
        "voice isolation element still alive after drop"
    );
}
