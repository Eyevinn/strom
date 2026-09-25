//! WHIP Input per-slot output chains, built once with the flow.

use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, APPSRC_MAX_BYTES_AUDIO,
    APPSRC_MAX_BYTES_VIDEO, APPSRC_MAX_TIME,
};
use crate::whip_session_manager::{ActivityStamp, WhipEndpointConfig};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use strom_types::block::StreamMode;
use strom_types::{element::ElementPadRef, PropertyValue};
use tracing::{info, warn};
use uuid::Uuid;

/// The part of a WHIP Input slot's audio format fixed at build time: two
/// channels, whoever is publishing into it.
///
/// A slot outlives its sessions, and caps travel with each sample pushed into
/// `appsrc_audio_<slot>`, so a second publisher can hand a running chain a
/// different format than the first. Consumers past the slot's tee have
/// committed to the first one — a muxer will not renegotiate mid-file, and its
/// `not-negotiated` travels back up and kills the appsrc's streaming thread.
/// The slot's capsfilter keeps the format on this side of the tee, where
/// `audioconvert` absorbs a change; [`lock_slot_audio_caps`] freezes it on
/// what the first session negotiated.
///
/// Only `channels` is pinned here, so a mono first publisher does not downmix
/// every later one. The other fields are left for downstream to choose,
/// because a build-time value can contradict what downstream accepts, and then
/// the slot's audio cannot link or negotiate at all and the seat gets no
/// audio. A pinned rate breaks a seat whose shared mixer settled on another
/// one; a pinned S16LE breaks a consumer that takes only float and has no
/// converter of its own, such as the Latency block's `audiolatency`.
fn slot_audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("channels", 2i32)
        .build()
}

/// Freeze a slot's audio capsfilter on the format that was actually negotiated.
///
/// [`slot_audio_caps`] pins only the channel count; downstream chooses the
/// sample format, layout and rate on the first session. Writing those caps
/// back into the capsfilter makes `audioconvert`/`audioresample` convert every
/// later session to them. The values came from downstream, so pinning them cannot
/// conflict with downstream — which build-time values can.
///
/// The format needs this even though `opusdec` always outputs S16LE: without
/// it, a mono session is converted to the consumer's preferred float, while a
/// stereo session already has the pinned channel count and `audioconvert`
/// passes its S16LE straight through. For the rate it is defence in depth:
/// `opusdec` always outputs 48 kHz.
///
/// CAPS events are rare; this is not a per-buffer probe.
fn lock_slot_audio_caps(capsfilter: &gst::Element, slot: usize) {
    let Some(src_pad) = capsfilter.static_pad("src") else {
        warn!(
            "WHIP Input: audio capsfilter for slot {} has no src pad",
            slot
        );
        return;
    };
    // Weak: the element owns the probe, so a strong ref would be a cycle and
    // would keep the pipeline from ever finalizing.
    let capsfilter_weak = capsfilter.downgrade();
    let locked = AtomicBool::new(false);
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        if locked.swap(true, Ordering::Relaxed) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(capsfilter) = capsfilter_weak.upgrade() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps().to_owned();
        capsfilter.set_property("caps", &caps);
        info!(
            "WHIP Input: slot {} audio format locked to {} for the life of the flow",
            slot, caps
        );
        gst::PadProbeReturn::Ok
    });
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

