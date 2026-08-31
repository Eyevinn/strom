//! Live audio channel router built on GStreamer's `audiomixmatrix`.
//!
//! Sibling of `builtin.audiorouter`, which wires a static
//! `deinterleave`/`tee`/`interleave` graph from a matrix snapshot taken at
//! `build()` time and therefore needs a flow restart to reroute a channel. This
//! block keeps a single `audiomixmatrix` whose `matrix` property is rewritten in
//! place, so `routing_matrix` is exposed as a live property.
//!
//! Chain: audioconvert → audiomixmatrix → audioconvert
//!
//! The surrounding `audioconvert` elements absorb the channel-count and format
//! negotiation on both sides: `audiomixmatrix` accepts exactly `in-channels`
//! channels on its sink and emits exactly `out-channels` on its src, which
//! upstream and downstream have no reason to already match.
//!
//! `routing_matrix` uses the same JSON shape as `builtin.audiorouter`
//! (`{"i0c0": ["o0c1"]}`). `audiomixmatrix` is a single-stream element, so the
//! stream index in those keys is always 0 and only the channel index varies.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{info, warn};

/// Maximum channels on either side of the matrix. Mirrors `audiorouter`'s limit.
const MAX_CHANNELS: usize = 64;

/// Element-id tail of the `audiomixmatrix`. The live-update interceptor matches
/// on it, so it must stay in step with the element naming in `build()` and with
/// the `PropertyMapping` in the block definition.
const MATRIX_ELEMENT_ID_TAIL: &str = ":live_router_matrix";

/// GStreamer type hint written into each serialized matrix cell.
///
/// `audiomixmatrix` declares `matrix` as an array of arrays of `gdouble`, and
/// the value has to deserialize under that spec. Kept as a named constant
/// because it is the one token in this file that a GStreamer version could
/// plausibly disagree about.
const MATRIX_CELL_TYPE: &str = "gdouble";

/// Live Audio Router block builder.
pub struct LiveAudioRouterBuilder;

/// Parse a channel count property, clamped to a matrix `audiomixmatrix` accepts.
fn parse_channel_count(
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
        .clamp(1, MAX_CHANNELS)
}

/// Parse a routing key like `i0c1` or `o2c3` into (stream index, channel index).
///
/// Duplicated from `audiorouter` rather than shared: that block is deliberately
/// left untouched by this change, and the key format is part of this block's own
/// property contract.
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

/// Serialize an `[output][input]` coefficient grid as a GStreamer value array.
fn serialize_matrix(rows: &[Vec<f64>]) -> String {
    let serialized_rows: Vec<String> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .map(|v| format!("({}){:.1}", MATRIX_CELL_TYPE, v))
                .collect();
            format!("<{}>", cells.join(", "))
        })
        .collect();
    format!("<{}>", serialized_rows.join(", "))
}

/// Convert the block's JSON routing matrix into the value-array string that
/// `audiomixmatrix`'s `matrix` property accepts.
///
/// `audiomixmatrix` indexes the matrix `[output][input]`, so the result has
/// `out_channels` rows of `in_channels` coefficients. An empty matrix (`""` or
/// `"{}"`) means straight through: output channel N takes input channel N.
///
/// Returns `None` when the JSON does not parse. Callers must not fall back to
/// forwarding the raw string: it would reach `set_property_from_str`, which
/// panics on a value it cannot deserialize. Entries that parse but address a
/// channel outside the configured counts are logged and skipped, since a matrix
/// left over from a wider configuration should not block the rest of the update.
pub fn routing_matrix_to_gst_array(
    json_str: &str,
    in_channels: usize,
    out_channels: usize,
) -> Option<String> {
    let mut rows = vec![vec![0.0f64; in_channels]; out_channels];

    let trimmed = json_str.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        for (out_ch, row) in rows.iter_mut().enumerate() {
            if let Some(cell) = row.get_mut(out_ch) {
                *cell = 1.0;
            }
        }
        return Some(serialize_matrix(&rows));
    }

    let parsed: HashMap<String, Vec<String>> = serde_json::from_str(trimmed).ok()?;

    for (src_key, dest_keys) in parsed {
        let Some((_, in_ch)) = parse_routing_key(&src_key, 'i') else {
            warn!(
                "Live Audio Router: invalid routing source key '{}'",
                src_key
            );
            continue;
        };
        if in_ch >= in_channels {
            warn!(
                "Live Audio Router: source key '{}' addresses input channel {} but only {} are configured",
                src_key, in_ch, in_channels
            );
            continue;
        }

        for dest_key in dest_keys {
            let Some((_, out_ch)) = parse_routing_key(&dest_key, 'o') else {
                warn!("Live Audio Router: invalid routing dest key '{}'", dest_key);
                continue;
            };
            if out_ch >= out_channels {
                warn!(
                    "Live Audio Router: destination key '{}' addresses output channel {} but only {} are configured",
                    dest_key, out_ch, out_channels
                );
                continue;
            }
            rows[out_ch][in_ch] = 1.0;
        }
    }

    Some(serialize_matrix(&rows))
}

