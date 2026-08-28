//! WHIP (WebRTC-HTTP Ingestion Protocol) block builders.
//!
//! WHIP Output - Sends media to an external WHIP server:
//! - `whipclientsink` (new): Uses signaller interface, handles encoding internally
//! - `whipsink` (legacy): Simpler implementation, requires pre-encoded RTP input
//!
//! WHIP Input - Hosts a WHIP server for clients to connect and send media:
//! - `whipserversrc`: One element per WHIP client session, created dynamically
//!   by the WhipSessionManager when a client POSTs an SDP offer.
//!   Each session is assigned to a numbered slot with independent output chains
//!   (appsrc → decodebin → convert → tee per slot).

use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, APPSRC_MAX_BYTES_AUDIO,
    APPSRC_MAX_BYTES_VIDEO, APPSRC_MAX_TIME,
};
use crate::gst::ice_preflight;
use crate::gst::keyframe_request;
use crate::gst::rtp_hdrext;
use crate::whip_session_manager::{SessionCleanupRequest, WhipEndpointConfig};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use strom_types::block::StreamMode;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

/// WHIP Input block builder (hosts WHIP server).
pub struct WHIPInputBuilder;

impl BlockBuilder for WHIPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Output block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHIP Output")?;

        // Get implementation choice (default to stable whipsink)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whipclientsink")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whipclientsink(instance_id, properties, ctx)
        } else {
            build_whipsink(instance_id, properties, ctx)
        }
    }
}

impl BlockBuilder for WHIPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Input block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHIP Input")?;
        build_whipserversrc(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or(StreamMode::AudioVideo);

        let max_sessions = properties
            .get("max_sessions")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some((*i).max(1) as usize),
                _ => None,
            })
            .unwrap_or(1);

        let mut outputs = Vec::new();

        for slot in 0..max_sessions {
            // Slot 0 always uses unsuffixed names (video_out, audio_out) so existing
            // connections are preserved when max_sessions is increased.
            // Additional slots use numbered names (video_out_1, audio_out_1, ...).
            let (video_name, audio_name) = if slot == 0 {
                ("video_out".to_string(), "audio_out".to_string())
            } else {
                (format!("video_out_{}", slot), format!("audio_out_{}", slot))
            };

            if mode.has_video() {
                outputs.push(ExternalPad {
                    label: Some(format!("V{}", slot)),
                    name: video_name,
                    media_type: MediaType::Video,
                    internal_element_id: format!("video_out_tee_{}", slot),
                    internal_pad_name: "src_%u".to_string(),
                });
            }

            if mode.has_audio() {
                outputs.push(ExternalPad {
                    label: Some(format!("A{}", slot)),
                    name: audio_name,
                    media_type: MediaType::Audio,
                    internal_element_id: format!("audio_out_tee_{}", slot),
                    internal_pad_name: "src_%u".to_string(),
                });
            }
        }

        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }
}

// ============================================================================
// WHIP Input (whipserversrc - hosts WHIP server)
// ============================================================================

/// Parse jitterbuffer_latency_ms from properties (default: 400, negative clamps to 0).
fn parse_jitterbuffer_latency_ms(properties: &HashMap<String, PropertyValue>) -> u32 {
    properties
        .get("jitterbuffer_latency_ms")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some((*i).max(0) as u32),
            _ => None,
        })
        .unwrap_or(400)
}

/// Parse do_retransmission from properties (default: true).
fn parse_do_retransmission(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("do_retransmission")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true)
}

