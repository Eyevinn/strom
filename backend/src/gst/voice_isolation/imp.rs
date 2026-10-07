use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};

use byte_slice_cast::*;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::BaseTransformExt;
use gstreamer_base::subclass::base_transform::BaseTransformMode;
use gstreamer_base::subclass::prelude::*;

use super::engine::{Engine, HOP, MODEL_DELAY, SAMPLE_RATE};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "stromvoiceisolation",
        gst::DebugColorFlags::empty(),
        Some("Strom voice isolation"),
    )
});

/// Wet output lags its input by this much: the model's delay plus one hop,
/// so a buffer that ends mid-hop can still be answered in full.
pub(super) const CONTENT_DELAY: usize = MODEL_DELAY + HOP;
/// Crossfade between dry and wet when the switch flips.
const FADE: usize = HOP;
const DEFAULT_ATTENUATION_LIMIT: f64 = 100.0;

#[derive(Clone, Copy)]
struct Settings {
    enabled: bool,
    attenuation_limit: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            attenuation_limit: DEFAULT_ATTENUATION_LIMIT,
        }
    }
}

#[derive(Default)]
struct State {
    channels: usize,
    /// The last CONTENT_DELAY mono input samples, oldest first, so a start
    /// can pick up where the dry signal is instead of from silence.
    history: VecDeque<f32>,
    engine: Option<Engine>,
    loading: Option<mpsc::Receiver<Result<Engine, String>>>,
    load_failed: bool,
    /// The engine is fed and wet output is mixed in (at gain `wet`).
    running: bool,
    wet: f32,
    pending: Vec<f32>,
    out: VecDeque<f32>,
    mono: Vec<f32>,
    hop_out: Vec<f32>,
}

