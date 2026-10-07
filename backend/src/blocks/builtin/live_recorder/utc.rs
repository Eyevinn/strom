//! Wall-clock time of a running time, shared by every recorder in a flow run.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use strom_types::FlowId;

/// Wall-clock time of running time zero, in nanoseconds since the Unix epoch, per
/// flow run: the flow, its pipeline clock and its base time. The clock is part of
/// the key because a flow with direct media timing keeps base time 0 on every run,
/// whatever clock it runs on.
///
/// Sampled once per run rather than per file. The pipeline clock and the system
/// wall clock drift apart (the monotonic clock on macOS by a few ppm, about 10 ms an
/// hour), so offsets sampled at different moments would put two recorders' files
/// out of step by that drift. One anchor keeps every recorder in the run on the
/// same mapping; only the absolute time drifts.
type RunAnchor = (gst::glib::WeakRef<gst::Clock>, gst::ClockTime, i128);
static RUNNING_ZERO_UTC_NS: OnceLock<Mutex<HashMap<FlowId, RunAnchor>>> = OnceLock::new();

/// Wall-clock time, in microseconds since the Unix epoch, of `running_time` in the
/// pipeline `element` belongs to. `None` until that pipeline has first gone to
/// PLAYING: it hands out its clock only then.
pub fn running_time_to_utc_us(
    flow_id: FlowId,
    element: &gst::Element,
    running_time: gst::ClockTime,
) -> Option<u64> {
    let base_time = element.base_time()?;
    let clock = element.clock()?;

    let mut anchors = RUNNING_ZERO_UTC_NS
        .get_or_init(Default::default)
        .lock()
        .ok()?;
    let zero_ns = match anchors.get(&flow_id) {
        Some((anchor_clock, base, zero_ns))
            if *base == base_time && anchor_clock.upgrade().as_ref() == Some(&clock) =>
        {
            *zero_ns
        }
        _ => {
            let before = clock.time();
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?;
            let after = clock.time();
            let clock_now = (before.nseconds() as i128 + after.nseconds() as i128) / 2;
            let zero_ns = wall.as_nanos() as i128 - (clock_now - base_time.nseconds() as i128);
            anchors.insert(flow_id, (clock.downgrade(), base_time, zero_ns));
            zero_ns
        }
    };
    u64::try_from((zero_ns + running_time.nseconds() as i128).div_euclid(1000)).ok()
}
