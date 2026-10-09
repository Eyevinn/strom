//! WHEP Output using `whepserversink`.

use crate::blocks::{
    set_ice_transport_policy, BlockBuildContext, BlockBuildError, BlockBuildResult,
};
use crate::gst::video_input_bridge;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{element::ElementPadRef, PropertyValue};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::resolve_track_counts;
use crate::blocks::builtin::webrtc_props::parse_do_retransmission;

/// Build WHEP Output using whepserversink (hosts HTTP server for WHEP clients).
///
/// This element creates an HTTP server that WHEP clients can connect to
/// in order to receive the WebRTC stream.
///
/// whepserversink is based on webrtcsink and handles encoding internally.
/// It uses request pads (audio_0, video_0) similar to whipclientsink.
///
/// The server binds to localhost on an auto-assigned free port.
/// Axum proxies requests from /api/whep/{endpoint_id}/... to the internal port.
pub(super) fn build_whepserversink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Output using whepserversink (server mode)");

    // Number of audio/video tracks to expose. Each track gets its own
    // audio_in / video_in pad, queue and request pad on whepserversink
    // (audio_0, audio_1, ..., video_0, video_1, ...). 0 disables the media
    // type on this endpoint.
    let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
    let has_audio = num_audio_tracks > 0;
    let has_video = num_video_tracks > 0;

    info!(
        "WHEP Output: num_audio_tracks={}, num_video_tracks={}",
        num_audio_tracks, num_video_tracks
    );

    let do_retransmission = parse_do_retransmission(properties);

    // Timestamp offset in milliseconds. A negative value shifts playout earlier,
    // reducing end-to-end latency for this output while maintaining A/V sync.
    // Applied as ts-offset on clocksync and appsink inside whepserversink.
    let ts_offset_ms = properties
        .get("ts_offset_ms")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);

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

    // Find a free port by binding to port 0
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to find free port: {}", e))
    })?;
    let internal_port = listener
        .local_addr()
        .map_err(|e| {
            BlockBuildError::InvalidConfiguration(format!("Failed to get local address: {}", e))
        })?
        .port();
    // Drop the listener to free the port for whepserversink
    drop(listener);

    info!(
        "WHEP Output: Found free port {} for endpoint_id '{}'",
        internal_port, endpoint_id
    );

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();
    let ice_transport_policy = ctx.resolve_ice_transport_policy(properties);

    // Create whepserversink element
    // This is based on webrtcsink and handles encoding internally
    let whepserversink_id = format!("{}:whepserversink", instance_id);
    let whepserversink = gst::ElementFactory::make("whepserversink")
        .name(&whepserversink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepserversink: {}", e)))?;

    // Set ICE server properties (explicitly clear defaults when not configured,
    // since webrtcsink defaults to stun://stun.l.google.com:19302)
    // Note: webrtcsink-based elements use "turn-servers" (plural, array) not "turn-server"
    match stun_server {
        Some(ref stun) => whepserversink.set_property("stun-server", stun),
        None => whepserversink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        let turn_servers = gst::Array::new([turn]);
        whepserversink.set_property("turn-servers", turn_servers);
    }

    // webrtcsink applies this to every consumer's webrtcbin as it creates it,
    // before that session's pipeline leaves NULL. The consumer-added handler
    // below sets the same value again, which covers a webrtcsink that does not
    // expose the property at all.
    set_ice_transport_policy(&whepserversink, &ice_transport_policy, "WHEP Output");

    // Disable FEC; RTX (retransmission) is configurable, default on.
    // - FEC adds proactive redundancy packets on every stream (~50% constant
    //   overhead, near-double bandwidth for pre-encoded high-bitrate video),
    //   so it stays off.
    // - RTX is reactive: it costs nothing while no packets are lost and only
    //   resends the exact packets the client NACKs. Without it, every loss
    //   escalates to PLI -> forced keyframe, which is far more expensive and
    //   leaves the picture broken until the keyframe arrives.
    whepserversink.set_property("do-fec", false);
    whepserversink.set_property("do-retransmission", do_retransmission);

    // Access the signaller child and set its properties
    // Bind to localhost only - axum will proxy external requests
    let signaller = whepserversink.property::<gst::glib::Object>("signaller");
    let host_addr = format!("http://127.0.0.1:{}", internal_port);
    signaller.set_property("host-addr", &host_addr);

    // Shift playout timing on clocksync and appsink inside whepserversink.
    // A negative ts_offset_ms makes this output release buffers earlier:
    //  - clocksync: negative ts-offset shifts its clock wait earlier
    //  - appsink: negative ts-offset shifts BaseSink's clock wait earlier
    //    (BaseSink formula: wait = running_time + latency + ts_offset)
    // ts-offset does NOT affect the latency query, so pipeline latency is
    // unchanged. Both elements keep sync=true — only the playout point shifts.
    //
    // Properties are applied via a deferred pad probe because webrtcsink's
    // StreamProducer configures these elements AFTER deep-element-added fires,
    // using direct C API calls that bypass g_object_notify.
    if ts_offset_ms != 0 {
        let ts_offset_ns = ts_offset_ms.saturating_mul(1_000_000);
        let instance_id_for_ts = instance_id.to_string();

        fn defer_ts_offset(element: &gst::Element, ts_offset_ns: i64, instance_id: &str) {
            if let Some(pad) = element.static_pad("sink") {
                let iid = instance_id.to_string();
                let name = element.name().to_string();
                pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, _info| {
                    if let Some(el) = pad.parent_element() {
                        el.set_property("ts-offset", ts_offset_ns);
                        info!(
                            "WHEP Output {}: Set ts-offset={}ns on {} (deferred)",
                            iid, ts_offset_ns, name
                        );
                    }
                    gst::PadProbeReturn::Remove
                });
            }
        }

        if let Ok(bin) = whepserversink.clone().downcast::<gst::Bin>() {
            for element in bin.iterate_recurse().into_iter().flatten() {
                let factory_name = element
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if (factory_name == "clocksync" || factory_name == "appsink")
                    && element.has_property("ts-offset")
                {
                    defer_ts_offset(&element, ts_offset_ns, &instance_id_for_ts);
                }
            }
            bin.connect("deep-element-added", false, move |args| {
                let added: gst::Element = args[2].get().unwrap();
                let factory_name = added
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if (factory_name == "clocksync" || factory_name == "appsink")
                    && added.has_property("ts-offset")
                {
                    defer_ts_offset(&added, ts_offset_ns, &instance_id_for_ts);
                }
                None
            });
        }
        info!(
            "WHEP Output: ts-offset={}ms applied to clocksync and appsink elements",
            ts_offset_ms
        );
    }

    // Configure audio/video caps based on which media types are enabled.
    // Video caps will be set dynamically when we detect the input codec.
    if !has_audio {
        whepserversink.set_property("audio-caps", gst::Caps::new_empty());
    }
    if !has_video {
        whepserversink.set_property("video-caps", gst::Caps::new_empty());
    }

    // Install thread priority on session pipelines via pad probes.
    //
    // consumer-pipeline-created fires BEFORE webrtcsink sets its bus sync handler,
    // giving us the session pipeline to connect deep-element-added. Each element
    // gets a one-shot EVENT_DOWNSTREAM probe that sets thread priority on the
    // streaming thread.
    //
    // We cannot use a bus sync handler because webrtcsink's own handler returns
    // BusSyncReply::Drop and routes all messages through an internal channel.
    // Replacing it breaks session lifecycle (sessions never terminate).
    let session_thread_config = ctx.session_thread_config();
    whepserversink.connect("consumer-pipeline-created", false, move |values| {
        if !session_thread_config.is_active() {
            return None;
        }
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let pipeline = values[2].get::<gst::Pipeline>().unwrap();
        session_thread_config.install_on_session_pipeline(&pipeline, &consumer_id);
        None
    });

    // WORKAROUND #1: Relax transceiver codec-preferences BEFORE SDP offer is processed.
    //
    // Problem: webrtcbin does strict caps matching on transceiver codec-preferences.
    // Browser offers profile=baseline, but transceivers have profile-level-id=42c015.
    // webrtcbin doesn't know these are compatible, so transceivers go inactive.
    //
    // Solution: Connect to consumer-added signal (fires BEFORE SDP offer is processed).
    // Modify all video transceivers' codec-preferences to remove profile constraints.
    //
    // Also register the webrtcbin for stats collection (since it's in a separate session pipeline).
    let dynamic_webrtcbin_store = ctx.dynamic_webrtcbin_store();
    let block_id_for_callback = instance_id.to_string();
    let ice_transport_policy = ice_transport_policy.clone();
    whepserversink.connect("consumer-added", false, move |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let webrtcbin = values[2].get::<gst::Element>().unwrap();

        debug!(
            "WHEP Output: consumer-added for {}, modifying transceiver codec-preferences",
            consumer_id
        );

        // Set ICE transport policy on webrtcbin (from config)
        if webrtcbin.has_property("ice-transport-policy") {
            webrtcbin.set_property_from_str("ice-transport-policy", &ice_transport_policy);
            info!(
                "WHEP Output: Set ice-transport-policy={} on webrtcbin for consumer {}",
                ice_transport_policy, consumer_id
            );
        }

        // Register webrtcbin for stats collection
        if let Ok(mut store) = dynamic_webrtcbin_store.lock() {
            store
                .entry(block_id_for_callback.clone())
                .or_default()
                .push((consumer_id.clone(), webrtcbin.clone()));
            debug!(
                "WHEP Output: Registered webrtcbin for block {} consumer {}",
                block_id_for_callback, consumer_id
            );
        }

        // Access transceivers through webrtcbin's sink pads
        // Each sink pad has a "transceiver" property pointing to the associated transceiver
        let mut transceiver_count = 0;
        for pad in webrtcbin.sink_pads() {
            let pad_name = pad.name();

            // Check if this pad has a transceiver property
            if !pad.has_property("transceiver") {
                continue;
            }

            // Get transceiver property from the pad as a generic Object
            let transceiver_value = pad.property_value("transceiver");
            let transceiver = match transceiver_value.get::<gst::glib::Object>() {
                Ok(t) => t,
                Err(_) => continue,
            };

            transceiver_count += 1;

            // Check if transceiver has codec-preferences property
            if !transceiver.has_property("codec-preferences") {
                debug!(
                    "WHEP Output: Transceiver for pad {} has no codec-preferences property",
                    pad_name
                );
                continue;
            }

            // Get current codec-preferences
            let codec_prefs_value = transceiver.property_value("codec-preferences");
            let codec_prefs = match codec_prefs_value.get::<gst::Caps>() {
                Ok(c) => c,
                Err(_) => continue,
            };

            if codec_prefs.is_empty() {
                debug!(
                    "WHEP Output: Transceiver for pad {} has empty codec-preferences",
                    pad_name
                );
                continue;
            }

            debug!(
                "WHEP Output: Transceiver for pad {} codec-preferences: {:?}",
                pad_name, codec_prefs
            );

            // Filter codec-preferences: remove outdated codecs and relax profile constraints.
            // IMPORTANT: Only keep ONE entry per codec type to avoid duplicate streams.
            // Browser may offer multiple H.264 profiles (baseline, main, high) - if we
            // accept all of them after relaxing profile matching, webrtcsink sends the
            // same data on multiple payloads, doubling bandwidth.
            let mut new_caps = gst::Caps::new_empty();
            let mut seen_codecs = std::collections::HashSet::new();
            for i in 0..codec_prefs.size() {
                if let Some(structure) = codec_prefs.structure(i) {
                    let codec_name = structure.name().as_str();
                    // Skip VP8 - outdated codec, not worth offering
                    if codec_name == "video/x-vp8" {
                        continue;
                    }
                    // Only add first occurrence of each codec type
                    if seen_codecs.insert(codec_name.to_string()) {
                        let mut new_structure = structure.to_owned();
                        // H.264 / AV1
                        new_structure.remove_field("profile-level-id");
                        new_structure.remove_field("profile");
                        new_structure.remove_field("level-idx");
                        new_structure.remove_field("tier");
                        // H.265
                        new_structure.remove_field("profile-id");
                        new_structure.remove_field("tier-flag");
                        new_structure.remove_field("level-id");
                        new_structure.remove_field("tx-mode");
                        new_caps.get_mut().unwrap().append_structure(new_structure);
                    }
                }
            }
            if new_caps != codec_prefs {
                debug!(
                    "WHEP Output: Modified transceiver for pad {} codec-preferences: {:?} -> {:?}",
                    pad_name, codec_prefs, new_caps
                );
                transceiver.set_property("codec-preferences", &new_caps);
            }
        }

        debug!(
            "WHEP Output: Processed {} transceivers for consumer {}",
            transceiver_count, consumer_id
        );

        None
    });

    // WORKAROUND #2: a viewer's offered profile must not block the stream.
    super::profile_filter::install(&whepserversink);

    // Handle consumer-removed to clean up webrtcbin from stats storage
    let dynamic_webrtcbin_store_remove = ctx.dynamic_webrtcbin_store();
    let block_id_for_remove = instance_id.to_string();
    whepserversink.connect("consumer-removed", false, move |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();

        // Remove webrtcbin from stats storage
        if let Ok(mut store) = dynamic_webrtcbin_store_remove.lock() {
            if let Some(consumers) = store.get_mut(&block_id_for_remove) {
                consumers.retain(|(cid, _)| cid != &consumer_id);
                debug!(
                    "WHEP Output: Unregistered webrtcbin for block {} consumer {}",
                    block_id_for_remove, consumer_id
                );
            }
        }

        None
    });

    // NOTE: Pre-encoded H.264 has a known limitation with webrtcsink:
    // webrtcsink runs codec discovery for each client, creating a fresh h264parse
    // that needs SPS/PPS from a keyframe. If discovery starts mid-GOP, it times out.
    // Workarounds:
    // 1. Use shorter GOP (30 frames / 1 second recommended for WebRTC)
    // 2. Feed raw video and let webrtcsink encode internally
    // 3. Use webrtcbin directly for full control

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // Create audio processing elements if mode includes audio.
    // For num_audio_tracks > 1 we expose multiple audio_in pads, each with its own
    // queue wired to a distinct request pad (audio_0, audio_1, ...) on
    // whepserversink. The audio-caps property on whepserversink is global, so only
    // the first queue's caps probe drives it — all audio inputs must share the
    // same format (all Opus or all raw). Mixed formats are not supported.
    if has_audio {
        // Shared latch: only the first input that sees a caps event sets audio-caps.
        let audio_caps_set = Arc::new(AtomicBool::new(false));

        for slot in 0..num_audio_tracks {
            // Slot 0 keeps the unsuffixed element id for backwards compatibility
            // with existing flows (matches get_external_pads above).
            let audio_queue_id = if slot == 0 {
                format!("{}:audio_queue", instance_id)
            } else {
                format!("{}:audio_queue_{}", instance_id, slot)
            };
            let audio_queue = gst::ElementFactory::make("queue")
                .name(&audio_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audio_queue (slot {}): {}", slot, e))
                })?;

            let whepserversink_weak = whepserversink.downgrade();
            let audio_caps_set_clone = audio_caps_set.clone();
            let instance_id_owned = instance_id.to_string();

            let audio_queue_sink = audio_queue.static_pad("sink").expect("queue has sink pad");
            audio_queue_sink.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_pad, info| {
                    if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                        if event.type_() == gst::EventType::Caps {
                            // Atomically claim the latch — only one probe wins
                            // and proceeds to set audio-caps; the rest pass.
                            if audio_caps_set_clone
                                .compare_exchange(
                                    false,
                                    true,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                )
                                .is_err()
                            {
                                return gst::PadProbeReturn::Pass;
                            }

                            if let gst::EventView::Caps(caps_event) = event.view() {
                                let caps = caps_event.caps();
                                if let Some(structure) = caps.structure(0) {
                                    let caps_name = structure.name().as_str();

                                    let audio_caps: Option<gst::Caps> = match caps_name {
                                        "audio/x-opus" => {
                                            debug!(
                                                "WHEP Output {} (slot {}): Detected Opus input, setting audio-caps",
                                                instance_id_owned, slot
                                            );
                                            Some(gst::Caps::builder("audio/x-opus").build())
                                        }
                                        "audio/x-raw" => {
                                            debug!(
                                                "WHEP Output {} (slot {}): Detected raw audio, using default audio-caps",
                                                instance_id_owned, slot
                                            );
                                            None
                                        }
                                        _ => {
                                            warn!(
                                                "WHEP Output {} (slot {}): Unknown audio format '{}', using default",
                                                instance_id_owned, slot, caps_name
                                            );
                                            None
                                        }
                                    };

                                    if let Some(caps) = audio_caps {
                                        if let Some(whepserversink) = whepserversink_weak.upgrade()
                                        {
                                            whepserversink.set_property("audio-caps", &caps);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    gst::PadProbeReturn::Pass
                },
            );

            // Audio link: queue -> whepserversink (audio_<slot> request pad)
            internal_links.push((
                ElementPadRef::pad(&audio_queue_id, "src"),
                ElementPadRef::pad(&whepserversink_id, format!("audio_{}", slot)),
            ));

            elements.push((audio_queue_id, audio_queue));
        }

        info!(
            "WHEP Output {}: configured {} audio track(s)",
            instance_id, num_audio_tracks
        );
    }

    // Create video processing elements if mode includes video.
    // For num_video_tracks > 1 we expose multiple video_in pads, each with its own
    // queue wired to a distinct request pad (video_0, video_1, ...) on
    // whepserversink. The video-caps property on whepserversink is global, so only
    // the first queue's caps probe drives it — all video inputs must share the
    // same codec.
    if has_video {
        // Shared latch: only the first input that sees a caps event sets video-caps.
        let video_caps_set = Arc::new(AtomicBool::new(false));

        for slot in 0..num_video_tracks {
            // Slot 0 keeps the unsuffixed element id for backwards compatibility
            // with existing flows (matches get_external_pads above).
            let video_queue_id = if slot == 0 {
                format!("{}:video_queue", instance_id)
            } else {
                format!("{}:video_queue_{}", instance_id, slot)
            };
            let video_queue = gst::ElementFactory::make("queue")
                .name(&video_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video_queue (slot {}): {}", slot, e))
                })?;

            // Dynamic video codec detection: detect input codec and set video-caps
            // on whepserversink before discovery runs. Works with any codec
            // (H264, H265, VP9, AV1, raw). Only the first slot to see a caps
            // event sets the global video-caps property — the rest pass through.
            let whepserversink_weak = whepserversink.downgrade();
            let video_caps_set_clone = video_caps_set.clone();
            let instance_id_owned = instance_id.to_string();

            let video_queue_sink = video_queue.static_pad("sink").expect("queue has sink pad");
            video_queue_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if event.type_() == gst::EventType::Caps {
                        // Atomically claim the latch — only one probe wins
                        // and proceeds to set video-caps; the rest pass.
                        if video_caps_set_clone
                            .compare_exchange(
                                false,
                                true,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            return gst::PadProbeReturn::Pass;
                        }

                        if let gst::EventView::Caps(caps_event) = event.view() {
                            let caps = caps_event.caps();
                            if let Some(structure) = caps.structure(0) {
                                let codec_name = structure.name().as_str();

                                // Map input caps to webrtc-compatible caps.
                                // For pre-encoded video, restrict to that codec only.
                                // For raw video, offer modern codecs (exclude VP8).
                                let video_caps: Option<gst::Caps> = match codec_name {
                                    "video/x-h264" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected H.264 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-h264").build())
                                    }
                                    "video/x-h265" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected H.265 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-h265").build())
                                    }
                                    "video/x-vp9" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected VP9 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-vp9").build())
                                    }
                                    "video/x-av1" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected AV1 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-av1").build())
                                    }
                                    "video/x-raw" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected raw video input, setting video-caps to H.264/H.265/VP9/AV1",
                                            instance_id_owned, slot
                                        );
                                        let mut caps = gst::Caps::new_empty();
                                        {
                                            let caps_mut = caps.get_mut().unwrap();
                                            caps_mut.append(gst::Caps::builder("video/x-h264").build());
                                            caps_mut.append(gst::Caps::builder("video/x-h265").build());
                                            caps_mut.append(gst::Caps::builder("video/x-vp9").build());
                                            caps_mut.append(gst::Caps::builder("video/x-av1").build());
                                        }
                                        Some(caps)
                                    }
                                    _ => {
                                        warn!(
                                            "WHEP Output {} (slot {}): Unknown video codec '{}', using default",
                                            instance_id_owned, slot, codec_name
                                        );
                                        None
                                    }
                                };

                                if let Some(caps) = video_caps {
                                    if let Some(whepserversink) = whepserversink_weak.upgrade() {
                                        whepserversink.set_property("video-caps", &caps);
                                    }
                                }
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Pass
            });

            // Normalize H.264/H.265 caps before they reach webrtcsink.
            // h264parse progressively adds fields (coded-picture-structure, chroma-format,
            // bit-depth-luma, bit-depth-chroma) as it parses the stream. webrtcsink's
            // input_caps_change_allowed() doesn't account for these and rejects them as
            // "renegotiation". This probe removes those fields from CAPS events to
            // prevent false renegotiation errors. Applied per-slot.
            let queue_src_pad = video_queue.static_pad("src").expect("queue has src pad");
            queue_src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if event.type_() == gst::EventType::Caps {
                        if let gst::EventView::Caps(caps_event) = event.view() {
                            let caps = caps_event.caps();
                            if let Some(structure) = caps.structure(0) {
                                if structure.name() == "video/x-h264"
                                    || structure.name() == "video/x-h265"
                                {
                                    let mut new_caps = caps.copy();
                                    if let Some(s) = new_caps.make_mut().structure_mut(0) {
                                        s.remove_fields([
                                            "coded-picture-structure",
                                            "chroma-format",
                                            "bit-depth-luma",
                                            "bit-depth-chroma",
                                        ]);
                                    }
                                    let new_event = gst::event::Caps::new(&new_caps);
                                    info.data = Some(gst::PadProbeData::Event(new_event));
                                }
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });

            // Consumer-side input adaptation: download GL memory, and convert
            // to a format the encoders take natively.
            //
            // whepserversink advertises video/x-raw(memory:GLMemory) on its
            // video request pads, so GL frames negotiate all the way to the
            // sink — and webrtcsink's encoder discovery then finds no encoder
            // that can take them. Video goes missing while audio, on a
            // non-GL path, keeps working. On macOS this is the normal case:
            // decodebin autoplugs vtdec_hw, which outputs GL memory.
            //
            // webrtcsink then builds one encoding chain per consumer, each
            // with its own videoconvert, so whatever format arrives here is
            // converted once per viewer. Converting once, before the sink fans
            // the stream out, leaves a hardware encoder's converter in
            // passthrough; VP9 and AV1 consumers still convert, but from NV12
            // to I420 rather than from RGBA.
            //
            // The producer cannot decide either for us (a GL vision mixer
            // feeding a GL consumer must stay on the GPU), and neither can
            // this block at build time, since the upstream decoder is
            // autoplugged. So both decisions are made from the negotiated caps,
            // again on every caps change: a Media Player moving on to another
            // file can switch memory type or format mid-stream.
            video_input_bridge::install_video_input_bridge(&queue_src_pad, &video_queue_id);

            // Video link: queue -> whepserversink (video_<slot> request pad)
            internal_links.push((
                ElementPadRef::pad(&video_queue_id, "src"),
                ElementPadRef::pad(&whepserversink_id, format!("video_{}", slot)),
            ));

            elements.push((video_queue_id, video_queue));
        }

        info!(
            "WHEP Output {}: configured {} video track(s)",
            instance_id, num_video_tracks
        );
    }

    // Add whepserversink last (after audio/video processing elements)
    elements.push((whepserversink_id.clone(), whepserversink));

    info!(
        "WHEP Output configured: endpoint_id='{}', internal_host={}, stun={:?}, turn={:?}, audio_tracks={}, video_tracks={}",
        endpoint_id, host_addr, stun_server, turn_server, num_audio_tracks, num_video_tracks
    );

    // Register WHEP endpoint with the build context
    ctx.register_whep_endpoint(
        instance_id,
        &endpoint_id,
        internal_port,
        num_audio_tracks,
        num_video_tracks,
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
    use std::time::{Duration, Instant};

    /// `whepserversink` comes from the `gst-plugin-webrtc` crate, which is only
    /// registered by the binary. Tests must register it themselves.
    fn init_gst() {
        let _ = gst::init();
        let _ = gstrswebrtc::plugin_register_static();
    }

    /// Build a property map from explicit key/value pairs.
    fn raw_props(entries: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// The block property must land on the `whepserversink` element itself.
    /// Hardcoding `do-retransmission` back to a literal fails the `false` case.
    #[test]
    fn do_retransmission_reaches_whepserversink() {
        init_gst();

        for expected in [true, false] {
            let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
            let result = build_whepserversink(
                "whep-rtx-test",
                &raw_props(&[("do_retransmission", PropertyValue::Bool(expected))]),
                &ctx,
            )
            .expect("build_whepserversink failed");

            let (_, sink) = result
                .elements
                .iter()
                .find(|(id, _)| id == "whep-rtx-test:whepserversink")
                .expect("whepserversink missing from build result");
            assert_eq!(sink.property::<bool>("do-retransmission"), expected);
        }
    }

    /// The block must convert a video input the encoders cannot take before
    /// `whepserversink` fans it out, or every consumer converts it again.
    #[test]
    fn video_input_is_converted_before_the_sink() {
        init_gst();

        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        let result = build_whepserversink(
            "whep-convert-test",
            &raw_props(&[
                ("num_audio_tracks", PropertyValue::UInt(0)),
                ("num_video_tracks", PropertyValue::UInt(1)),
            ]),
            &ctx,
        )
        .expect("build_whepserversink failed");

        let elements: HashMap<String, gst::Element> = result.elements.iter().cloned().collect();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("videotestsrc");
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .expect("capsfilter");
        pipeline.add_many([&src, &filter]).expect("add");
        for (_, element) in &result.elements {
            pipeline.add(element).expect("add block element");
        }

        let queue = elements
            .get("whep-convert-test:video_queue")
            .expect("video_queue");
        src.link(&filter).expect("link src");
        filter.link(queue).expect("link into the block");
        let sink_pad = elements
            .get("whep-convert-test:whepserversink")
            .expect("whepserversink")
            .request_pad_simple("video_0")
            .expect("video_0 pad");
        queue
            .static_pad("src")
            .expect("queue src")
            .link(&sink_pad)
            .expect("link queue to sink");

        pipeline.set_state(gst::State::Playing).expect("play");

        let bus = pipeline.bus().expect("bus");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut negotiated = String::new();
        while Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                if let gst::MessageView::Error(e) = msg.view() {
                    panic!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
            if let Some(format) = sink_pad
                .current_caps()
                .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()))
            {
                negotiated = format;
                break;
            }
        }

        pipeline.set_state(gst::State::Null).expect("null");
        assert_eq!(
            negotiated, "NV12",
            "the block should convert RGBA before whepserversink fans it out"
        );
    }
}