/// Build WHIP Input per-slot output chains.
///
/// At build time, per-slot chains are created in the main pipeline:
/// - decode=true: appsrc → decodebin → audioconvert → audioresample → tee (audio),
///   appsrc → decodebin → videoconvert → tee (video)
/// - decode=false: appsrc → tee (audio/video passthrough)
///
/// The actual whipserversrc elements are created dynamically per-session
/// by `create_whipserversrc_for_session` when clients connect. Each session
/// is assigned a slot and its appsink feeds the slot's appsrc.
fn build_whipserversrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Input per-slot output chains");

    // Get mode (audio_video, audio, or video)
    let mode = properties
        .get("mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(StreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or(StreamMode::AudioVideo);

    let max_sessions = properties
        .get("max_sessions")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some((*i).max(1) as usize),
            _ => None,
        })
        .unwrap_or(1);

    let decode = properties
        .get("decode")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true);

    // Jitterbuffer latency: how long to buffer before dropping/releasing packets.
    // Left unset, webrtcbin defaults to 200ms, which combined with
    // drop-on-latency=true (below) can be too tight for an initial video
    // keyframe's packet burst on a freshly-created per-session pipeline,
    // causing the whole video stream to stall (never reaching decodebin)
    // even though the packets arrived fine over the network.
    let jitterbuffer_latency_ms = parse_jitterbuffer_latency_ms(properties);
    let do_retransmission = parse_do_retransmission(properties);

    let max_video_bitrate_kbps = properties
        .get("max_video_bitrate")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some((*i).max(500) as u32),
            _ => None,
        })
        .unwrap_or(strom_types::whip::DEFAULT_MAX_VIDEO_BITRATE_KBPS);

    // Get endpoint_id (user-configurable, defaults to UUID)
    let endpoint_id = properties
        .get("endpoint_id")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    info!(
        "WHIP Input mode: {:?}, max_sessions: {}, decode: {}",
        mode, max_sessions, decode
    );

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();
    let mut slot_audio_appsrcs: Vec<gst_app::AppSrc> = Vec::new();
    let mut slot_video_appsrcs: Vec<gst_app::AppSrc> = Vec::new();

    // One flag per slot, set when decodebin exposes that slot's video pad.
    // A session stops asking the publisher for keyframes once its flag flips.
    let video_decoding: Arc<Vec<AtomicBool>> =
        Arc::new((0..max_sessions).map(|_| AtomicBool::new(false)).collect());

    for slot in 0..max_sessions {
        // Audio chain for this slot
        if mode.has_audio() {
            let appsrc_id = format!("{}:appsrc_audio_{}", instance_id, slot);
            let audio_out_tee_id = format!("{}:audio_out_tee_{}", instance_id, slot);

            let appsrc = gst_app::AppSrc::builder()
                .name(&appsrc_id)
                .format(gst::Format::Time)
                .is_live(true)
                .handle_segment_change(true)
                .max_bytes(APPSRC_MAX_BYTES_AUDIO)
                .max_time(APPSRC_MAX_TIME)
                .leaky_type(gst_app::AppLeakyType::Downstream)
                .automatic_eos(false)
                .build();

            let audio_out_tee = gst::ElementFactory::make("tee")
                .name(&audio_out_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audio_out_tee_{}: {}", slot, e))
                })?;

            if decode {
                let decodebin_id = format!("{}:decodebin_audio_{}", instance_id, slot);
                let audioconvert_id = format!("{}:audioconvert_{}", instance_id, slot);
                let audioresample_id = format!("{}:audioresample_{}", instance_id, slot);

                let decodebin = gst::ElementFactory::make("decodebin")
                    .name(&decodebin_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("decodebin_audio_{}: {}", slot, e))
                    })?;

                let audioconvert = gst::ElementFactory::make("audioconvert")
                    .name(&audioconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioconvert_{}: {}", slot, e))
                    })?;

                let audioresample = gst::ElementFactory::make("audioresample")
                    .name(&audioresample_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioresample_{}: {}", slot, e))
                    })?;

                // appsrc → decodebin
                internal_links.push((
                    ElementPadRef::pad(&appsrc_id, "src"),
                    ElementPadRef::pad(&decodebin_id, "sink"),
                ));

                // decodebin has dynamic pads — connect pad-added to link to audioconvert
                let audioconvert_weak = audioconvert.downgrade();
                decodebin.connect_pad_added(move |_dec, src_pad| {
                    if src_pad.direction() != gst::PadDirection::Src {
                        return;
                    }
                    if let Some(conv) = audioconvert_weak.upgrade() {
                        let sink = conv.static_pad("sink").unwrap();
                        if !sink.is_linked() {
                            if let Err(e) = src_pad.link(&sink) {
                                warn!("Failed to link decodebin audio pad to audioconvert: {:?}", e);
                            } else {
                                info!(
                                    "WHIP Input: decodebin audio pad linked to audioconvert for slot {}",
                                    slot
                                );
                            }
                        }
                    }
                });

                // audioconvert → audioresample → tee
                internal_links.push((
                    ElementPadRef::pad(&audioconvert_id, "src"),
                    ElementPadRef::pad(&audioresample_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&audioresample_id, "src"),
                    ElementPadRef::pad(&audio_out_tee_id, "sink"),
                ));

                elements.push((decodebin_id, decodebin));
                elements.push((audioconvert_id, audioconvert));
                elements.push((audioresample_id, audioresample));
            } else {
                // decode=false: clocksync → tee directly
                internal_links.push((
                    ElementPadRef::pad(&appsrc_id, "src"),
                    ElementPadRef::pad(&audio_out_tee_id, "sink"),
                ));
            }

            slot_audio_appsrcs.push(appsrc.clone());
            elements.push((appsrc_id, appsrc.upcast()));
            elements.push((audio_out_tee_id, audio_out_tee));
        }

        // Video chain for this slot
        if mode.has_video() {
            let appsrc_id = format!("{}:appsrc_video_{}", instance_id, slot);
            let video_out_tee_id = format!("{}:video_out_tee_{}", instance_id, slot);

            let appsrc = gst_app::AppSrc::builder()
                .name(&appsrc_id)
                .format(gst::Format::Time)
                .is_live(true)
                .handle_segment_change(true)
                .max_bytes(APPSRC_MAX_BYTES_VIDEO)
                .max_time(APPSRC_MAX_TIME)
                .leaky_type(gst_app::AppLeakyType::Downstream)
                .automatic_eos(false)
                .build();

            let video_out_tee = gst::ElementFactory::make("tee")
                .name(&video_out_tee_id)
                .property("allow-not-linked", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video_out_tee_{}: {}", slot, e))
                })?;

            if decode {
                let decodebin_id = format!("{}:decodebin_video_{}", instance_id, slot);
                let videoconvert_id = format!("{}:videoconvert_{}", instance_id, slot);

                let decodebin = gst::ElementFactory::make("decodebin")
                    .name(&decodebin_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("decodebin_video_{}: {}", slot, e))
                    })?;

                let videoconvert = gst::ElementFactory::make("videoconvert")
                    .name(&videoconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("videoconvert_{}: {}", slot, e))
                    })?;

                // clocksync → decodebin
                internal_links.push((
                    ElementPadRef::pad(&appsrc_id, "src"),
                    ElementPadRef::pad(&decodebin_id, "sink"),
                ));

                // decodebin has dynamic pads — connect pad-added to link to videoconvert
                let videoconvert_weak = videoconvert.downgrade();
                let video_decoding_for_pad = video_decoding.clone();
                decodebin.connect_pad_added(move |_dec, src_pad| {
                    if src_pad.direction() != gst::PadDirection::Src {
                        return;
                    }
                    // Video is decoding: whoever is asking for keyframes on
                    // this slot can stop.
                    if let Some(flag) = video_decoding_for_pad.get(slot) {
                        flag.store(true, Ordering::Relaxed);
                    }
                    if let Some(vc) = videoconvert_weak.upgrade() {
                        let sink = vc.static_pad("sink").unwrap();
                        if !sink.is_linked() {
                            if let Err(e) = src_pad.link(&sink) {
                                warn!("Failed to link decodebin video pad to videoconvert: {:?}", e);
                            } else {
                                info!(
                                    "WHIP Input: decodebin video pad linked to videoconvert for slot {}",
                                    slot
                                );
                            }
                        }
                    }
                });

                // videoconvert → tee
                internal_links.push((
                    ElementPadRef::pad(&videoconvert_id, "src"),
                    ElementPadRef::pad(&video_out_tee_id, "sink"),
                ));

                elements.push((decodebin_id, decodebin));
                elements.push((videoconvert_id, videoconvert));
            } else {
                // decode=false: clocksync → tee directly
                internal_links.push((
                    ElementPadRef::pad(&appsrc_id, "src"),
                    ElementPadRef::pad(&video_out_tee_id, "sink"),
                ));
            }

            slot_video_appsrcs.push(appsrc.clone());
            elements.push((appsrc_id, appsrc.upcast()));
            elements.push((video_out_tee_id, video_out_tee));
        }
    }

    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    info!(
        "WHIP Input configured: endpoint_id='{}', stun={:?}, turn={:?}, mode={:?}, decode={}, do_retransmission={}, max_sessions={} (whipserversrc created per-session)",
        endpoint_id, stun_server, turn_server, mode, decode, do_retransmission, max_sessions
    );

    // Register WHIP endpoint with the build context (port=0 placeholder, sessions get their own ports)
    ctx.register_whip_endpoint(instance_id, &endpoint_id, 0, mode);

    let slot_assignments = Arc::new(RwLock::new(vec![None; max_sessions]));

    // Store endpoint config for the session manager (will be wired up in start_flow)
    ctx.register_whip_endpoint_config(
        endpoint_id,
        WhipEndpointConfig {
            instance_id: instance_id.to_string(),
            endpoint_id: String::new(), // will be set by the manager
            mode,
            stun_server,
            turn_server,
            ice_transport_policy: ctx.ice_transport_policy().to_string(),
            pipeline_weak: gst::glib::WeakRef::new(),
            decode,
            video_decoding,
            jitterbuffer_latency_ms,
            do_retransmission,
            dynamic_webrtcbin_store: ctx.dynamic_webrtcbin_store(),
            max_video_bitrate_kbps,
            max_sessions,
            slot_audio_appsrcs,
            slot_video_appsrcs,
            slot_assignments,
        },
    );

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// How long a session may go without a buffer before the watchdog tears it down.
const INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Create a new whipserversrc element for a single WHIP client session.
///
/// Each session runs in its own isolated GStreamer pipeline to avoid
/// libnice issue #52 (multiple NiceAgent instances in the same pipeline
/// cause outbound UDP to stop working).
///
/// Media is bridged to the main pipeline via appsink→appsrc, where the
/// appsrc targets are the pre-built slot elements.
///
/// Returns (element, session_pipeline, port) on success.
/// `cleanup_sent` is owned by the caller so it can be handed to the session
/// manager alongside the session: every teardown path sets it, which both
/// suppresses duplicate cleanup requests and stops this session's inactivity
/// watchdog thread.
pub fn create_whipserversrc_for_session(
    config: &WhipEndpointConfig,
    slot: usize,
    cleanup_tx: tokio::sync::mpsc::UnboundedSender<SessionCleanupRequest>,
    cleanup_sent: Arc<AtomicBool>,
) -> Result<(gst::Element, gst::Pipeline, u16), String> {
    // Allocate a free port
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Failed to find free port: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get local address: {}", e))?
        .port();
    drop(listener);

    let host_addr = format!("http://127.0.0.1:{}", port);
    let session_uuid = Uuid::new_v4();
    let element_name = format!("{}:whipserversrc_{}", config.instance_id, session_uuid);

    info!(
        "WHIP Input: Creating whipserversrc '{}' on port {} in isolated pipeline (slot {})",
        element_name, port, slot
    );

    // Create an isolated pipeline for this session
    let session_pipeline = gst::Pipeline::builder()
        .name(format!("whip-session-{}", session_uuid))
        .build();

    // Create whipserversrc element
    let whipserversrc = gst::ElementFactory::make("whipserversrc")
        .name(&element_name)
        .build()
        .map_err(|e| format!("Failed to create whipserversrc: {}", e))?;

    // Set ICE server properties
    match config.stun_server {
        Some(ref stun) => whipserversrc.set_property("stun-server", stun),
        None => whipserversrc.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = config.turn_server {
        let turn_servers = gst::Array::new([turn]);
        whipserversrc.set_property("turn-servers", turn_servers);
    }

    // Set signaller host-addr
    let signaller = whipserversrc.property::<gst::glib::Object>("signaller");
    signaller.set_property("host-addr", &host_addr);

    whipserversrc.set_property("do-retransmission", config.do_retransmission);

    // Configure codec negotiation based on mode
    if config.mode.has_audio() {
        let audio_codecs = gst::Array::new(["OPUS"]);
        whipserversrc.set_property("audio-codecs", &audio_codecs);
    } else {
        let empty = gst::Array::new(Vec::<&str>::new());
        whipserversrc.set_property("audio-codecs", &empty);
    }
    if config.mode.has_video() {
        let video_codecs = gst::Array::new(["H264"]);
        whipserversrc.set_property("video-codecs", &video_codecs);
    } else {
        let empty = gst::Array::new(Vec::<&str>::new());
        whipserversrc.set_property("video-codecs", &empty);
    }

    // deep-element-added: ICE policy, TWCC, keyframe recovery, auto-cleanup on ICE failure
    let dynamic_webrtcbin_store = config.dynamic_webrtcbin_store.clone();
    let block_id_for_callback = config.instance_id.clone();
    let ice_transport_policy = config.ice_transport_policy.clone();
    let jitterbuffer_latency_ms = config.jitterbuffer_latency_ms;
    // `cleanup_sent` ensures only one cleanup request per session (shared across the
    // ICE callback, the inactivity watchdog and the session manager's teardown paths).
    let cleanup_sent_for_ice = cleanup_sent.clone();
    let cleanup_tx_for_ice = cleanup_tx.clone();

    if let Ok(bin) = whipserversrc.clone().downcast::<gst::Bin>() {
        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            // Workaround for GStreamer rtpjitterbuffer packet_spacing bug:
            // see comment in whep.rs build_whepsrc iterate_recurse for details.
            if element_name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                element.set_property("drop-on-latency", true);
                info!("WHIP Input: Set drop-on-latency=true on {}", element_name);
            }

            if element_name.starts_with("webrtcbin") {
                if element.has_property("ice-transport-policy") {
                    element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                    info!(
                        "WHIP Input: Set ice-transport-policy={} on webrtcbin {}",
                        ice_transport_policy, element_name
                    );
                }

                if element.has_property("latency") {
                    element.set_property("latency", jitterbuffer_latency_ms);
                    info!(
                        "WHIP Input: Set jitterbuffer latency={}ms on webrtcbin {}",
                        jitterbuffer_latency_ms, element_name
                    );
                }

                if let Ok(mut store) = dynamic_webrtcbin_store.lock() {
                    store
                        .entry(block_id_for_callback.clone())
                        .or_default()
                        .push(("whip-client".to_string(), element.clone()));
                }

                // Monitor ICE state and trigger auto-cleanup on failure
                let wrtc_name = element_name.to_string();
                let cleanup_tx = cleanup_tx_for_ice.clone();
                let cleanup_sent = cleanup_sent_for_ice.clone();
                element.connect_notify(Some("ice-connection-state"), move |elem, _pspec| {
                    let val = elem.property_value("ice-connection-state");
                    // The property is a GLib enum — extract the integer value
                    // via serialize (returns the nick like "connected") or
                    // via the raw glib enum value.
                    // Extract ICE state — try i32 first, fall back to serializing
                    // the GLib enum value to its nick string
                    let state_name = if let Ok(v) = val.get::<i32>() {
                        match v {
                            0 => "new",
                            1 => "checking",
                            2 => "connected",
                            3 => "completed",
                            4 => "failed",
                            5 => "disconnected",
                            6 => "closed",
                            _ => "unknown",
                        }
                        .to_string()
                    } else {
                        val.serialize()
                            .ok()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    };

                    let is_dead =
                        matches!(state_name.as_str(), "failed" | "disconnected" | "closed");

                    info!(
                        "WHIP Input: [SERVER] {} ice-connection-state = {}",
                        wrtc_name, state_name
                    );

                    if is_dead && !cleanup_sent.swap(true, Ordering::SeqCst) {
                        let reason = format!("ICE {}", state_name);
                        let _ = cleanup_tx.send(SessionCleanupRequest { port, reason });
                    }
                });
            }

            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            if factory_name == "rtpsession" && element.has_property("internal-session") {
                let internal: gst::glib::Object = element.property("internal-session");
                if internal.has_property("twcc-feedback-interval") {
                    let interval: u64 = 200_000_000;
                    internal.set_property("twcc-feedback-interval", interval);
                    info!(
                        "WHIP Input: Set twcc-feedback-interval=200ms on {}",
                        element_name
                    );
                }
            }

            // NOTE: a DISCONT-triggered keyframe request used to live here, on
            // the decoder's sink pad. It never ran, and could not have worked:
            // the decoder lives in the *main* pipeline, not in this session
            // pipeline, so the branch was never reached — and an upstream
            // force-key-unit sent from there dies at the main pipeline's
            // `appsrc` instead of crossing into the WebRTC source. Keyframe
            // requests are made from the session side instead, where they
            // reach the publisher. See `gst::keyframe_request`.
            None
        });
    }

    // Get the slot's appsrc refs — these are the targets in the main pipeline
    let slot_audio_appsrc: Option<gst_app::AppSrc> = config.slot_audio_appsrcs.get(slot).cloned();
    let slot_video_appsrc: Option<gst_app::AppSrc> = config.slot_video_appsrcs.get(slot).cloned();

    // Re-arm the slot: this session has to prove for itself that video decodes.
    // A previous session on the same slot left the flag set.
    let video_decoding = config.video_decoding.clone();
    if let Some(flag) = video_decoding.get(slot) {
        flag.store(false, Ordering::Relaxed);
    }

    // Shared timestamp offset for A/V sync across audio and video appsrcs.
    // Computed from the first buffer on either stream:
    //   offset = main_pipeline_running_time - buffer_pts
    // i64::MIN means "not yet computed".
    let shared_ts_offset = Arc::new(AtomicI64::new(i64::MIN));

    // Inactivity watchdog: tracks when the last buffer arrived on any stream.
    // A background thread checks this and triggers cleanup if no data arrives
    // for INACTIVITY_TIMEOUT (covers the case where ICE disconnect
    // notification doesn't fire from the isolated session pipeline).
    //
    // The wait is sliced rather than one long sleep so that `cleanup_sent` — set by
    // the ICE callback and by every teardown path in the session manager — ends the
    // thread promptly. A watchdog that outlived its session would send a cleanup
    // request for a port the manager no longer knows, and the manager would then mark
    // that (recycled) port pending cleanup for nothing.
    let last_buffer_epoch = Instant::now();
    let last_buffer_ms = Arc::new(AtomicU64::new(0));
    {
        let last_buffer_ms_watchdog = last_buffer_ms.clone();
        let cleanup_sent_watchdog = cleanup_sent.clone();
        let cleanup_tx_watchdog = cleanup_tx.clone();
        std::thread::Builder::new()
            .name(format!("whip-watchdog-{}", port))
            .spawn(move || {
                loop {
                    let deadline = Instant::now() + INACTIVITY_TIMEOUT;
                    if wait_until_deadline_or_stop(&cleanup_sent_watchdog, deadline) {
                        // Another path (ICE callback, DELETE, flow stop) finished
                        // with this session.
                        break;
                    }
                    let last = last_buffer_ms_watchdog.load(Ordering::Relaxed);
                    if last == 0 {
                        // No buffer received yet — keep waiting (session might still be negotiating)
                        continue;
                    }
                    let elapsed_ms = last_buffer_epoch.elapsed().as_millis() as u64;
                    let idle_ms = elapsed_ms.saturating_sub(last);
                    if idle_ms >= INACTIVITY_TIMEOUT.as_millis() as u64 {
                        if !cleanup_sent_watchdog.swap(true, Ordering::SeqCst) {
                            info!(
                                "WHIP Input: Inactivity timeout ({}s idle) on port {}, triggering cleanup",
                                idle_ms / 1000,
                                port
                            );
                            let _ = cleanup_tx_watchdog.send(SessionCleanupRequest {
                                port,
                                reason: format!("inactivity ({}s idle)", idle_ms / 1000),
                            });
                        }
                        break;
                    }
                }
            })
            .ok();
    }

    // pad-added: tee → fakesink (drain) + appsink (bridge to slot's appsrc)
    {
        let session_pipeline_weak = session_pipeline.downgrade();
        let main_pipeline_weak = config.pipeline_weak.clone();
        let prefix = element_name.clone();
        let stream_counter = Arc::new(AtomicUsize::new(0));
        let audio_connected = Arc::new(AtomicBool::new(false));
        let video_connected = Arc::new(AtomicBool::new(false));

        whipserversrc.connect_pad_added(move |_src, pad| {
            let pad_name = pad.name();
            let stream_num = stream_counter.fetch_add(1, Ordering::SeqCst);

            let session_pipeline: Option<gst::Pipeline> = session_pipeline_weak.upgrade();
            let Some(session_pipeline) = session_pipeline else {
                error!("WHIP Input: Session pipeline destroyed");
                return;
            };

            // Session pipeline: pad → tee → fakesink (drain) + appsink (bridge)
            let tee = match gst::ElementFactory::make("tee")
                .property("allow-not-linked", true)
                .build()
            {
                Ok(t) => t,
                Err(e) => {
                    error!("WHIP Input: Failed to create tee in pad-added: {}", e);
                    return;
                }
            };
            let fakesink = match gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
            {
                Ok(f) => f,
                Err(e) => {
                    error!("WHIP Input: Failed to create fakesink in pad-added: {}", e);
                    return;
                }
            };
            let appsink = gst_app::AppSink::builder()
                .name(format!("{}:{}_appsink_{}", prefix, pad_name, stream_num))
                .sync(false)
                .build();

            if let Err(e) = session_pipeline.add_many([&tee, &fakesink, appsink.upcast_ref()]) {
                error!("WHIP Input: Failed to add elements to session pipeline: {}", e);
                return;
            }
            if let Err(e) = pad.link(&tee.static_pad("sink").expect("tee has no sink pad")) {
                error!("WHIP Input: Failed to link pad to tee: {:?}", e);
                return;
            }
            if let (Some(tee_src1), Some(tee_src2)) = (
                tee.request_pad_simple("src_%u"),
                tee.request_pad_simple("src_%u"),
            ) {
                let _ = tee_src1.link(&fakesink.static_pad("sink").expect("fakesink has no sink pad"));
                let _ = tee_src2.link(&appsink.static_pad("sink").expect("appsink has no sink pad"));
            } else {
                error!("WHIP Input: Failed to request tee src pads");
                return;
            }
            let _ = tee.sync_state_with_parent();
            let _ = fakesink.sync_state_with_parent();
            let _ = appsink.sync_state_with_parent();

            // Determine which slot appsrc to feed based on pad type
            let target_appsrc: Option<gst_app::AppSrc> =
                if pad_name.starts_with("audio_") && !audio_connected.swap(true, Ordering::SeqCst)
                {
                    slot_audio_appsrc.clone()
                } else if pad_name.starts_with("video_")
                    && !video_connected.swap(true, Ordering::SeqCst)
                {
                    slot_video_appsrc.clone()
                } else {
                    None
                };

            if let Some(appsrc) = target_appsrc {
                // Bridge: appsink → slot appsrc with shared A/V timestamp offset.
                // The offset is computed once from the first buffer on either stream,
                // then applied to all buffers on both streams to preserve A/V sync.
                let media_type = if pad_name.starts_with("audio_") {
                    "audio"
                } else {
                    "video"
                };
                info!(
                    "WHIP Input: Pad {} (stream {}) → appsink → slot {} appsrc ({})",
                    pad_name, stream_num, slot, media_type
                );

                if media_type == "video" {
                    // A browser sends H.264 parameter sets only alongside a
                    // keyframe. If this session's first keyframe never arrives,
                    // the depayloader gets nothing but non-reference slices and
                    // can never output an access unit — decodebin exposes no
                    // pad and the flow's pipeline never leaves PAUSED, so WHEP
                    // viewers get nothing while audio plays fine.
                    //
                    // Ask for one. The event has to be sent here, on the WebRTC
                    // source's pad, because this is the only side of the
                    // appsink/appsrc boundary from which it reaches the
                    // publisher. It stops as soon as video decodes, so a
                    // healthy session is normally never asked at all.
                    let request_pad = pad.clone();
                    let request_flag = video_decoding.clone();
                    let request_slot = slot;
                    if let Err(e) = std::thread::Builder::new()
                        .name(format!("whip-keyframe-{}", request_slot))
                        .spawn(move || {
                            let policy = keyframe_request::KeyframeRequestPolicy::default();
                            let Some(flag) = request_flag.get(request_slot) else {
                                return;
                            };
                            let sent = keyframe_request::request_until_decoding(
                                policy,
                                flag,
                                std::thread::sleep,
                                |attempt| {
                                    debug!(
                                        "WHIP Input: video not decoding on slot {}, requesting keyframe (PLI) attempt {}/{}",
                                        request_slot, attempt, policy.attempts
                                    );
                                    request_pad.send_event(
                                        gst_video::UpstreamForceKeyUnitEvent::builder()
                                            .all_headers(true)
                                            .build(),
                                    );
                                },
                            );
                            if sent > 0 {
                                info!(
                                    "WHIP Input: requested a keyframe {} time(s) on slot {} (video had not started decoding)",
                                    sent, request_slot
                                );
                            }
                        })
                    {
                        warn!(
                            "WHIP Input: could not spawn keyframe requester for slot {}: {}",
                            slot, e
                        );
                    }
                }

                let ts_offset = shared_ts_offset.clone();
                let main_pipeline_for_ts = main_pipeline_weak.clone();
                let media_for_log = media_type.to_string();
                let last_buffer_ms_cb = last_buffer_ms.clone();
                let last_buffer_epoch_cb = last_buffer_epoch;

                appsink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            // Update inactivity watchdog
                            last_buffer_ms_cb.store(
                                last_buffer_epoch_cb.elapsed().as_millis() as u64,
                                Ordering::Relaxed,
                            );

                            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                            let pts = buffer.pts();

                            // Compute offset on the first buffer from either stream
                            let offset_ns = {
                                let current = ts_offset.load(Ordering::Relaxed);
                                if current != i64::MIN {
                                    current
                                } else if let (Some(pts_val), Some(main_pipeline)) =
                                    (pts, main_pipeline_for_ts.upgrade())
                                {
                                    let clock = main_pipeline.clock();
                                    let base_time = main_pipeline.base_time();
                                    if let (Some(clock), Some(base_time)) = (clock, base_time) {
                                        let now = clock.time();
                                        let running = now.saturating_sub(base_time);
                                        let offset = running.nseconds() as i64 - pts_val.nseconds() as i64;
                                        ts_offset.store(offset, Ordering::Relaxed);
                                        info!(
                                            "WHIP Input: Computed shared ts-offset={}ms from {} stream (slot {})",
                                            offset / 1_000_000,
                                            media_for_log,
                                            slot
                                        );
                                        offset
                                    } else {
                                        0
                                    }
                                } else {
                                    0
                                }
                            };

                            // Apply offset to buffer PTS
                            if offset_ns != 0 {
                                if let Some(pts_val) = pts {
                                    let adjusted = (pts_val.nseconds() as i64 + offset_ns).max(0) as u64;
                                    let mut new_buffer = buffer.copy();
                                    {
                                        let buf_ref = new_buffer.get_mut().unwrap();
                                        buf_ref.set_pts(gst::ClockTime::from_nseconds(adjusted));
                                    }
                                    let new_sample = gst::Sample::builder()
                                        .buffer(&new_buffer)
                                        .caps(&sample.caps().unwrap().to_owned())
                                        .build();
                                    let _ = appsrc.push_sample(&new_sample);
                                } else {
                                    let _ = appsrc.push_sample(&sample);
                                }
                            } else {
                                let _ = appsrc.push_sample(&sample);
                            }

                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
            } else {
                info!(
                    "WHIP Input: Pad {} (stream {}) → drain only (no slot appsrc or already connected)",
                    pad_name, stream_num
                );
            }
        });
    }

    // Add whipserversrc to the SESSION pipeline (not main pipeline)
    session_pipeline
        .add(&whipserversrc)
        .map_err(|e| format!("Failed to add whipserversrc to session pipeline: {}", e))?;

    // whipserversrc autoplugs RTP depayloaders inside its own bin, so this
    // pipeline needs the same gstreamer#5057 workaround as the main one.
    // Install while it is still NULL so no depayloader is missed.
    rtp_hdrext::install(&session_pipeline);

    // Set session pipeline to PLAYING and wait
    session_pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("Failed to set session pipeline to Playing: {:?}", e))?;

    let (result, current, _pending) = session_pipeline.state(gst::ClockTime::from_seconds(5));
    if result == Err(gst::StateChangeError) {
        return Err(format!(
            "Session pipeline state change to Playing failed (current: {:?})",
            current
        ));
    }
    info!(
        "WHIP Input: Session pipeline '{}' on port {}, state: {:?} (slot {})",
        session_pipeline.name(),
        port,
        current,
        slot
    );

    Ok((whipserversrc, session_pipeline, port))
}

