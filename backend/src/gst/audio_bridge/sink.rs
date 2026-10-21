//! `stromaudiobridgesink`: appends audio to a bridge channel.
//!
//! Unsynchronised and non-prerolling, like `interaudiosink`: audio goes into
//! the channel the moment it arrives, so the reader sees the producer's real
//! delivery pattern, and a flow with nobody reading is never held up.

use super::channel::{self, Channel};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::prelude::*;
use std::sync::{Arc, LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "stromaudiobridgesink",
        gst::DebugColorFlags::empty(),
        Some("Strom adaptive audio bridge sink"),
    )
});

glib::wrapper! {
    pub struct AudioBridgeSink(ObjectSubclass<imp::AudioBridgeSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AudioBridgeSink {
        channel_name: Mutex<String>,
        channel: Mutex<Option<Arc<Channel>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AudioBridgeSink {
        const NAME: &'static str = "StromAudioBridgeSink";
        type Type = super::AudioBridgeSink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for AudioBridgeSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![glib::ParamSpecString::builder("channel")
                    .nick("Channel")
                    .blurb("Name of the bridge channel to write to")
                    .mutable_ready()
                    .build()]
            });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            if pspec.name() == "channel" {
                *self.channel_name.lock().unwrap() = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "channel" => self.channel_name.lock().unwrap().to_value(),
                _ => unreachable!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_sync(false);
            obj.set_async(false);
        }
    }

    impl GstObjectImpl for AudioBridgeSink {}

    impl ElementImpl for AudioBridgeSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Adaptive audio bridge sink",
                    "Sink/Audio",
                    "Writes audio to a channel read by stromaudiobridgesrc in another pipeline",
                    "Strom",
                )
            });
            Some(&*META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                vec![gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &super::super::caps(),
                )
                .unwrap()]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseSinkImpl for AudioBridgeSink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let name = self.channel_name.lock().unwrap().clone();
            if name.is_empty() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["No channel name set"]
                ));
            }
            let channel = channel::acquire(&name);
            if !channel.claim_writer() {
                return Err(gst::error_msg!(
                    gst::ResourceError::Busy,
                    ["Audio bridge channel '{}' already has a writer", name]
                ));
            }
            gst::debug!(CAT, imp = self, "Writing to channel '{}'", name);
            *self.channel.lock().unwrap() = Some(channel);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            if let Some(channel) = self.channel.lock().unwrap().take() {
                channel.release_writer();
            }
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            // Copied rather than kept by reference: holding upstream's buffers
            // for as long as the backlog lasts could starve a bounded pool and
            // stall the producing flow.
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let data = map.as_slice().to_vec();
            drop(map);
            if let Some(channel) = self.channel.lock().unwrap().as_ref() {
                channel.lock().write(data);
            }
            Ok(gst::FlowSuccess::Ok)
        }
    }
}