/// Parse drop_on_latency from properties (default: true).
///
/// True works around a GStreamer rtpjitterbuffer bug (see the comment in
/// `whep.rs` `build_whepsrc` iterate_recurse). False keeps late packets for a
/// downstream WebRTC endpoint that buffers adaptively, and reinstates the stall.
fn parse_drop_on_latency(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("drop_on_latency")
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

/// Stamp a slot's output activity for every buffer that reaches its tee.
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
fn stamp_slot_output(tee: &gst::Element, stamp: Arc<ActivityStamp>, slot: usize, media: &str) {
    let Some(sink_pad) = tee.static_pad("sink") else {
        // Unreachable: a tee has a static sink pad. If it ever were not, nothing
        // would stamp this slot and every session on it would be reaped once its
        // decode grace ran out — recoverable, unlike a panic in a block build,
        // and this line is what makes it diagnosable.
        warn!(
            "WHIP Input: slot {} {} output tee has no sink pad, cannot track its liveness",
            slot, media
        );
        return;
    };
    sink_pad.add_probe(
        // BUFFER_LIST as well: nothing on this chain batches today, but an
        // element that started to would silently stop the stamp otherwise.
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, _info| {
            stamp.touch();
            gst::PadProbeReturn::Ok
        },
    );
}

/// Build WHIP Input per-slot output chains.
///
/// At build time, per-slot chains are created in the main pipeline:
/// - decode=true: appsrc → decodebin → audioconvert → audioresample → capsfilter → tee (audio),
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
///
/// Public so tests can build the slot chains on a host without ICE elements —
/// `WHIPInputBuilder::build` refuses there, but the slot chains themselves use
/// nothing from `gst-plugins-rs`.
pub fn build_whipserversrc(
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
    // drop-on-latency (below, on by default) can be too tight for an initial video
    // keyframe's packet burst on a freshly-created per-session pipeline,
    // causing the whole video stream to stall (never reaching decodebin)
    // even though the packets arrived fine over the network.
    let jitterbuffer_latency_ms = parse_jitterbuffer_latency_ms(properties);
    let do_retransmission = parse_do_retransmission(properties);
    let drop_on_latency = parse_drop_on_latency(properties);

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

    // One stamp per slot, written by a probe on that slot's output tees below.
    // This is where a session's media becomes usable to the flow, so this is
    // where its liveness is measured; see `SessionActivity`. All slots share an
    // epoch — the stamps are only ever compared against their own readings.
    let output_epoch = Instant::now();
    let slot_output: Vec<Arc<ActivityStamp>> = (0..max_sessions)
        .map(|_| Arc::new(ActivityStamp::new(output_epoch)))
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
                let audio_caps_id = format!("{}:audio_caps_{}", instance_id, slot);

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

                let audio_caps = gst::ElementFactory::make("capsfilter")
                    .name(&audio_caps_id)
                    .property("caps", slot_audio_caps())
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audio_caps_{}: {}", slot, e))
                    })?;
                lock_slot_audio_caps(&audio_caps, slot);

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

                // audioconvert → audioresample → capsfilter → tee.
                // The capsfilter is what makes the slot reusable by a publisher
                // whose audio format differs from the last one — see
                // `slot_audio_caps` and `lock_slot_audio_caps`.
                internal_links.push((
                    ElementPadRef::pad(&audioconvert_id, "src"),
                    ElementPadRef::pad(&audioresample_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&audioresample_id, "src"),
                    ElementPadRef::pad(&audio_caps_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&audio_caps_id, "src"),
                    ElementPadRef::pad(&audio_out_tee_id, "sink"),
                ));

                elements.push((decodebin_id, decodebin));
                elements.push((audioconvert_id, audioconvert));
                elements.push((audioresample_id, audioresample));
                elements.push((audio_caps_id, audio_caps));
            } else {
                // decode=false: clocksync → tee directly
                internal_links.push((
                    ElementPadRef::pad(&appsrc_id, "src"),
                    ElementPadRef::pad(&audio_out_tee_id, "sink"),
                ));
            }

            stamp_slot_output(&audio_out_tee, output_stamp.clone(), slot, "audio");

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

            stamp_slot_output(&video_out_tee, output_stamp.clone(), slot, "video");

            slot_video_appsrcs.push(appsrc.clone());
            elements.push((appsrc_id, appsrc.upcast()));
            elements.push((video_out_tee_id, video_out_tee));
        }

        slot_decodebins.push(decodebins_for_slot);
    }

    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();
    let ice_transport_policy = ctx.resolve_ice_transport_policy(properties);

    info!(
        "WHIP Input configured: endpoint_id='{}', stun={:?}, turn={:?}, ice_transport_policy={}, mode={:?}, decode={}, do_retransmission={}, drop_on_latency={}, max_sessions={} (whipserversrc created per-session)",
        endpoint_id, stun_server, turn_server, ice_transport_policy, mode, decode, do_retransmission, drop_on_latency, max_sessions
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
            ice_transport_policy,
            pipeline_weak: gst::glib::WeakRef::new(),
            decode,
            video_decoding,
            jitterbuffer_latency_ms,
            do_retransmission,
            drop_on_latency,
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
    fn drop_on_latency_defaults_to_true() {
        assert!(parse_drop_on_latency(&props(&[])));
    }

    #[test]
    fn drop_on_latency_respects_explicit_false() {
        assert!(!parse_drop_on_latency(&props(&[(
            "drop_on_latency",
            PropertyValue::Bool(false)
        )])));
    }

    #[test]
    fn do_retransmission_respects_explicit_false() {
        assert!(!parse_do_retransmission(&props(&[(
            "do_retransmission",
            PropertyValue::Bool(false)
        )])));
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
    /// second half is a seat that keeps receiving RTP while nothing gets
    /// through: here a stuck consumer blocks its tee, which is what a stalled
    /// recorder branch does in a real flow.
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
        let stamp = config.slot_output[0].clone();
        let appsrc = config.slot_video_appsrcs[0].clone();
        assert_eq!(
            stamp.last(),
            0,
            "nothing has gone through the slot's chain yet"
        );

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
            stamp.last() != 0
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
        let stalled_at = stamp.last();

        for index in 60..150 {
            appsrc.push_buffer(i420_frame(index)).expect("push frame");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert_eq!(
            stamp.last(),
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

    /// Same contract for `drop_on_latency`: hardcoding the rtpbin workaround
    /// back to a literal `true` fails the `false` case.
    #[test]
    fn drop_on_latency_reaches_whip_endpoint_config() {
        let _ = gst::init();

        for expected in [true, false] {
            let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
            build_whipserversrc(
                "whip-drop-on-latency-test",
                &props(&[("drop_on_latency", PropertyValue::Bool(expected))]),
                &ctx,
            )
            .expect("build_whipserversrc failed");

            let configs = ctx.take_whip_endpoint_configs();
            assert_eq!(configs.len(), 1, "expected exactly one endpoint config");
            assert_eq!(configs[0].1.drop_on_latency, expected);
        }
    }
}
