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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
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
    /// Ports for sessions that died before register_session was called.
    /// register_session checks this set and skips registration if the port is present.
    pending_cleanup_ports: Mutex<HashSet<u16>>,
}

impl WhipSessionManager {
    pub fn new() -> Self {
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        Self {
            endpoints: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            cleanup_tx,
            cleanup_rx: Mutex::new(Some(cleanup_rx)),
            pending_cleanup_ports: Mutex::new(HashSet::new()),
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
                    let mut pending = manager.pending_cleanup_ports.lock().unwrap();
                    pending.insert(req.port);
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
    pub fn register_session(
        &self,
        resource_id: String,
        port: u16,
        element: gst::Element,
        session_pipeline: gst::Pipeline,
        endpoint_id: String,
        slot: usize,
    ) -> bool {
        // Check if this port was marked for cleanup before we could register it
        {
            let mut pending = self.pending_cleanup_ports.lock().unwrap();
            if pending.remove(&port) {
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
        sessions
            .remove(resource_id)
            .map(|s| (s.element, s.session_pipeline, s.endpoint_id, s.port, s.slot))
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
