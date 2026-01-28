//! Audio Router block for flexible channel routing between multiple audio streams.
//!
//! This block provides:
//! - Dynamic number of inputs/outputs (1-8 each)
//! - Configurable channel count per stream (1-64 channels)
//! - Full routing matrix: any input channel to any output channel
//! - Fan-out: one input channel can route to multiple outputs (via tee)
//! - Mixing: multiple input channels can be summed to one output (via audiomixer)
//!   Note: audiomixer is only used when multiple inputs route to the same output channel
//!
//! Pipeline structure (simple routing):
//! ```text
//! Input_N → identity_N → deinterleave_N → [tee] → queue → interleave_M → capssetter_M → queue_out_M → Output_M
//! ```
//!
//! Pipeline structure (when mixing needed):
//! ```text
//! Input_N → identity_N → deinterleave_N → [tee] → queue → audiomixer → interleave_M → capssetter_M → queue_out_M → Output_M
//! ```
//!
//! The capssetter fixes channel-mask: 1ch=0x1, 2ch=0x3, 3+ch=0x0 (unpositioned)

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{error, info, warn};

/// Maximum number of input/output streams
const MAX_STREAMS: usize = 8;
/// Maximum channels per stream
const MAX_CHANNELS: usize = 64;

/// Routing destination: output stream index and channel index
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteDest {
    output_idx: usize,
    channel_idx: usize,
}

/// Routing matrix: maps (input_idx, channel_idx) -> Vec<RouteDest>
type RoutingMatrix = HashMap<(usize, usize), Vec<RouteDest>>;

/// Audio Router block builder.
pub struct AudioRouterBuilder;

