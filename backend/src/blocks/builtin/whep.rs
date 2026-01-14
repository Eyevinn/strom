//! WHEP (WebRTC-HTTP Egress Protocol) block builders.
//!
//! WHEP Input - Receives streams from external WHEP servers:
//! - `whepclientsrc` (new): Uses signaller interface
//! - `whepsrc` (stable): Simpler implementation with direct properties
//!
//! WHEP Output - Hosts a WHEP server for clients to connect and receive streams:
//! - `whepserversink`: Hosts HTTP endpoint, clients connect via WHEP to receive
//!
//! Handles dynamic pad creation by linking new audio streams to a liveadder mixer.

use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, WhepStreamMode,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// WHEP Input block builder.
pub struct WHEPInputBuilder;

/// WHEP Output block builder (hosts WHEP server).
pub struct WHEPOutputBuilder;

impl BlockBuilder for WHEPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Output block instance: {}", instance_id);
        build_whepserversink(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        // Get mode from properties
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(WhepStreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let mut inputs = Vec::new();

        if mode.has_audio() {
            inputs.push(ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }

        if mode.has_video() {
            inputs.push(ExternalPad {
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_queue".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }
}

impl BlockBuilder for WHEPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Input block instance: {}", instance_id);

        // Get implementation choice (default to stable whepsrc)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whepclientsrc")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whepclientsrc(instance_id, properties)
        } else {
            build_whepsrc(instance_id, properties)
        }
    }
}

/// Build using the stable whepsrc implementation
fn build_whepsrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Input using whepsrc (stable)");

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
    let whepsrc_id = format!("{}:whepsrc", instance_id);
    let liveadder_id = format!("{}:liveadder", instance_id);
    let capsfilter_id = format!("{}:capsfilter", instance_id);
    let output_audioconvert_id = format!("{}:output_audioconvert", instance_id);
    let output_audioresample_id = format!("{}:output_audioresample", instance_id);

    // Create whepsrc element (stable - direct properties)
    let whepsrc = gst::ElementFactory::make("whepsrc")
        .name(&whepsrc_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepsrc: {}", e)))?;

    // Set properties directly on whepsrc (no signaller child)
    whepsrc.set_property("whep-endpoint", &whep_endpoint);
    whepsrc.set_property("stun-server", &stun_server);

    if let Some(token) = &auth_token {
        whepsrc.set_property("auth-token", token);
    }

    // Create liveadder - this is our always-present mixer for dynamic audio streams
    // force-live=true: operate in live mode and aggregate on timeout even without upstream live sources
    // start-time-selection=first: use the first buffer's timestamp as start time (essential for PTP clocks)
    //   Without this, liveadder defaults to start-time=0, but PTP clock running time is billions of ns
    let liveadder = gst::ElementFactory::make("liveadder")
        .name(&liveadder_id)
        .property("latency", mixer_latency_ms as u32)
        .property("force-live", true)
        .property_from_str("start-time-selection", "first")
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

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

    // Create output audio processing chain
    let output_audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&output_audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioconvert: {}", e)))?;

    let output_audioresample = gst::ElementFactory::make("audioresample")
        .name(&output_audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioresample: {}", e)))?;

    // Counter for unique element naming
    let stream_counter = Arc::new(AtomicUsize::new(0));

    // Clone references for the pad-added callback
    let liveadder_weak = liveadder.downgrade();
    let stream_counter_clone = Arc::clone(&stream_counter);

    // Set up pad-added callback on whepsrc
    // whepsrc also creates dynamic src_%u pads like whepclientsrc
    whepsrc.connect_pad_added(move |src, pad| {
        let pad_name = pad.name();

        info!(
            "WHEP (stable): New pad added on whepsrc: {} - waiting for caps to determine media type",
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
            error!("WHEP (stable): liveadder no longer exists");
        }
    });

    debug!(
        "WHEP Input (whepsrc stable) configured: endpoint={}, stun={}",
        whep_endpoint, stun_server
    );

    // Internal links: liveadder -> capsfilter -> audioconvert -> audioresample
    // Note: No silence generator - using force-live=true on liveadder instead
    // WHEP audio streams are linked dynamically via pad-added callback
    let internal_links = vec![
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
            (whepsrc_id, whepsrc),
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

/// Build using the new whepclientsrc (signaller-based) implementation
fn build_whepclientsrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Input using whepclientsrc (new implementation)");

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
    // force-live=true: operate in live mode and aggregate on timeout even without upstream live sources
    // start-time-selection=first: use the first buffer's timestamp as start time (essential for PTP clocks)
    //   Without this, liveadder defaults to start-time=0, but PTP clock running time is billions of ns
    let liveadder = gst::ElementFactory::make("liveadder")
        .name(&liveadder_id)
        .property("latency", mixer_latency_ms as u32)
        .property("force-live", true)
        .property_from_str("start-time-selection", "first")
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

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
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioresample: {}", e)))?;

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

    // Internal links: liveadder -> capsfilter -> audioconvert -> audioresample
    // Note: No silence generator - using force-live=true on liveadder instead
    // WHEP audio streams are linked dynamically via pad-added callback
    let internal_links = vec![
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
fn build_whepserversink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Output using whepserversink (server mode)");

    // Get mode (audio, video, or audio_video)
    let mode = properties
        .get("mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(WhepStreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or_default();

    info!("WHEP Output mode: {:?}", mode);

    // Get endpoint_id (user-configurable, defaults to UUID)
    let endpoint_id = properties
        .get("endpoint_id")
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

    // Create whepserversink element
    // This is based on webrtcsink and handles encoding internally
    let whepserversink_id = format!("{}:whepserversink", instance_id);
    let whepserversink = gst::ElementFactory::make("whepserversink")
        .name(&whepserversink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepserversink: {}", e)))?;

    // Set STUN server property
    whepserversink.set_property("stun-server", &stun_server);

    // Disable FEC and RTX (retransmission) to avoid bandwidth overhead
    // These are enabled by default in webrtcsink and can significantly increase bandwidth:
    // - FEC adds redundancy packets (can add ~50% overhead)
    // - RTX sends duplicate packets for retransmission
    // For pre-encoded video at high bitrates, these can cause near-double bandwidth usage
    whepserversink.set_property("do-fec", false);
    whepserversink.set_property("do-retransmission", false);

    // Access the signaller child and set its properties
    // Bind to localhost only - axum will proxy external requests
    let signaller = whepserversink.property::<gst::glib::Object>("signaller");
    let host_addr = format!("http://127.0.0.1:{}", internal_port);
    signaller.set_property("host-addr", &host_addr);

    // Configure audio/video caps based on mode.
    // Video caps will be set dynamically when we detect the input codec.
    if !mode.has_audio() {
        whepserversink.set_property("audio-caps", gst::Caps::new_empty());
    }
    if !mode.has_video() {
        whepserversink.set_property("video-caps", gst::Caps::new_empty());
    }

    // WORKAROUND #1: Relax transceiver codec-preferences BEFORE SDP offer is processed.
    //
    // Problem: webrtcbin does strict caps matching on transceiver codec-preferences.
    // Browser offers profile=baseline, but transceivers have profile-level-id=42c015.
    // webrtcbin doesn't know these are compatible, so transceivers go inactive.
    //
    // Solution: Connect to consumer-added signal (fires BEFORE SDP offer is processed).
    // Modify all video transceivers' codec-preferences to remove profile constraints.
    whepserversink.connect("consumer-added", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let webrtcbin = values[2].get::<gst::Element>().unwrap();

        info!(
            "WHEP Output: consumer-added for {}, modifying transceiver codec-preferences",
            consumer_id
        );

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
                info!(
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
                info!(
                    "WHEP Output: Transceiver for pad {} has empty codec-preferences",
                    pad_name
                );
                continue;
            }

            info!(
                "WHEP Output: Transceiver for pad {} codec-preferences: {:?}",
                pad_name, codec_prefs
            );

            // Check if this transceiver has profile constraints that need relaxing
            let mut needs_modification = false;
            for i in 0..codec_prefs.size() {
                if let Some(s) = codec_prefs.structure(i) {
                    if s.has_field("profile-level-id") || s.has_field("profile") {
                        needs_modification = true;
                        break;
                    }
                }
            }

            if needs_modification {
                // Create new caps without profile constraints
                // IMPORTANT: Only keep ONE entry per codec type to avoid duplicate streams.
                // Browser may offer multiple H.264 profiles (baseline, main, high) - if we
                // accept all of them after relaxing profile matching, webrtcsink sends the
                // same data on multiple payloads, doubling bandwidth.
                let mut new_caps = gst::Caps::new_empty();
                let mut seen_codecs = std::collections::HashSet::new();
                for i in 0..codec_prefs.size() {
                    if let Some(structure) = codec_prefs.structure(i) {
                        let codec_name = structure.name().as_str();
                        // Only add first occurrence of each codec type
                        if seen_codecs.insert(codec_name.to_string()) {
                            let mut new_structure = structure.to_owned();
                            new_structure.remove_field("profile-level-id");
                            new_structure.remove_field("profile");
                            new_caps.get_mut().unwrap().append_structure(new_structure);
                        }
                    }
                }
                info!(
                    "WHEP Output: Relaxing transceiver for pad {} codec-preferences: {:?} -> {:?}",
                    pad_name, codec_prefs, new_caps
                );
                transceiver.set_property("codec-preferences", &new_caps);
            }
        }

        info!(
            "WHEP Output: Processed {} transceivers for consumer {}",
            transceiver_count, consumer_id
        );

        None
    });

    // WORKAROUND #2: Relax strict H.264 profile-level-id matching in capsfilters.
    //
    // Problem: Browsers send profile-level-id=42e01f (Constrained Baseline + Extended constraint).
    // Our encoder produces 42c0xx (Constrained Baseline without Extended constraint).
    // Both are codec-compatible, but webrtcsink does strict string matching on profile-level-id
    // in the capsfilter it creates during discovery, causing a 30-second timeout.
    //
    // Solution: Connect to payloader-setup signal. Find the downstream capsfilter (pay_filter)
    // and remove the profile-level-id constraint.
    whepserversink.connect("payloader-setup", false, |values| {
        // values[0] = self, values[1] = consumer_id, values[2] = pad_name, values[3] = payloader
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let payloader = values[3].get::<gst::Element>().unwrap();

        // Always relax profile matching - both for discovery and real consumer sessions.
        // This allows browsers requesting profile-level-id=42e01f to connect to streams
        // encoded with profile-level-id=42c015 (both are constrained-baseline compatible).
        // Find the downstream capsfilter by traversing from payloader's src pad
        if let Some(src_pad) = payloader.static_pad("src") {
            if let Some(peer_pad) = src_pad.peer() {
                if let Some(capsfilter) = peer_pad.parent_element() {
                    // Verify it's a capsfilter
                    if capsfilter
                        .factory()
                        .map(|f| f.name().as_str() == "capsfilter")
                        .unwrap_or(false)
                    {
                        let caps: gst::Caps = capsfilter.property("caps");
                        // Check if caps contain profile constraints (H.264 RTP caps)
                        // Browsers may send profile-level-id, profile, or both
                        if let Some(s) = caps.structure(0) {
                            if s.has_field("profile-level-id") || s.has_field("profile") {
                                // Create new caps without profile constraints
                                // Use merge_structure to deduplicate identical caps
                                let mut new_caps = gst::Caps::new_empty();
                                for i in 0..caps.size() {
                                    if let Some(structure) = caps.structure(i) {
                                        let mut new_structure: gst::Structure =
                                            structure.to_owned();
                                        new_structure.remove_field("profile-level-id");
                                        new_structure.remove_field("profile");
                                        new_caps.merge_structure(new_structure);
                                    }
                                }
                                info!(
                                    "WHEP Output: Relaxing capsfilter profile matching for {}: {:?} -> {:?}",
                                    consumer_id, caps, new_caps
                                );
                                capsfilter.set_property("caps", &new_caps);
                            }
                        }
                    }
                }
            }
        }
        // Return false to let default handler run as well
        Some(false.to_value())
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

    // Create audio processing elements if mode includes audio
    if mode.has_audio() {
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        // Audio links: audioconvert -> audioresample -> whepserversink (audio_0 request pad)
        internal_links.push((
            ElementPadRef::pad(&audioconvert_id, "src"),
            ElementPadRef::pad(&audioresample_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&audioresample_id, "src"),
            ElementPadRef::pad(&whepserversink_id, "audio_0"),
        ));

        elements.push((audioconvert_id, audioconvert));
        elements.push((audioresample_id, audioresample));
    }

    // Create video queue and link to whepserversink if mode includes video
    if mode.has_video() {
        let video_queue_id = format!("{}:video_queue", instance_id);

        let video_queue = gst::ElementFactory::make("queue")
            .name(&video_queue_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("video_queue: {}", e)))?;

        // Dynamic video codec detection: Add a pad probe to detect input codec
        // and set video-caps on whepserversink before discovery runs.
        // This allows the WHEP block to work with any codec (H264, H265, VP8, VP9, AV1, raw).
        let whepserversink_weak = whepserversink.downgrade();
        let caps_set = Arc::new(AtomicBool::new(false));
        let caps_set_clone = caps_set.clone();

        let video_queue_sink = video_queue.static_pad("sink").expect("queue has sink pad");
        video_queue_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            // Only process CAPS events, and only once
            if caps_set_clone.load(Ordering::SeqCst) {
                return gst::PadProbeReturn::Pass;
            }

            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                if event.type_() == gst::EventType::Caps {
                    if let gst::EventView::Caps(caps_event) = event.view() {
                        let caps = caps_event.caps();
                        if let Some(structure) = caps.structure(0) {
                            let codec_name = structure.name().as_str();

                            // Map input caps to webrtc-compatible caps
                            // For pre-encoded video, restrict to that codec only
                            // For raw video, don't set video-caps (let webrtcsink use defaults)
                            let video_caps: Option<gst::Caps> = match codec_name {
                                "video/x-h264" => {
                                    info!("WHEP Output: Detected H.264 input, setting video-caps");
                                    Some(gst::Caps::builder("video/x-h264").build())
                                }
                                "video/x-h265" => {
                                    info!("WHEP Output: Detected H.265 input, setting video-caps");
                                    Some(gst::Caps::builder("video/x-h265").build())
                                }
                                "video/x-vp8" => {
                                    info!("WHEP Output: Detected VP8 input, setting video-caps");
                                    Some(gst::Caps::builder("video/x-vp8").build())
                                }
                                "video/x-vp9" => {
                                    info!("WHEP Output: Detected VP9 input, setting video-caps");
                                    Some(gst::Caps::builder("video/x-vp9").build())
                                }
                                "video/x-av1" => {
                                    info!("WHEP Output: Detected AV1 input, setting video-caps");
                                    Some(gst::Caps::builder("video/x-av1").build())
                                }
                                "video/x-raw" => {
                                    // Raw video - let webrtcsink encode with default codecs
                                    info!(
                                        "WHEP Output: Detected raw video input, using default video-caps"
                                    );
                                    None
                                }
                                _ => {
                                    warn!(
                                        "WHEP Output: Unknown video codec '{}', using default",
                                        codec_name
                                    );
                                    None
                                }
                            };

                            if let Some(caps) = video_caps {
                                if let Some(whepserversink) = whepserversink_weak.upgrade() {
                                    whepserversink.set_property("video-caps", &caps);
                                }
                            }
                            caps_set_clone.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            gst::PadProbeReturn::Pass
        });

        // Skip additional parsing - feed directly to whepserversink.
        // webrtcsink handles codec discovery internally. Adding our own parser
        // caused issues because webrtcsink's internal discovery pipeline creates
        // its own parser that converts stream format, losing codec_data.
        //
        // For pre-encoded video, we rely on:
        // 1. Upstream parser (in VideoEncoder block) with config-interval=1 for H.264
        // 2. Dynamic video-caps detection (probe above sets caps based on input codec)
        info!("WHEP Output: Passing video directly to whepserversink (no additional parsing)");

        // Add a pad probe to normalize H.264 caps before they reach webrtcsink.
        // h264parse progressively adds fields (coded-picture-structure, chroma-format,
        // bit-depth-luma, bit-depth-chroma) as it parses the stream. webrtcsink's
        // input_caps_change_allowed() doesn't account for these and rejects them as "renegotiation".
        // This probe removes those fields from CAPS events to prevent false renegotiation errors.
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
                                // Create new caps without h264parse/h265parse-specific fields
                                let mut new_caps = caps.copy();
                                if let Some(s) = new_caps.make_mut().structure_mut(0) {
                                    s.remove_fields([
                                        "coded-picture-structure",
                                        "chroma-format",
                                        "bit-depth-luma",
                                        "bit-depth-chroma",
                                    ]);
                                }
                                // Replace the event with one containing cleaned caps
                                let new_event = gst::event::Caps::new(&new_caps);
                                info.data = Some(gst::PadProbeData::Event(new_event));
                            }
                        }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        // Video link: queue -> whepserversink (video_0 request pad)
        internal_links.push((
            ElementPadRef::pad(&video_queue_id, "src"),
            ElementPadRef::pad(&whepserversink_id, "video_0"),
        ));

        elements.push((video_queue_id, video_queue));
    }

    // Add whepserversink last (after audio/video processing elements)
    elements.push((whepserversink_id.clone(), whepserversink));

    info!(
        "WHEP Output configured: endpoint_id='{}', internal_host={}, stun={}, mode={:?}",
        endpoint_id, host_addr, stun_server, mode
    );

    // Register WHEP endpoint with the build context
    ctx.register_whep_endpoint(instance_id, &endpoint_id, internal_port, mode);

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Setup a stream from whepclientsrc/whepsrc with caps detection.
/// Uses an identity element to immediately claim the pad (preventing auto-tee),
/// then a pad probe to detect actual caps before deciding how to handle the stream:
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

    // Create identity element IMMEDIATELY to claim the pad and prevent auto-tee
    let identity_name = format!("{}:stream_identity_{}", instance_id, stream_num);
    let identity = gst::ElementFactory::make("identity")
        .name(&identity_name)
        .build()
        .map_err(|e| format!("Failed to create identity: {}", e))?;

    // Add identity to pipeline
    pipeline
        .add(&identity)
        .map_err(|e| format!("Failed to add identity to pipeline: {}", e))?;

    // Sync identity state with pipeline
    identity
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync identity state: {}", e))?;

    // Link src_pad to identity IMMEDIATELY - this prevents auto-tee from claiming the pad
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("Identity has no sink pad")?;
    src_pad
        .link(&identity_sink)
        .map_err(|e| format!("Failed to link to identity: {:?}", e))?;

    info!(
        "WHEP: Stream {} linked to identity (preventing auto-tee)",
        stream_num
    );

    // Get identity's src pad for the probe
    let identity_src = identity
        .static_pad("src")
        .ok_or("Identity has no src pad")?;

    // Create weak references for the probe callback
    let pipeline_weak = pipeline.downgrade();
    let liveadder_weak = liveadder.downgrade();
    let instance_id_owned = instance_id.to_string();

    // Flag to ensure we only handle this once
    let handled = Arc::new(AtomicBool::new(false));
    let handled_clone = Arc::clone(&handled);

    // Add a probe on identity's src pad to detect caps events
    identity_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
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

    info!(
        "WHEP: Caps probe installed on stream {} (via identity)",
        stream_num
    );
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
                        // Enable QoS messages on this pad so we can see if buffers are being dropped
                        liveadder_sink.set_property("qos-messages", true);
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
    vec![whep_input_definition(), whep_output_definition()]
}

/// Get WHEP Input block definition (metadata only).
fn whep_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_input".to_string(),
        name: "WHEP Input".to_string(),
        description: "Receives audio/video via WebRTC WHEP protocol. Default uses stable whepsrc element.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whepsrc (stable) or whepclientsrc (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whepsrc".to_string(),
                            label: Some("whepsrc (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whepclientsrc".to_string(),
                            label: Some("whepclientsrc (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whepsrc".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
            },
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
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get WHEP Output block definition (server mode - hosts WHEP endpoint).
fn whep_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_output".to_string(),
        name: "WHEP Output".to_string(),
        description: "Hosts a WHEP server endpoint. Clients can connect via WHEP to receive the WebRTC stream. Access at /api/whep/{endpoint_id}".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "What media to stream: audio only, video only, or both".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio Only".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video Only".to_string()),
                        },
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHEP endpoint. Leave empty to auto-generate a UUID. Access at /api/whep/{endpoint_id}".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "endpoint_id".to_string(),
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
        ],
        // Note: external_pads here are the static defaults. The actual pads are
        // determined dynamically by WHEPOutputBuilder::get_external_pads() based on mode.
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
