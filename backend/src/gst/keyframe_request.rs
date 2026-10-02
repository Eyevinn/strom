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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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

/// Whether a slot's video has lost data that only a keyframe can repair.
///
/// Once a session is decoding, a loss the jitterbuffer gives up on (an outage
/// longer than its latency, or a retransmission that came too late) leaves the
/// decoder with frames that reference pictures it never got. A browser
/// publisher sends no further keyframe unless asked, so the decoder stays
/// broken (smeared pictures, or every frame dropped with an error flag) until
/// the session is torn down.
///
/// Two signals, both from the slot's `decodebin` in the main pipeline, where a
/// keyframe request cannot reach the publisher (it dies at the slot's
/// `appsrc`). This flag carries them across to the session, which asks; see
/// [`recover_while`].
///
/// - **A gap.** The depayloader marks the first access unit after a
///   sequence-number gap DISCONT, and every non-keyframe DELTA_UNIT. A gap
///   damages the stream until the next intact keyframe, even while the decoder keeps
///   producing (concealed, smeared) pictures.
/// - **A silent decoder.** Data can also go missing with no gap: the
///   depayloader drops a fragment it cannot place ("missing FU start bit"), and
///   the decoder then rejects every frame that references it. A decoder error
///   posts nothing on the bus (`max-errors` is -1), so the sign is access units
///   going in and no pictures coming out. That clears only when pictures come
///   out again, so a decoder that stays wedged through keyframes is one bounded
///   episode, not a request loop.
#[derive(Debug, Default)]
pub struct VideoDamage {
    gap: AtomicBool,
    /// Access units out of the depayloader since the decoder last produced a
    /// picture.
    undecoded: AtomicU32,
}

/// Access units the decoder may take without producing a picture before the
/// slot counts as damaged. Above the decoder's own pipelining: frame-threaded
/// `avdec_h264` holds about one frame per thread before its first output.
/// About a second at 30 fps.
pub const SILENT_DECODER_FRAMES: u32 = 30;

impl VideoDamage {
    /// Record one access unit leaving the depayloader. Called per buffer, so it
    /// touches atomics only.
    ///
    /// Only a keyframe with no gap in front of it repairs a gap. A DISCONT
    /// keyframe may be the repair itself missing a packet: with several slices
    /// per picture the depayloader still passes on the slices it got, and
    /// `avdec_h264` conceals the rest and keeps producing. A DISCONT keyframe
    /// does not raise the flag either, which is how every session starts.
    pub fn depayloaded(&self, discont: bool, keyframe: bool) {
        self.undecoded.fetch_add(1, Ordering::Relaxed);
        let gap = self.gap.load(Ordering::Relaxed);
        if keyframe {
            if gap && !discont {
                self.gap.store(false, Ordering::Relaxed);
            }
        } else if discont && !gap {
            self.gap.store(true, Ordering::Relaxed);
        }
    }

    /// Record one picture leaving the decoder. Called per buffer.
    pub fn decoded(&self) {
        if self.undecoded.load(Ordering::Relaxed) != 0 {
            self.undecoded.store(0, Ordering::Relaxed);
        }
    }

    pub fn is_damaged(&self) -> bool {
        self.gap.load(Ordering::Relaxed)
            || self.undecoded.load(Ordering::Relaxed) >= SILENT_DECODER_FRAMES
    }

    /// Forget a previous occupant's damage when a session claims the slot.
    pub fn clear(&self) {
        self.gap.store(false, Ordering::Relaxed);
        self.undecoded.store(0, Ordering::Relaxed);
    }
}

/// How hard to push for a keyframe once a running session's video is damaged.
#[derive(Debug, Clone, Copy)]
pub struct RecoveryPolicy {
    /// Minimum time between two requests, across damage episodes too: a link
    /// losing data every few hundred milliseconds must not turn the publisher
    /// into a keyframe generator.
    pub interval: Duration,
    /// Requests per damage episode before giving up on it. A new episode
    /// starts only once a keyframe has repaired the previous one.
    pub attempts: u32,
    /// How often the session checks the slot's damage flag.
    pub poll: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            // A forced keyframe costs several times a normal frame. One a
            // second is what a receiver on a lossy link asks for anyway, and a
            // PLI lost on the same bad link is retried a second later.
            interval: Duration::from_secs(1),
            attempts: 10,
            poll: Duration::from_millis(100),
        }
    }
}

