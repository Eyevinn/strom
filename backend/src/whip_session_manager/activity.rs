//! Session liveness: the activity stamps and how a session is judged idle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One stream of buffers, reduced to when its first and most recent buffer went
/// past, as milliseconds from a fixed epoch.
///
/// Written from GStreamer's data path — an appsink callback, a pad probe — so
/// `touch` is one clock read (`Instant` takes the vDSO fast path) plus relaxed
/// atomics: no lock, no allocation, no formatting. 0 means "nothing yet", so a
/// buffer landing inside the first millisecond counts as 1.
pub struct ActivityStamp {
    epoch: Instant,
    first_ms: AtomicU64,
    last_ms: AtomicU64,
}

impl ActivityStamp {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            first_ms: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
        }
    }

    /// Stamp a buffer. Per-buffer hot path; see the type comment.
    pub fn touch(&self) {
        let ms = (self.epoch.elapsed().as_millis() as u64).max(1);
        self.last_ms.store(ms, Ordering::Relaxed);
        // Only the very first buffer writes `first_ms`; every later one pays a
        // relaxed load and a branch that predicts perfectly.
        if self.first_ms.load(Ordering::Relaxed) == 0 {
            let _ = self
                .first_ms
                .compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// Forget everything seen so far. Used when a new session claims a slot
    /// whose output chain outlives individual sessions.
    pub fn reset(&self) {
        self.first_ms.store(0, Ordering::Relaxed);
        self.last_ms.store(0, Ordering::Relaxed);
    }

    /// Milliseconds from `epoch` at which the most recent buffer went past,
    /// 0 if none has. Only meaningful compared against another reading of the
    /// same stamp — a value that changed between two of them is a stream that
    /// is still moving.
    pub fn last(&self) -> u64 {
        self.last_ms.load(Ordering::Relaxed)
    }

    /// Time since the most recent buffer, `None` if none has gone past.
    pub fn since_last(&self) -> Option<Duration> {
        self.since(self.last_ms.load(Ordering::Relaxed))
    }

    /// Time since the first buffer, `None` if none has gone past.
    pub fn since_first(&self) -> Option<Duration> {
        self.since(self.first_ms.load(Ordering::Relaxed))
    }

    /// A stamp that already looks as if its first buffer went past
    /// `since_first` ago and its most recent one `since_last` ago, so a test can
    /// stand a session up mid-life without waiting out real seconds.
    #[cfg(test)]
    pub fn backdated(since_first: Duration, since_last: Duration) -> Self {
        assert!(
            since_last <= since_first,
            "the first buffer cannot be newer than the last"
        );
        // 1 ms of headroom keeps `first_ms` clear of the 0 that means "nothing
        // yet", exactly as `touch` does.
        let stamp = Self::new(Instant::now() - since_first - Duration::from_millis(1));
        stamp.first_ms.store(1, Ordering::Relaxed);
        stamp.last_ms.store(
            (since_first - since_last).as_millis() as u64 + 1,
            Ordering::Relaxed,
        );
        stamp
    }

    fn since(&self, ms: u64) -> Option<Duration> {
        if ms == 0 {
            return None;
        }
        Some(
            self.epoch
                .elapsed()
                .saturating_sub(Duration::from_millis(ms)),
        )
    }
}

/// Which of a session's two stamps stopped moving; see `SessionActivity::idle`.
///
/// The two are different faults with different suspects, and a reap message that
/// says only "no usable media" gives an operator no way to tell a participant
/// whose laptop closed from a blocked consumer inside their own flow that is
/// costing every publisher its seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallSide {
    /// Nothing is arriving from the publisher any more. The suspect is the
    /// client or its transport.
    Ingress,
    /// Media is still arriving, but nothing usable is leaving the slot's chain.
    /// The suspect is inside the flow: a decoder that never got its keyframe, or
    /// a consumer downstream of the slot's tee blocking and backing pressure up.
    Output,
}

