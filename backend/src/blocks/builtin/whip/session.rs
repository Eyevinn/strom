//! One isolated whipserversrc pipeline per WHIP client session, bridged into its slot.

use crate::gst::keyframe_request::{self, RecoveryStep};
use crate::gst::orphan_guard;
use crate::gst::pipeline_bridge::{self, SessionBridge};
use crate::gst::rtp_hdrext;
use crate::whip_session_manager::{
    ActivityStamp, SessionActivity, SessionCleanupRequest, WhipEndpointConfig,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_rtp as gst_rtp;
use gstreamer_video as gst_video;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::watchdog::{wait_for_inactivity, INACTIVITY_TIMEOUT};

/// Report a mid-session recovery step for `slot`.
fn log_recovery_step(step: RecoveryStep, slot: usize) {
    match step {
        RecoveryStep::Wait => {}
        RecoveryStep::Request { attempt } => debug!(
            "WHIP Input: slot {} video damaged (a gap, or the decoder stopped producing), requesting keyframe (PLI) attempt {}",
            slot, attempt
        ),
        RecoveryStep::Recovered { after, requests } => info!(
            "WHIP Input: slot {} video recovered after {} ms ({} keyframe request(s))",
            slot,
            after.as_millis(),
            requests
        ),
        RecoveryStep::GaveUp { requests } => warn!(
            "WHIP Input: slot {} video still damaged after {} keyframe request(s); not asking again until it recovers",
            slot, requests
        ),
    }
}

/// Wire one session stream's appsink into its slot's appsrc in the main
/// pipeline.
///
/// A function rather than an inline closure so a test can drive the callback:
/// the guard that matters is here, not in [`pipeline_bridge`]. A bare
/// `appsrc.push_sample(&sample)` in this body re-opens the cascade an unstamped
/// buffer starts and fails nothing in that module's tests.
#[allow(clippy::too_many_arguments)]
fn install_slot_bridge(
    appsink: &gst_app::AppSink,
    appsrc: gst_app::AppSrc,
    bridge: Arc<SessionBridge>,
    main_pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    media_type: &str,
    slot: usize,
    activity: Arc<SessionActivity>,
    session_finished: Arc<AtomicBool>,
) {
    // Resolved here, not per buffer: the pad's media type is fixed.
    let pad_is_audio = media_type == "audio";
    let video_order = VideoPtsOrder::new();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // Media arriving from the publisher. Half of this session's
                // liveness; the other half is stamped on the slot's output tee
                // in the main pipeline.
                activity.touch_ingress(pad_is_audio);

                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                forward_sample_to_slot(
                    &sample,
                    &appsrc,
                    &session_finished,
                    &bridge,
                    &main_pipeline_weak,
                    if pad_is_audio {
                        SlotMedia::Audio
                    } else {
                        SlotMedia::Video(&video_order)
                    },
                    slot,
                )?;

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
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

/// Drain one `whipserversrc` pad into the session pipeline: `pad → tee →
/// fakesink + appsink`, with every element brought up to the pipeline's state.
///
/// Returns the appsink the caller installs its bridge callbacks on, or `None`
/// if the branch could not be built — every failure is logged here.
///
/// Both sinks run with `async` off. The pipeline they join is already PLAYING,
/// and a sink that still wants a preroll answers the state change with ASYNC.
/// whipserversrc adds its audio and video pads one after the other within a
/// millisecond, so the second branch lands while the first is still waiting,
/// and the second is then left below PLAYING: it takes a single buffer and
/// blocks its streaming thread for good. From the outside that is a publisher
/// whose audio or video never starts, with nothing in the log to say so —
/// `sync_state_with_parent` reports the state change as accepted either way.
pub fn attach_session_branch(
    session_pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    appsink_name: &str,
) -> Option<gst_app::AppSink> {
    let tee = match gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()
    {
        Ok(t) => t,
        Err(e) => {
            error!("WHIP Input: Failed to create tee in pad-added: {}", e);
            return None;
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
            return None;
        }
    };
    let appsink = gst_app::AppSink::builder()
        .name(appsink_name)
        .sync(false)
        .async_(false)
        .build();

    if let Err(e) = session_pipeline.add_many([&tee, &fakesink, appsink.upcast_ref()]) {
        error!(
            "WHIP Input: Failed to add elements to session pipeline: {}",
            e
        );
        return None;
    }
    if let Err(e) = pad.link(&tee.static_pad("sink").expect("tee has no sink pad")) {
        error!("WHIP Input: Failed to link pad to tee: {:?}", e);
        return None;
    }
    if let (Some(tee_src1), Some(tee_src2)) = (
        tee.request_pad_simple("src_%u"),
        tee.request_pad_simple("src_%u"),
    ) {
        let _ = tee_src1.link(
            &fakesink
                .static_pad("sink")
                .expect("fakesink has no sink pad"),
        );
        let _ = tee_src2.link(&appsink.static_pad("sink").expect("appsink has no sink pad"));
    } else {
        error!("WHIP Input: Failed to request tee src pads");
        return None;
    }
    let _ = tee.sync_state_with_parent();
    let _ = fakesink.sync_state_with_parent();
    let _ = appsink.sync_state_with_parent();
    Some(appsink)
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
    let drop_on_latency = config.drop_on_latency;
    // `cleanup_sent` ensures only one cleanup request per session (shared across the
    // ICE callback, the inactivity watchdog and the session manager's teardown paths).
    let cleanup_sent_for_ice = cleanup_sent.clone();
    let cleanup_tx_for_ice = cleanup_tx.clone();

    if let Ok(bin) = whipserversrc.clone().downcast::<gst::Bin>() {
        // whipserversrc removes each session's internal bin from a thread of its
        // own, which can land while this pipeline is on its way down and leave
        // the bin orphaned above NULL with its WebRTC sockets still open.
        orphan_guard::install(&bin);

        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            // Workaround for GStreamer rtpjitterbuffer packet_spacing bug:
            // see comment in whep.rs build_whepsrc iterate_recurse for details.
            // Configurable because the workaround costs late packets a
            // downstream WebRTC endpoint could still have used.
            if element_name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                element.set_property("drop-on-latency", drop_on_latency);
                info!(
                    "WHIP Input: Set drop-on-latency={} on {}",
                    drop_on_latency, element_name
                );
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
    let video_damage = config.video_damage.clone();
    if let Some(damage) = video_damage.get(slot) {
        damage.clear();
    }
    let decode = config.decode;

    // Shared appsink -> appsrc bridge state for this session: the A/V timestamp
    // offset both streams rebase onto, and the unstamped-buffer drop count.
    let session_bridge = Arc::new(SessionBridge::new());

    // Liveness for this session, in the shape the session manager reads it: it
    // has to tell a slot that still has a publisher producing media behind it
    // from one whose publisher went away without a WHIP DELETE, or whose media
    // arrives but never comes out of the slot's chain.
    let slot_output = match config.slot_output.get(slot) {
        Some(stamp) => stamp.clone(),
        None => {
            // Unreachable: one stamp is built per slot. The orphan below is
            // never written, so this session would be reaped once its decode
            // grace ran out; this line is what makes that diagnosable.
            warn!(
                "WHIP Input: no output stamp for slot {}, its liveness cannot be tracked",
                slot
            );
            Arc::new(ActivityStamp::new(Instant::now()))
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
                let Some((idle_ms, side)) = wait_for_inactivity(
                    &cleanup_sent_watchdog,
                    &activity_watchdog,
                    INACTIVITY_TIMEOUT,
                ) else {
                    return;
                };
                if !cleanup_sent_watchdog.swap(true, Ordering::SeqCst) {
                    info!(
                        "WHIP Input: Inactivity timeout ({}ms without usable media: {}) on port {}, triggering cleanup",
                        idle_ms, side, port
                    );
                    let _ = cleanup_tx_watchdog.send(SessionCleanupRequest {
                        port,
                        reason: format!("inactivity ({}ms without usable media: {})", idle_ms, side),
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
        let cleanup_sent_for_pads = cleanup_sent.clone();

        whipserversrc.connect_pad_added(move |_src, pad| {
            let pad_name = pad.name();
            let stream_num = stream_counter.fetch_add(1, Ordering::SeqCst);

            let session_pipeline: Option<gst::Pipeline> = session_pipeline_weak.upgrade();
            let Some(session_pipeline) = session_pipeline else {
                error!("WHIP Input: Session pipeline destroyed");
                return;
            };

            let Some(appsink) = attach_session_branch(
                &session_pipeline,
                pad,
                &format!("{}:{}_appsink_{}", prefix, pad_name, stream_num),
            ) else {
                return;
            };

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

                    // Once decoding, ask again whenever the slot's decode chain
                    // reports damage only a keyframe repairs. Without decode
                    // there is no decode chain, and nothing ever reports.
                    if decode {
                        let repair_pad = pad.downgrade();
                        let repair_damage = video_damage.clone();
                        let repair_stop = cleanup_sent_for_pads.clone();
                        let repair_slot = slot;
                        if let Err(e) = std::thread::Builder::new()
                            .name(format!("whip-repair-{}", repair_slot))
                            .spawn(move || {
                                let Some(damage) = repair_damage.get(repair_slot) else {
                                    return;
                                };
                                let epoch = Instant::now();
                                keyframe_request::recover_while(
                                    keyframe_request::RecoveryPolicy::default(),
                                    damage,
                                    // The session's teardown paths set the
                                    // flag; a dropped session pipeline also
                                    // ends it.
                                    || {
                                        repair_stop.load(Ordering::SeqCst)
                                            || repair_pad.upgrade().is_none()
                                    },
                                    || epoch.elapsed(),
                                    std::thread::sleep,
                                    |step| {
                                        log_recovery_step(step, repair_slot);
                                        if let RecoveryStep::Request { .. } = step {
                                            if let Some(pad) = repair_pad.upgrade() {
                                                pad.send_event(
                                                    gst_video::UpstreamForceKeyUnitEvent::builder()
                                                        .all_headers(true)
                                                        .build(),
                                                );
                                            }
                                        }
                                    },
                                );
                            })
                        {
                            warn!(
                                "WHIP Input: could not spawn keyframe repairer for slot {}: {}",
                                slot, e
                            );
                        }
                    }
                }

                install_slot_bridge(
                    &appsink,
                    appsrc,
                    session_bridge.clone(),
                    main_pipeline_weak.clone(),
                    media_type,
                    slot,
                    activity_for_pads.clone(),
                    cleanup_sent_for_pads.clone(),
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

    Ok(CreatedSession {
        element: whipserversrc,
        session_pipeline,
        port,
        activity,
    })
}

/// How far behind the previous frame's PTS a new frame's may be and still be
/// treated as landing on it. A run released on one PTS falls behind by 1 ns
/// per frame already moved; a frame interval is over 8 ms at up to 120 fps.
const MAX_PTS_NUDGE_NS: u64 = 1_000_000;

/// Gives each of a session's video frames its own PTS when several arrive on one.
///
/// The session's jitterbuffer can release a run of packets from different
/// frames all on one PTS, as it does when a publisher joins a slot. The
/// slot's `h264parse` takes a PTS only when it differs from the previous
/// buffer's, so every later frame of the run leaves it with none; a browser
/// stream has no framerate to derive one from, and the vision mixer's
/// compositor fails the whole flow on a frame without a timestamp.
///
/// Packets of one frame keep their shared PTS. A new frame whose PTS is at
/// most [`MAX_PTS_NUDGE_NS`] behind the previous frame's is moved 1 ns past it.
/// Any other PTS passes through: a larger step back is real timing (a B-frame,
/// sent before the frame it precedes, or a step in the sender's clock), and
/// holding later frames behind a PTS far ahead would freeze the seat. Only the
/// appsink's streaming thread touches it.
struct VideoPtsOrder {
    /// RTP timestamp of the last frame, `u64::MAX` before the first.
    last_rtptime: AtomicU64,
    last_pts: AtomicU64,
}

impl VideoPtsOrder {
    fn new() -> Self {
        Self {
            last_rtptime: AtomicU64::new(u64::MAX),
            last_pts: AtomicU64::new(0),
        }
    }

    fn order(&self, rtptime: u32, pts: u64) -> u64 {
        let last_rtptime = self.last_rtptime.load(Ordering::Relaxed);
        let last_pts = self.last_pts.load(Ordering::Relaxed);
        if last_rtptime == u64::from(rtptime) {
            return last_pts;
        }
        let duplicate =
            last_rtptime != u64::MAX && pts <= last_pts && last_pts - pts <= MAX_PTS_NUDGE_NS;
        let pts = if duplicate { last_pts + 1 } else { pts };
        self.last_rtptime
            .store(u64::from(rtptime), Ordering::Relaxed);
        self.last_pts.store(pts, Ordering::Relaxed);
        pts
    }
}

/// Which of a slot's streams a sample is bridged into.
#[derive(Clone, Copy)]
enum SlotMedia<'a> {
    Audio,
    Video(&'a VideoPtsOrder),
}

impl SlotMedia<'_> {
    fn name(self) -> &'static str {
        match self {
            SlotMedia::Audio => "audio",
            SlotMedia::Video(_) => "video",
        }
    }
}

/// `sample` with its PTS moved by `order`, or `None` when it stays as it is.
/// Copies the buffer only when the PTS changes. A buffer with no PTS, or one
/// that is not RTP, is left for the bridge to judge.
fn order_video_pts(sample: &gst::Sample, order: &VideoPtsOrder) -> Option<gst::Sample> {
    let buffer = sample.buffer()?;
    let pts = buffer.pts()?.nseconds();
    let rtptime = gst_rtp::RTPBuffer::from_buffer_readable(buffer)
        .ok()?
        .timestamp();
    let ordered = order.order(rtptime, pts);
    if ordered == pts {
        return None;
    }
    let mut new_buffer = buffer.copy();
    new_buffer
        .make_mut()
        .set_pts(gst::ClockTime::from_nseconds(ordered));
    let mut builder = gst::Sample::builder().buffer(&new_buffer);
    let caps = sample.caps_owned();
    if let Some(caps) = caps.as_ref() {
        builder = builder.caps(caps);
    }
    Some(builder.build())
}

/// Bridge one sample from a session's appsink into its slot's appsrc, shifting
/// its PTS by the offset shared across the session's audio and video.
///
/// The offset and the drop of unstamped buffers are [`SessionBridge`]'s: a
/// buffer with no PTS never reaches the slot, because a muxer downstream
/// answers it with `GST_FLOW_ERROR` and takes the whole flow down.
///
/// Nothing is pushed once `session_finished` is set. The slot's appsrc belongs to
/// whoever holds the slot, and takeover releases the slot while the displaced
/// session may still be delivering media: without this check, two sessions push
/// into one appsrc with different offsets until the old pipeline reaches NULL.
fn forward_sample_to_slot(
    sample: &gst::Sample,
    appsrc: &gst_app::AppSrc,
    session_finished: &AtomicBool,
    bridge: &SessionBridge,
    main_pipeline: &gst::glib::WeakRef<gst::Pipeline>,
    media: SlotMedia,
    slot: usize,
) -> Result<(), gst::FlowError> {
    if session_finished.load(Ordering::Relaxed) {
        return Ok(());
    }

    let ordered = match media {
        SlotMedia::Video(order) => order_video_pts(sample, order),
        SlotMedia::Audio => None,
    };
    let sample = ordered.as_ref().unwrap_or(sample);

    let outcome = bridge
        .forward(sample, appsrc, || {
            let main_pipeline = main_pipeline.upgrade()?;
            let clock = main_pipeline.clock()?;
            let base_time = main_pipeline.base_time()?;
            Some(clock.time().saturating_sub(base_time))
        })
        .ok_or(gst::FlowError::Error)?;

    match outcome {
        pipeline_bridge::Forwarded::OffsetComputed(offset) => {
            info!(
                "WHIP Input: Computed shared ts-offset={}ms from {} stream (slot {})",
                offset / 1_000_000,
                media.name(),
                slot
            );
        }
        pipeline_bridge::Forwarded::DroppedUnstamped { dropped } => {
            if pipeline_bridge::should_log_drop(dropped) {
                warn!(
                    "WHIP Input: dropped {} buffer(s) with no PTS on the {} stream (slot {}); forwarding one fails the downstream muxer and takes the whole flow with it",
                    dropped, media.name(), slot
                );
            }
        }
        pipeline_bridge::Forwarded::Restamped
        | pipeline_bridge::Forwarded::Unadjusted
        | pipeline_bridge::Forwarded::PushFailed(_) => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gst::pipeline_bridge::test_support::{at, Harness, SessionPipeline};

    /// The wiring guard for this block. [`pipeline_bridge`] can only promise
    /// that [`SessionBridge::forward`] drops an unstamped buffer; it cannot
    /// promise this block still calls it. So drive the real appsink callback:
    /// three buffers into a session pipeline, the middle one with no PTS, and
    /// only the two stamped ones may reach the slot's appsrc.
    ///
    /// Forwarding that buffer is what makes `qtmux` answer `Buffer has no PTS`
    /// with `GST_FLOW_ERROR`, which tears down the whole flow — every seat, not
    /// just this one.
    #[test]
    fn an_unstamped_buffer_never_reaches_the_slot_appsrc() {
        let slot_pipeline = Harness::new();
        let session = SessionPipeline::new();

        let appsink = gst_app::AppSink::builder().sync(false).build();
        session
            .pipeline
            .add(appsink.upcast_ref::<gst::Element>())
            .expect("add appsink");
        session
            .src
            .link(&appsink)
            .expect("link session appsrc to appsink");

        install_slot_bridge(
            &appsink,
            slot_pipeline.src.clone(),
            Arc::new(SessionBridge::new()),
            slot_pipeline.pipeline_weak(),
            "video",
            0,
            Arc::new(SessionActivity::new(
                Instant::now(),
                Arc::new(ActivityStamp::new(Instant::now())),
            )),
            Arc::new(AtomicBool::new(false)),
        );

        session.start();
        session.push(at(1));
        session.push(None);
        session.push(at(3));

        let first = slot_pipeline.next_pts().expect("first buffer crossed");
        assert!(first.is_some(), "a stamped buffer must keep its stamp");
        let second = slot_pipeline
            .next_pts()
            .expect("the third buffer crossed too");
        assert!(
            second.is_some(),
            "the unstamped buffer must not be here — the third one is next"
        );
        assert!(second > first, "and it must follow the first");
        assert_eq!(
            slot_pipeline.next_pts(),
            None,
            "nothing else should have crossed"
        );
    }

    /// Takeover releases a slot while the displaced session may still be
    /// delivering media, and the slot's appsrc passes to the new session. Once
    /// the old session is marked finished, its samples must stop reaching that
    /// appsrc, or both sessions feed it with their own timestamp offsets.
    #[test]
    fn a_finished_session_stops_feeding_its_slot() {
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();
        appsrc.link(&appsink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let caps = gst::Caps::builder("application/x-test").build();
        let sample_at = |seconds| {
            let mut buffer = gst::Buffer::new();
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_seconds(seconds));
            gst::Sample::builder().buffer(&buffer).caps(&caps).build()
        };
        // No main pipeline, so the bridge forwards both samples unadjusted.
        let bridge = SessionBridge::new();
        let no_main_pipeline = gst::glib::WeakRef::new();

        let displaced = AtomicBool::new(true);
        forward_sample_to_slot(
            &sample_at(1),
            &appsrc,
            &displaced,
            &bridge,
            &no_main_pipeline,
            SlotMedia::Audio,
            0,
        )
        .unwrap();

        let current = AtomicBool::new(false);
        forward_sample_to_slot(
            &sample_at(2),
            &appsrc,
            &current,
            &bridge,
            &no_main_pipeline,
            SlotMedia::Audio,
            0,
        )
        .unwrap();

        let first = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("the current session's sample must reach the slot");
        pipeline.set_state(gst::State::Null).unwrap();

        assert_eq!(
            first.buffer().unwrap().pts(),
            Some(gst::ClockTime::from_seconds(2)),
            "the first sample in the slot must be the current session's, not the displaced one's"
        );
    }

    /// Bridge RTP video packets, given as (RTP timestamp, PTS in ms), through
    /// one session's `forward_sample_to_slot` and return the PTS each reached
    /// the slot with, in ns.
    fn bridge_video_packets(packets: &[(u32, u64)]) -> Vec<u64> {
        use gst_rtp::prelude::RTPBufferExt;
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();
        appsrc.link(&appsink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("encoding-name", "H264")
            .build();
        // No main pipeline, so the bridge forwards every PTS unadjusted and
        // only the ordering moves it.
        let bridge = SessionBridge::new();
        let no_main_pipeline = gst::glib::WeakRef::new();
        let session_finished = AtomicBool::new(false);
        let order = VideoPtsOrder::new();

        let mut out = Vec::new();
        for &(rtptime, pts_ms) in packets {
            let mut buffer = gst::Buffer::new_rtp_with_sizes(4, 0, 0).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(gst::ClockTime::from_mseconds(pts_ms));
                let mut rtp = gst_rtp::RTPBuffer::from_buffer_writable(buffer).unwrap();
                rtp.set_timestamp(rtptime);
            }
            let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
            forward_sample_to_slot(
                &sample,
                &appsrc,
                &session_finished,
                &bridge,
                &no_main_pipeline,
                SlotMedia::Video(&order),
                0,
            )
            .unwrap();
            let pulled = appsink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .expect("every packet must reach the slot");
            out.push(pulled.buffer().unwrap().pts().unwrap().nseconds());
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out
    }

    /// A session's jitterbuffer can release packets of several frames on one
    /// PTS. Each frame must still reach the slot with its own, increasing PTS,
    /// and the packets of one frame with a shared one.
    #[test]
    fn video_frames_on_one_pts_reach_the_slot_in_order() {
        // Two packets of frame A, then frames B and C released on A's PTS,
        // then frame D on its own later PTS.
        let out = bridge_video_packets(&[
            (1000, 1000),
            (1000, 1000),
            (4000, 1000),
            (7000, 1000),
            (10000, 1100),
        ]);

        let ms = 1_000_000;
        assert_eq!(out[0], 1000 * ms);
        assert_eq!(out[1], out[0], "packets of one frame share a PTS");
        assert!(out[2] > out[1], "frame B must follow frame A: {:?}", out);
        assert!(out[3] > out[2], "frame C must follow frame B: {:?}", out);
        assert_eq!(out[4], 1100 * ms, "a frame already in order keeps its PTS");
    }

    /// An encoder with B-frames sends a frame after the one it precedes, so
    /// its PTS is a frame or more behind the previous packet's. It must keep
    /// that PTS: moving it past the later frame bunches the decoded video.
    #[test]
    fn video_b_frames_keep_their_pts() {
        // Decode order I P B B P, 33 ms per frame: display order I B B P P.
        let out = bridge_video_packets(&[
            (0, 1000),
            (9000, 1100),
            (3000, 1033),
            (6000, 1066),
            (18000, 1200),
        ]);

        let ms = 1_000_000;
        assert_eq!(
            out,
            vec![1000 * ms, 1100 * ms, 1033 * ms, 1066 * ms, 1200 * ms],
            "B-frames must reach the slot with their own PTS"
        );
    }

    /// One frame with a PTS far ahead of the rest must not hold back the
    /// frames after it, or the seat freezes until real time catches up.
    #[test]
    fn video_frame_far_ahead_does_not_hold_back_later_frames() {
        let out = bridge_video_packets(&[
            (0, 1000),
            (3000, 1033),
            (6000, 10_000),
            (9000, 1100),
            (12000, 1133),
        ]);

        let ms = 1_000_000;
        assert_eq!(
            out,
            vec![1000 * ms, 1033 * ms, 10_000 * ms, 1100 * ms, 1133 * ms],
            "frames after an outlier must keep their own PTS"
        );
    }
}
