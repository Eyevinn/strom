//! Element that stamps each buffer with a `GstReferenceTimestampMeta` holding the
//! wall-clock time the pipeline clock gives the buffer.
//!
//! The stamp is the pipeline clock's time for the buffer (`base_time + running
//! time`), mapped to TAI by the running time -> TAI mapping shared by the flow
//! (`crate::tams::tai_mapping`), and written as UTC (`timestamp/x-unix`) or TAI
//! (`timestamp/x-tai`). For a live source, or a stream synchronized to the clock
//! (`clocksync`), that is the time the buffer was captured or released. It is a
//! function of running time, so equal running times get equal stamps in every
//! branch of a flow, also when the mapping re-anchors after a clock jump (see
//! [`SharedStamps`]), and frames keep their exact spacing (no arrival jitter).
//!
//! A buffer that already carries a capture time keeps it: upstream capture time
//! wins. A reference timestamp in the stamp's own domain is left as it is; one in
//! another domain the mapping knows (`x-tai`, `x-ptp`, `x-unix`, `x-ntp`) is
//! converted to the stamp's domain and added, so a consumer that reads the
//! stamp's domain gets the capture time, not the pipeline clock's.
//!
//! A buffer that passes before the pipeline has a clock and a base time, the
//! preroll frame of a non-live stream, gets no stamp: the time it will be
//! played at is not known yet. The first such buffer of a run is logged.

use crate::tams::tai_mapping::{
    reference_tai_ns, shared_pipeline_stamps, HostTai, PipelineTaiMapping, SharedStamps,
};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::prelude::*;
use std::sync::{Arc, Mutex};
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

    /// Share the flow's running time -> TAI mapping with the flow's other
    /// stampers. Without a flow id the element samples a mapping of its own.
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
        /// The flow's shared stamps; `None` without a flow id.
        stamps: Option<Arc<SharedStamps>>,
        /// Running time at which the shared mapping was last asked for.
        checked_rt_ns: u64,
    }

    /// How often (in running time) the shared mapping is asked again, so a jump
    /// (a sleep, a clock step) is noticed. When another branch notices it first,
    /// the stamps here follow at once, through the shared stamps.
    const RECHECK_NS: u64 = 1_000_000_000;

    struct State {
        tai: HostTai,
        tai_minus_utc_ns: u64,
        caps: gst::Caps,
        timescale: Timescale,
        flow_id: Option<FlowId>,
        cached: Option<Cached>,
        /// A buffer without a clock (preroll) has been logged this run.
        logged_unclocked: bool,
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
            let tai = HostTai::new();
            let tai_minus_utc_ns = tai.tai_minus_utc_ns();
            *self.state.lock().unwrap() = Some(State {
                tai,
                tai_minus_utc_ns,
                caps: gst::Caps::new_empty_simple(settings.timescale.caps_name()),
                timescale: settings.timescale,
                flow_id: settings.flow_id,
                cached: None,
                logged_unclocked: false,
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
                let mut guard = self.state.lock().unwrap();
                if let Some(state) = guard.as_mut() {
                    if !std::mem::replace(&mut state.logged_unclocked, true) {
                        gst::info!(
                            CAT,
                            imp = self,
                            "buffer before the pipeline has a clock (preroll of a non-live \
                             stream): passed without a stamp"
                        );
                    }
                }
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
            // A capture time in another domain wins over the pipeline clock.
            if let Some(tai_ns) = reference_tai_ns(buf, state.tai_minus_utc_ns) {
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
                return Ok(gst::FlowSuccess::Ok);
            }

            let base_ns = base_time.nseconds();
            let rt_ns = running_time.nseconds();
            let fresh = match &state.cached {
                Some(c) => c.base_time_ns != base_ns || c.clock != clock,
                None => true,
            };
            let recheck = state.flow_id.is_some()
                && state.cached.as_ref().is_some_and(|c| {
                    rt_ns < c.checked_rt_ns || rt_ns - c.checked_rt_ns >= RECHECK_NS
                });
            if recheck && !fresh {
                if let (Some(flow_id), Some(c)) = (state.flow_id, state.cached.as_mut()) {
                    let (mapping, stamps) =
                        shared_pipeline_stamps(flow_id, &clock, base_ns, &state.tai);
                    if mapping.epoch != c.mapping.epoch {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "running time -> TAI re-anchored after a clock jump, offset {} ns",
                            mapping.offset_ns
                        );
                    }
                    c.mapping = mapping;
                    c.stamps = Some(stamps);
                    c.checked_rt_ns = rt_ns;
                }
            }
            if fresh {
                let (mapping, stamps) = match state.flow_id {
                    Some(flow_id) => {
                        let (m, s) = shared_pipeline_stamps(flow_id, &clock, base_ns, &state.tai);
                        (m, Some(s))
                    }
                    None => (PipelineTaiMapping::for_clock(&clock, &state.tai), None),
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
                    stamps,
                    checked_rt_ns: rt_ns,
                });
            }
            let Some(c) = state.cached.as_ref() else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let tai_ns = match &c.stamps {
                Some(stamps) => stamps.tai_ns(base_ns, rt_ns),
                None => c.mapping.tai_ns(base_ns, rt_ns),
            };
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