impl std::fmt::Display for StallSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StallSide::Ingress => f.write_str("nothing arriving from the publisher"),
            StallSide::Output => {
                f.write_str("still receiving, nothing usable leaving the slot's chain")
            }
        }
    }
}

/// Liveness of one WHIP session, in the only terms that matter to the slot it
/// occupies: is it still producing media the flow can use?
///
/// Two stamps decide it, because arriving bytes are not the same thing as usable
/// media:
///
/// - `ingress` is stamped by the session pipeline's appsink, once per buffer
///   that crosses the appsink→appsrc bridge. It says the publisher is still
///   sending, and nothing more.
/// - `output` is stamped by a pad probe on the slot's output tee, in the *main*
///   pipeline, downstream of the slot's `decodebin`. It says frames are coming
///   out the far end and reaching the flow's consumers.
///
/// A third, `ingress_audio`, does not measure liveness at all — it only records
/// whether this session ever carried audio, which is what decides how short a
/// silence may be held against it; see `has_delivered_audio`.
///
/// A seat can sit at the first without the second indefinitely — a decoder that
/// never gets the keyframe it needs, or a downstream consumer that blocks and
/// backs pressure up through the slot's tee — and by `ingress` alone it looks
/// perfectly healthy while producing nothing.
///
/// Read by the session's inactivity watchdog and by
/// `allocate_slot_or_take_over`.
pub struct SessionActivity {
    ingress: ActivityStamp,
    /// The same arrivals, counting audio buffers only. Shares `ingress`'s epoch.
    /// Non-zero means this session negotiated an audio stream and it produced
    /// something, which is what makes `TAKEOVER_IDLE_THRESHOLD` a sound bound;
    /// see `has_delivered_audio`.
    ingress_audio: ActivityStamp,
    /// Shared with the slot, not owned by the session: the slot's output chain
    /// is built once, at flow build time, and outlives the sessions that pass
    /// through it. `SessionActivity::new` resets it so one session never
    /// inherits its predecessor's liveness.
    output: Arc<ActivityStamp>,
}

impl SessionActivity {
    /// `epoch` is session start; `output` is the stamp belonging to the slot
    /// this session was assigned.
    pub fn new(epoch: Instant, output: Arc<ActivityStamp>) -> Self {
        // This session has to prove for itself that its media comes out of the
        // slot's chain. Frames already in flight from the previous occupant can
        // still stamp it; `idle` holds the decode grace so they cannot count
        // against it.
        output.reset();
        Self {
            ingress: ActivityStamp::new(epoch),
            ingress_audio: ActivityStamp::new(epoch),
            output,
        }
    }

    /// Assemble a session from stamps a test has already positioned in time.
    /// Skips the reset `new` does, which would wipe them.
    #[cfg(test)]
    pub fn from_stamps(ingress: ActivityStamp, output: Arc<ActivityStamp>) -> Self {
        // Carries audio, so it is in displacement range at all.
        let ingress_audio = ActivityStamp::new(Instant::now());
        ingress_audio.touch();
        Self {
            ingress,
            ingress_audio,
            output,
        }
    }

    /// Assemble a session that has never carried audio, so nothing about it can
    /// be judged on a short silence; see `has_delivered_audio`.
    #[cfg(test)]
    pub fn video_only_from_stamps(ingress: ActivityStamp, output: Arc<ActivityStamp>) -> Self {
        Self {
            ingress,
            ingress_audio: ActivityStamp::new(Instant::now()),
            output,
        }
    }

    /// Stamp the arrival of a buffer from the publisher. Called from the
    /// appsink callback, so this is the per-buffer hot path.
    pub fn touch_ingress(&self, is_audio: bool) {
        self.ingress.touch();
        if is_audio {
            self.ingress_audio.touch();
        }
    }