// ============================================================================
// WHIP Output (whipclientsink / whipsink)
// ============================================================================

/// Build using the new whipclientsink (signaller-based) implementation
fn build_whipclientsink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Output using whipclientsink (new implementation)");

    // Get required WHIP endpoint
    let whip_endpoint = properties
        .get("whip_endpoint")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            BlockBuildError::InvalidProperty("whip_endpoint property required".to_string())
        })?;

    // Get optional auth token
    let auth_token = properties.get("auth_token").and_then(|v| {
        if let PropertyValue::String(s) = v {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        } else {
            None
        }
    });

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Create namespaced element IDs
    let whipclientsink_id = format!("{}:whipclientsink", instance_id);
    let audioconvert_id = format!("{}:audioconvert", instance_id);
    let audioresample_id = format!("{}:audioresample", instance_id);

    // Create audio processing elements
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

    // Create whipclientsink element
    let whipclientsink = gst::ElementFactory::make("whipclientsink")
        .name(&whipclientsink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whipclientsink: {}", e)))?;

    // Set ICE server properties (explicitly clear defaults when not configured,
    // since webrtcsink defaults to stun://stun.l.google.com:19302)
    match stun_server {
        Some(ref stun) => whipclientsink.set_property("stun-server", stun),
        None => whipclientsink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        let turn_servers = gst::Array::new([turn]);
        whipclientsink.set_property("turn-servers", turn_servers);
    }

    // Disable video codecs by setting video-caps to empty
    whipclientsink.set_property("video-caps", gst::Caps::new_empty());

    // Access the signaller child and set its properties
    let signaller = whipclientsink.property::<gst::glib::Object>("signaller");
    signaller.set_property("whip-endpoint", &whip_endpoint);

    if let Some(token) = &auth_token {
        signaller.set_property("auth-token", token);
    }

    // Read Opus encoder settings
    let opus_complexity = properties
        .get("opus_complexity")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_COMPLEXITY);

    let opus_bitrate = properties
        .get("opus_bitrate")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_BITRATE);

    // Configure internal elements via deep-element-added:
    // - ICE transport policy on webrtcbin
    // - Opus encoder settings on opusenc
    if let Ok(bin) = whipclientsink.clone().downcast::<gst::Bin>() {
        let ice_transport_policy = ctx.ice_transport_policy().to_string();
        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            if element_name.starts_with("webrtcbin") && element.has_property("ice-transport-policy")
            {
                element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                info!(
                    "WHIP (whipclientsink): Set ice-transport-policy={} on webrtcbin {}",
                    ice_transport_policy, element_name
                );
            }

            if element_name.starts_with("opusenc") {
                element.set_property("complexity", opus_complexity);
                element.set_property("bitrate", opus_bitrate);
                info!(
                    "WHIP (whipclientsink): Set opusenc {}: complexity={}, bitrate={}",
                    element_name, opus_complexity, opus_bitrate
                );
            }
            None
        });
    }

    debug!(
        "WHIP Output (whipclientsink) configured: endpoint={}, stun={:?}, turn={:?}",
        whip_endpoint, stun_server, turn_server
    );

    // Define internal links
    let internal_links = vec![
        (
            ElementPadRef::pad(&audioconvert_id, "src"),
            ElementPadRef::pad(&audioresample_id, "sink"),
        ),
        (
            ElementPadRef::pad(&audioresample_id, "src"),
            ElementPadRef::pad(&whipclientsink_id, "audio_0"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (audioconvert_id, audioconvert),
            (audioresample_id, audioresample),
            (whipclientsink_id, whipclientsink),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Build using the stable whipsink implementation
fn build_whipsink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Output using whipsink (stable)");

    let whip_endpoint = properties
        .get("whip_endpoint")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            BlockBuildError::InvalidProperty("whip_endpoint property required".to_string())
        })?;

    let auth_token = properties.get("auth_token").and_then(|v| {
        if let PropertyValue::String(s) = v {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        } else {
            None
        }
    });

    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    let whipsink_id = format!("{}:whipsink", instance_id);
    let audioconvert_id = format!("{}:audioconvert", instance_id);
    let audioresample_id = format!("{}:audioresample", instance_id);
    let opusenc_id = format!("{}:opusenc", instance_id);
    let rtpopuspay_id = format!("{}:rtpopuspay", instance_id);

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

    let opus_complexity = properties
        .get("opus_complexity")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_COMPLEXITY);

    let opus_bitrate = properties
        .get("opus_bitrate")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_BITRATE);

    let opusenc = gst::ElementFactory::make("opusenc")
        .name(&opusenc_id)
        .property("complexity", opus_complexity)
        .property("bitrate", opus_bitrate)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("opusenc: {}", e)))?;

    info!(
        "WHIP Output opusenc: complexity={}, bitrate={}",
        opus_complexity, opus_bitrate
    );

    let rtpopuspay = gst::ElementFactory::make("rtpopuspay")
        .name(&rtpopuspay_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("rtpopuspay: {}", e)))?;

    let whipsink = gst::ElementFactory::make("whipsink")
        .name(&whipsink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whipsink: {}", e)))?;

    whipsink.set_property("whip-endpoint", &whip_endpoint);
    // Explicitly clear defaults when not configured,
    // since whipsink defaults to stun://stun.l.google.com:19302
    match stun_server {
        Some(ref stun) => whipsink.set_property("stun-server", stun),
        None => whipsink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        whipsink.set_property("turn-server", turn);
    }
    if let Some(token) = &auth_token {
        whipsink.set_property("auth-token", token);
    }

    debug!(
        "WHIP Output (whipsink legacy) configured: endpoint={}, stun={:?}, turn={:?}",
        whip_endpoint, stun_server, turn_server
    );

    setup_incoming_rtp_handler(&whipsink, instance_id, ctx.ice_transport_policy());

    let internal_links = vec![
        (
            ElementPadRef::pad(&audioconvert_id, "src"),
            ElementPadRef::pad(&audioresample_id, "sink"),
        ),
        (
            ElementPadRef::pad(&audioresample_id, "src"),
            ElementPadRef::pad(&opusenc_id, "sink"),
        ),
        (
            ElementPadRef::pad(&opusenc_id, "src"),
            ElementPadRef::pad(&rtpopuspay_id, "sink"),
        ),
        (
            ElementPadRef::pad(&rtpopuspay_id, "src"),
            ElementPadRef::pad(&whipsink_id, "sink_0"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (audioconvert_id, audioconvert),
            (audioresample_id, audioresample),
            (opusenc_id, opusenc),
            (rtpopuspay_id, rtpopuspay),
            (whipsink_id, whipsink),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

// ============================================================================
// Block Definitions
// ============================================================================

/// Get metadata for WHIP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whip_output_definition(), whip_input_definition()]
}

/// WHIP Output block definition.
fn whip_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_output".to_string(),
        name: "WHIP Output".to_string(),
        description: "Sends audio via WebRTC WHIP protocol. Default uses stable whipsink element.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whipsink (stable) or whipclientsink (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whipsink".to_string(),
                            label: Some("whipsink (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whipclientsink".to_string(),
                            label: Some("whipclientsink (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whipsink".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "whip_endpoint".to_string(),
                label: "WHIP Endpoint".to_string(),
                description: "WHIP server endpoint URL (e.g., https://example.com/whip/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whip_endpoint".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "auth_token".to_string(),
                label: "Auth Token".to_string(),
                description: "Bearer token for authentication (optional)".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "auth_token".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "opus_complexity".to_string(),
                label: "Opus Complexity".to_string(),
                description: "Opus encoder complexity (0-10). Lower values use less CPU. 5 is recommended for real-time.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_OPUS_COMPLEXITY as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "opus_complexity".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "opus_bitrate".to_string(),
                label: "Opus Bitrate".to_string(),
                description: "Opus encoder bitrate in bps (4000-650000)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_OPUS_BITRATE as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "opus_bitrate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// WHIP Input block definition (server mode - hosts WHIP endpoint).
fn whip_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_input".to_string(),
        name: "WHIP Input".to_string(),
        description: "Hosts a WHIP server endpoint. Clients (browsers, OBS, encoders) connect via WHIP to send media. Access ingest page at /player/whip-ingest".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "What media to accept: audio + video, audio only, or video only".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio Only".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video Only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHIP endpoint. Leave empty to auto-generate. Ingest at /whip/{endpoint_id}".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "endpoint_id".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode incoming RTP to raw audio/video. When disabled, outputs RTP (application/x-rtp).".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "jitterbuffer_latency_ms".to_string(),
                label: "Jitterbuffer Latency (ms)".to_string(),
                description: "How long to buffer incoming RTP before releasing/dropping it. Too low can cause an initial video keyframe's packet burst to be dropped locally on connect, stalling video entirely even though packets arrived fine. Increase if video never starts.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(400)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "jitterbuffer_latency_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "do_retransmission".to_string(),
                label: "Retransmission (RTX)".to_string(),
                description: "Request retransmission of lost packets from the publisher (NACK-based). Without it, any packet loss forces a full keyframe request instead of a cheap resend.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "do_retransmission".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_video_bitrate".to_string(),
                label: "Max Video Bitrate (kbps)".to_string(),
                description: "Maximum video bitrate hint sent to the browser via SDP. The browser's encoder will ramp up to this value.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(
                    strom_types::whip::DEFAULT_MAX_VIDEO_BITRATE_KBPS as i64,
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_video_bitrate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_sessions".to_string(),
                label: "Max Sessions".to_string(),
                description: "Maximum number of simultaneous WHIP client connections. Each session gets its own independent output.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_sessions".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        // Note: external_pads here are the static defaults for audio_video mode with max_sessions=1.
        // Actual pads are determined dynamically by WHIPInputBuilder::get_external_pads().
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_out_tee_0".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_out_tee_0".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📹".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

// ============================================================================
// WHIP Output Helper (incoming RTP handler for legacy whipsink)
// ============================================================================

/// Setup handler for unexpected incoming RTP on WHIP sink elements.
fn setup_incoming_rtp_handler(
    whip_element: &gst::Element,
    instance_id: &str,
    ice_transport_policy: &str,
) {
    let bin = match whip_element.clone().downcast::<gst::Bin>() {
        Ok(b) => b,
        Err(_) => {
            warn!("WHIP: Element is not a bin, cannot setup incoming RTP handler");
            return;
        }
    };

    let ice_transport_policy = ice_transport_policy.to_string();

    bin.connect("deep-element-added", false, move |values| {
        let parent_bin = values[0].get::<gst::Bin>().unwrap();
        let element = values[2].get::<gst::Element>().unwrap();
        let element_name = element.name();
        let element_type = element.type_().name();

        if element_name.starts_with("webrtcbin") && element.has_property("ice-transport-policy") {
            element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
            info!(
                "WHIP: Set ice-transport-policy={} on webrtcbin {}",
                ice_transport_policy, element_name
            );
        }

        if element_type == "TransportReceiveBin" {
            info!(
                "WHIP: Found {} (parent bin: {}), checking for unlinked src pads",
                element_name,
                parent_bin.name()
            );

            let element_name_clone = element_name.to_string();

            for pad in element.src_pads() {
                let pad_name = pad.name();
                if !pad.is_linked() && pad_name.contains("rtp_src") {
                    let direct_parent = match element.parent() {
                        Some(p) => match p.downcast::<gst::Bin>() {
                            Ok(bin) => bin,
                            Err(_) => continue,
                        },
                        None => continue,
                    };

                    let fakesink_name = format!("whip_fakesink_{}", pad_name);
                    if let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                        .name(&fakesink_name)
                        .property("sync", false)
                        .property("async", false)
                        .build()
                    {
                        if direct_parent.add(&fakesink).is_err() {
                            continue;
                        }
                        let _ = fakesink.sync_state_with_parent();
                        if let Some(sink_pad) = fakesink.static_pad("sink") {
                            if pad.link(&sink_pad).is_ok() {
                                info!("WHIP: Linked {} to fakesink", pad_name);
                            }
                        }
                    }
                }
            }

            element.connect_pad_added(move |elem, pad| {
                let pad_name = pad.name();
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }

                info!("WHIP: {} pad-added: {}", element_name_clone, pad_name);

                if pad.is_linked() || !pad_name.contains("rtp_src") {
                    return;
                }

                let direct_parent = match elem.parent() {
                    Some(p) => match p.downcast::<gst::Bin>() {
                        Ok(bin) => bin,
                        Err(_) => return,
                    },
                    None => return,
                };

                let fakesink_name = format!("whip_fakesink_{}", pad_name);
                if let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                    .name(&fakesink_name)
                    .property("sync", false)
                    .property("async", false)
                    .build()
                {
                    if direct_parent.add(&fakesink).is_err() {
                        return;
                    }
                    let _ = fakesink.sync_state_with_parent();
                    if let Some(sink_pad) = fakesink.static_pad("sink") {
                        if pad.link(&sink_pad).is_ok() {
                            info!("WHIP: Linked new pad {} to fakesink", pad_name);
                        }
                    }
                }
            });
        }

        None
    });

    info!("WHIP: Incoming RTP handler installed for {}", instance_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(entries: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn jitterbuffer_latency_ms_defaults_to_400() {
        assert_eq!(parse_jitterbuffer_latency_ms(&props(&[])), 400);
    }

    #[test]
    fn jitterbuffer_latency_ms_respects_explicit_value() {
        assert_eq!(
            parse_jitterbuffer_latency_ms(&props(&[(
                "jitterbuffer_latency_ms",
                PropertyValue::Int(150)
            )])),
            150
        );
    }

    #[test]
    fn jitterbuffer_latency_ms_clamps_negative_to_zero() {
        assert_eq!(
            parse_jitterbuffer_latency_ms(&props(&[(
                "jitterbuffer_latency_ms",
                PropertyValue::Int(-50)
            )])),
            0
        );
    }

    #[test]
    fn do_retransmission_defaults_to_true() {
        assert!(parse_do_retransmission(&props(&[])));
    }

    #[test]
    fn do_retransmission_respects_explicit_true() {
        assert!(parse_do_retransmission(&props(&[(
            "do_retransmission",
            PropertyValue::Bool(true)
        )])));
    }

    #[test]
    fn do_retransmission_respects_explicit_false() {
        assert!(!parse_do_retransmission(&props(&[(
            "do_retransmission",
            PropertyValue::Bool(false)
        )])));
    }

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

    /// The block property must reach the `WhipEndpointConfig` handed to the
    /// session manager, which is the value `create_whipserversrc_for_session`
    /// applies to `whipserversrc`.
    #[test]
    fn do_retransmission_reaches_whip_endpoint_config() {
        let _ = gst::init();

        for expected in [true, false] {
            let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
            build_whipserversrc(
                "whip-rtx-test",
                &props(&[("do_retransmission", PropertyValue::Bool(expected))]),
                &ctx,
            )
            .expect("build_whipserversrc failed");

            let configs = ctx.take_whip_endpoint_configs();
            assert_eq!(configs.len(), 1, "expected exactly one endpoint config");
            assert_eq!(configs[0].1.do_retransmission, expected);
        }
    }
}
