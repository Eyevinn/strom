//! The last element inside `stromaudiobridgesrc`: it puts `scaletempo`'s
//! output back on one ordinary timeline.
//!
//! `scaletempo` derives each output timestamp from its input segment and emits
//! a new segment on every rate change, so downstream would see its running
//! time jump by `scaletempo`'s own latency several times a second, and an
//! aggregator would see overlapping buffers. This stamps output by sample
//! count from the reader's first running time and forwards a single rate-1.0
//! segment.
//!
//! It is an element rather than a pad probe because `scaletempo` emits empty
//! buffers between strides. Dropping those from a probe leaves GStreamer
//! unreffing a null pointer (`GStreamer-CRITICAL` per buffer on 1.24), and
//! passing them on makes the audio aggregator warn per buffer;
//! `BASE_TRANSFORM_FLOW_DROPPED` is the clean way to swallow one.

use super::reader::{frames_to_ns, Restamp};
use super::BPF;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::prelude::*;
use gstreamer_base::subclass::BaseTransformMode;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

glib::wrapper! {
    pub struct AudioBridgeRestamp(ObjectSubclass<imp::AudioBridgeRestamp>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl AudioBridgeRestamp {
    /// Share the reader's output timeline.
    pub fn set_timeline(&self, restamp: Arc<Restamp>) {
        *self.imp().restamp.lock().unwrap() = Some(restamp);
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AudioBridgeRestamp {
        pub(super) restamp: Mutex<Option<Arc<Restamp>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AudioBridgeRestamp {
        const NAME: &'static str = "StromAudioBridgeRestamp";
        type Type = super::AudioBridgeRestamp;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for AudioBridgeRestamp {}

    impl GstObjectImpl for AudioBridgeRestamp {}

    impl ElementImpl for AudioBridgeRestamp {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = super::super::caps();
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for AudioBridgeRestamp {
        const MODE: BaseTransformMode = BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn transform_ip(
            &self,
            buf: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let guard = self.restamp.lock().unwrap();
            let Some(restamp) = guard.as_ref() else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let frames = (buf.size() / BPF) as u64;
            if frames == 0 {
                return Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED);
            }
            let done = restamp.frames_out.fetch_add(frames, Ordering::Relaxed);
            let base = restamp.base_rt.load(Ordering::Relaxed);
            let pts = base + frames_to_ns(done);
            let end = base + frames_to_ns(done + frames);
            buf.set_pts(gst::ClockTime::from_nseconds(pts));
            buf.set_duration(gst::ClockTime::from_nseconds(end - pts));
            Ok(gst::FlowSuccess::Ok)
        }

        fn sink_event(&self, event: gst::Event) -> bool {
            if event.type_() == gst::EventType::Segment {
                let first = self
                    .restamp
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_none_or(|r| !r.segment_sent.swap(true, Ordering::Relaxed));
                if !first {
                    return true;
                }
                let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                return self
                    .obj()
                    .src_pad()
                    .push_event(gst::event::Segment::new(&segment));
            }
            self.parent_sink_event(event)
        }
    }
}
