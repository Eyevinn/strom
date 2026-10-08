//! WHEP Input using `whepclientsrc` (signaller-based).

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult};
use crate::gst::whep_probe;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue};
use tracing::{debug, error, info};

use super::stream::{get_pipeline_from_element, setup_stream_with_caps_detection};
use crate::blocks::builtin::webrtc_props::parse_drop_on_latency;

/// Build using the new whepclientsrc (signaller-based) implementation
pub(super) fn build_whepclientsrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Input using whepclientsrc (new implementation)");

    // Get required WHEP endpoint
    let whep_endpoint = properties
        .get("whep_endpoint")
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

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();
    let ice_transport_policy = ctx.resolve_ice_transport_policy(properties);

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

    // Get jitterbuffer latency (default 200ms is GStreamer's webrtcbin default)
    let jitterbuffer_latency_ms = properties
        .get("jitterbuffer_latency_ms")
        .and_then(|v| {
            if let PropertyValue::Int(i) = v {
                Some(*i as u32)
            } else {
                None
            }
        })
        .unwrap_or(DEFAULT_JITTERBUFFER_LATENCY_MS as u32);
    let drop_on_latency = parse_drop_on_latency(properties);

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

    // Set ICE server properties on the source (explicitly clear defaults when
    // not configured, since webrtcsrc defaults to stun://stun.l.google.com:19302)
    match stun_server {
        Some(ref stun) => whepclientsrc.set_property("stun-server", stun),
        None => whepclientsrc.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        whepclientsrc.set_property("turn-server", turn);
    }

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

    // Set min-upstream-latency so liveadder accounts for jitterbuffer buffering delay
    if liveadder.find_property("min-upstream-latency").is_some() {
        let min_upstream_ns = jitterbuffer_latency_ms as u64 * 1_000_000;
        liveadder.set_property(
            "min-upstream-latency",
            min_upstream_ns * gst::ClockTime::NSECOND,
        );
        info!(
            "WHEP Input (whepclientsrc): Set min-upstream-latency={}ms on liveadder",
            jitterbuffer_latency_ms
        );
    }

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

    // Set up WHEP diagnostic probes if enabled
    let probe_registry = whep_probe::setup_whep_probes(&whepclientsrc, &instance_id_owned);

    // Counter for unique element naming
    let stream_counter = Arc::new(AtomicUsize::new(0));

    // Clone references for the pad-added callback
    let liveadder_weak = liveadder.downgrade();
    let stream_counter_clone = Arc::clone(&stream_counter);
    let probe_registry_clone = probe_registry.clone();

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
                &probe_registry_clone,
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
        let ice_transport_policy = ice_transport_policy.clone();

        // Use deep-element-added to catch webrtcbin when it's created
        bin.connect("deep-element-added", false, move |values| {
                let _bin = values[0].get::<gst::Bin>().unwrap();
                let element = values[2].get::<gst::Element>().unwrap();
                let element_name = element.name();

                // Workaround for GStreamer rtpjitterbuffer packet_spacing bug:
                // see comment in build_whepsrc iterate_recurse for details.
                if element_name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                    element.set_property("drop-on-latency", drop_on_latency);
                    info!(
                        "WHEP Input (whepclientsrc): Set drop-on-latency={} on {}",
                        drop_on_latency, element_name
                    );
                }

                // Look for webrtcbin
                if element_name.starts_with("webrtcbin") {
                    info!("WHEP: Found webrtcbin: {}", element_name);

                    // Set jitterbuffer latency on webrtcbin
                    if element.has_property("latency") {
                        element.set_property("latency", jitterbuffer_latency_ms);
                        info!(
                            "WHEP Input (whepclientsrc): Set jitterbuffer latency={}ms on {}",
                            jitterbuffer_latency_ms, element_name
                        );
                    }

                    // Set ICE transport policy on webrtcbin (from config)
                    if element.has_property("ice-transport-policy") {
                        element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                        info!(
                            "WHEP Input: Set ice-transport-policy={} on webrtcbin {}",
                            ice_transport_policy, element_name
                        );
                    }

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
        "WHEP Input configured: endpoint={}, stun={:?}, turn={:?}, ice_transport_policy={}",
        whep_endpoint, stun_server, turn_server, ice_transport_policy
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
