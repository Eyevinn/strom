//! WHIP session manager for per-client whipserversrc elements.
//!
//! Each WHIP client session gets its own isolated GStreamer pipeline with a
//! whipserversrc. Media is bridged to the main pipeline via appsink→appsrc,
//! where each session is assigned to a numbered slot with independent output chains.
//!
//! Dead sessions (ICE disconnect, pipeline error) are automatically cleaned up
//! via a background task that receives cleanup requests through an mpsc channel.

use crate::blocks::DynamicWebrtcbinStore;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use strom_types::block::StreamMode;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Configuration for a WHIP endpoint, registered at pipeline start.
///
/// Stores everything needed to create a new whipserversrc for each session,
/// including per-slot appsrc references for the media bridge.
pub struct WhipEndpointConfig {
    pub instance_id: String,
    pub endpoint_id: String,
    pub mode: StreamMode,
    pub stun_server: Option<String>,
    pub turn_server: Option<String>,
    pub ice_transport_policy: String,
    /// Weak ref to the pipeline
    pub pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    /// Whether to decode RTP to raw media (true) or pass through RTP (false)
    pub decode: bool,
    /// Per-slot flag, set once the main pipeline's `decodebin` has exposed a
    /// video pad for that slot — i.e. video is genuinely being decoded.
    ///
    /// A session asks the publisher for a keyframe until this flips, because
    /// without the parameter sets that travel with a keyframe the depayloader
    /// can never produce an access unit. See `gst::keyframe_request`.
    pub video_decoding: Arc<Vec<AtomicBool>>,
    /// Jitterbuffer latency in milliseconds for the per-session webrtcbin.
    pub jitterbuffer_latency_ms: u32,
    /// Whether whipserversrc should request retransmission (NACK) of lost
    /// packets from the publisher.
    pub do_retransmission: bool,
    /// Shared dynamic webrtcbin store for ICE policy tracking
    pub dynamic_webrtcbin_store: DynamicWebrtcbinStore,
    /// Maximum video bitrate hint for Chrome (kbps). Injected into the SDP
    /// answer as x-google-max-bitrate so Chrome's encoder ramps up accordingly.
    pub max_video_bitrate_kbps: u32,
    /// Maximum number of simultaneous client slots
    pub max_sessions: usize,
    /// Per-slot audio appsrc elements (main pipeline side, created at build time)
    pub slot_audio_appsrcs: Vec<gst_app::AppSrc>,
    /// Per-slot video appsrc elements (main pipeline side, created at build time)
    pub slot_video_appsrcs: Vec<gst_app::AppSrc>,
    /// Per-slot stamp of the media coming out of that slot's chain, written by
    /// a pad probe on its output tee. The session sitting in a slot borrows its
    /// stamp; see `SessionActivity`.
    pub slot_output: Vec<Arc<ActivityStamp>>,
    /// Slot assignments: slot index → Option<resource_id>
    /// Protected by RwLock for concurrent access from HTTP handlers.
    pub slot_assignments: Arc<RwLock<Vec<Option<String>>>>,
}

impl WhipEndpointConfig {
    /// Allocate a free slot for a new session.
    /// Returns the slot index, or None if all slots are occupied.
    pub fn allocate_slot(&self, resource_id: &str) -> Option<usize> {
        let mut slots = self.slot_assignments.write().unwrap();
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(resource_id.to_string());
                info!(
                    "WhipEndpointConfig: Allocated slot {} for session '{}'",
                    i, resource_id
                );
                return Some(i);
            }
        }
        None
    }

    /// Release a slot when a session disconnects.
    pub fn release_slot(&self, slot: usize) {
        let mut slots = self.slot_assignments.write().unwrap();
        if slot < slots.len() {
            let old = slots[slot].take();
            info!(
                "WhipEndpointConfig: Released slot {} (was session '{}')",
                slot,
                old.as_deref().unwrap_or("unknown")
            );
        }
    }
}

