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
use crate::whip_session_manager::{
    Medium, SessionActivity, SessionCleanupRequest, SlotOutput, WhipEndpointConfig,
    WhipSlotLiveness,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
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

/// Keep a slot with no publisher from holding the pipeline out of PLAYING.
///
/// A `decodebin` cannot complete READY->PAUSED until data arrives and it can
/// typefind, and a pipeline with any child still ASYNC never completes its own
/// transition. Two guards, for the two moments this bites:
/// - Locked state keeps an idle slot out of the pipeline's state changes
///   entirely: it sits in NULL and contributes nothing to the aggregated state.
/// - `async-handling` makes the decodebin absorb its own ASYNC once unlocked,
///   so a slot claimed by a session that then sends no media cannot pull a
///   running pipeline back out of PLAYING.
///
/// Neither hides a real preroll failure: a decodebin that errors still posts
/// its ERROR to the pipeline bus.
fn prepare_idle_decodebin(decodebin: &gst::Element) {
    decodebin.set_property("async-handling", true);
    decodebin.set_locked_state(true);
}

/// Stamp one medium of a slot's output for every buffer that reaches its tee.
///
/// The tee's sink pad is the far end of the slot's chain: with `decode=true`
/// everything upstream of it — `decodebin`, the converter — has already run, and
/// everything downstream is a flow consumer. A buffer here is media the flow can
/// actually use, which is the thing the session's appsink cannot see. The appsink
/// sits in the isolated session pipeline, upstream of all of this, so it stamps
/// bytes arriving over the network and nothing more.
///
/// It is also where a stall shows up: a blocked consumer backs pressure up
/// through the tee, and the probe simply stops firing while RTP keeps arriving.
///
/// BUFFER probes fire per buffer, so the callback is one clock read (`Instant`
/// takes the vDSO fast path) and a couple of relaxed atomic ops — no lock, no
/// allocation, no formatting. See `ActivityStamp::touch`.
fn stamp_slot_output(tee: &gst::Element, output: Arc<SlotOutput>, slot: usize, medium: Medium) {
    let Some(sink_pad) = tee.static_pad("sink") else {
        // Unreachable: a tee has a static sink pad. If it ever were not, nothing
        // would stamp this slot and every session on it would be reaped once its
        // decode grace ran out — recoverable, unlike a panic in a block build,
        // and this line is what makes it diagnosable.
        warn!(
            "WHIP Input: slot {} {} output tee has no sink pad, cannot track its liveness",
            slot,
            medium.as_str()
        );
        return;
    };
    sink_pad.add_probe(
        // BUFFER_LIST as well: nothing on this chain batches today, but an
        // element that started to would silently stop the stamp otherwise.
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, _info| {
            output.touch(medium);
            gst::PadProbeReturn::Ok
        },
    );
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
///
/// A slot's `decodebin` starts with its state locked (see
/// `prepare_idle_decodebin`); `WhipEndpointConfig::allocate_slot` unlocks it
/// when a session claims the slot.
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
    // Per-slot decodebins, locked until a session claims the slot. Weak refs:
    // the pipeline owns them.
    let mut slot_decodebins: Vec<Vec<gst::glib::WeakRef<gst::Element>>> = Vec::new();

    // One flag per slot, set when decodebin exposes that slot's video pad.
    // A session stops asking the publisher for keyframes once its flag flips.
    let video_decoding: Arc<Vec<AtomicBool>> =
        Arc::new((0..max_sessions).map(|_| AtomicBool::new(false)).collect());

    // One record per slot, one stamp per medium inside it, written by a probe on
    // that slot's output tees below. This is where a session's media becomes
    // usable to the flow, so this is where its liveness is measured; see
    // `SessionActivity` and `WhipSlotLiveness`. All slots share an epoch — the
    // stamps are only ever compared against their own readings.
    let output_epoch = Instant::now();
    let slot_output: Vec<Arc<SlotOutput>> = (0..max_sessions)
        .map(|_| Arc::new(SlotOutput::new(output_epoch)))
        .collect();

    for (slot, output_stamp) in slot_output.iter().enumerate() {
        let mut decodebins_for_slot: Vec<gst::glib::WeakRef<gst::Element>> = Vec::new();

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

                prepare_idle_decodebin(&decodebin);
                decodebins_for_slot.push(decodebin.downgrade());

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

            stamp_slot_output(&audio_out_tee, output_stamp.clone(), slot, Medium::Audio);

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

                prepare_idle_decodebin(&decodebin);
                decodebins_for_slot.push(decodebin.downgrade());

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

            stamp_slot_output(&video_out_tee, output_stamp.clone(), slot, Medium::Video);

            slot_video_appsrcs.push(appsrc.clone());
            elements.push((appsrc_id, appsrc.upcast()));
            elements.push((video_out_tee_id, video_out_tee));
        }

        slot_decodebins.push(decodebins_for_slot);
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

    // A slot medium that stops producing stalls nothing — its appsrc is simply
    // never pushed to — so the flow's stalled-pad-task scan cannot see it. Hand
    // the scan the stamps instead; see `WhipSlotLiveness`.
    ctx.register_block_liveness(
        instance_id,
        Arc::new(WhipSlotLiveness::new(
            endpoint_id.clone(),
            mode,
            slot_output.clone(),
            slot_assignments.clone(),
        )),
    );

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
            slot_decodebins,
            slot_output,
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

/// How long a session may go without producing usable media before the watchdog
/// tears it down; see `SessionActivity::idle`.
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

/// Block until the session has gone `timeout` without producing usable media, or
/// until `stop` is set.
///
/// Returns `Some(idle_ms)` once the idle time crosses `timeout`, or `None` if a
/// teardown path set `stop` first.
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
fn wait_for_inactivity(
    stop: &AtomicBool,
    activity: &SessionActivity,
    timeout: std::time::Duration,
) -> Option<u64> {
    loop {
        if wait_until_deadline_or_stop(stop, Instant::now() + WATCHDOG_POLL) {
            return None;
        }
        let Some(idle) = activity.idle() else {
            continue;
        };
        if idle >= timeout {
            return Some(idle.as_millis() as u64);
        }
    }
}

/// A whipserversrc session that has been created and is playing, handed back to
/// the HTTP handler so it can register the session with the manager.
pub struct CreatedSession {
    pub element: gst::Element,
    pub session_pipeline: gst::Pipeline,
    /// Internal port the session's whipserversrc is listening on.
    pub port: u16,
    /// Liveness handle, shared with the appsink callbacks that feed the slot.
    pub activity: Arc<SessionActivity>,
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
/// `cleanup_sent` is owned by the caller so it can be handed to the session
/// manager alongside the session: every teardown path sets it, which both
/// suppresses duplicate cleanup requests and stops this session's inactivity
/// watchdog thread.
pub fn create_whipserversrc_for_session(
    config: &WhipEndpointConfig,
    slot: usize,
    cleanup_tx: tokio::sync::mpsc::UnboundedSender<SessionCleanupRequest>,
    cleanup_sent: Arc<AtomicBool>,
) -> Result<CreatedSession, String> {
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

    // Liveness for this session, in the shape the session manager reads it: it
    // has to tell a slot that still has a publisher producing media behind it
    // from one whose publisher went away without a WHIP DELETE, or whose media
    // arrives but never comes out of the slot's chain.
    let slot_output = match config.slot_output.get(slot) {
        Some(output) => output.clone(),
        None => {
            // Unreachable: one record is built per slot. The orphan below is
            // never written, so this session would be reaped once its decode
            // grace ran out; this line is what makes that diagnosable.
            warn!(
                "WHIP Input: no output stamps for slot {}, its liveness cannot be tracked",
                slot
            );
            Arc::new(SlotOutput::new(Instant::now()))
        }
    };
    let activity = Arc::new(SessionActivity::new(Instant::now(), slot_output));

    // Inactivity watchdog. A background thread triggers cleanup once the session
    // has gone INACTIVITY_TIMEOUT without producing usable media — which covers
    // both a transport that went away without the ICE disconnect notification
    // reaching this isolated session pipeline, and a seat that keeps receiving
    // RTP while nothing decodes out the other end. Neither is worth a slot.
    //
    // The wait is sliced rather than one long sleep so that `cleanup_sent` — set by
    // the ICE callback and by every teardown path in the session manager — ends the
    // thread promptly. A watchdog that outlived its session would send a cleanup
    // request for a port the manager no longer knows, and the manager would then mark
    // that (recycled) port pending cleanup for nothing.
    {
        let activity_watchdog = activity.clone();
        let cleanup_sent_watchdog = cleanup_sent.clone();
        let cleanup_tx_watchdog = cleanup_tx.clone();
        std::thread::Builder::new()
            .name(format!("whip-watchdog-{}", port))
            .spawn(move || {
                // Returns None when another path (ICE callback, DELETE, flow stop)
                // finished with this session first.
                let Some(idle_ms) = wait_for_inactivity(
                    &cleanup_sent_watchdog,
                    &activity_watchdog,
                    INACTIVITY_TIMEOUT,
                ) else {
                    return;
                };
                if !cleanup_sent_watchdog.swap(true, Ordering::SeqCst) {
                    info!(
                        "WHIP Input: Inactivity timeout ({}ms without usable media) on port {}, triggering cleanup",
                        idle_ms, port
                    );
                    let _ = cleanup_tx_watchdog.send(SessionCleanupRequest {
                        port,
                        reason: format!("inactivity ({}ms without usable media)", idle_ms),
                    });
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
        let activity_for_pads = activity.clone();

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
                let activity_cb = activity_for_pads.clone();
                // Resolved here, not per buffer: the pad's media type is fixed.
                let pad_is_audio = media_type == "audio";

                appsink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            // Media arriving from the publisher. Half of this
                            // session's liveness; the other half is stamped on
                            // the slot's output tee in the main pipeline.
                            activity_cb.touch_ingress(pad_is_audio);

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

    Ok(CreatedSession {
        element: whipserversrc,
        session_pipeline,
        port,
        activity,
    })
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
    use crate::whip_session_manager::{ActivityStamp, SlotMediumStall, StallThresholds};

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
        let output = Arc::new(SlotOutput::new(Instant::now()));
        let activity = Arc::new(SessionActivity::new(Instant::now(), output.clone()));

        // One buffer that both arrives and comes out of the slot, then nothing.
        let publisher = activity.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            publisher.touch_ingress(true);
            output.touch(Medium::Audio);
        });

        let started = Instant::now();
        let idle_ms = wait_for_inactivity(&stop, &activity, timeout)
            .expect("watchdog must report inactivity, not a stop");
        let detection = started.elapsed();

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
            Arc::new(SlotOutput::new(Instant::now())),
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
            ActivityStamp::new(Instant::now()),
            Arc::new(SlotOutput::new(Instant::now())),
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
            matches!(reaped, Ok(Some(ms)) if ms >= INACTIVITY_TIMEOUT.as_millis() as u64),
            "a seat receiving RTP it never decodes must be reaped, not held \
             alive by its arriving-bytes counter: {:?}",
            reaped
        );
    }

    /// Assemble a built block's elements into a pipeline the way the flow
    /// builder does: add them all, then make the links the block asked for.
    /// `decodebin`'s src pad is dynamic and is linked by the block's own
    /// `pad-added` handler, so it is deliberately absent from `internal_links`.
    fn assemble(result: &BlockBuildResult) -> gst::Pipeline {
        let pipeline = gst::Pipeline::new();
        for (_, element) in &result.elements {
            pipeline.add(element).expect("add element");
        }
        let by_id: HashMap<&str, &gst::Element> = result
            .elements
            .iter()
            .map(|(id, element)| (id.as_str(), element))
            .collect();
        for (from, to) in &result.internal_links {
            let src = by_id[from.element_id.as_str()]
                .static_pad(from.pad_name.as_deref().unwrap_or("src"))
                .expect("source pad");
            let sink = by_id[to.element_id.as_str()]
                .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
                .expect("sink pad");
            src.link(&sink).expect("internal link");
        }
        pipeline
    }

    fn i420_frame(index: u64) -> gst::Buffer {
        let mut buffer = gst::Buffer::with_size(64 * 64 * 3 / 2).expect("allocate frame");
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(index * 33));
            buffer.set_duration(gst::ClockTime::from_mseconds(33));
        }
        buffer
    }

    /// Poll until `check` holds, up to five seconds. Buffers cross a pipeline on
    /// its own streaming threads, so a test cannot read the result straight after
    /// pushing.
    fn wait_for(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("timed out waiting for {}", what);
    }

    /// The signal a WHIP slot is judged by has to mean "this session is producing
    /// media the flow can use", so it is stamped at the end of the slot's chain,
    /// past `decodebin` and the converter. The session's own appsink is upstream
    /// of all of that, in a separate pipeline, and only ever sees bytes arrive.
    ///
    /// Both halves matter, so this checks both: media that gets through stamps
    /// the slot, and media that arrives but cannot get through does not. The
    /// second half is the seat that holds a slot for 45 minutes while receiving
    /// RTP the whole time — here its tee is blocked by a stuck consumer, which is
    /// what a stalled recorder branch does to it in a real flow.
    #[test]
    fn the_slot_stamp_follows_media_out_of_the_decode_chain_not_into_it() {
        let _ = gst::init();

        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        let result = build_whipserversrc(
            "whip-liveness-test",
            &props(&[
                ("mode", PropertyValue::String("video".to_string())),
                ("decode", PropertyValue::Bool(true)),
                ("max_sessions", PropertyValue::Int(1)),
            ]),
            &ctx,
        )
        .expect("build_whipserversrc failed");

        let configs = ctx.take_whip_endpoint_configs();
        let config = &configs[0].1;
        let output = config.slot_output[0].clone();
        let stamp = || output.stamp(Medium::Video).last();
        let appsrc = config.slot_video_appsrcs[0].clone();
        assert_eq!(stamp(), 0, "nothing has gone through the slot's chain yet");

        let pipeline = assemble(&result);

        // The slot's `decodebin` is built with its state locked so an unclaimed
        // slot cannot hold the pipeline short of PLAYING. Claiming the slot is
        // what unlocks it, exactly as a WHIP POST does.
        assert_eq!(
            config.allocate_slot("test-session"),
            Some(0),
            "the endpoint starts with its only slot free"
        );

        // Somewhere for the slot's tee to push, and a pad this test can block to
        // stall the chain.
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .expect("fakesink is part of gstreamer core");
        pipeline.add(&sink).expect("add fakesink");
        let tee = result
            .elements
            .iter()
            .find(|(id, _)| id.ends_with(":video_out_tee_0"))
            .map(|(_, element)| element.clone())
            .expect("the block builds an output tee per slot");
        let tee_src = tee.request_pad_simple("src_%u").expect("tee src pad");
        tee_src
            .link(&sink.static_pad("sink").unwrap())
            .expect("link tee to fakesink");

        appsrc.set_caps(Some(
            &gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 64i32)
                .field("height", 64i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        ));
        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline to PLAYING");

        for index in 0..30 {
            appsrc.push_buffer(i420_frame(index)).expect("push frame");
        }
        wait_for("the slot's stamp to follow the first frames", || {
            stamp() != 0
        });

        // Now stall the slot's consumer, the way a stuck recorder branch does:
        // hold the streaming thread inside a probe until the test releases it.
        // Returning from the callback would let the buffer straight through.
        let release = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let block = tee_src
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, move |_, _| {
                while !gate.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                gst::PadProbeReturn::Ok
            })
            .expect("block probe");
        // One buffer still reaches the tee's sink pad — it is the one that runs
        // into the block — and stamps the slot on its way. Let it, and read the
        // stamp afterwards: everything behind it is stuck upstream.
        for index in 30..60 {
            appsrc.push_buffer(i420_frame(index)).expect("push frame");
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        let stalled_at = stamp();

        for index in 60..150 {
            appsrc.push_buffer(i420_frame(index)).expect("push frame");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert_eq!(
            stamp(),
            stalled_at,
            "media kept arriving at the slot's appsrc while its chain was blocked; \
             the stamp must not move for media that never gets through"
        );

        // Let the held streaming thread go before removing the probe: removing it
        // waits for the callback to return.
        release.store(true, Ordering::Relaxed);
        tee_src.remove_probe(block);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
    }

    fn s16_audio_chunk(index: u64) -> gst::Buffer {
        // 10 ms of stereo 48 kHz S16LE, the shape an Opus decoder produces.
        let mut buffer = gst::Buffer::with_size(480 * 2 * 2).expect("allocate audio chunk");
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(index * 10));
            buffer.set_duration(gst::ClockTime::from_mseconds(10));
        }
        buffer
    }

    /// A seat whose audio is dead while its video still flows must be reported as
    /// exactly that.
    ///
    /// This is the failure a single per-slot stamp cannot express: either medium
    /// stamping it keeps the whole seat looking alive, so a commentator whose
    /// microphone has stopped reaching the mix holds its slot to the end of the
    /// session with every liveness signal green. Nothing else catches it either —
    /// an unfed chain stalls no pad task, so the flow's health scan has nothing
    /// to find, and the publisher is still connected so the watchdog has no
    /// reason to act.
    ///
    /// Drives the real probes on the real block's output tees and reads the real
    /// reporter the health scan polls. Thresholds are the only thing the test
    /// supplies, so it does not have to wait out the production budgets.
    #[test]
    fn a_slot_reports_the_medium_that_died_while_the_other_kept_flowing() {
        let _ = gst::init();

        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        let result = build_whipserversrc(
            "whip-half-dead-test",
            &props(&[
                ("mode", PropertyValue::String("audio_video".to_string())),
                // RTP passthrough: appsrc straight to the slot's output tee, so
                // the test drives the tee probes without standing up a decoder.
                ("decode", PropertyValue::Bool(false)),
                ("max_sessions", PropertyValue::Int(1)),
            ]),
            &ctx,
        )
        .expect("build_whipserversrc failed");

        // The block has to hand the health scan a reporter, or none of this is
        // visible to an operator however well the stamps work.
        let reporters = ctx.take_block_liveness();
        assert_eq!(
            reporters
                .iter()
                .map(|(block_id, _)| block_id.as_str())
                .collect::<Vec<_>>(),
            vec!["whip-half-dead-test"],
            "the block must register itself with the block health scan"
        );
        assert!(
            reporters[0].1.failure().is_none(),
            "an endpoint with no session yet has nothing to report"
        );

        let configs = ctx.take_whip_endpoint_configs();
        let config = &configs[0].1;
        // The same shared state the block just registered, read with thresholds
        // short enough for a test.
        let liveness = WhipSlotLiveness::new(
            "half-dead-test".to_string(),
            StreamMode::AudioVideo,
            config.slot_output.clone(),
            config.slot_assignments.clone(),
        );
        let thresholds = StallThresholds {
            audio: std::time::Duration::from_millis(200),
            video: std::time::Duration::from_millis(500),
            absent: std::time::Duration::from_millis(200),
        };

        let audio_src = config.slot_audio_appsrcs[0].clone();
        let video_src = config.slot_video_appsrcs[0].clone();
        audio_src.set_caps(Some(
            &gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("rate", 48000i32)
                .field("channels", 2i32)
                .field("layout", "interleaved")
                .build(),
        ));
        video_src.set_caps(Some(
            &gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 64i32)
                .field("height", 64i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        ));

        let pipeline = assemble(&result);
        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline to PLAYING");
        assert_eq!(
            config.allocate_slot("half-dead-test"),
            Some(0),
            "the endpoint has one free slot"
        );

        // A publisher whose video arrives and whose audio never does.
        let video_index = std::sync::atomic::AtomicU64::new(0);
        let push_video = |count: u64| {
            for _ in 0..count {
                video_src
                    .push_buffer(i420_frame(video_index.fetch_add(1, Ordering::Relaxed)))
                    .expect("push frame");
            }
        };
        push_video(30);
        wait_for("the slot's video stamp to move", || {
            liveness
                .stalls(thresholds)
                .iter()
                .any(|stall| stall.medium == Medium::Audio && stall.silent_for.is_none())
        });
        assert_eq!(
            liveness.stalls(thresholds),
            vec![SlotMediumStall {
                slot: 0,
                medium: Medium::Audio,
                silent_for: None,
            }],
            "video is flowing and audio has never arrived; only the audio is dead"
        );

        // Audio starts: the seat is whole again and must report nothing.
        for index in 0..30 {
            audio_src
                .push_buffer(s16_audio_chunk(index))
                .expect("push audio chunk");
        }
        push_video(15);
        wait_for("both media to be flowing", || {
            liveness.stalls(thresholds).is_empty()
        });

        // Audio stops for good while video carries on: the failure this exists
        // for, and the one the session's own watchdog will never see.
        wait_for("the audio to fall past its threshold", || {
            push_video(5);
            liveness
                .stalls(thresholds)
                .iter()
                .any(|stall| stall.medium == Medium::Audio && stall.silent_for.is_some())
        });
        let stalls = liveness.stalls(thresholds);
        assert_eq!(stalls.len(), 1, "video is healthy: {:?}", stalls);
        assert_eq!(stalls[0].slot, 0);
        assert_eq!(stalls[0].medium, Medium::Audio);
        assert!(
            stalls[0].to_string().contains("no audio"),
            "an operator has to be able to grep the medium out of the detail: {}",
            stalls[0]
        );

        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
    }

    /// An unoccupied slot produces nothing by definition, and a whole seat going
    /// quiet is the inactivity watchdog's business. Neither may be reported as a
    /// half-dead slot, or every idle endpoint in a flow paints the flow red.
    #[test]
    fn an_empty_or_wholly_idle_slot_is_not_reported() {
        let _ = gst::init();

        let assignments = Arc::new(RwLock::new(vec![None]));
        let output = Arc::new(SlotOutput::new(Instant::now()));
        let liveness = WhipSlotLiveness::new(
            "quiet-test".to_string(),
            StreamMode::AudioVideo,
            vec![output.clone()],
            assignments.clone(),
        );
        let thresholds = StallThresholds {
            audio: std::time::Duration::from_millis(50),
            video: std::time::Duration::from_millis(50),
            absent: std::time::Duration::from_millis(50),
        };

        output.touch(Medium::Video);
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            liveness.stalls(thresholds).is_empty(),
            "nobody is in the slot"
        );

        assignments.write().unwrap()[0] = Some("someone".to_string());
        assert!(
            liveness.stalls(thresholds).is_empty(),
            "both media are silent, so this is a seat that left, not a half-dead one"
        );
    }

    /// A seat that loses its transport stops both media at once, and their
    /// budgets differ: audio is past its own seconds before video is anywhere
    /// near its own. That window must not read as a dead microphone — the seat
    /// is simply gone, and the inactivity watchdog owns it.
    #[test]
    fn a_seat_that_stopped_altogether_is_not_reported_as_half_dead() {
        let _ = gst::init();

        let assignments = Arc::new(RwLock::new(vec![Some("gone".to_string())]));
        let output = Arc::new(SlotOutput::new(Instant::now()));
        let liveness = WhipSlotLiveness::new(
            "dropped-test".to_string(),
            StreamMode::AudioVideo,
            vec![output.clone()],
            assignments,
        );
        // The shape of a real drop: audio's cadence is tight and video's is not,
        // so audio's budget is much the shorter of the two.
        let thresholds = StallThresholds {
            audio: std::time::Duration::from_millis(100),
            video: std::time::Duration::from_millis(2000),
            absent: std::time::Duration::from_millis(100),
        };

        // Both stamps stop within a frame interval of each other, as they do
        // when the transport goes away.
        output.touch(Medium::Audio);
        std::thread::sleep(std::time::Duration::from_millis(30));
        output.touch(Medium::Video);
        std::thread::sleep(std::time::Duration::from_millis(400));

        assert!(
            liveness.stalls(thresholds).is_empty(),
            "audio is well past its budget, but video outlived it by 30 ms, not by a              failure: {:?}",
            liveness.stalls(thresholds)
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
