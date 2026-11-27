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
//! treats both kinds alike. A request that comes within the interval of the
//! last forced keyframe (or of the last keyframe while requests were pending)
//! stays queued and is honoured on the first frame after the interval has run
//! out (`gst_video_encoder_chain` in `gstvideoencoder.c`, GStreamer 1.28). It
//! is delayed, not dropped. With a 1 s floor, a recorder that asks for a
//! keyframe on every point of a 1 s grid (TAMS Output) got each keyframe a few
//! frames late, and its segment boundaries left the grid (measured 84-215 ms
//! past it) without a word in the log.
//!
//! The block therefore sets that property to 0 and applies its own floor in an
//! upstream event probe, to "now" requests only. Like the base class, it holds
//! an early "now" request back rather than losing it: the first one within the
//! interval is sent on when the interval runs out, and further ones until then
//! are folded into it. Scheduled requests always pass and count towards the
//! floor, so a PLI right after a grid keyframe does not force a second one.
//! The probe fires per event, not per buffer. See `docs/UPSTREAM_NOTES.md`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// What to do with one keyframe request.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Pass it to the encoder.
    Pass,
    /// Hold it back and send it on after this long.
    Defer(Duration),
    /// Drop it: an earlier held-back request will force the keyframe.
    Fold,
}

#[derive(Default)]
struct FloorState {
    /// When the last request passed.
    last_passed: Option<Instant>,
    /// When the request now held back came in.
    deferred_since: Option<Instant>,
    /// The held-back request is on its way to the encoder; let it pass.
    releasing: bool,
}

/// Admits "now" keyframe requests at most once per interval and holds back
/// the first early one; scheduled requests always pass.
pub(super) struct KeyframeRequestFloor {
    interval: Duration,
    state: Mutex<FloorState>,
}

impl KeyframeRequestFloor {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(FloorState::default()),
        }
    }

    /// Decide on a request seen at `now`. `scheduled` is a request with a
    /// running time.
    pub(super) fn on_request(&self, scheduled: bool, now: Instant) -> Decision {
        if self.interval.is_zero() {
            return Decision::Pass;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if scheduled || std::mem::take(&mut state.releasing) {
            state.last_passed = Some(now);
            return Decision::Pass;
        }
        let elapsed = state
            .last_passed
            .map(|last| now.saturating_duration_since(last));
        match elapsed {
            Some(elapsed) if elapsed < self.interval => {
                if state.deferred_since.is_some() {
                    Decision::Fold
                } else {
                    state.deferred_since = Some(now);
                    Decision::Defer(self.interval - elapsed)
                }
            }
            _ => {
                state.last_passed = Some(now);
                Decision::Pass
            }
        }
    }

    /// The held-back request is due. Returns whether to send it on: not when
    /// a request that passed after it came in has already forced a keyframe.
    /// When it returns true, the next request the probe sees passes.
    pub(super) fn on_deferred_due(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(since) = state.deferred_since.take() else {
            return false;
        };
        if state.last_passed.is_some_and(|last| last >= since) {
            return false;
        }
        state.releasing = true;
        true
    }

    /// The held-back request will not be sent after all (the pad is gone or
    /// the timer failed).
    fn cancel_release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.deferred_since = None;
        state.releasing = false;
    }
}

/// Apply the floor of `interval_ms` to `encoder`: clear the base-class floor
/// and hold back "now" requests that come sooner than `interval_ms` after the
/// last request that passed.
pub(super) fn install(encoder: &gst::Element, interval_ms: u64) {
    if encoder.has_property("min-force-key-unit-interval") {
        encoder.set_property("min-force-key-unit-interval", 0u64);
    }
    if interval_ms == 0 {
        return;
    }
    let Some(src) = encoder.static_pad("src") else {
        warn!(
            "{}: no src pad, keyframe requests are not rate limited",
            encoder.name()
        );
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
        match floor.on_request(scheduled, Instant::now()) {
            Decision::Pass => gst::PadProbeReturn::Ok,
            Decision::Defer(wait) => {
                debug!(
                    "{}: holding a keyframe request back for {} ms",
                    element_name(pad),
                    wait.as_millis()
                );
                release_later(pad, &floor, event.clone(), wait);
                gst::PadProbeReturn::Drop
            }
            Decision::Fold => {
                debug!(
                    "{}: keyframe request folded into the one held back",
                    element_name(pad)
                );
                gst::PadProbeReturn::Drop
            }
        }
    });
}

