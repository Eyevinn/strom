//! Floor on keyframes forced by downstream requests.
//!
//! Two kinds of `GstForceKeyUnit` request reach an encoder from downstream:
//! - "now" requests (no running time): a WebRTC viewer's PLI, which webrtcsink
//!   or rtpsession turns into a force-keyunit event. A loss spiral sends many of
//!   them, and each forced keyframe is large, so they are rate limited.
//! - scheduled requests (a running time is set): a recorder asking for a
//!   keyframe at a segment boundary, e.g. TAMS Output on its TAI grid or
//!   splitmuxsink. Each one marks a boundary that must land where asked.
//!
//! The GstVideoEncoder base-class property `min-force-key-unit-interval`
//! drops both kinds alike. With a 1 s floor, a recorder that asks for a
//! keyframe on every point of a 1 s grid (TAMS Output) lost each request that
//! came less than a second after the last forced keyframe, and its segment
//! boundaries left the grid (measured 84-215 ms past it) without a word in the
//! log. The block therefore sets that property to 0 and applies its own floor
//! in an upstream event probe, to "now" requests only. The probe fires per
//! event, not per buffer. See `docs/UPSTREAM_NOTES.md`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::debug;

/// Admits "now" keyframe requests at most once per interval; scheduled
/// requests always pass.
pub(super) struct KeyframeRequestFloor {
    interval: Duration,
    epoch: Instant,
    /// Time of the last admitted "now" request, in ns since `epoch`, plus one
    /// (0 = none yet).
    last_ns_plus_one: AtomicU64,
}

impl KeyframeRequestFloor {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            epoch: Instant::now(),
            last_ns_plus_one: AtomicU64::new(0),
        }
    }

    /// Whether a request seen at `now` passes. `scheduled` is a request with a
    /// running time.
    pub(super) fn admit(&self, scheduled: bool, now: Instant) -> bool {
        if scheduled || self.interval.is_zero() {
            return true;
        }
        let now_ns = now.saturating_duration_since(self.epoch).as_nanos() as u64;
        let last = self.last_ns_plus_one.load(Ordering::Relaxed);
        if last != 0 && now_ns.saturating_sub(last - 1) < self.interval.as_nanos() as u64 {
            return false;
        }
        // A racing request may also pass; one extra keyframe is harmless.
        self.last_ns_plus_one.store(now_ns + 1, Ordering::Relaxed);
        true
    }
}

/// Apply the floor of `interval_ms` to `encoder`: clear the base-class floor
/// and drop "now" requests that come sooner than `interval_ms` after the last
/// admitted one.
pub(super) fn install(encoder: &gst::Element, interval_ms: u64) {
    if encoder.has_property("min-force-key-unit-interval") {
        encoder.set_property("min-force-key-unit-interval", 0u64);
    }
    if interval_ms == 0 {
        return;
    }
    let Some(src) = encoder.static_pad("src") else {
        return;
    };
    let floor = Arc::new(KeyframeRequestFloor::new(Duration::from_millis(
        interval_ms,
    )));
    src.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(request) = gstreamer_video::UpstreamForceKeyUnitEvent::parse(event) else {
            return gst::PadProbeReturn::Ok;
        };
        let scheduled = request.running_time.is_some();
        if floor.admit(scheduled, Instant::now()) {
            gst::PadProbeReturn::Ok
        } else {
            debug!(
                "{}: dropping a keyframe request within {} ms of the last one",
                pad.parent()
                    .map(|p| p.name().to_string())
                    .unwrap_or_default(),
                interval_ms
            );
            gst::PadProbeReturn::Drop
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_requests_are_rate_limited() {
        let floor = KeyframeRequestFloor::new(Duration::from_millis(1000));
        let t0 = floor.epoch;
        assert!(floor.admit(false, t0));
        assert!(!floor.admit(false, t0 + Duration::from_millis(300)));
        assert!(!floor.admit(false, t0 + Duration::from_millis(999)));
        assert!(floor.admit(false, t0 + Duration::from_millis(1000)));
        assert!(!floor.admit(false, t0 + Duration::from_millis(1500)));
    }

    /// Scheduled requests (TAMS Output's grid, splitmuxsink) always pass, also
    /// back to back and right after a "now" request, and do not delay the next
    /// "now" request.
    #[test]
    fn scheduled_requests_always_pass() {
        let floor = KeyframeRequestFloor::new(Duration::from_millis(1000));
        let t0 = floor.epoch;
        assert!(floor.admit(false, t0));
        for ms in [0, 10, 500, 990] {
            assert!(floor.admit(true, t0 + Duration::from_millis(ms)));
        }
        assert!(floor.admit(false, t0 + Duration::from_millis(1000)));
    }

    #[test]
    fn zero_interval_admits_everything() {
        let floor = KeyframeRequestFloor::new(Duration::ZERO);
        let t0 = floor.epoch;
        assert!(floor.admit(false, t0));
        assert!(floor.admit(false, t0));
    }
}
