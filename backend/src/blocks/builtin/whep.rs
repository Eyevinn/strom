//! WHEP (WebRTC-HTTP Egress Protocol) input block builder.
//!
//! Uses GStreamer's whepclientsrc for receiving WebRTC streams via WHEP signalling.
//! Handles dynamic pad creation by linking new audio streams to a liveadder mixer.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

/// WHEP Input block builder.
pub struct WHEPInputBuilder;

impl BlockBuilder for WHEPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Input block instance: {}", instance_id);

        // Get required WHEP endpoint
        let whep_endpoint = properties
            .get("whep_endpoint")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.clone())
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidProperty("whep_endpoint property required".to_string())
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

        // Get STUN server (optional)
        let stun_server = properties
            .get("stun_server")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.clone())
                    }
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "stun://stun.l.google.com:19302".to_string());

        // Get mixer latency (default 30ms - lower than default 200ms for lower latency)
        let mixer_latency_ms = properties
            .get("mixer_latency_ms")
            .and_then(|v| {
                if let PropertyValue::Int(i) = v {
                    Some(*i as u64)
                } else {
                    None
                }
            })
            .unwrap_or(30);

        // Create namespaced element IDs
        let instance_id_owned = instance_id.to_string();
        let whepclientsrc_id = format!("{}:whepclientsrc", instance_id);
        let liveadder_id = format!("{}:liveadder", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);
        let output_audioconvert_id = format!("{}:output_audioconvert", instance_id);
        let output_audioresample_id = format!("{}:output_audioresample", instance_id);

        // Create whepclientsrc element
        let whepclientsrc = gst::ElementFactory::make("whepclientsrc")
            .name(&whepclientsrc_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("whepclientsrc: {}", e)))?;

        // Set STUN server property on the source
        whepclientsrc.set_property("stun-server", &stun_server);

        // Access the signaller child and set its properties
        let signaller = whepclientsrc.property::<gst::glib::Object>("signaller");
        signaller.set_property("whep-endpoint", &whep_endpoint);

        if let Some(token) = &auth_token {
            signaller.set_property("auth-token", token);
        }

        // Create liveadder - this is our always-present mixer for dynamic audio streams
        // latency property is in milliseconds as a guint (u32)
        let liveadder = gst::ElementFactory::make("liveadder")
            .name(&liveadder_id)
            .property("latency", mixer_latency_ms as u32)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

        // Create a silent audio source to keep liveadder running even without input
        // This prevents the pipeline from getting stuck when no audio streams are present
        let silence_id = format!("{}:silence", instance_id);
        let silence = gst::ElementFactory::make("audiotestsrc")
            .name(&silence_id)
            .property_from_str("wave", "silence")
            .property("is-live", true)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("audiotestsrc (silence): {}", e))
            })?;

        // Create capsfilter to enforce 48kHz stereo audio after liveadder
        let caps = gst::Caps::builder("audio/x-raw")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        // Create output audio processing chain (after liveadder -> capsfilter)
        let output_audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&output_audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioconvert: {}", e)))?;

        let output_audioresample = gst::ElementFactory::make("audioresample")
            .name(&output_audioresample_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("output_audioresample: {}", e))
            })?;

        // Counter for unique element naming
        let stream_counter = Arc::new(AtomicUsize::new(0));

        // Clone references for the pad-added callback
        let liveadder_weak = liveadder.downgrade();
        let stream_counter_clone = Arc::clone(&stream_counter);

        // Set up pad-added callback on whepclientsrc
        // This handles dynamic pads created when WebRTC streams are negotiated
        // NOTE: We can't trust pad names OR query_caps at pad-added time.
        // The actual caps are only set after negotiation completes.
        // Strategy: Install a pad probe to detect actual caps, then:
        // - Audio: decode and route to liveadder
        // - Video: discard via fakesink (no decode - that would be expensive)
        whepclientsrc.connect_pad_added(move |src, pad| {
            let pad_name = pad.name();

            info!(
                "WHEP: New pad added on whepclientsrc: {} - waiting for caps to determine media type",
                pad_name
            );

            if let Some(liveadder) = liveadder_weak.upgrade() {
                let stream_num = stream_counter_clone.fetch_add(1, Ordering::SeqCst);
                if let Err(e) = setup_stream_with_caps_detection(
                    src,
                    pad,
                    &liveadder,
                    &instance_id_owned,
                    stream_num,
                ) {
                    error!("Failed to setup stream with caps detection: {}", e);
                }
            } else {
                error!("WHEP: liveadder no longer exists");
            }
        });

        // ALSO hook into the internal webrtcbin to catch pads that don't get ghostpadded
        // whepclientsrc is a GstBin - we need to find the webrtcbin inside and listen to its pad-added
        if let Ok(bin) = whepclientsrc.clone().downcast::<gst::Bin>() {
            let liveadder_weak2 = liveadder.downgrade();
            let whepclientsrc_weak = whepclientsrc.downgrade();

            // Use deep-element-added to catch webrtcbin when it's created
            bin.connect("deep-element-added", false, move |values| {
                let _bin = values[0].get::<gst::Bin>().unwrap();
                let element = values[2].get::<gst::Element>().unwrap();
                let element_name = element.name();

                // Look for webrtcbin
                if element_name.starts_with("webrtcbin") {
                    info!("WHEP: Found webrtcbin: {}", element_name);

                    let liveadder_weak3 = liveadder_weak2.clone();
                    let whepclientsrc_weak2 = whepclientsrc_weak.clone();

                    // Connect to webrtcbin's pad-added signal
                    element.connect_pad_added(move |_webrtcbin, pad| {
                        let pad_name = pad.name();

                        // Only handle src pads
                        if pad.direction() != gst::PadDirection::Src {
                            return;
                        }

                        info!(
                            "WHEP: webrtcbin pad-added: {} (direction: {:?})",
                            pad_name,
                            pad.direction()
                        );

                        // Check if this pad is already linked (ghostpadded)
                        if pad.is_linked() {
                            info!(
                                "WHEP: webrtcbin pad {} is already linked, skipping",
                                pad_name
                            );
                            return;
                        }

                        // This pad is NOT linked - we need to handle it ourselves
                        info!(
                            "WHEP: webrtcbin pad {} is NOT linked - handling directly",
                            pad_name
                        );

                        // Get whepclientsrc - we need it to create ghost pads
                        let whepclientsrc = match whepclientsrc_weak2.upgrade() {
                            Some(e) => e,
                            None => {
                                error!("WHEP: whepclientsrc no longer exists");
                                return;
                            }
                        };

                        // We don't need the pipeline here anymore since the whepclientsrc pad-added
                        // callback will handle the stream setup, but keep the check to detect errors early
                        let _pipeline = match get_pipeline_from_element(&whepclientsrc) {
                            Ok(p) => p,
                            Err(e) => {
                                error!("WHEP: Failed to get pipeline: {}", e);
                                return;
                            }
                        };

                        if let Some(_liveadder) = liveadder_weak3.upgrade() {
                            // Don't increment stream counter here - the whepclientsrc pad-added callback will do it
                            info!(
                                "WHEP: Setting up unlinked webrtcbin pad {}",
                                pad_name
                            );

                            // We need to ghostpad through the bin hierarchy:
                            // webrtcbin (pad) -> whep-client bin (ghost) -> whepclientsrc (ghost)

                            // Step 1: Find the whep-client bin (parent of webrtcbin)
                            let webrtcbin = match pad.parent_element() {
                                Some(e) => e,
                                None => {
                                    error!("WHEP: Could not get parent element of pad {}", pad_name);
                                    return;
                                }
                            };

                            let whep_client_bin = match webrtcbin.parent() {
                                Some(p) => p,
                                None => {
                                    error!("WHEP: Could not get parent of webrtcbin");
                                    return;
                                }
                            };

                            let whep_client_bin = match whep_client_bin.downcast::<gst::Bin>() {
                                Ok(b) => b,
                                Err(_) => {
                                    error!("WHEP: Parent of webrtcbin is not a bin");
                                    return;
                                }
                            };

                            info!("WHEP: Found intermediate bin: {}", whep_client_bin.name());

                            // Step 2: Create ghost pad on whep-client bin to expose webrtcbin pad
                            let intermediate_ghost_name = format!("ghost_intermediate_{}", pad_name);
                            let intermediate_ghost = match gst::GhostPad::builder_with_target(pad) {
                                Ok(builder) => builder.name(&intermediate_ghost_name).build(),
                                Err(e) => {
                                    error!("WHEP: Failed to create intermediate ghost pad: {}", e);
                                    return;
                                }
                            };

                            if let Err(e) = whep_client_bin.add_pad(&intermediate_ghost) {
                                error!("WHEP: Failed to add intermediate ghost pad to whep-client bin: {}", e);
                                return;
                            }

                            if let Err(e) = intermediate_ghost.set_active(true) {
                                error!("WHEP: Failed to activate intermediate ghost pad: {}", e);
                                return;
                            }

                            info!("WHEP: Created intermediate ghost pad {} on whep-client bin", intermediate_ghost_name);

                            // Step 3: Create ghost pad on whepclientsrc to expose the intermediate ghost pad
                            let outer_ghost_name = format!("ghost_audio_{}", pad_name);
                            let outer_ghost = match gst::GhostPad::builder_with_target(&intermediate_ghost) {
                                Ok(builder) => builder.name(&outer_ghost_name).build(),
                                Err(e) => {
                                    error!("WHEP: Failed to create outer ghost pad: {}", e);
                                    return;
                                }
                            };

                            if let Ok(whepclientsrc_bin) = whepclientsrc.clone().downcast::<gst::Bin>() {
                                if let Err(e) = whepclientsrc_bin.add_pad(&outer_ghost) {
                                    error!("WHEP: Failed to add outer ghost pad to whepclientsrc: {}", e);
                                    return;
                                }

                                if let Err(e) = outer_ghost.set_active(true) {
                                    error!("WHEP: Failed to activate outer ghost pad: {}", e);
                                    return;
                                }

                                info!(
                                    "WHEP: Created outer ghost pad {} on whepclientsrc - will be handled by pad-added callback",
                                    outer_ghost_name
                                );
                            } else {
                                error!("WHEP: whepclientsrc is not a bin, cannot add ghost pad");
                            }
                        }
                    });
                }

                None
            });
        }

        debug!(
            "WHEP Input configured: endpoint={}, stun={}",
            whep_endpoint, stun_server
        );

        // Internal links: silence -> liveadder -> capsfilter -> audioconvert -> audioresample
        // The whepclientsrc pads are linked dynamically via pad-added callback
        let internal_links = vec![
            (
                ElementPadRef::pad(&silence_id, "src"),
                ElementPadRef::pad(&liveadder_id, "sink_%u"),
            ),
            (
                ElementPadRef::pad(&liveadder_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
            (
                ElementPadRef::pad(&capsfilter_id, "src"),
                ElementPadRef::pad(&output_audioconvert_id, "sink"),
            ),
            (
                ElementPadRef::pad(&output_audioconvert_id, "src"),
                ElementPadRef::pad(&output_audioresample_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (whepclientsrc_id, whepclientsrc),
                (silence_id, silence),
                (liveadder_id, liveadder),
                (capsfilter_id, capsfilter),
                (output_audioconvert_id, output_audioconvert),
                (output_audioresample_id, output_audioresample),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Setup a stream from whepclientsrc with caps detection.
/// Uses a pad probe to detect actual caps before deciding how to handle the stream:
/// - Audio: decode and route to liveadder
/// - Video: discard via fakesink (no decode to avoid expensive video decoding)
fn setup_stream_with_caps_detection(
    src: &gst::Element,
    src_pad: &gst::Pad,
    liveadder: &gst::Element,
    instance_id: &str,
    stream_num: usize,
) -> Result<(), String> {
    // Get the pipeline
    let pipeline = get_pipeline_from_element(src)?;

    // Create weak references for the probe callback
    let pipeline_weak = pipeline.downgrade();
    let liveadder_weak = liveadder.downgrade();
    let instance_id_owned = instance_id.to_string();

    // Flag to ensure we only handle this once
    let handled = Arc::new(AtomicBool::new(false));
    let handled_clone = Arc::clone(&handled);

    // Add a probe to detect caps events
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        // Only handle once
        if handled_clone.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Pass;
        }

        if let Some(gst::PadProbeData::Event(ref event)) = info.data {
            if event.type_() == gst::EventType::Caps {
                // Get the caps from the event by viewing it as a Caps event
                if let gst::EventView::Caps(c) = event.view() {
                    let caps = c.caps();
                    if let Some(structure) = caps.structure(0) {
                        let caps_name = structure.name();
                        info!("WHEP: Stream {} detected caps: {}", stream_num, caps_name);

                        // Determine media type - for RTP, look at the "media" field
                        let is_audio = if caps_name == "application/x-rtp" {
                            // RTP caps - check the "media" field
                            let media_field = structure.get::<&str>("media").ok().unwrap_or("");
                            let encoding = structure
                                .get::<&str>("encoding-name")
                                .ok()
                                .unwrap_or("unknown");
                            info!(
                                "WHEP: Stream {} RTP media={}, encoding={}",
                                stream_num, media_field, encoding
                            );
                            media_field == "audio"
                        } else {
                            caps_name.starts_with("audio/")
                        };

                        let is_video = if caps_name == "application/x-rtp" {
                            let media_field = structure.get::<&str>("media").ok().unwrap_or("");
                            media_field == "video"
                        } else {
                            caps_name.starts_with("video/")
                        };

                        // Mark as handled
                        handled_clone.store(true, Ordering::SeqCst);

                        // Get pipeline and liveadder
                        let pipeline = match pipeline_weak.upgrade() {
                            Some(p) => p,
                            None => {
                                error!("WHEP: Pipeline no longer exists");
                                return gst::PadProbeReturn::Remove;
                            }
                        };

                        if is_audio {
                            // Audio stream - use decodebin to decode, then route to liveadder
                            info!(
                                "WHEP: Stream {} is audio, setting up decode chain",
                                stream_num
                            );
                            if let Some(liveadder) = liveadder_weak.upgrade() {
                                if let Err(e) = setup_audio_decode_chain(
                                    pad,
                                    &pipeline,
                                    &liveadder,
                                    &instance_id_owned,
                                    stream_num,
                                ) {
                                    error!("WHEP: Failed to setup audio decode chain: {}", e);
                                }
                            }
                        } else if is_video {
                            // Video stream - use fakesink to discard (no decode)
                            info!(
                                "WHEP: Stream {} is video, discarding via fakesink (no decode)",
                                stream_num
                            );
                            if let Err(e) =
                                setup_video_discard(pad, &pipeline, &instance_id_owned, stream_num)
                            {
                                error!("WHEP: Failed to setup video discard: {}", e);
                            }
                        } else {
                            warn!(
                                "WHEP: Stream {} has unknown media type: {}",
                                stream_num, caps_name
                            );
                        }

                        return gst::PadProbeReturn::Remove;
                    }
                }
            }
        }

        gst::PadProbeReturn::Pass
    });

    info!("WHEP: Caps probe installed on stream {}", stream_num);
    Ok(())
}

/// Get the pipeline from an element, handling nested bins
fn get_pipeline_from_element(element: &gst::Element) -> Result<gst::Pipeline, String> {
    let parent = element
        .parent()
        .ok_or("Could not get parent from element")?;

    // Try direct pipeline
    if let Ok(pipeline) = parent.clone().downcast::<gst::Pipeline>() {
        return Ok(pipeline);
    }

    // Try parent of parent (for nested bins)
    if let Some(grandparent) = parent.parent() {
        if let Ok(pipeline) = grandparent.downcast::<gst::Pipeline>() {
            return Ok(pipeline);
        }
    }

    // Try to get from bin
    if let Ok(bin) = parent.downcast::<gst::Bin>() {
        if let Some(p) = bin.parent() {
            if let Ok(pipeline) = p.downcast::<gst::Pipeline>() {
                return Ok(pipeline);
            }
        }
    }

    Err("Could not find pipeline from element".to_string())
}

/// Setup audio decode chain: decodebin -> audioconvert -> audioresample -> liveadder
fn setup_audio_decode_chain(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    liveadder: &gst::Element,
    instance_id: &str,
    stream_num: usize,
) -> Result<(), String> {
    // Create unique element names
    let decodebin_name = format!("{}:decodebin_{}", instance_id, stream_num);
    let audioconvert_name = format!("{}:stream_audioconvert_{}", instance_id, stream_num);
    let audioresample_name = format!("{}:stream_audioresample_{}", instance_id, stream_num);

    // Create decodebin for audio decoding
    let decodebin = gst::ElementFactory::make("decodebin")
        .name(&decodebin_name)
        .build()
        .map_err(|e| format!("Failed to create decodebin: {}", e))?;

    // Create audioconvert and audioresample
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_name)
        .build()
        .map_err(|e| format!("Failed to create audioconvert: {}", e))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_name)
        .build()
        .map_err(|e| format!("Failed to create audioresample: {}", e))?;

    // Add elements to pipeline IMMEDIATELY so they don't get dropped when this function returns
    // The callback will fire later, and we need these elements to still exist
    pipeline
        .add(&audioconvert)
        .map_err(|e| format!("Failed to add audioconvert to pipeline: {}", e))?;
    pipeline
        .add(&audioresample)
        .map_err(|e| format!("Failed to add audioresample to pipeline: {}", e))?;

    info!(
        "WHEP: Added stream {} audioconvert and audioresample to pipeline",
        stream_num
    );

    // Clone references for decodebin's pad-added callback
    let audioconvert_weak = audioconvert.downgrade();
    let audioresample_weak = audioresample.downgrade();
    let liveadder_weak = liveadder.downgrade();
    let stream_num_clone = stream_num;

    // Set up decodebin's pad-added callback to link to audioconvert
    decodebin.connect_pad_added(move |_decodebin, pad| {
        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        if let Some(caps) = caps {
            if let Some(structure) = caps.structure(0) {
                if structure.name().starts_with("audio/") {
                    info!(
                        "WHEP: Stream {} decodebin output pad is audio, linking to processing chain",
                        stream_num_clone
                    );

                    // Upgrade weak refs - elements are already in the pipeline so they should exist
                    let (audioconvert, audioresample, liveadder) = match (
                        audioconvert_weak.upgrade(),
                        audioresample_weak.upgrade(),
                        liveadder_weak.upgrade(),
                    ) {
                        (Some(a), Some(b), Some(c)) => (a, b, c),
                        _ => {
                            error!(
                                "WHEP: Stream {} - Failed to upgrade element refs in callback",
                                stream_num_clone
                            );
                            return;
                        }
                    };

                    // Sync element states BEFORE linking (need at least READY state)
                    if let Err(e) = audioconvert.sync_state_with_parent() {
                        error!("Failed to sync audioconvert state: {}", e);
                        return;
                    }
                    if let Err(e) = audioresample.sync_state_with_parent() {
                        error!("Failed to sync audioresample state: {}", e);
                        return;
                    }
                    info!(
                        "WHEP: Stream {} synced audioconvert and audioresample states",
                        stream_num_clone
                    );

                    // Link decodebin -> audioconvert
                    let audioconvert_sink = audioconvert.static_pad("sink").unwrap();
                    if let Err(e) = pad.link(&audioconvert_sink) {
                        error!("Failed to link decodebin to audioconvert: {:?}", e);
                        return;
                    }
                    info!("WHEP: Stream {} linked decodebin to audioconvert", stream_num_clone);

                    // Link audioconvert -> audioresample
                    if let Err(e) = audioconvert.link(&audioresample) {
                        error!("Failed to link audioconvert to audioresample: {:?}", e);
                        return;
                    }
                    info!(
                        "WHEP: Stream {} linked audioconvert to audioresample",
                        stream_num_clone
                    );

                    // Request a sink pad from liveadder and link
                    if let Some(liveadder_sink) = liveadder.request_pad_simple("sink_%u") {
                        info!(
                            "WHEP: Stream {} got liveadder sink pad: {}",
                            stream_num_clone,
                            liveadder_sink.name()
                        );
                        let audioresample_src = audioresample.static_pad("src").unwrap();
                        if let Err(e) = audioresample_src.link(&liveadder_sink) {
                            error!("Failed to link audioresample to liveadder: {:?}", e);
                            return;
                        }
                        info!(
                            "WHEP: Stream {} successfully linked audio stream to liveadder",
                            stream_num_clone
                        );
                    } else {
                        error!("Failed to request sink pad from liveadder");
                    }
                }
            }
        }
    });

    // Add decodebin to pipeline
    pipeline
        .add(&decodebin)
        .map_err(|e| format!("Failed to add decodebin to pipeline: {}", e))?;

    // Link src_pad to decodebin sink
    let decodebin_sink = decodebin
        .static_pad("sink")
        .ok_or("Decodebin has no sink pad")?;
    src_pad
        .link(&decodebin_sink)
        .map_err(|e| format!("Failed to link to decodebin: {:?}", e))?;

    // Sync decodebin state with pipeline
    decodebin
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync decodebin state: {}", e))?;

    info!(
        "WHEP: Audio decode chain setup complete for stream {}",
        stream_num
    );
    Ok(())
}