    /// Whether this session has ever carried an audio buffer.
    ///
    /// Only an audio-bearing session can be judged dead on a couple of seconds
    /// of silence, because only audio has a cadence tight enough for a gap that
    /// short to mean anything: ~20 ms per packet, against a video stream whose
    /// interval is whatever the publisher's encoder decided to emit. A
    /// video-only publisher of static content — a screen share of a window
    /// nobody is touching — legitimately goes seconds between frames on a
    /// perfectly healthy transport, and is indistinguishable from a dead one by
    /// any media-based signal. Such a session is left to the inactivity
    /// watchdog; see `allocate_slot_or_take_over`.
    ///
    /// Read on the arriving side, not the slot's output: the output stamp is
    /// shared by the slot's audio and video tees and cannot tell them apart,
    /// and the question is what the publisher negotiated.
    pub fn has_delivered_audio(&self) -> bool {
        self.ingress_audio.last() != 0
    }

    /// The slot's output counter, for comparing two readings a poll apart. A
    /// value that changed is a session that is genuinely still producing.
    pub fn last_usable(&self) -> u64 {
        self.output.last()
    }

    /// Time since this session last produced usable media, or `None` if it must
    /// not be judged yet.
    ///
    /// `None` means one of two states that must both be left alone: nothing has
    /// arrived at all (still negotiating — ICE through a TURN relay is slow, and
    /// evicting it lets two clients take turns throwing each other off before
    /// either sends media), or the first buffers arrived less than
    /// `DECODE_GRACE` ago and the decoder may still be waiting for the keyframe
    /// that carries H.264's parameter sets.
    ///
    /// The grace holds even if the output stamp has moved. After a takeover the
    /// displaced session's frames already inside the slot's chain still cross
    /// the tee after `new` reset the stamp, and those must not start the clock
    /// on a newcomer whose own decoder has not produced anything yet.
    ///
    /// Otherwise it is the staler of the two stamps: a session is usable only
    /// while both move, and a publisher going away freezes `ingress` first while
    /// a stall below the decoder freezes `output` first. The `StallSide` that
    /// comes with it is for the reap log only and never changes the duration.
    pub fn idle(&self) -> Option<(Duration, StallSide)> {
        let ingress_idle = self.ingress.since_last()?;
        let past_grace = self.ingress.since_first()?.checked_sub(DECODE_GRACE)?;

        let output_idle = match self.output.since_last() {
            Some(idle) => idle,
            // Media is arriving but nothing has come out of the decode chain.
            // Past the grace this is the failure the output stamp exists to
            // catch, and the session has been useless since the grace ran out.
            None => past_grace,
        };

        // A publisher that goes away freezes `ingress` while the last buffers
        // are still draining through the slot, so it is the staler one and
        // anything close to a tie belongs to it. Only `output` being clearly
        // staler means media is still arriving, which is the case worth naming.
        let side = if output_idle > ingress_idle + STALL_SIDE_MARGIN {
            StallSide::Output
        } else {
            StallSide::Ingress
        };
        Some((ingress_idle.max(output_idle), side))
    }
}

/// How long a session may receive media without any of it coming out of its
/// slot's chain before it counts as producing nothing.
///
/// Some delay is normal: H.264 cannot be decoded until a keyframe brings its
/// parameter sets, and `decodebin` has to autoplug a decoder first. Measured
/// from the session's *first* buffer, so a session that spent a minute
/// negotiating still gets the full grace once media starts. It has to stay under
/// the watchdog's `INACTIVITY_TIMEOUT` for the watchdog to reap a session that
/// never decodes at all.
const DECODE_GRACE: Duration = Duration::from_secs(5);