/// What a session should do after looking at its slot's damage flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStep {
    /// Video is fine, or damaged and waiting out the interval.
    Wait,
    /// Ask the publisher for a keyframe. `attempt` counts from 1 per episode.
    Request { attempt: u32 },
    /// A keyframe arrived. `after` is measured from when the damage was first
    /// seen, `requests` is how many were sent for it.
    Recovered { after: Duration, requests: u32 },
    /// The attempts ran out with the video still damaged. Reported once.
    GaveUp { requests: u32 },
}

/// One session's mid-session recovery state. Time is passed in as a duration
/// since an arbitrary epoch so the policy can be tested without a clock.
#[derive(Debug)]
pub struct Recovery {
    policy: RecoveryPolicy,
    damaged_since: Option<Duration>,
    last_request: Option<Duration>,
    requests: u32,
    gave_up: bool,
}

impl Recovery {
    pub fn new(policy: RecoveryPolicy) -> Self {
        Self {
            policy,
            damaged_since: None,
            last_request: None,
            requests: 0,
            gave_up: false,
        }
    }

    pub fn poll(&mut self, now: Duration, damaged: bool) -> RecoveryStep {
        if !damaged {
            let Some(since) = self.damaged_since.take() else {
                return RecoveryStep::Wait;
            };
            let requests = std::mem::take(&mut self.requests);
            self.gave_up = false;
            return RecoveryStep::Recovered {
                after: now.saturating_sub(since),
                requests,
            };
        }
        self.damaged_since.get_or_insert(now);
        if self.requests >= self.policy.attempts {
            if self.gave_up {
                return RecoveryStep::Wait;
            }
            self.gave_up = true;
            return RecoveryStep::GaveUp {
                requests: self.requests,
            };
        }
        let due = self
            .last_request
            .is_none_or(|last| now.saturating_sub(last) >= self.policy.interval);
        if !due {
            return RecoveryStep::Wait;
        }
        self.last_request = Some(now);
        self.requests += 1;
        RecoveryStep::Request {
            attempt: self.requests,
        }
    }
}