/// Try to handle a property update as a Live Audio Router matrix change.
///
/// Returns `true` when the update belongs to this block, in which case the
/// caller must skip the default property-set path. The block's `routing_matrix`
/// is JSON, while the element's own property is a `gdouble` value array named
/// `matrix`, so the generic path cannot carry it — and would panic handing JSON
/// to `set_property_from_str`. A value this function rejects is therefore still
/// reported as handled: the previous matrix stays in effect and the audio keeps
/// flowing.
///
/// Coupling note: the matching keys (`routing_matrix`, `:live_router_matrix`)
/// mirror the block's `PropertyMapping` and element naming.
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

    let PropertyValue::String(json_str) = value else {
        warn!(
            "Live Audio Router '{}' received a non-string routing_matrix value {:?}, keeping previous matrix",
            element_id, value
        );
        return true;
    };

    let in_channels = element.property::<u32>("in-channels") as usize;
    let out_channels = element.property::<u32>("out-channels") as usize;
    if in_channels == 0 || out_channels == 0 {
        warn!(
            "Live Audio Router '{}' has in-channels={} out-channels={}, cannot size a matrix",
            element_id, in_channels, out_channels
        );
        return true;
    }

    let Some(serialized) = routing_matrix_to_gst_array(json_str, in_channels, out_channels) else {
        warn!(
            "Live Audio Router '{}' received an unparseable routing_matrix, keeping previous matrix",
            element_id
        );
        return true;
    };

    element.set_property_from_str("matrix", &serialized);
    info!(
        "Live Audio Router '{}' matrix updated ({} in x {} out)",
        element_id, in_channels, out_channels
    );
    true
}

/// Output channel mask for a given channel count, matching `audiorouter`:
/// 1ch = front-mono, 2ch = stereo pair, 3+ = unpositioned.
fn channel_mask(channels: usize) -> u64 {
    match channels {
        1 => 0x1,
        2 => 0x3,
        _ => 0x0,
    }
}

impl BlockBuilder for LiveAudioRouterBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building LiveAudioRouter block instance: {}", instance_id);

        let in_channels = parse_channel_count(properties, "in_channels", 2);
        let out_channels = parse_channel_count(properties, "out_channels", 2);

        let json_str = match properties.get("routing_matrix") {
            Some(PropertyValue::String(s)) => s.as_str(),
            _ => "{}",
        };

        let matrix = match routing_matrix_to_gst_array(json_str, in_channels, out_channels) {
            Some(matrix) => matrix,
            None => {
                warn!(
                    "LiveAudioRouter {}: routing_matrix is not valid JSON, starting straight through",
                    instance_id
                );
                routing_matrix_to_gst_array("{}", in_channels, out_channels)
                    .expect("an empty matrix always serializes")
            }
        };

        info!(
            "LiveAudioRouter config: {} in x {} out, matrix {}",
            in_channels, out_channels, matrix
        );

        let in_id = format!("{}:live_router_in", instance_id);
        let in_elem = gst::ElementFactory::make("audioconvert")
            .name(&in_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let matrix_id = format!("{}{}", instance_id, MATRIX_ELEMENT_ID_TAIL);
        let matrix_elem = gst::ElementFactory::make("audiomixmatrix")
            .name(&matrix_id)
            .property("in-channels", in_channels as u32)
            .property("out-channels", out_channels as u32)
            .property("channel-mask", channel_mask(out_channels))
            .property_from_str("matrix", &matrix)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audiomixmatrix: {}", e)))?;

        let out_id = format!("{}:live_router_out", instance_id);
        let out_elem = gst::ElementFactory::make("audioconvert")
            .name(&out_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let internal_links = vec![
            (
                ElementPadRef::pad(&in_id, "src"),
                ElementPadRef::pad(&matrix_id, "sink"),
            ),
            (
                ElementPadRef::pad(&matrix_id, "src"),
                ElementPadRef::pad(&out_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (in_id, in_elem),
                (matrix_id, matrix_elem),
                (out_id, out_elem),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

// ============================================================================
// Block Definition
// ============================================================================

/// Get Live Audio Router block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![liveaudiorouter_definition()]
}

fn liveaudiorouter_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.liveaudiorouter".to_string(),
        name: "Live Audio Router".to_string(),
        description: "Route audio channels through a matrix that can be changed while the flow is running. Single input and output stream; use Audio Router for multi-stream routing that does not need live changes.".to_string(),
        category: "Audio".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "in_channels".to_string(),
                label: "Input Channels".to_string(),
                description: "Number of channels on the input stream (1-64). Changing this rebuilds the flow.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(2)),
                mapping: PropertyMapping {
                    element_id: "live_router_matrix".to_string(),
                    property_name: "in-channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "out_channels".to_string(),
                label: "Output Channels".to_string(),
                description: "Number of channels on the output stream (1-64). Changing this rebuilds the flow.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(2)),
                mapping: PropertyMapping {
                    element_id: "live_router_matrix".to_string(),
                    property_name: "out-channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "routing_matrix".to_string(),
                label: "Routing Matrix".to_string(),
                description: "JSON routing matrix, applied without restarting the flow. Format: {\"i0c0\": [\"o0c0\", \"o0c1\"]} where iXcY = input channel Y, oXcY = output channel Y; the stream index X is always 0. Empty or {} routes every input channel straight through to the output channel with the same index.".to_string(),
                property_type: PropertyType::Multiline,
                default_value: Some(PropertyValue::String("{}".to_string())),
                mapping: PropertyMapping {
                    element_id: "live_router_matrix".to_string(),
                    property_name: "routing_matrix".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "live_router_in".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "live_router_out".to_string(),
                internal_pad_name: "src".to_string(),
            }],
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
