//! Per-session inactivity watchdog for WHIP Input.

use crate::whip_session_manager::{SessionActivity, StallSide};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// How long a session may go without producing usable media before the watchdog
/// tears it down; see `SessionActivity::idle`.
pub(super) const INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How often the watchdog re-checks its stop flag while waiting.
const WATCHDOG_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Sleep until `deadline`, waking every `WATCHDOG_POLL` to re-check `stop`.
///
/// Returns true if `stop` was set (the caller should give up), false if the
/// deadline was reached. The wait is sliced rather than one long sleep so a
/// session's watchdog thread cannot outlive the session: a watchdog that fires
/// after teardown asks the session manager to clean up a port it no longer knows,
/// and the manager then marks that recycled port pending cleanup for nothing.
fn wait_until_deadline_or_stop(stop: &AtomicBool, deadline: Instant) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(WATCHDOG_POLL.min(deadline - now));
    }
}

/// Block until the session has gone `timeout` without producing usable media, or
/// until `stop` is set.
///
/// Returns `Some((idle_ms, side))` once the idle time crosses `timeout`, or
/// `None` if a teardown path set `stop` first. `side` names which stamp froze,
/// so the reap log can point at the publisher or at the flow's own consumers.
///
/// Idle comes from `SessionActivity::idle`, so this reaps both a publisher that
/// went away and a seat that keeps receiving RTP while nothing comes out of its
/// slot's chain. `None` from it means the session must not be judged yet — still
/// negotiating, or inside the grace a decoder gets before its first frame — and
/// the clock has not started.
///
/// Idle is re-evaluated on every `WATCHDOG_POLL` tick. The poll must stay finer than
/// `timeout`: evaluating once per `timeout` puts detection anywhere between one and
/// two full timeouts, since a drop landing just after a check goes unnoticed until
/// the next one.
pub(super) fn wait_for_inactivity(
    stop: &AtomicBool,
    activity: &SessionActivity,
    timeout: std::time::Duration,
) -> Option<(u64, StallSide)> {
    loop {
        if wait_until_deadline_or_stop(stop, Instant::now() + WATCHDOG_POLL) {
            return None;
        }
        let Some((idle, side)) = activity.idle() else {
            continue;
        };
        if idle >= timeout {
            return Some((idle.as_millis() as u64, side));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whip_session_manager::ActivityStamp;
    use std::sync::Arc;

    /// The inactivity watchdog must stop as soon as a teardown path sets the
    /// session's `cleanup_sent` flag, not when its next timeout would have been.
    /// A watchdog that outlives its session sends a cleanup request for a port the
    /// session manager no longer knows, which marks that recycled port poisoned.
    #[test]
    fn watchdog_wait_returns_as_soon_as_the_stop_flag_is_set() {
        let stop = Arc::new(AtomicBool::new(false));
        let setter = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });

        // A deadline far beyond the flag: a wait that ignores the flag fails here by
        // running the full 30 s, rather than returning in a poll interval.
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let started = Instant::now();
        let stopped = wait_until_deadline_or_stop(&stop, deadline);

        assert!(
            stopped,
            "wait must report that it was stopped, not timed out"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "wait took {:?} — it is not polling the stop flag",
            started.elapsed()
        );
    }

    /// With no stop flag set the wait must still run to its deadline, otherwise the
    /// watchdog would never reach its inactivity check.
    #[test]
    fn watchdog_wait_runs_to_the_deadline_when_not_stopped() {
        let stop = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + std::time::Duration::from_millis(300);
        let stopped = wait_until_deadline_or_stop(&stop, deadline);

        assert!(!stopped, "wait must report a timeout, not a stop");
        assert!(
            Instant::now() >= deadline,
            "wait returned before its deadline"
        );
    }

    /// A dead session must be detected one poll interval after the inactivity
    /// threshold, not one whole extra timeout later.
    ///
    /// The session's only buffer lands 150 ms in, so the threshold is crossed at
    /// ~1150 ms — just *after* a once-per-timeout check at 1000 ms would have run,
    /// and far enough past it that scheduler slop cannot blur the two. Evaluating once
    /// per `timeout` instead of once per poll fails this test: it detects at ~2000 ms,
    /// where polling detects at ~1250 ms.
    #[test]
    fn watchdog_detects_inactivity_within_one_poll_of_the_timeout() {
        let timeout = std::time::Duration::from_millis(1000);
        let stop = Arc::new(AtomicBool::new(false));
        // Running for a minute already, so the decoder's grace is long spent and
        // only the inactivity timeout is under test.
        let running = || {
            ActivityStamp::backdated(
                std::time::Duration::from_secs(60),
                std::time::Duration::ZERO,
            )
        };
        let output = Arc::new(running());
        let activity = Arc::new(SessionActivity::from_stamps(running(), output.clone()));

        // One more buffer that both arrives and comes out of the slot, then nothing.
        let publisher = activity.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            publisher.touch_ingress(true);
            output.touch();
        });

        let started = Instant::now();
        let (idle_ms, side) = wait_for_inactivity(&stop, &activity, timeout)
            .expect("watchdog must report inactivity, not a stop");
        let detection = started.elapsed();

        // Both stamps froze together, which is a publisher that went away.
        assert_eq!(
            side,
            StallSide::Ingress,
            "a session whose arrivals stopped must be reported against the publisher"
        );

        // Slack over the expected 1250 ms covers scheduler jitter but stays well
        // clear of the 2000 ms the once-per-timeout evaluation would take.
        assert!(
            detection < std::time::Duration::from_millis(1600),
            "inactivity took {:?} to detect with a {:?} timeout — idle is being \
             evaluated once per timeout, not once per poll",
            detection,
            timeout
        );
        assert!(
            idle_ms < 2 * timeout.as_millis() as u64,
            "reported idle time was {} ms for a {:?} timeout — the check is too coarse",
            idle_ms,
            timeout
        );
    }

    /// The inactivity wait must abandon a session the moment a teardown path claims
    /// it, even though the session never went idle.
    #[test]
    fn watchdog_inactivity_wait_gives_up_when_stopped() {
        let stop = Arc::new(AtomicBool::new(false));
        // Still negotiating: nothing has arrived, so the idle clock never starts
        // and only the stop flag can end the wait.
        let activity = Arc::new(SessionActivity::new(
            Instant::now(),
            Arc::new(ActivityStamp::new(Instant::now())),
        ));

        let setter = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let result = wait_for_inactivity(&stop, &activity, std::time::Duration::from_secs(30));

        assert!(
            result.is_none(),
            "a stopped wait must not report inactivity"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "wait took {:?} — it is not polling the stop flag",
            started.elapsed()
        );
    }

    /// The watchdog reads the same signal slot takeover does, so it reaps a seat
    /// that keeps receiving RTP while nothing comes out of its slot's chain.
    /// Measured on arriving bytes alone such a seat never goes idle, and holds
    /// its slot for as long as its publisher keeps sending.
    #[test]
    fn watchdog_reaps_a_session_that_receives_media_it_never_decodes() {
        let stop = Arc::new(AtomicBool::new(false));
        // Receiving for a minute already, so the decoder's grace is long spent.
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(
                std::time::Duration::from_secs(60),
                std::time::Duration::ZERO,
            ),
            Arc::new(ActivityStamp::new(Instant::now())),
        ));

        let publisher_stop = Arc::new(AtomicBool::new(false));
        let receiving = activity.clone();
        let stop_receiving = publisher_stop.clone();
        std::thread::spawn(move || {
            while !stop_receiving.load(Ordering::SeqCst) {
                receiving.touch_ingress(true);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });

        // The wait never returns while the seat counts as live, so it runs on its
        // own thread: a regression has to fail this test, not hang it.
        let (tx, rx) = std::sync::mpsc::channel();
        let watched = activity.clone();
        std::thread::spawn(move || {
            let _ = tx.send(wait_for_inactivity(&stop, &watched, INACTIVITY_TIMEOUT));
        });
        let reaped = rx.recv_timeout(std::time::Duration::from_secs(5));
        publisher_stop.store(true, Ordering::SeqCst);

        assert!(
            matches!(reaped, Ok(Some((ms, StallSide::Output))) if ms >= INACTIVITY_TIMEOUT.as_millis() as u64),
            "a seat receiving RTP it never decodes must be reaped against the \
             slot's output, not held alive by its arriving-bytes counter: {:?}",
            reaped
        );
    }
}
