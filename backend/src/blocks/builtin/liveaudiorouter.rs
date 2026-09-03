//! Live Audio Router block — same routing model as `builtin.audiorouter`, but
//! the crosspoints can be switched on a running flow.
//!
//! `builtin.audiorouter` expresses routing as pipeline topology (deinterleave,
//! tee, audiomixer, interleave), which is why its `routing_matrix` is
//! `live: false` — changing a route means rebuilding the graph. This block
//! expresses the same routing as coefficients in a single `audiomixmatrix`,
//! whose `matrix` property can be replaced while data is flowing.
//!
//! ```text
//! audio_in_N → identity_in_N → deinterleave_in_N ┐
//!                                                 ├→ interleave_in → capssetter_in → matrix
//!                                                 ┘
//! matrix → capssetter_matrix → deinterleave_out → interleave_M → capssetter_M → queue_out_M
//! ```
//!
//! Fan-out is one input column feeding several output rows; fan-in is one
//! output row with several non-zero columns, which `audiomixmatrix` sums. An
//! unrouted output channel is an all-zero row and is therefore silent, so this
//! block needs no silence source.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

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

/// Internal element id of the `audiomixmatrix` that carries the live routing.
const MATRIX_ELEMENT_ID_TAIL: &str = ":matrix";

// ============================================================================
// Routing model → audiomixmatrix coefficients
// ============================================================================

/// Running start index of each stream in the concatenated channel space.
fn channel_offsets(channels: &[usize]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(channels.len());
    let mut acc = 0;
    for c in channels {
        offsets.push(acc);
        acc += c;
    }
    offsets
}

/// Build the `audiomixmatrix` coefficient matrix, indexed `[output][input]`.
///
/// Both fan-out and fan-in fall out of this representation: a column with
/// several non-zero rows is fan-out, and a row with several non-zero columns is
/// fan-in, which the element sums.
fn build_matrix(
    routing: &RoutingMatrix,
    input_channels: &[usize],
    output_channels: &[usize],
) -> Vec<Vec<f64>> {
    let total_in: usize = input_channels.iter().sum();
    let total_out: usize = output_channels.iter().sum();
    let in_offsets = channel_offsets(input_channels);
    let out_offsets = channel_offsets(output_channels);

    let mut matrix = vec![vec![0.0f64; total_in]; total_out];

    for ((in_idx, in_ch), destinations) in routing {
        if *in_idx >= input_channels.len() || *in_ch >= input_channels[*in_idx] {
            warn!(
                "LiveAudioRouter: routing source i{}c{} is outside the configured inputs",
                in_idx, in_ch
            );
            continue;
        }
        let column = in_offsets[*in_idx] + in_ch;

        for dest in destinations {
            if dest.output_idx >= output_channels.len()
                || dest.channel_idx >= output_channels[dest.output_idx]
            {
                warn!(
                    "LiveAudioRouter: routing destination o{}c{} is outside the configured outputs",
                    dest.output_idx, dest.channel_idx
                );
                continue;
            }
            let row = out_offsets[dest.output_idx] + dest.channel_idx;
            matrix[row][column] = 1.0;
        }
    }

    matrix
}

/// Build the coefficient matrix straight from a block property map.
///
/// This is the seam the routing model is tested through, and the same one the
/// live update path uses, so a drift between build time and runtime is not
/// possible.
pub fn matrix_from_properties(properties: &HashMap<String, PropertyValue>) -> Vec<Vec<f64>> {
    let num_inputs = parse_num_streams(properties, "num_inputs", 2);
    let num_outputs = parse_num_streams(properties, "num_outputs", 2);
    let input_channels: Vec<usize> = (0..num_inputs)
        .map(|i| parse_channels(properties, &format!("input_{}_channels", i), 2))
        .collect();
    let output_channels: Vec<usize> = (0..num_outputs)
        .map(|i| parse_channels(properties, &format!("output_{}_channels", i), 2))
        .collect();
    let routing = parse_routing_matrix(properties);

    build_matrix(&routing, &input_channels, &output_channels)
}

/// Convert a coefficient matrix into the `GstValueArray` of `GstValueArray` of
/// `gdouble` that `audiomixmatrix` expects, outer array indexed by output
/// channel.
fn matrix_to_value(matrix: &[Vec<f64>]) -> gst::Array {
    gst::Array::new(
        matrix
            .iter()
            .map(|row| gst::Array::new(row.iter().copied())),
    )
}

// ============================================================================
// Live routing updates
// ============================================================================

