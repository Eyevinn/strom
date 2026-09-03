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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Per-slot `decodebin` elements (main pipeline side, created at build
    /// time), indexed by slot. Locked while the slot has no publisher so it
    /// cannot hold the pipeline short of PLAYING; `allocate_slot` unlocks them.
    /// Empty when the endpoint runs with `decode=false`.
    pub slot_decodebins: Vec<Vec<gst::glib::WeakRef<gst::Element>>>,
    /// Slot assignments: slot index → Option<resource_id>
    /// Protected by RwLock for concurrent access from HTTP handlers.
    pub slot_assignments: Arc<RwLock<Vec<Option<String>>>>,
}

impl WhipEndpointConfig {
    /// Allocate a free slot for a new session.
    /// Returns the slot index, or None if all slots are occupied.
    pub fn allocate_slot(&self, resource_id: &str) -> Option<usize> {
        let allocated = {
            let mut slots = self.slot_assignments.write().unwrap();
            let mut allocated = None;
            for (i, slot) in slots.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(resource_id.to_string());
                    info!(
                        "WhipEndpointConfig: Allocated slot {} for session '{}'",
                        i, resource_id
                    );
                    allocated = Some(i);
                    break;
                }
            }
            allocated
        };

        // A publisher is on its way, so the slot's decode chain can join the
        // pipeline's state changes. The SDP exchange is well ahead of the first
        // RTP packet: ICE and DTLS still have to complete before media arrives.
        if let Some(slot) = allocated {
            self.activate_slot_decoders(slot);
        }
        allocated
    }

    /// Bring a slot's `decodebin` elements into the running pipeline.
    ///
    /// They are built with their state locked (see `prepare_idle_decodebin` in
    /// the WHIP block builder). Idempotent: a slot reused by a later session
    /// re-syncs a decodebin that is already running.
    fn activate_slot_decoders(&self, slot: usize) {
        let Some(decodebins) = self.slot_decodebins.get(slot) else {
            return;
        };
        for weak in decodebins {
            let Some(decodebin) = weak.upgrade() else {
                // Pipeline already torn down.
                continue;
            };
            decodebin.set_locked_state(false);
            if let Err(e) = decodebin.sync_state_with_parent() {
                warn!(
                    "WhipEndpointConfig: Failed to sync {} with pipeline state: {}",
                    decodebin.name(),
                    e
                );
            } else {
                debug!(
                    "WhipEndpointConfig: Activated {} for slot {}",
                    decodebin.name(),
                    slot
                );
            }
        }
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
            },
        );
        true
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

    /// A session's `cleanup_sent` flag is the only way to stop its inactivity
    /// watchdog thread. Every path that removes a session must set it, or the
    /// watchdog outlives the session and asks for cleanup of a port that is gone.
    fn register(manager: &WhipSessionManager, resource_id: &str, port: u16) -> Arc<AtomicBool> {
        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: resource_id.to_string(),
            port,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            cleanup_sent: cleanup_sent.clone(),
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
        });

        assert!(!registered, "a fresh mark must still reject the session");
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "a rejected session's watchdog must be stopped too"
        );
    }
}