/// Request to clean up a dead WHIP session.
///
/// Sent from GStreamer callbacks (ICE state, bus watch) via the cleanup channel.
/// Uses `port` as the session identifier since it's known at session creation time,
/// before the resource_id is assigned.
pub struct SessionCleanupRequest {
    /// The internal port uniquely identifying the session
    pub port: u16,
    /// Why the session is being cleaned up
    pub reason: String,
}

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

/// Liveness of one WHIP session, in the only terms that matter to the slot it
/// occupies: is it still producing media the flow can use?
///
/// Two stamps, because arriving bytes are not the same thing as usable media:
///
/// - `ingress` is stamped by the session pipeline's appsink, once per buffer
///   that crosses the appsink→appsrc bridge. It says the publisher is still
///   sending, and nothing more.
/// - `output` is stamped by a pad probe on the slot's output tee, in the *main*
///   pipeline, downstream of the slot's `decodebin`. It says frames are coming
///   out the far end and reaching the flow's consumers.
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
        // slot's chain. Buffers left in flight from the previous occupant can
        // still stamp it for a moment afterwards, which only ever makes the new
        // session look healthier than it is — the safe direction, and it
        // corrects itself as soon as the chain drains.
        output.reset();
        Self {
            ingress: ActivityStamp::new(epoch),
            output,
        }
    }

    /// Assemble a session from stamps a test has already positioned in time.
    /// Skips the reset `new` does, which would wipe them.
    #[cfg(test)]
    pub fn from_stamps(ingress: ActivityStamp, output: Arc<ActivityStamp>) -> Self {
        Self { ingress, output }
    }

    /// Stamp the arrival of a buffer from the publisher. Called from the
    /// appsink callback, so this is the per-buffer hot path.
    pub fn touch_ingress(&self) {
        self.ingress.touch();
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
    /// Otherwise it is the staler of the two stamps: a session is usable only
    /// while both move, and a publisher going away freezes `ingress` first while
    /// a stall below the decoder freezes `output` first.
    pub fn idle(&self) -> Option<Duration> {
        let ingress_idle = self.ingress.since_last()?;

        let output_idle = match self.output.since_last() {
            Some(idle) => idle,
            // Media is arriving but nothing has come out of the decode chain.
            // Inside the grace that is just preroll; past it, this is the
            // failure the output stamp exists to catch, and the session has
            // been useless since the grace ran out.
            None => self.ingress.since_first()?.checked_sub(DECODE_GRACE)?,
        };

        Some(ingress_idle.max(output_idle))
    }
}

/// An active WHIP session (one whipserversrc element per client).
/// Each session runs in its own GStreamer pipeline to isolate NiceAgent instances.
struct WhipSession {
    /// Internal port where this session's whipserversrc is listening
    port: u16,
    /// The whipserversrc element for this session
    element: gst::Element,
    /// The isolated pipeline for this session's whipserversrc
    session_pipeline: gst::Pipeline,
    /// The endpoint this session belongs to
    endpoint_id: String,
    /// The slot index assigned to this session
    slot: usize,
    /// Set once the session is finished, by whichever path tears it down.
    /// Stops the session's inactivity watchdog thread and suppresses duplicate
    /// cleanup requests. Shared with the callbacks in `whip.rs`.
    cleanup_sent: Arc<AtomicBool>,
    /// When this session last delivered media. Shared with the session's appsink
    /// callbacks; see `SessionActivity`.
    activity: Arc<SessionActivity>,
}

