//! Slot takeover: displacing a session that no longer produces usable media.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

use super::{SessionCleanupRequest, StallSide, WhipEndpointConfig, WhipSessionManager};

/// How long a session must have gone without media before a new client is
/// allowed to take its slot.
///
/// A connected WebRTC publisher delivers audio every ~20 ms and video every
/// ~33 ms, so two seconds of nothing means the transport is gone, not that the
/// network had a bad moment. The threshold has to stay well under the session
/// watchdog's own inactivity timeout, otherwise the watchdog frees the slot
/// first and takeover buys nothing.
const TAKEOVER_IDLE_THRESHOLD: Duration = Duration::from_secs(2);

/// How long a POST may be held while the sitting session is judged. Long enough
/// for a dead session to cross `TAKEOVER_IDLE_THRESHOLD` with polls to spare; the
/// reasoning is on `allocate_slot_or_take_over`.
const TAKEOVER_WAIT: Duration = Duration::from_secs(3);

/// How often the takeover wait re-reads the slots and the sitting session's
/// buffer counter. A live publisher stamps that counter every audio packet
/// (~20 ms) and every video frame (~33 ms), so one poll is plenty to catch it
/// moving.
const TAKEOVER_POLL: Duration = Duration::from_millis(100);

/// The least-live session on an endpoint: the one a new client would displace.
struct IdlestSession {
    resource_id: String,
    port: u16,
    /// Time since it last produced usable media and which stamp froze, `None`
    /// if it must not be judged yet; see `SessionActivity::idle`.
    idle: Option<(Duration, StallSide)>,
    /// Its slot's output counter, for comparison against the previous poll.
    last_usable: u64,
    /// Another path is already tearing it down, so its slot is about to free.
    dying: bool,
    /// Whether it has ever delivered audio; see `SessionActivity::has_delivered_audio`.
    has_audio: bool,
    cleanup_sent: Arc<AtomicBool>,
}

impl WhipSessionManager {
    /// Allocate a slot for a new client, displacing a session that is no longer
    /// producing usable media if the endpoint is full.
    ///
    /// Two ways a seat stops being worth its slot: the publisher dies without
    /// sending a WHIP DELETE (network loss, the common case for a real
    /// participant), or its media keeps arriving while nothing usable comes out
    /// the far end — a decoder that never gets its keyframe, or a consumer
    /// downstream of the slot's tee that blocks and backs pressure up the chain.
    /// `SessionActivity` covers both.
    ///
    /// When all slots are taken, this watches the sitting session for up to
    /// `TAKEOVER_WAIT` instead of refusing outright. A session whose output
    /// counter is still moving is producing for real and is never touched: the
    /// new client gets its 503 as soon as the counter is seen to move, which is
    /// a poll interval, not a wait. A counter frozen past
    /// `TAKEOVER_IDLE_THRESHOLD` means the seat is dead, and the session is
    /// handed to the ordinary cleanup path so the new client can take the slot
    /// it releases.
    ///
    /// Both cases start out looking identical — a reconnect that lands 300 ms
    /// after the drop sees the same near-zero idle time as a healthy stream —
    /// which is why the decision is made on the counter moving rather than on a
    /// single reading of it.
    ///
    /// Only a session that has delivered audio is ever displaced. Audio is what
    /// makes `TAKEOVER_IDLE_THRESHOLD` mean anything; a video-only session can
    /// sit that long between frames while its publisher is healthy, so it is
    /// left to the inactivity watchdog and the new client gets a 503 at once. The
    /// residual case is a session whose audio stopped for good — a muted or
    /// failed microphone — while its video continues at gaps wider than the
    /// threshold: that one can still be displaced.
    pub async fn allocate_slot_or_take_over(
        &self,
        config: &WhipEndpointConfig,
        resource_id: &str,
    ) -> Option<usize> {
        let deadline = Instant::now() + TAKEOVER_WAIT;
        // The candidate's output counter as of the previous poll, so this poll
        // can tell whether it moved.
        let mut previous: Option<(String, u64)> = None;

        loop {
            if let Some(slot) = config.allocate_slot(resource_id) {
                return Some(slot);
            }

            // Full. Nothing registered on this endpoint means the slots are held
            // by sessions still being set up: there is nothing to displace.
            let candidate = self.idlest_session(&config.endpoint_id)?;

            if candidate.dying {
                // Another path is already tearing it down. Wait for the slot it
                // is about to release rather than asking for cleanup twice.
            } else if !candidate.has_audio {
                // Sessions with audio sort first, so no session on this endpoint
                // can be displaced. One that starts delivering audio while we
                // wait moves its counter, which refuses too, so answer now.
                return None;
            } else if let Some((idle, side)) = candidate
                .idle
                .filter(|(idle, _)| *idle >= TAKEOVER_IDLE_THRESHOLD)
            {
                // Win the flag every other teardown path uses, so the session is
                // cleaned up exactly once and its watchdog thread stops. The
                // cleanup task is what releases the slot; the next poll takes it.
                if !candidate.cleanup_sent.swap(true, Ordering::SeqCst) {
                    let idle_ms = idle.as_millis();
                    warn!(
                        "WhipSessionManager: Displacing session '{}' on port {} ({} ms without usable media: {}) so a new client can take its slot on endpoint '{}'",
                        candidate.resource_id, candidate.port, idle_ms, side, config.endpoint_id
                    );
                    let _ = self.cleanup_tx.send(SessionCleanupRequest {
                        port: candidate.port,
                        reason: format!(
                            "displaced by a new client after {} ms without usable media: {}",
                            idle_ms, side
                        ),
                    });
                }
            } else if previous.as_ref().is_some_and(|(id, last)| {
                *id == candidate.resource_id && *last != candidate.last_usable
            }) {
                // Its counter moved while we watched: media is still coming out
                // of that slot and the endpoint is genuinely full.
                return None;
            }

            if Instant::now() + TAKEOVER_POLL >= deadline {
                return None;
            }
            previous = Some((candidate.resource_id, candidate.last_usable));
            tokio::time::sleep(TAKEOVER_POLL).await;
        }
    }

