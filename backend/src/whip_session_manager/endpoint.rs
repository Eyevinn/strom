//! Per-endpoint configuration and slot allocation.

use crate::blocks::DynamicWebrtcbinStore;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use strom_types::block::StreamMode;
use tracing::{debug, info, warn};

use super::ActivityStamp;

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
    /// Whether the per-session rtpbin's jitterbuffers drop queued packets that
    /// exceed `jitterbuffer_latency_ms` rather than holding them.
    pub drop_on_latency: bool,
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