/// Apply a `routing_matrix` change to a running `audiomixmatrix`.
///
/// `audiomixmatrix`' `matrix` is an array of arrays of doubles, which
/// `PropertyValue` cannot carry, so this block cannot use the `translate_property`
/// route that `audiogain` uses — it intercepts the update the way
/// `time_offset::try_apply_live_offset` does and sets the property directly.
/// Returns true when the update was claimed and applied.
pub fn try_apply_live_matrix(
    element: &gst::Element,
    element_id: &str,
    prop_name: &str,
    value: &PropertyValue,
) -> bool {
    if prop_name != "routing_matrix" {
        return false;
    }
    if !element_id.ends_with(MATRIX_ELEMENT_ID_TAIL) {
        return false;
    }
    let PropertyValue::String(json) = value else {
        warn!(
            "Live Audio Router '{}' received a non-string routing_matrix {:?}",
            element_id, value
        );
        return false;
    };

    // `iXcY` keys are per-stream, so translating them into matrix coordinates
    // needs the channel layout, which only `build()` knows.
    let Some(layout) = layout_for(element_id) else {
        warn!(
            "Live Audio Router '{}' has no recorded channel layout — the flow was not built by this block",
            element_id
        );
        return false;
    };

    let routing = parse_routing_json(json);
    let matrix = build_matrix(&routing, &layout.inputs, &layout.outputs);

    element.set_property("matrix", matrix_to_value(&matrix));
    debug!(
        "Live Audio Router '{}' applied a {}x{} routing matrix",
        element_id,
        matrix.len(),
        matrix.first().map(|r| r.len()).unwrap_or(0)
    );
    true
}

/// Channel layout of one built block instance.
#[derive(Debug, Clone)]
struct ChannelLayout {
    inputs: Vec<usize>,
    outputs: Vec<usize>,
}

/// Channel layouts by matrix element id, recorded at build time.
///
/// The live update path is handed an element and a value, with no route back to
/// the block's properties, so the layout that turns `iXcY` keys into matrix
/// coordinates has to be carried across. Keyed by element id, so rebuilding an
/// instance replaces its entry rather than adding one.
static LAYOUTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, ChannelLayout>>> =
    std::sync::OnceLock::new();

fn layouts() -> &'static std::sync::Mutex<HashMap<String, ChannelLayout>> {
    LAYOUTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn record_layout(element_id: &str, inputs: Vec<usize>, outputs: Vec<usize>) {
    if let Ok(mut map) = layouts().lock() {
        map.insert(element_id.to_string(), ChannelLayout { inputs, outputs });
    }
}

fn layout_for(element_id: &str) -> Option<ChannelLayout> {
    layouts().lock().ok()?.get(element_id).cloned()
}

// ============================================================================
// Builder
// ============================================================================

/// Live Audio Router block builder.
pub struct LiveAudioRouterBuilder;