/// Watch a slot's damage flag for the life of a session and act on it.
///
/// Runs until `ended` returns true. `ended`, `now`, `sleep` and `step` are
/// injected so the loop can be tested without a pipeline or a clock; `step`
/// receives every step but [`RecoveryStep::Wait`], and sends the request for
/// `Request`.
pub fn recover_while(
    policy: RecoveryPolicy,
    damage: &VideoDamage,
    mut ended: impl FnMut() -> bool,
    mut now: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
    mut step: impl FnMut(RecoveryStep),
) {
    let mut recovery = Recovery::new(policy);
    while !ended() {
        match recovery.poll(now(), damage.is_damaged()) {
            RecoveryStep::Wait => {}
            other => step(other),
        }
        sleep(policy.poll);
    }
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

    // --- Mid-session recovery ---

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn recovery() -> Recovery {
        Recovery::new(RecoveryPolicy {
            interval: ms(1000),
            attempts: 3,
            poll: ms(100),
        })
    }

    /// Every session starts with a DISCONT keyframe; that is not damage.
    #[test]
    fn a_session_opening_on_a_discont_keyframe_is_not_damaged() {
        let damage = VideoDamage::default();
        damage.depayloaded(true, true);
        assert!(!damage.is_damaged());
    }

    /// A gap before a delta frame is damage; only a keyframe repairs it, and
    /// further delta frames, DISCONT or not, leave it damaged.
    #[test]
    fn a_gap_damages_until_a_keyframe() {
        let damage = VideoDamage::default();
        damage.depayloaded(false, false);
        assert!(!damage.is_damaged(), "a delta frame alone is not damage");
        damage.depayloaded(true, false);
        assert!(damage.is_damaged());
        damage.depayloaded(false, false);
        damage.depayloaded(true, false);
        assert!(damage.is_damaged(), "only a keyframe repairs");
        damage.depayloaded(false, true);
        assert!(!damage.is_damaged());
    }

    /// A keyframe that arrives after a gap of its own may be missing slices,
    /// so it does not repair; the next intact one does.
    #[test]
    fn a_keyframe_with_a_gap_in_front_does_not_repair() {
        let damage = VideoDamage::default();
        damage.depayloaded(true, false);
        damage.depayloaded(true, true);
        assert!(damage.is_damaged(), "a DISCONT keyframe does not repair");
        damage.depayloaded(false, false);
        assert!(damage.is_damaged());
        damage.depayloaded(false, true);
        assert!(!damage.is_damaged());
    }

    #[test]
    fn clear_forgets_a_previous_occupants_damage() {
        let damage = VideoDamage::default();
        damage.depayloaded(true, false);
        damage.clear();
        assert!(!damage.is_damaged());
    }

    /// A decoder that keeps up, including one that pipelines a number of frames
    /// before its first picture, is never damaged.
    #[test]
    fn a_decoder_that_keeps_up_is_never_damaged() {
        let damage = VideoDamage::default();
        damage.depayloaded(true, true);
        for _ in 1..SILENT_DECODER_FRAMES - 1 {
            damage.depayloaded(false, false);
        }
        assert!(!damage.is_damaged(), "a decoder filling its pipeline");
        for _ in 0..10_000 {
            damage.depayloaded(false, false);
            damage.decoded();
            assert!(!damage.is_damaged());
        }
    }

    /// Data lost without a gap: access units keep coming, the decoder rejects
    /// them all. Damaged once the threshold is reached; a keyframe going in is
    /// not enough, only a picture coming out clears it.
    #[test]
    fn a_silent_decoder_damages_until_it_produces_again() {
        let damage = VideoDamage::default();
        damage.depayloaded(true, true);
        damage.decoded();
        for _ in 0..SILENT_DECODER_FRAMES - 1 {
            damage.depayloaded(false, false);
        }
        assert!(!damage.is_damaged(), "one under the threshold");
        damage.depayloaded(false, false);
        assert!(damage.is_damaged());
        damage.depayloaded(false, true);
        assert!(
            damage.is_damaged(),
            "a keyframe the decoder also rejects repairs nothing"
        );
        damage.decoded();
        assert!(!damage.is_damaged());
    }

    #[test]
    fn clear_forgets_a_previous_occupants_silent_decoder() {
        let damage = VideoDamage::default();
        for _ in 0..SILENT_DECODER_FRAMES {
            damage.depayloaded(false, false);
        }
        assert!(damage.is_damaged());
        damage.clear();
        assert!(!damage.is_damaged());
    }

    /// Healthy video is never asked, however long it runs.
    #[test]
    fn healthy_video_is_never_asked() {
        let mut r = recovery();
        for t in 0..1000 {
            assert_eq!(r.poll(ms(t * 100), false), RecoveryStep::Wait);
        }
    }

    /// Damage is acted on at once: the jitterbuffer has already waited out its
    /// latency before the gap reached the depayloader.
    #[test]
    fn damage_is_asked_for_at_once_then_once_per_interval() {
        let mut r = recovery();
        assert_eq!(r.poll(ms(0), true), RecoveryStep::Request { attempt: 1 });
        assert_eq!(r.poll(ms(100), true), RecoveryStep::Wait);
        assert_eq!(r.poll(ms(999), true), RecoveryStep::Wait);
        assert_eq!(r.poll(ms(1000), true), RecoveryStep::Request { attempt: 2 });
        assert_eq!(r.poll(ms(1500), true), RecoveryStep::Wait);
        assert_eq!(r.poll(ms(2000), true), RecoveryStep::Request { attempt: 3 });
    }

    /// A publisher that never answers is not asked forever, and giving up is
    /// reported once, not on every poll.
    #[test]
    fn it_gives_up_after_the_attempts_and_says_so_once() {
        let mut r = recovery();
        for t in [0, 1000, 2000] {
            assert!(matches!(r.poll(ms(t), true), RecoveryStep::Request { .. }));
        }
        assert_eq!(r.poll(ms(3000), true), RecoveryStep::GaveUp { requests: 3 });
        for t in 31..200 {
            assert_eq!(r.poll(ms(t * 100), true), RecoveryStep::Wait);
        }
    }

    /// A keyframe ends the episode: the recovery is reported with its time and
    /// request count, and nothing more is asked.
    #[test]
    fn a_keyframe_ends_the_episode_and_reports_it() {
        let mut r = recovery();
        assert_eq!(r.poll(ms(500), true), RecoveryStep::Request { attempt: 1 });
        assert_eq!(
            r.poll(ms(800), false),
            RecoveryStep::Recovered {
                after: ms(300),
                requests: 1
            }
        );
        assert_eq!(r.poll(ms(900), false), RecoveryStep::Wait);
        assert_eq!(r.poll(ms(5000), false), RecoveryStep::Wait);
    }

    /// The attempt budget belongs to an episode: after a recovery, even one
    /// that came after giving up, the next damage gets the full budget again.
    #[test]
    fn each_episode_gets_its_own_attempts() {
        let mut r = recovery();
        for t in [0, 1000, 2000] {
            assert!(matches!(r.poll(ms(t), true), RecoveryStep::Request { .. }));
        }
        assert_eq!(r.poll(ms(3000), true), RecoveryStep::GaveUp { requests: 3 });
        assert!(matches!(
            r.poll(ms(9000), false),
            RecoveryStep::Recovered { requests: 3, .. }
        ));
        assert_eq!(
            r.poll(ms(20_000), true),
            RecoveryStep::Request { attempt: 1 }
        );
        assert_eq!(
            r.poll(ms(21_000), true),
            RecoveryStep::Request { attempt: 2 }
        );
    }

    /// Damage that keeps coming back right after each repair is still asked
    /// for at most once per interval.
    #[test]
    fn flapping_damage_is_still_rate_limited() {
        let mut r = recovery();
        let mut requests = 0;
        // Damaged for 100 ms, repaired for 100 ms, for ten seconds.
        for t in 0..100 {
            if let RecoveryStep::Request { .. } = r.poll(ms(t * 100), t % 2 == 0) {
                requests += 1;
            }
        }
        assert_eq!(requests, 10, "one request per second over ten seconds");
    }

    /// The loop polls the flag, hands every non-Wait step out, and stops when
    /// the session ends.
    #[test]
    fn the_loop_asks_while_damaged_and_ends_with_the_session() {
        let damage = VideoDamage::default();
        let stop = AtomicBool::new(false);
        let clock = std::cell::Cell::new(Duration::ZERO);
        let mut steps = Vec::new();
        recover_while(
            RecoveryPolicy {
                interval: ms(1000),
                attempts: 10,
                poll: ms(100),
            },
            &damage,
            || stop.load(Ordering::SeqCst),
            || clock.get(),
            |d| {
                let t = clock.get() + d;
                clock.set(t);
                // Damage at 1 s, repaired at 2.5 s, session ends at 4 s.
                if t == ms(1000) {
                    damage.depayloaded(true, false);
                }
                if t == ms(2500) {
                    damage.depayloaded(false, true);
                }
                if t >= ms(4000) {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            |s| steps.push((clock.get(), s)),
        );
        assert_eq!(
            steps,
            vec![
                (ms(1000), RecoveryStep::Request { attempt: 1 }),
                (ms(2000), RecoveryStep::Request { attempt: 2 }),
                (
                    ms(2500),
                    RecoveryStep::Recovered {
                        after: ms(1500),
                        requests: 2
                    }
                ),
            ]
        );
        assert_eq!(clock.get(), ms(4000), "the loop ended with the session");
    }

    #[test]
    fn the_default_recovery_policy_is_rate_limited_and_bounded() {
        let p = RecoveryPolicy::default();
        assert!(p.interval >= ms(500), "{:?}", p);
        assert!(p.attempts >= 1 && p.attempts <= 30, "{:?}", p);
        assert!(p.poll < p.interval, "{:?}", p);
    }
}