impl BlockBuilder for AudioRouterBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);

        // Create input pads dynamically - connect to identity element
        let inputs = (0..num_inputs)
            .map(|i| ExternalPad {
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("identity_in_{}", i),
                internal_pad_name: "sink".to_string(),
            })
            .collect();

        // Create output pads dynamically
        let outputs = (0..num_outputs)
            .map(|i| ExternalPad {
                name: format!("audio_out_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("queue_out_{}", i),
                internal_pad_name: "src".to_string(),
            })
            .collect();

        Some(ExternalPads { inputs, outputs })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building AudioRouter block instance: {}", instance_id);

        // Parse configuration
        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);
        let input_channels: Vec<usize> = (0..num_inputs)
            .map(|i| parse_channels(properties, &format!("input_{}_channels", i), 2))
            .collect();
        let output_channels: Vec<usize> = (0..num_outputs)
            .map(|i| parse_channels(properties, &format!("output_{}_channels", i), 2))
            .collect();
        let routing_matrix = parse_routing_matrix(properties);

        let total_input_channels: usize = input_channels.iter().sum();
        let total_output_channels: usize = output_channels.iter().sum();

        info!(
            "AudioRouter config: {} inputs ({} total ch), {} outputs ({} total ch)",
            num_inputs, total_input_channels, num_outputs, total_output_channels
        );

        // ========================================================================
        // Analyze routing to determine which output channels need audiomixer
        // Only use audiomixer when multiple inputs route to the same output
        // ========================================================================
        let mut output_input_count: HashMap<(usize, usize), usize> = HashMap::new();
        for destinations in routing_matrix.values() {
            for dest in destinations {
                *output_input_count
                    .entry((dest.output_idx, dest.channel_idx))
                    .or_insert(0) += 1;
            }
        }

        // Outputs that need audiomixer (more than one input)
        let outputs_needing_mixer: HashSet<(usize, usize)> = output_input_count
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(key, _)| *key)
            .collect();

        info!(
            "Routing analysis: {} output channels need audiomixer for mixing",
            outputs_needing_mixer.len()
        );

        // Determine which output channels have NO routing at all (need silence)
        let mut routed_outputs: HashSet<(usize, usize)> = HashSet::new();
        for destinations in routing_matrix.values() {
            for dest in destinations {
                routed_outputs.insert((dest.output_idx, dest.channel_idx));
            }
        }

        let mut unrouted_outputs: Vec<(usize, usize)> = Vec::new();
        for (out_idx, &out_ch_count) in output_channels.iter().enumerate().take(num_outputs) {
            for out_ch in 0..out_ch_count {
                if !routed_outputs.contains(&(out_idx, out_ch)) {
                    unrouted_outputs.push((out_idx, out_ch));
                }
            }
        }

        if !unrouted_outputs.is_empty() {
            info!(
                "Routing analysis: {} output channels have no routing (will use silence)",
                unrouted_outputs.len()
            );
        }

        let mut elements = Vec::new();
        let mut internal_links = Vec::new();

        // ========================================================================
        // Create OUTPUT side (audiomixers only where needed, and interleaves)
        // ========================================================================

        // Store audiomixers for output channels that need mixing
        // Key: (output_idx, channel_idx), Value: audiomixer element
        let audiomixers: Arc<Mutex<HashMap<(usize, usize), gst::Element>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Store interleaves for each output stream
        let interleaves: Arc<Mutex<Vec<gst::Element>>> = Arc::new(Mutex::new(Vec::new()));

        // Track which output channels need mixer (shared with pad-added callback)
        let outputs_needing_mixer = Arc::new(outputs_needing_mixer);

        for (out_idx, &out_ch_count) in output_channels.iter().enumerate().take(num_outputs) {
            // Create audiomixer ONLY for output channels that have multiple inputs
            for out_ch in 0..out_ch_count {
                if outputs_needing_mixer.contains(&(out_idx, out_ch)) {
                    let mixer_id = format!("{}:audiomixer_o{}c{}", instance_id, out_idx, out_ch);
                    let mixer = gst::ElementFactory::make("audiomixer")
                        .name(&mixer_id)
                        .build()
                        .map_err(|e| {
                            BlockBuildError::ElementCreation(format!("audiomixer: {}", e))
                        })?;

                    info!(
                        "Creating audiomixer for output {} channel {} (multiple inputs)",
                        out_idx, out_ch
                    );
                    elements.push((mixer_id, mixer.clone()));
                    audiomixers.lock().unwrap().insert((out_idx, out_ch), mixer);
                }
            }

            // Create interleave for this output stream
            let interleave_id = format!("{}:interleave_{}", instance_id, out_idx);
            let interleave = gst::ElementFactory::make("interleave")
                .name(&interleave_id)
                .property("channel-positions-from-input", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("interleave: {}", e)))?;

            // Request sink pads on interleave for each channel
            for out_ch in 0..out_ch_count {
                let _sink_pad = interleave.request_pad_simple("sink_%u").ok_or_else(|| {
                    BlockBuildError::ElementCreation(format!(
                        "Failed to request sink pad {} on interleave_{}",
                        out_ch, out_idx
                    ))
                })?;
            }

            elements.push((interleave_id.clone(), interleave.clone()));
            interleaves.lock().unwrap().push(interleave.clone());

            // Create capssetter to fix channel-mask for downstream elements
            // 1 channel: 0x1 (front center), 2 channels: 0x3 (front left + right), 3+: 0x0 (unpositioned)
            let capssetter_id = format!("{}:capssetter_{}", instance_id, out_idx);
            let channel_mask: u64 = match out_ch_count {
                1 => 0x1,
                2 => 0x3,
                _ => 0x0,
            };
            let caps = gst::Caps::builder("audio/x-raw")
                .field("channel-mask", gst::Bitmask::new(channel_mask))
                .build();
            let capssetter = gst::ElementFactory::make("capssetter")
                .name(&capssetter_id)
                .property("caps", &caps)
                .property("join", true) // merge with existing caps
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("capssetter: {}", e)))?;

            elements.push((capssetter_id.clone(), capssetter));

            // Create output queue
            let queue_out_id = format!("{}:queue_out_{}", instance_id, out_idx);
            let queue_out = gst::ElementFactory::make("queue")
                .name(&queue_out_id)
                .property("max-size-buffers", 3u32)
                .property("max-size-time", 0u64)
                .property("max-size-bytes", 0u32)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("queue_out: {}", e)))?;

            elements.push((queue_out_id.clone(), queue_out));

            // Link: interleave → capssetter → queue_out
            internal_links.push((
                ElementPadRef::pad(&interleave_id, "src"),
                ElementPadRef::pad(&capssetter_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&capssetter_id, "src"),
                ElementPadRef::pad(&queue_out_id, "sink"),
            ));

            // Link: audiomixer_oXcY → interleave sink_Y (only for channels with mixer)
            for out_ch in 0..out_ch_count {
                if outputs_needing_mixer.contains(&(out_idx, out_ch)) {
                    let mixer_id = format!("{}:audiomixer_o{}c{}", instance_id, out_idx, out_ch);
                    let interleave_sink = format!("sink_{}", out_ch);
                    internal_links.push((
                        ElementPadRef::pad(&mixer_id, "src"),
                        ElementPadRef::pad(&interleave_id, &interleave_sink),
                    ));
                }
                // Note: for channels without mixer, we link directly in pad-added callback
                // Note: for unrouted channels, we link silence source below
            }
        }

        // ========================================================================
        // Create SILENCE source for unrouted output channels
        // ========================================================================
        if !unrouted_outputs.is_empty() {
            // Create a single mono silence source
            let silence_id = format!("{}:silence_src", instance_id);
            let silence = gst::ElementFactory::make("audiotestsrc")
                .name(&silence_id)
                .property("is-live", true)
                .property_from_str("wave", "silence")
                .property("samplesperbuffer", 1024i32)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audiotestsrc: {}", e)))?;

            elements.push((silence_id.clone(), silence));

            // If multiple unrouted outputs, use a tee
            if unrouted_outputs.len() > 1 {
                let silence_tee_id = format!("{}:silence_tee", instance_id);
                let silence_tee = gst::ElementFactory::make("tee")
                    .name(&silence_tee_id)
                    .property("allow-not-linked", true)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("tee: {}", e)))?;

                elements.push((silence_tee_id.clone(), silence_tee));

                // Link: silence_src → silence_tee
                internal_links.push((
                    ElementPadRef::pad(&silence_id, "src"),
                    ElementPadRef::pad(&silence_tee_id, "sink"),
                ));

                // For each unrouted output channel, create queue and link from tee
                for (out_idx, out_ch) in &unrouted_outputs {
                    let silence_queue_id =
                        format!("{}:silence_queue_o{}c{}", instance_id, out_idx, out_ch);
                    let silence_queue = gst::ElementFactory::make("queue")
                        .name(&silence_queue_id)
                        .property("max-size-buffers", 3u32)
                        .property("max-size-time", 0u64)
                        .property("max-size-bytes", 0u32)
                        .build()
                        .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))?;

                    elements.push((silence_queue_id.clone(), silence_queue));

                    // Link: silence_tee src_%u → silence_queue
                    // The pipeline manager will handle the request pad template
                    internal_links.push((
                        ElementPadRef::pad(&silence_tee_id, "src_%u"),
                        ElementPadRef::pad(&silence_queue_id, "sink"),
                    ));

                    // Link: silence_queue → interleave sink_Y
                    let interleave_id = format!("{}:interleave_{}", instance_id, out_idx);
                    let interleave_sink = format!("sink_{}", out_ch);
                    internal_links.push((
                        ElementPadRef::pad(&silence_queue_id, "src"),
                        ElementPadRef::pad(&interleave_id, &interleave_sink),
                    ));

                    info!(
                        "Routing silence → output {} ch {} (no input routed)",
                        out_idx, out_ch
                    );
                }
            } else {
                // Only one unrouted output, connect directly
                let (out_idx, out_ch) = unrouted_outputs[0];

                let silence_queue_id =
                    format!("{}:silence_queue_o{}c{}", instance_id, out_idx, out_ch);
                let silence_queue = gst::ElementFactory::make("queue")
                    .name(&silence_queue_id)
                    .property("max-size-buffers", 3u32)
                    .property("max-size-time", 0u64)
                    .property("max-size-bytes", 0u32)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))?;

                elements.push((silence_queue_id.clone(), silence_queue));

                // Link: silence_src → silence_queue
                internal_links.push((
                    ElementPadRef::pad(&silence_id, "src"),
                    ElementPadRef::pad(&silence_queue_id, "sink"),
                ));

                // Link: silence_queue → interleave sink_Y
                let interleave_id = format!("{}:interleave_{}", instance_id, out_idx);
                let interleave_sink = format!("sink_{}", out_ch);
                internal_links.push((
                    ElementPadRef::pad(&silence_queue_id, "src"),
                    ElementPadRef::pad(&interleave_id, &interleave_sink),
                ));

                info!(
                    "Routing silence → output {} ch {} (no input routed)",
                    out_idx, out_ch
                );
            }
        }

        // ========================================================================
        // Create INPUT side (identity pass-through → deinterleave)
        // ========================================================================

        for (in_idx, _in_ch_count) in input_channels.iter().enumerate().take(num_inputs) {
            // Create identity element (pass-through, no buffering)
            let identity_id = format!("{}:identity_in_{}", instance_id, in_idx);
            let identity = gst::ElementFactory::make("identity")
                .name(&identity_id)
                .property("silent", true)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;

            elements.push((identity_id.clone(), identity));

            // Create deinterleave
            let deinterleave_id = format!("{}:deinterleave_{}", instance_id, in_idx);
            let deinterleave = gst::ElementFactory::make("deinterleave")
                .name(&deinterleave_id)
                .property("keep-positions", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("deinterleave: {}", e)))?;

            // Link: identity → deinterleave
            internal_links.push((
                ElementPadRef::pad(&identity_id, "src"),
                ElementPadRef::pad(&deinterleave_id, "sink"),
            ));

            // Setup pad-added callback for deinterleave
            // This is called when deinterleave creates dynamic src pads
            let audiomixers_clone = audiomixers.clone();
            let interleaves_clone = interleaves.clone();
            let outputs_needing_mixer_clone = outputs_needing_mixer.clone();
            let routing_matrix_clone = routing_matrix.clone();
            let instance_id_owned = instance_id.to_string();
            let current_in_idx = in_idx;

            deinterleave.connect_pad_added(move |element, pad| {
                let pad_name = pad.name().to_string();
                info!(
                    "AudioRouter: Deinterleave {} pad-added: {}",
                    element.name(),
                    pad_name
                );

                // Parse channel index from pad name (e.g., "src_0" -> 0)
                let channel_idx = match parse_channel_from_pad_name(&pad_name) {
                    Some(idx) => idx,
                    None => {
                        warn!("Could not parse channel index from pad name: {}", pad_name);
                        return;
                    }
                };

                // Get parent bin to add elements
                let Some(parent) = element.parent() else {
                    error!("Deinterleave has no parent");
                    return;
                };
                let Some(bin) = parent.downcast_ref::<gst::Bin>() else {
                    error!("Parent is not a Bin");
                    return;
                };

                // Look up routing for this input channel
                let key = (current_in_idx, channel_idx);
                let destinations = match routing_matrix_clone.get(&key) {
                    Some(dests) if !dests.is_empty() => dests.clone(),
                    _ => {
                        info!(
                            "No routing configured for input {} channel {} - pad will be unlinked",
                            current_in_idx, channel_idx
                        );
                        return;
                    }
                };

                // If this input routes to multiple outputs (fan-out), we need a tee
                // Otherwise, we can connect directly
                let needs_tee = destinations.len() > 1;

                let tee = if needs_tee {
                    // Create tee for fan-out
                    let tee_id = format!(
                        "{}:tee_i{}c{}",
                        instance_id_owned, current_in_idx, channel_idx
                    );
                    let tee = match gst::ElementFactory::make("tee")
                        .name(&tee_id)
                        .property("allow-not-linked", true)
                        .build()
                    {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Failed to create tee: {}", e);
                            return;
                        }
                    };

                    if bin.add(&tee).is_err() {
                        error!("Failed to add tee to bin");
                        return;
                    }

                    if tee.sync_state_with_parent().is_err() {
                        error!("Failed to sync tee state");
                        return;
                    }

                    // Link deinterleave pad → tee
                    let tee_sink = match tee.static_pad("sink") {
                        Some(p) => p,
                        None => {
                            error!("Tee has no sink pad");
                            return;
                        }
                    };

                    if pad.link(&tee_sink).is_err() {
                        error!("Failed to link deinterleave to tee");
                        return;
                    }

                    Some(tee)
                } else {
                    None
                };

                let mixers = audiomixers_clone.lock().unwrap();
                let interleaves = interleaves_clone.lock().unwrap();

                for dest in destinations {
                    let dest_key = (dest.output_idx, dest.channel_idx);
                    let needs_mixer = outputs_needing_mixer_clone.contains(&dest_key);

                    // Create routing queue
                    let route_queue_id = format!(
                        "{}:queue_route_i{}c{}_o{}c{}",
                        instance_id_owned,
                        current_in_idx,
                        channel_idx,
                        dest.output_idx,
                        dest.channel_idx
                    );
                    let route_queue = match gst::ElementFactory::make("queue")
                        .name(&route_queue_id)
                        .property("max-size-buffers", 3u32)
                        .property("max-size-time", 0u64)
                        .property("max-size-bytes", 0u32)
                        .build()
                    {
                        Ok(q) => q,
                        Err(e) => {
                            error!("Failed to create route queue: {}", e);
                            continue;
                        }
                    };

                    if bin.add(&route_queue).is_err() {
                        error!("Failed to add route queue to bin");
                        continue;
                    }

                    if route_queue.sync_state_with_parent().is_err() {
                        error!("Failed to sync route queue state");
                        continue;
                    }

                    // Get queue sink pad
                    let Some(queue_sink) = route_queue.static_pad("sink") else {
                        error!("Route queue has no sink pad");
                        continue;
                    };

                    // Link input to queue (via tee if fan-out, otherwise direct)
                    if let Some(ref tee) = tee {
                        // Fan-out: tee → queue
                        let Some(tee_src) = tee.request_pad_simple("src_%u") else {
                            error!("Failed to request src pad from tee");
                            continue;
                        };
                        if tee_src.link(&queue_sink).is_err() {
                            error!("Failed to link tee to route queue");
                            continue;
                        }
                    } else {
                        // Direct: deinterleave pad → queue
                        if pad.link(&queue_sink).is_err() {
                            error!("Failed to link deinterleave to route queue");
                            continue;
                        }
                    }

                    // Get queue src pad
                    let Some(queue_src) = route_queue.static_pad("src") else {
                        error!("Route queue has no src pad");
                        continue;
                    };

                    // Link queue to output (via audiomixer if mixing, otherwise direct to interleave)
                    if needs_mixer {
                        // Multiple inputs to this output → use audiomixer
                        let Some(mixer) = mixers.get(&dest_key) else {
                            error!(
                                "No audiomixer found for output {} channel {}",
                                dest.output_idx, dest.channel_idx
                            );
                            continue;
                        };

                        let Some(mixer_sink) = mixer.request_pad_simple("sink_%u") else {
                            error!("Failed to request sink pad from audiomixer");
                            continue;
                        };

                        if queue_src.link(&mixer_sink).is_err() {
                            error!("Failed to link route queue to audiomixer");
                            continue;
                        }

                        info!(
                            "Routed input {} ch {} → audiomixer → output {} ch {}",
                            current_in_idx, channel_idx, dest.output_idx, dest.channel_idx
                        );
                    } else {
                        // Single input to this output → direct to interleave
                        let Some(interleave) = interleaves.get(dest.output_idx) else {
                            error!("No interleave found for output {}", dest.output_idx);
                            continue;
                        };

                        let interleave_sink_name = format!("sink_{}", dest.channel_idx);
                        // Find pad in pads list - request pads aren't accessible via static_pad()
                        let interleave_sink = interleave
                            .pads()
                            .into_iter()
                            .find(|p| p.name() == interleave_sink_name);
                        let Some(interleave_sink) = interleave_sink else {
                            error!(
                                "Interleave {} has no pad {} (available: {:?})",
                                dest.output_idx,
                                interleave_sink_name,
                                interleave
                                    .pads()
                                    .iter()
                                    .map(|p| p.name().to_string())
                                    .collect::<Vec<_>>()
                            );
                            continue;
                        };

                        if queue_src.link(&interleave_sink).is_err() {
                            error!("Failed to link route queue to interleave");
                            continue;
                        }

                        info!(
                            "Routed input {} ch {} → output {} ch {} (direct)",
                            current_in_idx, channel_idx, dest.output_idx, dest.channel_idx
                        );
                    }
                }
            });

            elements.push((deinterleave_id, deinterleave));
        }

        info!(
            "AudioRouter block created: {} inputs, {} outputs",
            num_inputs, num_outputs
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

// ============================================================================
// Property Parsing Helpers
// ============================================================================

/// Parse number of streams from properties.
fn parse_num_streams(
    properties: &HashMap<String, PropertyValue>,
    key: &str,
    default: usize,
) -> usize {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(default)
        .clamp(1, MAX_STREAMS)
}

/// Parse channel count from properties.
fn parse_channels(properties: &HashMap<String, PropertyValue>, key: &str, default: usize) -> usize {
    properties
        .get(key)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(default)
        .clamp(1, MAX_CHANNELS)
}

/// Parse routing matrix from JSON property.
///
/// Format: `{"i0c0": ["o0c0", "o1c0"], "i0c1": ["o0c1"]}`
/// Where iXcY = input X channel Y, oXcY = output X channel Y
fn parse_routing_matrix(properties: &HashMap<String, PropertyValue>) -> RoutingMatrix {
    let mut matrix = RoutingMatrix::new();

    let json_str = match properties.get("routing_matrix") {
        Some(PropertyValue::String(s)) => s.clone(),
        _ => return matrix,
    };

    if json_str.is_empty() || json_str == "{}" {
        return matrix;
    }

    // Parse JSON
    let parsed: Result<HashMap<String, Vec<String>>, _> = serde_json::from_str(&json_str);
    let Ok(json_matrix) = parsed else {
        warn!("Failed to parse routing matrix JSON: {}", json_str);
        return matrix;
    };

    for (src_key, dest_list) in json_matrix {
        // Parse source key (e.g., "i0c1" -> input 0, channel 1)
        let Some((in_idx, in_ch)) = parse_routing_key(&src_key, 'i') else {
            warn!("Invalid routing source key: {}", src_key);
            continue;
        };

        let mut destinations = Vec::new();
        for dest_key in dest_list {
            // Parse destination key (e.g., "o1c0" -> output 1, channel 0)
            let Some((out_idx, out_ch)) = parse_routing_key(&dest_key, 'o') else {
                warn!("Invalid routing destination key: {}", dest_key);
                continue;
            };
            destinations.push(RouteDest {
                output_idx: out_idx,
                channel_idx: out_ch,
            });
        }

        if !destinations.is_empty() {
            matrix.insert((in_idx, in_ch), destinations);
        }
    }

    info!("Parsed routing matrix with {} entries", matrix.len());
    matrix
}

/// Parse a routing key like "i0c1" or "o2c3" into (stream_idx, channel_idx).
fn parse_routing_key(key: &str, prefix: char) -> Option<(usize, usize)> {
    if !key.starts_with(prefix) {
        return None;
    }

    let rest = &key[1..];
    let c_pos = rest.find('c')?;

    let stream_idx: usize = rest[..c_pos].parse().ok()?;
    let channel_idx: usize = rest[c_pos + 1..].parse().ok()?;

    Some((stream_idx, channel_idx))
}

/// Parse channel index from deinterleave pad name (e.g., "src_0" -> 0).
fn parse_channel_from_pad_name(pad_name: &str) -> Option<usize> {
    pad_name.strip_prefix("src_").and_then(|s| s.parse().ok())
}

// ============================================================================
// Block Definition
// ============================================================================

/// Get Audio Router block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audiorouter_definition()]
}