/// Setup video discard: fakesink (no decoding, just discard the video stream)
fn setup_video_discard(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    instance_id: &str,
    stream_num: usize,
) -> Result<(), String> {
    let fakesink_name = format!("{}:video_fakesink_{}", instance_id, stream_num);

    // Create fakesink to discard video without decoding
    let fakesink = gst::ElementFactory::make("fakesink")
        .name(&fakesink_name)
        .property("sync", false) // Don't sync, just drop
        .property("async", false)
        .build()
        .map_err(|e| format!("Failed to create fakesink: {}", e))?;

    // Add to pipeline
    pipeline
        .add(&fakesink)
        .map_err(|e| format!("Failed to add fakesink to pipeline: {}", e))?;

    // Link src_pad to fakesink
    let fakesink_sink = fakesink
        .static_pad("sink")
        .ok_or("Fakesink has no sink pad")?;
    src_pad
        .link(&fakesink_sink)
        .map_err(|e| format!("Failed to link to fakesink: {:?}", e))?;

    // Sync fakesink state with pipeline
    fakesink
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync fakesink state: {}", e))?;

    info!(
        "WHEP: Video discard (fakesink) setup complete for stream {}",
        stream_num
    );
    Ok(())
}

/// Get metadata for WHEP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whep_input_definition()]
}

/// Get WHEP Input block definition (metadata only).
fn whep_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_input".to_string(),
        name: "WHEP Input".to_string(),
        description: "Receives audio/video via WebRTC WHEP protocol. Uses whepclientsrc with liveadder for multi-stream mixing.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "whep_endpoint".to_string(),
                label: "WHEP Endpoint".to_string(),
                description: "WHEP server endpoint URL (e.g., https://example.com/whep/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whep_endpoint".to_string(),
                    transform: None,
                },
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
            },
            ExposedProperty {
                name: "stun_server".to_string(),
                label: "STUN Server".to_string(),
                description: "STUN server URL for NAT traversal".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(
                    "stun://stun.l.google.com:19302".to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stun_server".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "mixer_latency_ms".to_string(),
                label: "Mixer Latency (ms)".to_string(),
                description: "Latency of the audio mixer in milliseconds (default 30ms, lower = less delay but may cause glitches)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(30)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mixer_latency_ms".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "output_audioresample".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            color: Some("#4CAF50".to_string()), // Green for inputs
            width: Some(2.5),
            height: Some(1.5),
        }),
    }
}