impl BlockBuilder for LiveAudioRouterBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);

        let inputs = (0..num_inputs)
            .map(|i| ExternalPad {
                label: Some(format!("A{i}")),
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("identity_in_{}", i),
                internal_pad_name: "sink".to_string(),
            })
            .collect();

        let outputs = (0..num_outputs)
            .map(|i| ExternalPad {
                label: Some(format!("A{i}")),
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
        info!("Building LiveAudioRouter block instance: {}", instance_id);

        let num_inputs = parse_num_streams(properties, "num_inputs", 2);
        let num_outputs = parse_num_streams(properties, "num_outputs", 2);
        let input_channels: Vec<usize> = (0..num_inputs)
            .map(|i| parse_channels(properties, &format!("input_{}_channels", i), 2))
            .collect();
        let output_channels: Vec<usize> = (0..num_outputs)
            .map(|i| parse_channels(properties, &format!("output_{}_channels", i), 2))
            .collect();
        let routing = parse_routing_matrix(properties);

        let total_input_channels: usize = input_channels.iter().sum();
        let total_output_channels: usize = output_channels.iter().sum();
        let in_offsets = channel_offsets(&input_channels);
        let out_offsets = channel_offsets(&output_channels);

        info!(
            "LiveAudioRouter config: {} inputs ({} total ch), {} outputs ({} total ch)",
            num_inputs, total_input_channels, num_outputs, total_output_channels
        );

        let mut elements: Vec<(String, gst::Element)> = Vec::new();
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        // --------------------------------------------------------------
        // Single interleave collecting every input channel
        // --------------------------------------------------------------
        let interleave_in_id = format!("{}:interleave_in", instance_id);
        let interleave_in = gst::ElementFactory::make("interleave")
            .name(&interleave_in_id)
            .property("channel-positions-from-input", false)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("interleave: {}", e)))?;
        for _ in 0..total_input_channels {
            interleave_in.request_pad_simple("sink_%u").ok_or_else(|| {
                BlockBuildError::ElementCreation(
                    "Failed to request sink pad on interleave_in".to_string(),
                )
            })?;
        }
        elements.push((interleave_in_id.clone(), interleave_in.clone()));

        let capssetter_in_id = format!("{}:capssetter_in", instance_id);
        let capssetter_in = make_capssetter(&capssetter_in_id, total_input_channels)?;
        elements.push((capssetter_in_id.clone(), capssetter_in));

        // --------------------------------------------------------------
        // The matrix itself
        // --------------------------------------------------------------
        let matrix_id = format!("{}{}", instance_id, MATRIX_ELEMENT_ID_TAIL);
        let coefficients = build_matrix(&routing, &input_channels, &output_channels);
        let matrix = gst::ElementFactory::make("audiomixmatrix")
            .name(&matrix_id)
            .property("in-channels", total_input_channels as u32)
            .property("out-channels", total_output_channels as u32)
            .property("matrix", matrix_to_value(&coefficients))
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audiomixmatrix: {}", e)))?;
        record_layout(&matrix_id, input_channels.clone(), output_channels.clone());
        elements.push((matrix_id.clone(), matrix));

        let capssetter_matrix_id = format!("{}:capssetter_matrix", instance_id);
        let capssetter_matrix = make_capssetter(&capssetter_matrix_id, total_output_channels)?;
        elements.push((capssetter_matrix_id.clone(), capssetter_matrix));

        let deinterleave_out_id = format!("{}:deinterleave_out", instance_id);
        let deinterleave_out = gst::ElementFactory::make("deinterleave")
            .name(&deinterleave_out_id)
            .property("keep-positions", false)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("deinterleave: {}", e)))?;

        internal_links.push((
            ElementPadRef::pad(&interleave_in_id, "src"),
            ElementPadRef::pad(&capssetter_in_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&capssetter_in_id, "src"),
            ElementPadRef::pad(&matrix_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&matrix_id, "src"),
            ElementPadRef::pad(&capssetter_matrix_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&capssetter_matrix_id, "src"),
            ElementPadRef::pad(&deinterleave_out_id, "sink"),
        ));

        // --------------------------------------------------------------
        // Output side: one interleave per output stream
        // --------------------------------------------------------------
        let mut output_interleaves: Vec<glib::WeakRef<gst::Element>> = Vec::new();

        for (out_idx, &out_ch_count) in output_channels.iter().enumerate() {
            let interleave_id = format!("{}:interleave_{}", instance_id, out_idx);
            let interleave = gst::ElementFactory::make("interleave")
                .name(&interleave_id)
                .property("channel-positions-from-input", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("interleave: {}", e)))?;
            for out_ch in 0..out_ch_count {
                interleave.request_pad_simple("sink_%u").ok_or_else(|| {
                    BlockBuildError::ElementCreation(format!(
                        "Failed to request sink pad {} on interleave_{}",
                        out_ch, out_idx
                    ))
                })?;
            }
            output_interleaves.push(interleave.downgrade());
            elements.push((interleave_id.clone(), interleave));

            let capssetter_id = format!("{}:capssetter_{}", instance_id, out_idx);
            let capssetter = make_capssetter(&capssetter_id, out_ch_count)?;
            elements.push((capssetter_id.clone(), capssetter));

            let queue_out_id = format!("{}:queue_out_{}", instance_id, out_idx);
            let queue_out = gst::ElementFactory::make("queue")
                .name(&queue_out_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("queue_out: {}", e)))?;
            elements.push((queue_out_id.clone(), queue_out));

            internal_links.push((
                ElementPadRef::pad(&interleave_id, "src"),
                ElementPadRef::pad(&capssetter_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&capssetter_id, "src"),
                ElementPadRef::pad(&queue_out_id, "sink"),
            ));
        }

        // Split the matrix output back into the per-stream interleaves. The
        // pads appear once caps are negotiated, so this is done on pad-added.
        // Only weak references are captured — a strong one here would keep the
        // whole pipeline alive (see CLAUDE.md).
        let out_offsets_for_cb = out_offsets.clone();
        let output_channels_for_cb = output_channels.clone();
        let instance_for_cb = instance_id.to_string();
        deinterleave_out.connect_pad_added(move |element, pad| {
            let Some(channel) = parse_channel_from_pad_name(&pad.name()) else {
                warn!(
                    "LiveAudioRouter: unparsable deinterleave pad {}",
                    pad.name()
                );
                return;
            };
            let Some(bin) = element.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) else {
                error!("LiveAudioRouter: deinterleave_out has no parent bin");
                return;
            };

            let Some(out_idx) =
                stream_for_channel(channel, &out_offsets_for_cb, &output_channels_for_cb)
            else {
                warn!(
                    "LiveAudioRouter: matrix produced channel {} with no output stream",
                    channel
                );
                return;
            };
            let local_ch = channel - out_offsets_for_cb[out_idx];

            let Some(interleave) = output_interleaves
                .get(out_idx)
                .and_then(|weak| weak.upgrade())
            else {
                // Pipeline is being torn down.
                return;
            };

            let queue_id = format!("{}:queue_out_ch{}", instance_for_cb, channel);
            link_through_queue(&bin, pad, &queue_id, &interleave, local_ch);
        });

        elements.push((deinterleave_out_id, deinterleave_out));

        // --------------------------------------------------------------
        // Input side: one deinterleave per input stream
        // --------------------------------------------------------------
        let interleave_in_weak = interleave_in.downgrade();

        for in_idx in 0..num_inputs {
            let identity_id = format!("{}:identity_in_{}", instance_id, in_idx);
            let identity = gst::ElementFactory::make("identity")
                .name(&identity_id)
                .property("silent", true)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;
            elements.push((identity_id.clone(), identity));

            let deinterleave_id = format!("{}:deinterleave_in_{}", instance_id, in_idx);
            let deinterleave = gst::ElementFactory::make("deinterleave")
                .name(&deinterleave_id)
                .property("keep-positions", false)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("deinterleave: {}", e)))?;

            internal_links.push((
                ElementPadRef::pad(&identity_id, "src"),
                ElementPadRef::pad(&deinterleave_id, "sink"),
            ));

            let interleave_weak = interleave_in_weak.clone();
            let base = in_offsets[in_idx];
            let channels_here = input_channels[in_idx];
            let instance_for_cb = instance_id.to_string();

            deinterleave.connect_pad_added(move |element, pad| {
                let Some(channel) = parse_channel_from_pad_name(&pad.name()) else {
                    warn!(
                        "LiveAudioRouter: unparsable deinterleave pad {}",
                        pad.name()
                    );
                    return;
                };
                if channel >= channels_here {
                    warn!(
                        "LiveAudioRouter: input {} produced channel {} but is configured for {}",
                        base, channel, channels_here
                    );
                    return;
                }
                let Some(bin) = element.parent().and_then(|p| p.downcast::<gst::Bin>().ok()) else {
                    error!("LiveAudioRouter: deinterleave_in has no parent bin");
                    return;
                };
                let Some(interleave) = interleave_weak.upgrade() else {
                    // Pipeline is being torn down.
                    return;
                };

                let global = base + channel;
                let queue_id = format!("{}:queue_in_ch{}", instance_for_cb, global);
                link_through_queue(&bin, pad, &queue_id, &interleave, global);
            });

            elements.push((deinterleave_id, deinterleave));
        }

        info!(
            "LiveAudioRouter block created: {} inputs, {} outputs, {}x{} matrix",
            num_inputs, num_outputs, total_output_channels, total_input_channels
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Add a queue between a freshly added deinterleave pad and the numbered sink
/// pad of an interleave.
fn link_through_queue(
    bin: &gst::Bin,
    pad: &gst::Pad,
    queue_id: &str,
    interleave: &gst::Element,
    sink_index: usize,
) {
    let queue = match gst::ElementFactory::make("queue").name(queue_id).build() {
        Ok(q) => q,
        Err(e) => {
            error!("LiveAudioRouter: failed to create {}: {}", queue_id, e);
            return;
        }
    };
    if bin.add(&queue).is_err() {
        error!("LiveAudioRouter: failed to add {} to bin", queue_id);
        return;
    }
    if queue.sync_state_with_parent().is_err() {
        error!("LiveAudioRouter: failed to sync state of {}", queue_id);
        return;
    }

    let Some(queue_sink) = queue.static_pad("sink") else {
        error!("LiveAudioRouter: {} has no sink pad", queue_id);
        return;
    };
    if pad.link(&queue_sink).is_err() {
        error!(
            "LiveAudioRouter: failed to link {} into {}",
            pad.name(),
            queue_id
        );
        return;
    }

    let Some(queue_src) = queue.static_pad("src") else {
        error!("LiveAudioRouter: {} has no src pad", queue_id);
        return;
    };

    let sink_name = format!("sink_{}", sink_index);
    let Some(interleave_sink) = interleave
        .pads()
        .into_iter()
        .find(|p| p.name() == sink_name)
    else {
        error!(
            "LiveAudioRouter: interleave {} has no pad {}",
            interleave.name(),
            sink_name
        );
        return;
    };

    if queue_src.link(&interleave_sink).is_err() {
        error!(
            "LiveAudioRouter: failed to link {} to {}",
            queue_id, sink_name
        );
        return;
    }

    debug!(
        "LiveAudioRouter: linked {} into {} {}",
        pad.name(),
        interleave.name(),
        sink_name
    );
}

/// Which output stream a flat channel index belongs to.
fn stream_for_channel(channel: usize, offsets: &[usize], channels: &[usize]) -> Option<usize> {
    offsets
        .iter()
        .zip(channels.iter())
        .position(|(&base, &count)| channel >= base && channel < base + count)
}

/// capssetter fixing the channel-mask the way `builtin.audiorouter` does:
/// 1ch = 0x1, 2ch = 0x3, 3+ch = 0x0 (unpositioned).
fn make_capssetter(id: &str, channels: usize) -> Result<gst::Element, BlockBuildError> {
    let channel_mask: u64 = match channels {
        1 => 0x1,
        2 => 0x3,
        _ => 0x0,
    };
    let caps = gst::Caps::builder("audio/x-raw")
        .field("channel-mask", gst::Bitmask::new(channel_mask))
        .build();
    gst::ElementFactory::make("capssetter")
        .name(id)
        .property("caps", &caps)
        .property("join", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capssetter: {}", e)))
}

// ============================================================================
// Property Parsing Helpers
// ============================================================================

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

/// Parse the routing matrix JSON property.
///
/// Format: `{"i0c0": ["o0c0", "o1c0"], "i0c1": ["o0c1"]}`
fn parse_routing_matrix(properties: &HashMap<String, PropertyValue>) -> RoutingMatrix {
    match properties.get("routing_matrix") {
        Some(PropertyValue::String(s)) => parse_routing_json(s),
        _ => RoutingMatrix::new(),
    }
}

fn parse_routing_json(json_str: &str) -> RoutingMatrix {
    let mut matrix = RoutingMatrix::new();

    if json_str.is_empty() || json_str == "{}" {
        return matrix;
    }

    let parsed: Result<HashMap<String, Vec<String>>, _> = serde_json::from_str(json_str);
    let Ok(json_matrix) = parsed else {
        warn!("Failed to parse routing matrix JSON: {}", json_str);
        return matrix;
    };

    for (src_key, dest_list) in json_matrix {
        let Some((in_idx, in_ch)) = parse_routing_key(&src_key, 'i') else {
            warn!("Invalid routing source key: {}", src_key);
            continue;
        };

        let mut destinations = Vec::new();
        for dest_key in dest_list {
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

/// Parse channel index from a deinterleave pad name (e.g., "src_0" -> 0).
fn parse_channel_from_pad_name(pad_name: &str) -> Option<usize> {
    pad_name.strip_prefix("src_").and_then(|s| s.parse().ok())
}

// ============================================================================
// Block Definition
// ============================================================================

/// Get Live Audio Router block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![liveaudiorouter_definition()]
}

fn liveaudiorouter_definition() -> BlockDefinition {
    let mut exposed_properties = vec![
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
            live: false,
            persist: None,
        },
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
            live: false,
            persist: None,
        },
    ];

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
            live: false,
            persist: None,
        });
    }

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
            live: false,
            persist: None,
        });
    }

    exposed_properties.push(ExposedProperty {
        name: "routing_matrix".to_string(),
        label: "Routing Matrix".to_string(),
        description: "JSON routing matrix. Format: {\"i0c0\": [\"o0c0\", \"o1c0\"]} where iXcY = input X channel Y, oXcY = output X channel Y. Applies to a running flow.".to_string(),
        property_type: PropertyType::Multiline,
        default_value: Some(PropertyValue::String("{}".to_string())),
        mapping: PropertyMapping {
            element_id: "matrix".to_string(),
            property_name: "routing_matrix".to_string(),
            transform: None,
        },
        live: true,
        persist: None,
    });

    BlockDefinition {
        id: "builtin.liveaudiorouter".to_string(),
        name: "Live Audio Router".to_string(),
        description: "Route audio channels between multiple input and output streams using a flexible routing matrix. Supports fan-out and mixing, and the routing can be changed while the flow is running.".to_string(),
        category: "Audio".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A1".to_string()),
                    name: "audio_in_1".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "identity_in_1".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_out_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "queue_out_0".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A1".to_string()),
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
