//! Asking a WHIP publisher for a keyframe, and knowing when to stop.
//!
//! A browser sends H.264 parameter sets (SPS/PPS, as a STAP-A packet) only
//! alongside a keyframe. If a session's first keyframe does not arrive — the
//! publisher was already running, the burst was dropped, the receiver came up
//! a moment late — then `rtph264depay` receives nothing but fragmented
//! non-reference slices. It can reassemble them (the FU-A start/end bits pair
//! up fine) but without parameter sets it cannot set caps, so it never outputs
//! an access unit, `decodebin` never exposes a pad, and the flow's pipeline
//! never leaves PAUSED. Audio is unaffected, which is why the symptom reads as
//! "audio works, video doesn't".
//!
//! Measured on a real session: 14830 FU-A fragments, 1987 complete fragmented
//! NAL units, zero STAP-A packets, zero completed access units. Every session
//! that worked had at least one STAP-A.
//!
//! The remedy is to ask: an upstream force-key-unit event on the WHIP source
//! pad becomes a PLI to the publisher, which answers with a keyframe and the
//! parameter sets that travel with it.
//!
//! This module is the part worth testing: *when* to ask, and when to stop.
//! Asking is cheap but not free — a forced keyframe is a bitrate spike — so a
//! healthy session should never be asked at all, and a stalled one should be
//! asked a bounded number of times and then left alone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How persistently to ask for a keyframe before giving up.
#[derive(Debug, Clone, Copy)]
pub struct KeyframeRequestPolicy {
    /// Maximum number of requests to send.
    pub attempts: u32,
    /// Delay before the first request, and between subsequent ones.
    pub interval: Duration,
}

impl Default for KeyframeRequestPolicy {
    fn default() -> Self {
        Self {
            // A PLI is answered in well under a second on a working connection.
            // Five tries over 2.5s covers a stalled start without turning into
            // a keyframe generator if the publisher never answers.
            attempts: 5,
            interval: Duration::from_millis(500),
        }
    }
}

/// Ask for a keyframe until video is decoding, or until the attempts run out.
///
/// Waits *before* each request rather than after, so a session that starts
/// normally is never asked: on a healthy connect the decoder appears within a
/// few hundred milliseconds and the first check already sees it.
///
/// `wait` and `send` are injected so the policy can be tested without sleeping
/// and without a pipeline. Returns the number of requests actually sent.
pub fn request_until_decoding(
    policy: KeyframeRequestPolicy,
    decoding: &AtomicBool,
    mut wait: impl FnMut(Duration),
    mut send: impl FnMut(u32),
) -> u32 {
    let mut sent = 0;
    for attempt in 1..=policy.attempts {
        wait(policy.interval);
        if decoding.load(Ordering::Relaxed) {
            break;
        }
        send(attempt);
        sent += 1;
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(attempts: u32) -> KeyframeRequestPolicy {
        KeyframeRequestPolicy {
            attempts,
            interval: Duration::from_millis(500),
        }
    }

    /// The common case. A session that starts normally must not be asked for a
    /// keyframe at all — every request is a bitrate spike on a stream that is
    /// already fine.
    #[test]
    fn a_healthy_session_is_never_asked() {
        let decoding = AtomicBool::new(true);
        let sent = request_until_decoding(policy(5), &decoding, |_| {}, |_| panic!("asked anyway"));
        assert_eq!(sent, 0);
    }

    /// Video starts during the first wait: exactly one check, no request.
    #[test]
    fn video_arriving_during_the_first_wait_stops_it() {
        let decoding = AtomicBool::new(false);
        let sent = request_until_decoding(
            policy(5),
            &decoding,
            |_| decoding.store(true, Ordering::Relaxed),
            |_| panic!("asked after video started"),
        );
        assert_eq!(sent, 0);
    }

    /// The request worked: stop after it, do not keep asking.
    #[test]
    fn it_stops_as_soon_as_video_decodes() {
        let decoding = AtomicBool::new(false);
        let mut waits = 0;
        let sent = request_until_decoding(
            policy(5),
            &decoding,
            |_| waits += 1,
            |_| decoding.store(true, Ordering::Relaxed),
        );
        assert_eq!(sent, 1, "one request should have been enough");
        assert_eq!(
            waits, 2,
            "one wait before the request, one before giving up"
        );
    }

    /// An unresponsive publisher must not be asked forever.
    #[test]
    fn it_gives_up_after_the_configured_attempts() {
        let decoding = AtomicBool::new(false);
        let mut attempts_seen = Vec::new();
        let sent = request_until_decoding(
            policy(5),
            &decoding,
            |_| {},
            |attempt| attempts_seen.push(attempt),
        );
        assert_eq!(sent, 5);
        assert_eq!(attempts_seen, vec![1, 2, 3, 4, 5]);
    }

    /// One wait per attempt and no trailing wait after the last request — the
    /// loop must not keep a thread alive doing nothing.
    #[test]
    fn it_waits_once_per_attempt_and_not_after_the_last() {
        let decoding = AtomicBool::new(false);
        let mut waits = 0;
        request_until_decoding(policy(3), &decoding, |_| waits += 1, |_| {});
        assert_eq!(waits, 3);
    }

    #[test]
    fn the_default_policy_is_bounded_and_short() {
        let p = KeyframeRequestPolicy::default();
        assert!(p.attempts >= 1 && p.attempts <= 10, "{:?}", p);
        let total = p.interval * p.attempts;
        assert!(
            total <= Duration::from_secs(5),
            "a stalled start should be resolved or abandoned quickly, not {:?}",
            total
        );
    }
}