    /// The session on an endpoint that has gone longest without producing usable
    /// media — the one a new client would displace. `None` if the endpoint has
    /// no registered session at all.
    ///
    /// A session already being torn down sorts first, then sessions that have
    /// delivered audio, so a sparse video-only session cannot hide a dead one
    /// behind its longer idle time.
    ///
    /// A session `SessionActivity::idle` refuses to judge sorts last and is
    /// never displaced: it is still negotiating (ICE through a TURN relay can be
    /// slow), or still inside `DECODE_GRACE`. Evicting it would let two clients
    /// take turns throwing each other off before either ever produces media.
    fn idlest_session(&self, endpoint_id: &str) -> Option<IdlestSession> {
        let sessions = self.sessions.read().unwrap();
        sessions
            .iter()
            .filter(|(_, s)| s.endpoint_id == endpoint_id)
            .map(|(resource_id, s)| IdlestSession {
                resource_id: resource_id.clone(),
                port: s.port,
                idle: s.activity.idle(),
                last_usable: s.activity.last_usable(),
                dying: s.cleanup_sent.load(Ordering::SeqCst),
                has_audio: s.activity.has_delivered_audio(),
                cleanup_sent: s.cleanup_sent.clone(),
            })
            .max_by_key(|c| (c.dying, c.has_audio, c.idle.map(|(idle, _)| idle)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    /// The bug this guards: a publisher that dies without sending a WHIP DELETE
    /// (network loss) leaves its slot occupied, and the rejoining client is
    /// refused with 503 until the inactivity watchdog reclaims the slot seconds
    /// later. A session whose media has stopped must give its slot up instead.
    #[tokio::test]
    async fn a_dead_session_gives_its_slot_to_a_new_client() {
        let (manager, config, cleanup_sent) = full_endpoint(
            "dead-session",
            40010,
            dead_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(0),
            "the rejoining client must get the dead session's slot"
        );
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "the displaced session's watchdog must be stopped"
        );
        assert!(
            manager.get_session_port("dead-session").is_none(),
            "the displaced session must be torn down by the ordinary cleanup path"
        );
        assert_eq!(
            config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("rejoining-client"),
            "the slot must be assigned to the new client, not left free"
        );
    }

    /// The case a single reading of the idle time gets wrong: the publisher is
    /// SIGKILLed and its client reconnects a few hundred milliseconds later,
    /// while the dead session still looks freshly fed. It is the counter staying
    /// frozen, not its value, that gives the session away.
    #[tokio::test]
    async fn a_session_that_died_moments_before_the_post_is_still_displaced() {
        let (manager, config, cleanup_sent) =
            full_endpoint("just-died", 40013, dead_publisher(Duration::ZERO));

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(0),
            "a client reconnecting straight after the drop must still get the slot"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(
            started.elapsed() >= TAKEOVER_IDLE_THRESHOLD,
            "the session must not be displaced before it has been quiet long enough"
        );
    }

    /// The risk in takeover: a second participant must not be able to evict a
    /// publisher that is streaming fine. Its buffer counter is moving, so that
    /// client still gets 503, and gets it in a poll interval rather than after
    /// the full takeover wait.
    #[tokio::test]
    async fn a_live_session_is_never_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) =
            full_endpoint("live-session", 40011, live_publisher(stop.clone()));

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, None, "a second client must still be refused");
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a session that is delivering media must not be torn down"
        );
        assert!(
            manager.get_session_port("live-session").is_some(),
            "the live session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a live publisher must be recognised from its moving counter, not \
             waited out: took {:?}",
            started.elapsed()
        );
    }

    /// THE BUG. A seat keeps receiving RTP while nothing usable ever comes out
    /// of its slot — the decoder never got a keyframe it could use, or a
    /// consumer below the slot's tee is blocking the chain. Its arriving-bytes
    /// counter moves the whole time, so liveness measured there says "healthy"
    /// and every rejoining client is refused for as long as the seat sits there.
    #[tokio::test]
    async fn a_session_receiving_media_it_never_decodes_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "receiving-nothing-usable",
            40014,
            receiving_but_never_usable(stop.clone()),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot,
            Some(0),
            "a seat producing nothing usable must give its slot up, however much RTP it receives"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager
                .get_session_port("receiving-nothing-usable")
                .is_none(),
            "the displaced session must be torn down by the ordinary cleanup path"
        );
    }

    /// The same seat by the other route: it decoded fine and then froze, which is
    /// what tee backpressure from a stuck consumer does to it. RTP keeps
    /// arriving either way.
    #[tokio::test]
    async fn a_session_whose_output_has_stalled_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "stalled-output",
            40015,
            receiving_but_stalled(stop.clone(), TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, Some(0), "a stalled seat must give its slot up");
        assert!(cleanup_sent.load(Ordering::SeqCst));
    }

    /// The risk in judging a seat by its decoded output: media arrives before it
    /// can be decoded, because H.264 carries its parameter sets with a keyframe
    /// and `decodebin` has a decoder to autoplug first. A session inside that
    /// window has produced nothing usable yet and must still be left alone.
    #[tokio::test]
    async fn a_session_still_waiting_for_its_first_decoded_frame_is_not_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) =
            full_endpoint("prerolling", 40016, still_prerolling(stop.clone()));

        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, None, "a prerolling session must keep its slot");
        assert!(!cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("prerolling").is_some(),
            "the prerolling session must still be registered"
        );
    }

    /// A session that has not produced a buffer yet may just be slow to
    /// negotiate (ICE through a TURN relay). Displacing it would let two clients
    /// take turns evicting each other before either ever sends media.
    #[tokio::test]
    async fn a_session_that_has_not_delivered_media_yet_is_not_displaced() {
        let (manager, config, cleanup_sent) = full_endpoint("negotiating", 40012, no_media_yet());

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;

        assert_eq!(slot, None, "a negotiating session must keep its slot");
        assert!(!cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("negotiating").is_some(),
            "the negotiating session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a session that cannot be displaced must not hold the POST: took {:?}",
            started.elapsed()
        );
    }

    /// A video-only publisher of static content goes seconds between frames on a
    /// healthy transport, so its frozen counter says nothing about whether it is
    /// still there.
    #[tokio::test]
    async fn a_sparse_video_only_session_is_not_displaced() {
        let (manager, config, cleanup_sent) = full_endpoint(
            "screenshare",
            40013,
            sparse_video_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;

        assert_eq!(
            slot, None,
            "a video-only session must keep its slot however wide its frame gap"
        );
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a healthy publisher must not be torn down"
        );
        assert!(
            manager.get_session_port("screenshare").is_some(),
            "the video-only session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a session that cannot be displaced must not hold the POST: took {:?}",
            started.elapsed()
        );
    }

    /// With more than one slot, the idlest session is not necessarily the one
    /// that can be displaced. A video-only seat that has been quiet longer must
    /// not shield a dead audio-bearing seat from takeover.
    #[tokio::test]
    async fn a_dead_session_is_displaced_past_a_quieter_video_only_one() {
        let manager = Arc::new(WhipSessionManager::new());
        manager.start_cleanup_task();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(2));
        let config = manager
            .get_endpoint_config("endpoint")
            .expect("endpoint was just registered");

        assert_eq!(config.allocate_slot("screenshare"), Some(0));
        let screenshare_cleanup = register_in_slot(
            &manager,
            "screenshare",
            40014,
            0,
            sparse_video_publisher(TAKEOVER_IDLE_THRESHOLD * 3),
        );
        assert_eq!(config.allocate_slot("dead-session"), Some(1));
        let dead_cleanup = register_in_slot(
            &manager,
            "dead-session",
            40015,
            1,
            dead_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(1),
            "the rejoining client must get the dead session's slot"
        );
        assert!(dead_cleanup.load(Ordering::SeqCst));
        assert!(
            !screenshare_cleanup.load(Ordering::SeqCst),
            "the video-only session must not be torn down"
        );
    }
}