/// A freshly created WHIP session, handed to `register_session`.
pub struct NewWhipSession {
    /// The resource_id assigned by the internal whipserversrc signaller
    pub resource_id: String,
    /// Internal port where this session's whipserversrc is listening
    pub port: u16,
    /// The whipserversrc element for this session
    pub element: gst::Element,
    /// The isolated pipeline for this session's whipserversrc
    pub session_pipeline: gst::Pipeline,
    /// The endpoint this session belongs to
    pub endpoint_id: String,
    /// The slot index assigned to this session
    pub slot: usize,
    /// Shared with the session's own callbacks; see `WhipSession::cleanup_sent`.
    pub cleanup_sent: Arc<AtomicBool>,
    /// Shared with the session's appsink callbacks; see `SessionActivity`.
    pub activity: Arc<SessionActivity>,
}

/// Manages WHIP sessions across all endpoints.
///
/// Thread-safe: uses RwLock for the sessions map and read-only Arc for endpoint configs.
pub struct WhipSessionManager {
    /// endpoint_id -> config (registered at pipeline start, immutable after that)
    endpoints: RwLock<HashMap<String, Arc<WhipEndpointConfig>>>,
    /// resource_id -> session (created/removed dynamically as clients connect/disconnect)
    sessions: RwLock<HashMap<String, WhipSession>>,
    /// Channel sender for cleanup requests from GStreamer callbacks
    cleanup_tx: mpsc::UnboundedSender<SessionCleanupRequest>,
    /// Channel receiver — taken once when starting the cleanup task
    cleanup_rx: Mutex<Option<mpsc::UnboundedReceiver<SessionCleanupRequest>>>,
    /// Ports for sessions that died before register_session was called, with the
    /// time they were marked. register_session checks this map and skips
    /// registration if the port is present and the mark has not expired.
    ///
    /// Marks expire after `PENDING_CLEANUP_TTL`: session ports come from the OS
    /// ephemeral range and are recycled, so a mark that is never claimed must not
    /// poison a later, unrelated session that happens to be given the same port.
    pending_cleanup_ports: Mutex<HashMap<u16, Instant>>,
}

/// How long a pending-cleanup mark stays valid. The window it has to cover is the
/// gap between a session dying and `register_session` running for it, which is
/// sub-second in practice.
const PENDING_CLEANUP_TTL: Duration = Duration::from_secs(30);

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
    /// Time since it last produced usable media, `None` if it must not be
    /// judged yet; see `SessionActivity::idle`.
    idle: Option<Duration>,
    /// Its slot's output counter, for comparison against the previous poll.
    last_usable: u64,
    /// Another path is already tearing it down, so its slot is about to free.
    dying: bool,
    cleanup_sent: Arc<AtomicBool>,
}

impl WhipSessionManager {
    pub fn new() -> Self {
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        Self {
            endpoints: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            cleanup_tx,
            cleanup_rx: Mutex::new(Some(cleanup_rx)),
            pending_cleanup_ports: Mutex::new(HashMap::new()),
        }
    }

    /// Get a clone of the cleanup channel sender.
    /// Pass this to `create_whipserversrc_for_session` so GStreamer callbacks can
    /// send cleanup requests.
    pub fn cleanup_sender(&self) -> mpsc::UnboundedSender<SessionCleanupRequest> {
        self.cleanup_tx.clone()
    }