#[derive(Default)]
pub struct VoiceIsolation {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for VoiceIsolation {
    const NAME: &'static str = "StromVoiceIsolation";
    type Type = super::VoiceIsolation;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for VoiceIsolation {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("enabled")
                    .nick("Enabled")
                    .blurb("Keep speech and suppress everything else (adds 60 ms while on)")
                    .default_value(false)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("attenuation-limit")
                    .nick("Attenuation limit")
                    .blurb("Most the model may reduce any frequency, in dB; 100 means no limit")
                    .minimum(0.0)
                    .maximum(100.0)
                    .default_value(DEFAULT_ATTENUATION_LIMIT)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "enabled" => {
                settings.enabled = value.get().expect("type checked upstream");
                if settings.enabled {
                    // Fading out leaves passthrough on; processing needs it off.
                    self.obj().set_passthrough(false);
                }
            }
            "attenuation-limit" => {
                settings.attenuation_limit = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "enabled" => settings.enabled.to_value(),
            "attenuation-limit" => settings.attenuation_limit.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .set_passthrough(!self.settings.lock().unwrap().enabled);
    }
}

impl GstObjectImpl for VoiceIsolation {}

impl ElementImpl for VoiceIsolation {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Voice isolation",
                "Filter/Effect/Audio",
                "Keeps speech and suppresses background sound with the DPDFNet model. \
                 While enabled the audio is delayed 60 ms inside unchanged timestamps; \
                 no latency is reported, so channels without it are not held back.",
                "Strom",
            )
        });
        Some(&*META)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("rate", SAMPLE_RATE)
                .field("channels", gst::IntRange::new(1, 8))
                .field("layout", "interleaved")
                .build();
            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for VoiceIsolation {
    const MODE: BaseTransformMode = BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let channels = incaps
            .structure(0)
            .and_then(|s| s.get::<i32>("channels").ok())
            .ok_or_else(|| gst::loggable_error!(CAT, "caps without channels: {incaps}"))?;
        self.state.lock().unwrap().channels = channels as usize;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        // Keep a loaded engine across restarts; drop everything else.
        let mut state = self.state.lock().unwrap();
        let engine = state.engine.take();
        *state = State {
            engine,
            ..Default::default()
        };
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::FlushStop(_) = event.view() {
            let mut state = self.state.lock().unwrap();
            state.running = false;
            state.wet = 0.0;
            state.history.clear();
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buf: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
        let samples = map
            .as_slice_of::<f32>()
            .map_err(|_| gst::FlowError::Error)?;
        let mut state = self.state.lock().unwrap();
        let state = &mut *state;
        downmix(samples, state.channels, &mut state.mono);
        remember(&mut state.history, &state.mono);
        drop(map);
        self.leave_passthrough_if_enabled();
        Ok(gst::FlowSuccess::Ok)
    }

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *self.settings.lock().unwrap();
        let mut map = buf.map_writable().map_err(|_| gst::FlowError::Error)?;
        let samples = map
            .as_mut_slice_of::<f32>()
            .map_err(|_| gst::FlowError::Error)?;
        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;
        let channels = state.channels.max(1);
        downmix(samples, channels, &mut state.mono);

        if settings.enabled
            && state.engine.is_none()
            && state.loading.is_none()
            && !state.load_failed
        {
            self.start_loading(state);
        }
        if let Some(rx) = &state.loading {
            match rx.try_recv() {
                Ok(Ok(engine)) => {
                    gst::info!(CAT, imp = self, "model loaded");
                    state.engine = Some(engine);
                    state.loading = None;
                }
                Ok(Err(e)) => {
                    gst::element_imp_warning!(
                        self,
                        gst::LibraryError::Init,
                        ["voice isolation model failed to load: {e}"]
                    );
                    state.load_failed = true;
                    state.loading = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.load_failed = true;
                    state.loading = None;
                }
            }
        }

        let want_wet = settings.enabled && state.engine.is_some();
        if want_wet && !state.running {
            if let Err(e) = self.start(state) {
                gst::element_imp_warning!(
                    self,
                    gst::LibraryError::Failed,
                    ["voice isolation failed to start: {e}"]
                );
                state.load_failed = true;
            }
        }
        if state.running {
            if let Err(e) = self.run(state, settings.attenuation_limit) {
                gst::element_imp_warning!(
                    self,
                    gst::LibraryError::Failed,
                    ["voice isolation stopped: {e}"]
                );
                state.load_failed = true;
                state.running = false;
                state.wet = 0.0;
                state.out.clear();
            } else {
                mix(samples, channels, &mut state.out, &mut state.wet, want_wet);
                if !want_wet && state.wet == 0.0 {
                    state.running = false;
                    state.out.clear();
                }
            }
        }
        remember(&mut state.history, &state.mono);

        let idle = !state.running;
        drop(guard);
        if idle {
            // Under the settings lock, so a concurrent enable is not undone.
            let settings = self.settings.lock().unwrap();
            if !settings.enabled {
                self.obj().set_passthrough(true);
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }
}

impl VoiceIsolation {
    fn start_loading(&self, state: &mut State) {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("voiceiso-load".into())
            .spawn(move || {
                let _ = tx.send(Engine::load());
            });
        match spawned {
            Ok(_) => state.loading = Some(rx),
            Err(e) => {
                gst::element_imp_warning!(
                    self,
                    gst::LibraryError::Init,
                    ["cannot start the model loader: {e}"]
                );
                state.load_failed = true;
            }
        }
    }

    /// `enabled` was set while a buffer decided to go idle; undo that.
    fn leave_passthrough_if_enabled(&self) {
        let settings = self.settings.lock().unwrap();
        if settings.enabled {
            self.obj().set_passthrough(false);
        }
    }

    /// Prime the engine with the recent dry signal so wet output continues
    /// from it, then drop what the model emits for audio before that.
    fn start(&self, state: &mut State) -> Result<(), String> {
        let engine = state.engine.as_mut().expect("checked by caller");
        engine.reset();
        let mut primed = Vec::with_capacity(CONTENT_DELAY);
        let mut hop_out = vec![0.0; HOP];
        let history: Vec<f32> = std::iter::repeat_n(0.0, CONTENT_DELAY - state.history.len())
            .chain(state.history.iter().copied())
            .collect();
        for hop in history.as_chunks::<HOP>().0 {
            engine.process_hop(hop, &mut hop_out)?;
            primed.extend_from_slice(&hop_out);
        }
        state.out = primed[MODEL_DELAY..].iter().copied().collect();
        state.pending.clear();
        state.hop_out = hop_out;
        state.running = true;
        gst::debug!(CAT, imp = self, "started");
        Ok(())
    }

    fn run(&self, state: &mut State, attenuation_limit: f64) -> Result<(), String> {
        let engine = state.engine.as_mut().expect("running implies an engine");
        engine.set_attenuation_limit(attenuation_limit);
        state.pending.extend_from_slice(&state.mono);
        let mut done = 0;
        while state.pending.len() - done >= HOP {
            engine.process_hop(&state.pending[done..done + HOP], &mut state.hop_out)?;
            state.out.extend(state.hop_out.iter().copied());
            done += HOP;
        }
        state.pending.drain(..done);
        Ok(())
    }
}

fn downmix(samples: &[f32], channels: usize, mono: &mut Vec<f32>) {
    let channels = channels.max(1);
    let scale = 1.0 / channels as f32;
    mono.clear();
    mono.extend(
        samples
            .chunks_exact(channels)
            .map(|f| f.iter().sum::<f32>() * scale),
    );
}

fn remember(history: &mut VecDeque<f32>, mono: &[f32]) {
    history.extend(mono.iter().copied());
    let excess = history.len().saturating_sub(CONTENT_DELAY);
    history.drain(..excess);
}

/// Write the wet signal into every channel, crossfading with the dry signal
/// while `wet` moves toward 1 (`want_wet`) or 0.
fn mix(
    samples: &mut [f32],
    channels: usize,
    out: &mut VecDeque<f32>,
    wet: &mut f32,
    want_wet: bool,
) {
    let step = 1.0 / FADE as f32;
    for frame in samples.chunks_exact_mut(channels) {
        let w = out.pop_front().unwrap_or(0.0);
        *wet = if want_wet {
            (*wet + step).min(1.0)
        } else {
            (*wet - step).max(0.0)
        };
        let g = *wet;
        for s in frame {
            *s = *s * (1.0 - g) + w * g;
        }
    }
}