fn element_name(pad: &gst::Pad) -> String {
    pad.parent()
        .map(|p| p.name().to_string())
        .unwrap_or_default()
}

/// Send `event` to `pad` after `wait`, on the system clock's thread, unless a
/// keyframe has been forced by then. Holds the pad weakly.
fn release_later(
    pad: &gst::Pad,
    floor: &Arc<KeyframeRequestFloor>,
    event: gst::Event,
    wait: Duration,
) {
    let clock = gst::SystemClock::obtain();
    let wait = gst::ClockTime::from_nseconds(u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX));
    let id = clock.new_single_shot_id(clock.time().saturating_add(wait));
    let weak_pad = pad.downgrade();
    let floor_cb = Arc::clone(floor);
    let result = id.wait_async(move |_, _, _| {
        if !floor_cb.on_deferred_due() {
            return;
        }
        let sent = weak_pad.upgrade().is_some_and(|pad| pad.send_event(event));
        if !sent {
            floor_cb.cancel_release();
        }
    });
    if let Err(e) = result {
        warn!(
            "{}: could not schedule a held-back keyframe request: {:?}",
            element_name(pad),
            e
        );
        floor.cancel_release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn early_now_request_is_held_back_and_later_ones_fold_into_it() {
        let floor = KeyframeRequestFloor::new(ms(1000));
        let t0 = Instant::now();
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
        assert_eq!(
            floor.on_request(false, t0 + ms(300)),
            Decision::Defer(ms(700))
        );
        assert_eq!(floor.on_request(false, t0 + ms(600)), Decision::Fold);
        assert!(floor.on_deferred_due());
        // The released request passes, and restarts the floor.
        assert_eq!(floor.on_request(false, t0 + ms(1000)), Decision::Pass);
        assert_eq!(
            floor.on_request(false, t0 + ms(1500)),
            Decision::Defer(ms(500))
        );
    }

    #[test]
    fn now_request_after_the_interval_passes() {
        let floor = KeyframeRequestFloor::new(ms(1000));
        let t0 = Instant::now();
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
        assert_eq!(floor.on_request(false, t0 + ms(1000)), Decision::Pass);
        assert!(!floor.on_deferred_due(), "nothing was held back");
    }

    /// Scheduled requests (TAMS Output's grid, splitmuxsink) always pass,
    /// also back to back, and count towards the floor: a PLI right after a
    /// grid keyframe is held back, not forcing a second keyframe at once.
    #[test]
    fn scheduled_requests_pass_and_count_towards_the_floor() {
        let floor = KeyframeRequestFloor::new(ms(1000));
        let t0 = Instant::now();
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
        for n in [0, 10, 500, 990] {
            assert_eq!(floor.on_request(true, t0 + ms(n)), Decision::Pass);
        }
        assert_eq!(
            floor.on_request(false, t0 + ms(1000)),
            Decision::Defer(ms(990))
        );
    }

    /// A request that passes while one is held back already forces a
    /// keyframe after it, so the held-back one is not sent.
    #[test]
    fn held_back_request_is_skipped_after_a_newer_keyframe() {
        let floor = KeyframeRequestFloor::new(ms(1000));
        let t0 = Instant::now();
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
        assert_eq!(
            floor.on_request(false, t0 + ms(300)),
            Decision::Defer(ms(700))
        );
        assert_eq!(floor.on_request(true, t0 + ms(500)), Decision::Pass);
        assert!(!floor.on_deferred_due());
        assert_eq!(
            floor.on_request(false, t0 + ms(1000)),
            Decision::Defer(ms(500))
        );
    }

    #[test]
    fn zero_interval_admits_everything() {
        let floor = KeyframeRequestFloor::new(Duration::ZERO);
        let t0 = Instant::now();
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
        assert_eq!(floor.on_request(false, t0), Decision::Pass);
    }
}