    /// Start the background cleanup task.
    ///
    /// Receives cleanup requests from GStreamer callbacks and tears down dead sessions.
    /// Must be called once after the WhipSessionManager is created (from a tokio context).
    pub fn start_cleanup_task(self: &Arc<Self>) {
        let rx = self
            .cleanup_rx
            .lock()
            .unwrap()
            .take()
            .expect("start_cleanup_task called more than once");

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            Self::run_cleanup_loop(manager, rx).await;
        });
        info!("WhipSessionManager: Cleanup task started");
    }

    async fn run_cleanup_loop(
        manager: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<SessionCleanupRequest>,
    ) {
        while let Some(req) = rx.recv().await {
            info!(
                "WhipSessionManager: Auto-cleanup request for port {} (reason: {})",
                req.port, req.reason
            );

            // Try to find and remove the session by port
            let removed = manager.remove_session_by_port(req.port);

            match removed {
                Some((resource_id, element, session_pipeline, endpoint_id, _port, slot)) => {
                    // Release the slot
                    let webrtcbin_store =
                        if let Some(config) = manager.get_endpoint_config(&endpoint_id) {
                            config.release_slot(slot);
                            Some((
                                config.dynamic_webrtcbin_store.clone(),
                                config.instance_id.clone(),
                            ))
                        } else {
                            None
                        };

                    // Tear down session pipeline on a blocking thread.
                    // Keep element alive until after pipeline reaches NULL.
                    tokio::task::spawn_blocking(move || {
                        Self::teardown_session_pipeline(&session_pipeline);
                        drop(element);
                        // Remove stale webrtcbin entries so frontend stops showing dead stats
                        if let Some((store, block_id)) = webrtcbin_store {
                            Self::cleanup_dynamic_webrtcbin_store(&store, &block_id);
                        }
                    });

                    info!(
                        "WhipSessionManager: Auto-cleaned session '{}' for endpoint '{}' (slot {}, reason: {})",
                        resource_id, endpoint_id, slot, req.reason
                    );
                }
                None => {
                    // Session not registered yet (ICE failed before register_session).
                    // Mark port as pending cleanup so register_session skips it.
                    // The mark expires after PENDING_CLEANUP_TTL so it cannot poison
                    // a later session that is handed the same recycled port.
                    let mut pending = manager.pending_cleanup_ports.lock().unwrap();
                    pending.retain(|_, marked| marked.elapsed() < PENDING_CLEANUP_TTL);
                    pending.insert(req.port, Instant::now());
                    warn!(
                        "WhipSessionManager: Session on port {} not found, marked for pending cleanup (reason: {})",
                        req.port, req.reason
                    );
                }
            }
        }
        debug!("WhipSessionManager: Cleanup task exiting (channel closed)");
    }

    /// Register an endpoint configuration (called once per WHIP Input block at pipeline start).
    pub fn register_endpoint(&self, endpoint_id: String, config: WhipEndpointConfig) {
        info!(
            "WhipSessionManager: Registering endpoint '{}' (instance: {}, mode: {:?}, max_sessions: {})",
            endpoint_id, config.instance_id, config.mode, config.max_sessions
        );
        let mut endpoints = self.endpoints.write().unwrap();
        endpoints.insert(endpoint_id, Arc::new(config));
    }

    /// Get the endpoint configuration for creating new sessions.
    pub fn get_endpoint_config(&self, endpoint_id: &str) -> Option<Arc<WhipEndpointConfig>> {
        let endpoints = self.endpoints.read().unwrap();
        endpoints.get(endpoint_id).cloned()
    }

    /// Register a new session after a whipserversrc has been created.
    ///
    /// If the session's port is in the pending_cleanup_ports set (ICE failed before
    /// registration), the session is immediately torn down instead of being registered.
    /// Returns true if registered, false if immediately cleaned up.
    pub fn register_session(&self, session: NewWhipSession) -> bool {
        let NewWhipSession {
            resource_id,
            port,
            element,
            session_pipeline,
            endpoint_id,
            slot,
            cleanup_sent,
            activity,
        } = session;

        // Check if this port was marked for cleanup before we could register it
        {
            let mut pending = self.pending_cleanup_ports.lock().unwrap();
            pending.retain(|_, marked| marked.elapsed() < PENDING_CLEANUP_TTL);
            if pending.remove(&port).is_some() {
                // Nothing will tear this session down later, so stop its watchdog here.
                cleanup_sent.store(true, Ordering::SeqCst);
                warn!(
                    "WhipSessionManager: Session '{}' on port {} died before registration, tearing down immediately",
                    resource_id, port
                );
                // Release slot and tear down
                if let Some(config) = self.get_endpoint_config(&endpoint_id) {
                    config.release_slot(slot);
                }
                let pipeline = session_pipeline;
                std::thread::spawn(move || {
                    Self::teardown_session_pipeline(&pipeline);
                    drop(element);
                });
                return false;
            }
        }

        info!(
            "WhipSessionManager: Registering session '{}' on port {} for endpoint '{}' (slot {})",
            resource_id, port, endpoint_id, slot
        );
        let mut sessions = self.sessions.write().unwrap();
        sessions.insert(
            resource_id,
            WhipSession {
                port,
                element,
                session_pipeline,
                endpoint_id,
                slot,
                cleanup_sent,
                activity,
            },
        );
        true
    }

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
            } else if candidate
                .idle
                .is_some_and(|idle| idle >= TAKEOVER_IDLE_THRESHOLD)
            {
                // Win the flag every other teardown path uses, so the session is
                // cleaned up exactly once and its watchdog thread stops. The
                // cleanup task is what releases the slot; the next poll takes it.
                if !candidate.cleanup_sent.swap(true, Ordering::SeqCst) {
                    let idle_ms = candidate.idle.unwrap_or_default().as_millis();
                    warn!(
                        "WhipSessionManager: Displacing session '{}' on port {} ({} ms without usable media) so a new client can take its slot on endpoint '{}'",
                        candidate.resource_id, candidate.port, idle_ms, config.endpoint_id
                    );
                    let _ = self.cleanup_tx.send(SessionCleanupRequest {
                        port: candidate.port,
                        reason: format!(
                            "displaced by a new client after {} ms without usable media",
                            idle_ms
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
    /// A session `SessionActivity::idle` refuses to judge sorts last and is
    /// never displaced: it is still negotiating, or still inside `DECODE_GRACE`.
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
                cleanup_sent: s.cleanup_sent.clone(),
            })
            .max_by_key(|c| (c.dying, c.idle))
    }

    /// Look up the port for a session by resource_id.
    pub fn get_session_port(&self, resource_id: &str) -> Option<u16> {
        let sessions = self.sessions.read().unwrap();
        sessions.get(resource_id).map(|s| s.port)
    }

    /// Look up the port for a session, also returning the endpoint_id.
    pub fn get_session_info(&self, resource_id: &str) -> Option<(u16, String)> {
        let sessions = self.sessions.read().unwrap();
        sessions
            .get(resource_id)
            .map(|s| (s.port, s.endpoint_id.clone()))
    }

    /// Remove a session and return (element, session_pipeline, endpoint_id, port, slot) for teardown.
    pub fn remove_session(
        &self,
        resource_id: &str,
    ) -> Option<(gst::Element, gst::Pipeline, String, u16, usize)> {
        let mut sessions = self.sessions.write().unwrap();
        sessions.remove(resource_id).map(|s| {
            s.cleanup_sent.store(true, Ordering::SeqCst);
            (s.element, s.session_pipeline, s.endpoint_id, s.port, s.slot)
        })
    }

    /// Remove a session by its internal port (reverse lookup for auto-cleanup).
    /// Returns (resource_id, element, session_pipeline, endpoint_id, port, slot).
    fn remove_session_by_port(
        &self,
        port: u16,
    ) -> Option<(String, gst::Element, gst::Pipeline, String, u16, usize)> {
        let mut sessions = self.sessions.write().unwrap();
        let resource_id = sessions
            .iter()
            .find(|(_, s)| s.port == port)
            .map(|(k, _)| k.clone());

        if let Some(rid) = resource_id {
            sessions.remove(&rid).map(|s| {
                s.cleanup_sent.store(true, Ordering::SeqCst);
                (
                    rid,
                    s.element,
                    s.session_pipeline,
                    s.endpoint_id,
                    s.port,
                    s.slot,
                )
            })
        } else {
            None
        }
    }

    /// Remove all sessions for a given endpoint (called during pipeline stop).
    /// Returns (session_pipeline, element) pairs for teardown. The element must be
    /// kept alive until after the pipeline reaches NULL state.
    pub fn remove_all_sessions(&self, endpoint_id: &str) -> Vec<(gst::Pipeline, gst::Element)> {
        let mut sessions = self.sessions.write().unwrap();
        let resource_ids: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.endpoint_id == endpoint_id)
            .map(|(k, _)| k.clone())
            .collect();

        let mut result = Vec::new();
        for resource_id in &resource_ids {
            if let Some(session) = sessions.remove(resource_id) {
                info!(
                    "WhipSessionManager: Removing session '{}' for endpoint '{}'",
                    resource_id, endpoint_id
                );
                session.cleanup_sent.store(true, Ordering::SeqCst);
                result.push((session.session_pipeline, session.element));
            }
        }
        result
    }

    /// Unregister an endpoint (called during pipeline stop).
    pub fn unregister_endpoint(&self, endpoint_id: &str) {
        info!(
            "WhipSessionManager: Unregistering endpoint '{}'",
            endpoint_id
        );
        let mut endpoints = self.endpoints.write().unwrap();
        endpoints.remove(endpoint_id);
    }

    /// List all registered endpoint IDs.
    pub fn list_endpoints(&self) -> Vec<String> {
        let endpoints = self.endpoints.read().unwrap();
        endpoints.keys().cloned().collect()
    }

    /// Remove stale entries from the dynamic webrtcbin store for a block.
    ///
    /// After a session pipeline is set to NULL, its webrtcbin elements are dead
    /// but still referenced in the store (used for WebRTC stats in the frontend).
    /// This removes entries where the element is in NULL state.
    pub fn cleanup_dynamic_webrtcbin_store(store: &DynamicWebrtcbinStore, block_id: &str) {
        if let Ok(mut store) = store.lock() {
            if let Some(entries) = store.get_mut(block_id) {
                let before = entries.len();
                entries.retain(|(_, elem)| {
                    let (_, state, _) = elem.state(gst::ClockTime::ZERO);
                    state != gst::State::Null
                });
                let removed = before - entries.len();
                if removed > 0 {
                    debug!(
                        "WhipSessionManager: Removed {} stale webrtcbin entries for block '{}'",
                        removed, block_id
                    );
                }
            }
        }
    }

    /// Teardown a session's isolated pipeline.
    pub fn teardown_session_pipeline(session_pipeline: &gst::Pipeline) {
        let name = session_pipeline.name().to_string();
        debug!(
            "WhipSessionManager: Tearing down session pipeline '{}'",
            name
        );

        if let Err(e) = session_pipeline.set_state(gst::State::Null) {
            warn!(
                "WhipSessionManager: Failed to set session pipeline {} to Null: {:?}",
                name, e
            );
        }
    }
}