/// How much staler `output` must be than `ingress` before a reap is blamed on
/// the flow rather than on the publisher; see `StallSide`. It moves the label
/// only, never the idle duration.
///
/// The two stamps keep separate epochs and store whole milliseconds, so
/// simultaneous events can read a millisecond apart either way, and a publisher
/// going away freezes both within one buffer of each other. The fault this
/// margin exists to name holds `ingress` live for seconds.
const STALL_SIDE_MARGIN: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    /// The output stamp belongs to the slot, which outlives the sessions passing
    /// through it. A new session must not be judged on frames its predecessor
    /// produced: inheriting a stale one makes the newcomer look stalled the
    /// moment its own media starts arriving, and the next client evicts it.
    #[test]
    fn a_new_session_does_not_inherit_the_slots_previous_liveness() {
        let slot_output = Arc::new(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        assert!(
            slot_output.last() != 0,
            "the previous occupant left the slot's stamp set"
        );

        let session = SessionActivity::new(Instant::now(), slot_output.clone());
        session.touch_ingress(true);

        assert_eq!(
            slot_output.last(),
            0,
            "claiming a slot must clear what the previous session left on it"
        );
        assert_eq!(
            session.idle(),
            None,
            "a session whose own media has only just started must not be judged yet"
        );
    }

    /// After a takeover, frames the displaced session had already pushed into
    /// the slot's chain can cross the tee after the newcomer reset the stamp.
    /// They must not stand in for the newcomer's own first decoded frame: they
    /// stop within moments, and judging on them would let the next client evict
    /// a session whose decoder has not had its keyframe yet.
    #[test]
    fn a_predecessors_trailing_frames_do_not_cut_the_decode_grace_short() {
        let slot_output = Arc::new(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        let session = SessionActivity::from_stamps(
            ActivityStamp::backdated(Duration::from_millis(200), Duration::ZERO),
            slot_output.clone(),
        );
        slot_output.reset();
        // The predecessor's tail crosses the tee, then the newcomer's media arrives.
        slot_output.touch();
        std::thread::sleep(Duration::from_millis(50));
        session.touch_ingress(true);

        assert_eq!(
            session.idle(),
            None,
            "a session inside its decode grace must not be judged on its predecessor's frames"
        );
    }

    /// Both faults reap the seat, but they have different suspects, and the reap
    /// log is the only place an operator sees which one it was: a participant
    /// who went away, or a consumer inside their own flow that is blocking the
    /// slot's chain and will do the same to whoever reconnects.
    #[test]
    fn a_reaped_session_names_which_side_of_the_chain_stopped() {
        // Arrivals stopped first and the buffers already in the slot's chain
        // drained out after them, which is the order a publisher going away
        // always produces.
        let publisher_gone = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2),
            Arc::new(ActivityStamp::backdated(
                RUNNING_FOR,
                RUNNING_FOR / 2 - Duration::from_secs(1),
            )),
        );
        assert_eq!(
            publisher_gone.idle().map(|(_, side)| side),
            Some(StallSide::Ingress),
            "nothing arriving is the publisher's fault, not the flow's"
        );

        // Still receiving; the slot's chain stopped producing a while ago.
        let stuck_consumer = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            Arc::new(ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2)),
        );
        assert_eq!(
            stuck_consumer.idle().map(|(_, side)| side),
            Some(StallSide::Output),
            "a seat that still receives must be reaped against the slot's output"
        );

        // The stamps hold whole milliseconds against separate epochs, so a
        // publisher drop that freezes both at once can read either way round.
        // Anything inside the margin must not be reported as a stuck consumer,
        // and the label must not change how long the session counts as idle.
        let near_tie_output = Arc::new(ActivityStamp::backdated(
            RUNNING_FOR,
            RUNNING_FOR / 2 + STALL_SIDE_MARGIN / 2,
        ));
        let near_tie = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2),
            near_tie_output.clone(),
        );
        let output_idle = near_tie_output.since_last().unwrap();
        let (idle, side) = near_tie.idle().unwrap();
        assert_eq!(
            side,
            StallSide::Ingress,
            "stamp noise inside the margin must not move the blame to the flow"
        );
        assert!(
            idle >= output_idle,
            "idle must be the staler stamp whichever side is named: {idle:?} < {output_idle:?}"
        );
    }
}
