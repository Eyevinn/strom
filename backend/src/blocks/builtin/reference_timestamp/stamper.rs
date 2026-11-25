//! Element that stamps each buffer with a `GstReferenceTimestampMeta` holding the
//! wall-clock time the pipeline clock gives the buffer.
//!
//! The stamp is the pipeline clock's time for the buffer (`base_time + running
//! time`), mapped to TAI by the running time -> TAI mapping shared by the flow
//! (`crate::tams::timeline`), and written as UTC (`timestamp/x-unix`) or TAI
//! (`timestamp/x-tai`). For a live source, or a stream synchronized to the clock
//! (`clocksync`), that is the time the buffer was captured or released. It is a
//! function of running time, so equal running times get equal stamps in every
//! branch of a flow, and frames keep their exact spacing (no arrival jitter).
//!
//! A buffer that already carries a reference timestamp in the same domain keeps
//! it: upstream capture time wins.

use crate::tams::timeline::{shared_pipeline_mapping, PipelineTaiMapping, TaiClock};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::prelude::*;
use std::sync::Mutex;
use strom_types::FlowId;

/// The timescale a stamp is written in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Timescale {
    /// `timestamp/x-unix`: UTC nanoseconds since 1970-01-01.
    #[default]
    Utc,
    /// `timestamp/x-tai`: TAI nanoseconds since 1970-01-01 TAI.
    Tai,
}

impl Timescale {
    pub fn caps_name(self) -> &'static str {
        match self {
            Timescale::Utc => "timestamp/x-unix",
            Timescale::Tai => "timestamp/x-tai",
        }
    }
}

glib::wrapper! {
    pub struct ReferenceTimestamper(ObjectSubclass<imp::ReferenceTimestamper>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

impl ReferenceTimestamper {
    pub fn new(name: &str, timescale: Timescale) -> Self {
        let el: Self = glib::Object::builder().property("name", name).build();
        el.imp().settings.lock().unwrap().timescale = timescale;
        el
    }

    /// Share the flow's running time -> TAI mapping (the one TAMS Output uses).
    /// Without a flow id the element samples a mapping of its own.
    pub fn set_flow_id(&self, flow_id: FlowId) {
        self.imp().settings.lock().unwrap().flow_id = Some(flow_id);
    }
}

mod imp {
    use super::*;
    use std::sync::LazyLock;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "stromreftimestamp",
            gst::DebugColorFlags::empty(),
            Some("Strom reference timestamp stamper"),
        )
    });

    #[derive(Default)]
    pub struct Settings {
        pub timescale: Timescale,
        pub flow_id: Option<FlowId>,
    }

    struct Cached {
        clock: gst::Clock,
        base_time_ns: u64,
        mapping: PipelineTaiMapping,
    }

    struct State {
        tai: TaiClock,
        tai_minus_utc_ns: u64,
        caps: gst::Caps,
        timescale: Timescale,
        flow_id: Option<FlowId>,
        cached: Option<Cached>,
    }

    #[derive(Default)]
    pub struct ReferenceTimestamper {
        pub(super) settings: Mutex<Settings>,
        state: Mutex<Option<State>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ReferenceTimestamper {
        const NAME: &'static str = "StromReferenceTimestamper";
        type Type = super::ReferenceTimestamper;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for ReferenceTimestamper {}
    impl GstObjectImpl for ReferenceTimestamper {}

    impl ElementImpl for ReferenceTimestamper {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst::Caps::new_any();
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

    impl BaseTransformImpl for ReferenceTimestamper {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let settings = self.settings.lock().unwrap();
            let tai = TaiClock::new();
            let tai_minus_utc_ns = tai.tai_minus_utc_ns();
            *self.state.lock().unwrap() = Some(State {
                tai,
                tai_minus_utc_ns,
                caps: gst::Caps::new_empty_simple(settings.timescale.caps_name()),
                timescale: settings.timescale,
                flow_id: settings.flow_id,
                cached: None,
            });
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = None;
            Ok(())
        }

        fn transform_ip(
            &self,
            buf: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let obj = self.obj();
            let Some(pts) = buf.pts() else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let (Some(clock), Some(base_time)) = (obj.clock(), obj.base_time()) else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let segment = obj.segment();
            let running_time = match segment.downcast_ref::<gst::format::Time>() {
                Some(seg) => match seg.to_running_time(pts) {
                    Some(rt) => rt,
                    None => return Ok(gst::FlowSuccess::Ok),
                },
                None => pts,
            };

            let mut guard = self.state.lock().unwrap();
            let Some(state) = guard.as_mut() else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let caps_name = state.timescale.caps_name();
            let already = buf.iter_meta::<gst::ReferenceTimestampMeta>().any(|m| {
                m.reference()
                    .structure(0)
                    .is_some_and(|s| s.name() == caps_name)
            });
            if already {
                return Ok(gst::FlowSuccess::Ok);
            }

            let base_ns = base_time.nseconds();
            let fresh = match &state.cached {
                Some(c) => c.base_time_ns != base_ns || c.clock != clock,
                None => true,
            };
            if fresh {
                let mapping = match state.flow_id {
                    Some(flow_id) => shared_pipeline_mapping(flow_id, &clock, base_ns, &state.tai),
                    None => PipelineTaiMapping::for_clock(&clock, &state.tai),
                };
                gst::info!(
                    CAT,
                    imp = self,
                    "clock {} ({:?}), base_time {} ns, running time -> TAI offset {} ns",
                    clock.type_().name(),
                    mapping.domain,
                    base_ns,
                    mapping.offset_ns
                );
                state.cached = Some(Cached {
                    clock,
                    base_time_ns: base_ns,
                    mapping,
                });
            }
            let Some(mapping) = state.cached.as_ref().map(|c| c.mapping) else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let tai_ns = mapping.tai_ns(base_ns, running_time.nseconds());
            let stamp_ns = match state.timescale {
                Timescale::Tai => tai_ns,
                Timescale::Utc => tai_ns.saturating_sub(state.tai_minus_utc_ns),
            };
            let duration = buf.duration();
            gst::ReferenceTimestampMeta::add(
                buf,
                &state.caps,
                gst::ClockTime::from_nseconds(stamp_ns),
                duration,
            );
            Ok(gst::FlowSuccess::Ok)
        }
    }
}