impl Default for WhipSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_session() -> (gst::Element, gst::Pipeline, Arc<AtomicBool>) {
        let _ = gst::init();
        let element = gst::ElementFactory::make("fakesrc")
            .build()
            .expect("fakesrc is part of gstreamer core");
        let pipeline = gst::Pipeline::new();
        (element, pipeline, Arc::new(AtomicBool::new(false)))
    }

    /// How long a fixture session has been running before the test looks at it.
    /// Comfortably past `DECODE_GRACE`, so a session that has produced nothing
    /// usable in that time is genuinely broken rather than still starting up.
    const RUNNING_FOR: Duration = Duration::from_secs(60);

    /// Tick `stamp` every 20 ms until `stop` is set — the rate the session
    /// appsink and the slot's output probe stamp a running publisher at.
    fn tick_until_stopped(stop: Arc<AtomicBool>, stamp: impl Fn() + Send + 'static) {
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                stamp();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    /// A publisher whose transport is gone: nothing has arrived and nothing has
    /// come out of its slot for `idle`. `idle` of zero is the case that matters
    /// — a client reconnecting the instant its publisher died looks, at that
    /// moment, exactly like a healthy one.
    fn dead_publisher(idle: Duration) -> Arc<SessionActivity> {
        let ran_for = RUNNING_FOR + idle;
        Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(ran_for, idle),
            Arc::new(ActivityStamp::backdated(ran_for, idle)),
        ))
    }

    /// A publisher that is still sending and whose media still comes out of its
    /// slot: both stamps tick, the way the appsink callback and the tee probe do
    /// for a running session. The caller sets `stop` to end it.
    fn live_publisher(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let output = Arc::new(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            output.clone(),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop.clone(), move || ingress.touch_ingress());
        tick_until_stopped(stop, move || output.touch());
        activity
    }

    /// The bug: RTP keeps arriving and has done for a minute, but nothing has
    /// ever come out of the slot's chain — the decoder never got a usable
    /// keyframe, or a consumer below the slot's tee is blocking it. Judged on
    /// arriving bytes alone this seat looks perfectly healthy.
    fn receiving_but_never_usable(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            Arc::new(ActivityStamp::new(Instant::now())),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress());
        activity
    }

    /// The other half of the bug: the seat decoded fine and then froze
    /// `stalled_for` ago, while RTP keeps arriving.
    fn receiving_but_stalled(stop: Arc<AtomicBool>, stalled_for: Duration) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            Arc::new(ActivityStamp::backdated(RUNNING_FOR, stalled_for)),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress());
        activity
    }

    /// Media has only just started arriving and nothing has decoded yet. Normal:
    /// H.264 cannot be decoded until a keyframe brings its parameter sets.
    fn still_prerolling(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(Duration::from_millis(200), Duration::ZERO),
            Arc::new(ActivityStamp::new(Instant::now())),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress());
        activity
    }

    /// A session that is still negotiating: connected, but nothing has arrived.
    fn no_media_yet() -> Arc<SessionActivity> {
        Arc::new(SessionActivity::new(
            Instant::now(),
            Arc::new(ActivityStamp::new(Instant::now())),
        ))
    }

    fn endpoint_config(max_sessions: usize) -> WhipEndpointConfig {
        WhipEndpointConfig {
            instance_id: "whip-input".to_string(),
            endpoint_id: "endpoint".to_string(),
            mode: StreamMode::AudioVideo,
            stun_server: None,
            turn_server: None,
            ice_transport_policy: "all".to_string(),
            pipeline_weak: Default::default(),
            decode: true,
            video_decoding: Arc::new((0..max_sessions).map(|_| AtomicBool::new(false)).collect()),
            jitterbuffer_latency_ms: 200,
            do_retransmission: true,
            dynamic_webrtcbin_store: Arc::new(Mutex::new(HashMap::new())),
            max_video_bitrate_kbps: 4000,
            max_sessions,
            slot_audio_appsrcs: Vec::new(),
            slot_video_appsrcs: Vec::new(),
            slot_output: (0..max_sessions)
                .map(|_| Arc::new(ActivityStamp::new(Instant::now())))
                .collect(),
            slot_assignments: Arc::new(RwLock::new(vec![None; max_sessions])),
        }
    }

    /// A manager with one single-slot endpoint whose only slot is held by a
    /// registered session with the given liveness — i.e. a full endpoint.
    fn full_endpoint(
        resource_id: &str,
        port: u16,
        activity: Arc<SessionActivity>,
    ) -> (
        Arc<WhipSessionManager>,
        Arc<WhipEndpointConfig>,
        Arc<AtomicBool>,
    ) {
        let manager = Arc::new(WhipSessionManager::new());
        manager.start_cleanup_task();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        let config = manager
            .get_endpoint_config("endpoint")
            .expect("endpoint was just registered");

        assert_eq!(
            config.allocate_slot(resource_id),
            Some(0),
            "the endpoint starts with its only slot free"
        );
        let cleanup_sent = register_with_activity(&manager, resource_id, port, activity);
        assert_eq!(
            config.allocate_slot("someone-else"),
            None,
            "the endpoint is now full"
        );
        (manager, config, cleanup_sent)
    }

    /// A session's `cleanup_sent` flag is the only way to stop its inactivity
    /// watchdog thread. Every path that removes a session must set it, or the
    /// watchdog outlives the session and asks for cleanup of a port that is gone.
    fn register(manager: &WhipSessionManager, resource_id: &str, port: u16) -> Arc<AtomicBool> {
        register_with_activity(manager, resource_id, port, dead_publisher(Duration::ZERO))
    }

    fn register_with_activity(
        manager: &WhipSessionManager,
        resource_id: &str,
        port: u16,
        activity: Arc<SessionActivity>,
    ) -> Arc<AtomicBool> {
        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: resource_id.to_string(),
            port,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            cleanup_sent: cleanup_sent.clone(),
            activity,
        });
        assert!(registered, "session should register");
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a freshly registered session is not finished"
        );
        cleanup_sent
    }

    #[test]
    fn remove_session_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-a", 40001);

        assert!(manager.remove_session("resource-a").is_some());

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "remove_session (WHIP DELETE) must stop the session's watchdog"
        );
    }

    #[test]
    fn remove_session_by_port_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-b", 40002);

        assert!(manager.remove_session_by_port(40002).is_some());

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "auto-cleanup by port must stop the session's watchdog"
        );
    }

    #[test]
    fn remove_all_sessions_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-c", 40003);

        assert_eq!(manager.remove_all_sessions("endpoint").len(), 1);

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "flow stop must stop the session's watchdog"
        );
    }

    /// A pending-cleanup mark that is never claimed must not survive: session ports
    /// come from the OS ephemeral range and are recycled, so a stale mark would
    /// destroy a later, unrelated session that is handed the same port.
    #[test]
    fn expired_pending_cleanup_mark_does_not_reject_a_recycled_port() {
        let manager = WhipSessionManager::new();
        {
            let mut pending = manager.pending_cleanup_ports.lock().unwrap();
            pending.insert(40004, Instant::now() - PENDING_CLEANUP_TTL * 2);
        }

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "resource-d".to_string(),
            port: 40004,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(
            registered,
            "an expired pending-cleanup mark must not poison a recycled port"
        );
        assert!(!cleanup_sent.load(Ordering::SeqCst));
    }

    /// The mark must still do its job inside the TTL: a session that died before
    /// registration is torn down rather than registered.
    #[test]
    fn fresh_pending_cleanup_mark_still_rejects_the_session() {
        let manager = WhipSessionManager::new();
        {
            let mut pending = manager.pending_cleanup_ports.lock().unwrap();
            pending.insert(40005, Instant::now());
        }

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "resource-e".to_string(),
            port: 40005,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(!registered, "a fresh mark must still reject the session");
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "a rejected session's watchdog must be stopped too"
        );
    }

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

    /// The measured case, and the one a single reading of the idle time gets
    /// wrong: the publisher is SIGKILLed and its client reconnects a few hundred
    /// milliseconds later, while the dead session still looks freshly fed. It is
    /// the counter staying frozen, not its value, that gives the session away.
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
        session.touch_ingress();

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

    /// A session that has not produced a buffer yet may just be slow to
    /// negotiate (ICE through a TURN relay). Displacing it would let two clients
    /// take turns evicting each other before either ever sends media.
    #[tokio::test]
    async fn a_session_that_has_not_delivered_media_yet_is_not_displaced() {
        let (manager, config, cleanup_sent) = full_endpoint("negotiating", 40012, no_media_yet());

        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;

        assert_eq!(slot, None, "a negotiating session must keep its slot");
        assert!(!cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("negotiating").is_some(),
            "the negotiating session must still be registered"
        );
    }
}
