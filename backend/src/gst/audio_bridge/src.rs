//! `stromaudiobridgesrc`: plays a bridge channel out at a low target latency.
//!
//! A bin of the reader and `scaletempo`. The reader and the scaler have to sit
//! in one element: a scaler fed at real time from outside can only starve what
//! is downstream of it, since nothing upstream hands it more than real time.

use super::reader::AudioBridgeReader;
use super::restamp::AudioBridgeRestamp;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use std::sync::{LazyLock, OnceLock};
use strom_types::audio_bridge::{
    AudioBridgeStats, DEFAULT_MAX_LATENCY_MS, DEFAULT_MAX_RATE_CHANGE_PERCENT,
    DEFAULT_TARGET_LATENCY_MS, MAX_MAX_LATENCY_MS, MAX_RATE_CHANGE_PERCENT, MAX_TARGET_LATENCY_MS,
    MIN_TARGET_LATENCY_MS,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "stromaudiobridgesrc",
        gst::DebugColorFlags::empty(),
        Some("Strom adaptive audio bridge source"),
    )
});

glib::wrapper! {
    pub struct AudioBridgeSrc(ObjectSubclass<imp::AudioBridgeSrc>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

impl AudioBridgeSrc {
    pub fn stats(&self) -> AudioBridgeStats {
        self.imp().reader.stats()
    }
}

mod imp {
    use super::*;

    pub struct AudioBridgeSrc {
        pub(super) reader: AudioBridgeReader,
        srcpad: gst::GhostPad,
        /// Set when `scaletempo` could not be created; the bin then refuses
        /// to leave NULL.
        missing: OnceLock<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AudioBridgeSrc {
        const NAME: &'static str = "StromAudioBridgeSrc";
        type Type = super::AudioBridgeSrc;
        type ParentType = gst::Bin;

        fn with_class(klass: &Self::Class) -> Self {
            let templ = klass.pad_template("src").unwrap();
            Self {
                reader: glib::Object::builder().property("name", "reader").build(),
                srcpad: gst::GhostPad::builder_from_template(&templ).build(),
                missing: OnceLock::new(),
            }
        }
    }

    impl ObjectImpl for AudioBridgeSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("channel")
                        .nick("Channel")
                        .blurb("Name of the bridge channel to read from")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt::builder("target-latency")
                        .nick("Target latency")
                        .blurb("Backlog to hold, in milliseconds")
                        .minimum(MIN_TARGET_LATENCY_MS as u32)
                        .maximum(MAX_TARGET_LATENCY_MS as u32)
                        .default_value(DEFAULT_TARGET_LATENCY_MS as u32)
                        .mutable_playing()
                        .build(),
                    glib::ParamSpecDouble::builder("max-rate-change")
                        .nick("Maximum rate change")
                        .blurb(
                            "Bound on how far the playback rate moves from 1.0 to drain or \
                             refill the backlog, in percent. 5 is inaudible on speech, 10 is \
                             at most very slightly noticeable and halves the recovery. \
                             0 disables time-scaling",
                        )
                        .minimum(0.0)
                        .maximum(MAX_RATE_CHANGE_PERCENT)
                        .default_value(DEFAULT_MAX_RATE_CHANGE_PERCENT)
                        .mutable_playing()
                        .build(),
                    glib::ParamSpecUInt::builder("max-latency")
                        .nick("Maximum latency")
                        .blurb(
                            "Backlog above which the bridge skips back to the target instead \
                             of draining, in milliseconds. Raised if it is set too close to \
                             the target to leave a draining band",
                        )
                        .minimum(MIN_TARGET_LATENCY_MS as u32)
                        .maximum(MAX_MAX_LATENCY_MS as u32)
                        .default_value(DEFAULT_MAX_LATENCY_MS as u32)
                        .mutable_playing()
                        .build(),
                    glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                        .nick("Statistics")
                        .blurb("Backlog, underruns, time-scaling and input gaps")
                        .read_only()
                        .build(),
                ]
            });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "channel" => {
                    let channel = value
                        .get::<Option<String>>()
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    self.reader.update_settings(|s| s.channel = channel);
                }
                "target-latency" => {
                    let v = value.get::<u32>().unwrap() as u64;
                    self.reader.update_settings(|s| s.target_latency_ms = v);
                }
                "max-rate-change" => {
                    let v = value.get::<f64>().unwrap();
                    self.reader
                        .update_settings(|s| s.max_rate_change_percent = v);
                }
                "max-latency" => {
                    let v = value.get::<u32>().unwrap() as u64;
                    self.reader.update_settings(|s| s.max_latency_ms = v);
                }
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let s = self.reader.settings();
            match pspec.name() {
                "channel" => s.channel.to_value(),
                "target-latency" => (s.target_latency_ms as u32).to_value(),
                "max-rate-change" => s.max_rate_change_percent.to_value(),
                "max-latency" => (s.max_latency_ms as u32).to_value(),
                "stats" => stats_structure(&self.reader.stats()).to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add(&self.reader).unwrap();

            // Only the defaults of scaletempo's WSOLA are proven here (30 ms
            // stride, 14 ms search, 20 % overlap). Its latency, about 50 ms,
            // is reported through the latency query.
            match gst::ElementFactory::make("scaletempo")
                .name("scaletempo")
                .build()
            {
                Ok(scaletempo) => {
                    let restamp: AudioBridgeRestamp =
                        glib::Object::builder().property("name", "restamp").build();
                    restamp.set_timeline(self.reader.restamp());
                    obj.add(&scaletempo).unwrap();
                    obj.add(&restamp).unwrap();
                    self.reader.link(&scaletempo).unwrap();
                    scaletempo.link(&restamp).unwrap();
                    self.srcpad
                        .set_target(Some(&restamp.static_pad("src").unwrap()))
                        .unwrap();
                }
                Err(e) => {
                    let _ = self.missing.set(format!("scaletempo: {e}"));
                }
            }
            obj.add_pad(&self.srcpad).unwrap();
        }
    }

    impl GstObjectImpl for AudioBridgeSrc {}

    impl ElementImpl for AudioBridgeSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Adaptive audio bridge source",
                    "Source/Audio",
                    "Plays audio written by stromaudiobridgesink at a low target latency, \
                     time-scaling to absorb stalls and to drain the backlog they leave",
                    "Strom",
                )
            });
            Some(&*META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &super::super::caps(),
                )
                .unwrap()]
            });
            TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::NullToReady {
                if let Some(missing) = self.missing.get() {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::MissingPlugin,
                        ["Cannot create {}", missing]
                    );
                    return Err(gst::StateChangeError);
                }
            }
            gst::trace!(CAT, imp = self, "{:?}", transition);
            self.parent_change_state(transition)
        }
    }

    impl BinImpl for AudioBridgeSrc {}
}