/// Get Audio Router block definition (metadata only).
fn audiorouter_definition() -> BlockDefinition {
    let mut exposed_properties = vec![
        // Number of inputs
        ExposedProperty {
            name: "num_inputs".to_string(),
            label: "Number of Inputs".to_string(),
            description: "Number of input audio streams (1-8)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_inputs".to_string(),
                transform: None,
            },
        },
        // Number of outputs
        ExposedProperty {
            name: "num_outputs".to_string(),
            label: "Number of Outputs".to_string(),
            description: "Number of output audio streams (1-8)".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "num_outputs".to_string(),
                transform: None,
            },
        },
    ];

    // Generate per-input channel count properties
    for i in 0..MAX_STREAMS {
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_channels", i),
            label: format!("Input {} Channels", i),
            description: format!("Number of channels for input stream {} (1-64)", i),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_channels", i),
                transform: None,
            },
        });
    }

    // Generate per-output channel count properties
    for i in 0..MAX_STREAMS {
        exposed_properties.push(ExposedProperty {
            name: format!("output_{}_channels", i),
            label: format!("Output {} Channels", i),
            description: format!("Number of channels for output stream {} (1-64)", i),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(2)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("output_{}_channels", i),
                transform: None,
            },
        });
    }

    // Routing matrix property (JSON format)
    exposed_properties.push(ExposedProperty {
        name: "routing_matrix".to_string(),
        label: "Routing Matrix".to_string(),
        description: "JSON routing matrix. Format: {\"i0c0\": [\"o0c0\", \"o1c0\"]} where iXcY = input X channel Y, oXcY = output X channel Y".to_string(),
        property_type: PropertyType::Multiline,
        default_value: Some(PropertyValue::String("{}".to_string())),
        mapping: PropertyMapping {
            element_id: "_block".to_string(),
            property_name: "routing_matrix".to_string(),
            transform: None,
        },
    });

    BlockDefinition {
        id: "builtin.audiorouter".to_string(),
        name: "Audio Router".to_string(),
        description: "Route audio channels between multiple input and output streams using a flexible routing matrix. Supports fan-out (one input to multiple outputs) and mixing (multiple inputs to one output).".to_string(),
        category: "Audio".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    name: "audio_in_1".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_1".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![
                ExternalPad {
                    name: "audio_out_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "queue_out_0".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "audio_out_1".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "queue_out_1".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🔀".to_string()),
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