fn stats_structure(s: &AudioBridgeStats) -> gst::Structure {
    gst::Structure::builder("audio-bridge-stats")
        .field("target-latency-ms", s.target_latency_ms)
        .field("depth-ms", s.depth_ms)
        .field("floor-ms", s.floor_ms)
        .field("max-depth-ms", s.max_depth_ms)
        .field("rate", s.rate)
        .field("underruns", s.underruns)
        .field("underrun-ms", s.underrun_ms)
        .field("longest-underrun-ms", s.longest_underrun_ms)
        .field("drained-ms", s.drained_ms)
        .field("stretched-ms", s.stretched_ms)
        .field("time-scaled-ms", s.time_scaled_ms)
        .field("skips", s.skips)
        .field("skipped-ms", s.skipped_ms)
        .field("input-gaps-50ms", s.input_gaps_50ms)
        .field("input-gaps-100ms", s.input_gaps_100ms)
        .field("input-gaps-200ms", s.input_gaps_200ms)
        .field("input-gaps-400ms", s.input_gaps_400ms)
        .field("longest-input-gap-ms", s.longest_input_gap_ms)
        .field("overflow-ms", s.overflow_ms)
        .field("producer-attached", s.producer_attached)
        .field("producer-overrun", s.producer_overrun)
        .build()
}
