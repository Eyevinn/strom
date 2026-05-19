//! Audio spectrum analyzer block using GStreamer spectrum element.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue, StromEvent, *};
use tracing::{debug, trace, warn};

/// Audio Spectrum block builder.
pub struct SpectrumBuilder;

impl BlockBuilder for SpectrumBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building Spectrum block instance: {}", instance_id);

        // Get interval property (in milliseconds, convert to nanoseconds for GStreamer)
        let interval_ms = properties
            .get("interval")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i),
                PropertyValue::String(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(100);

        let interval_ns = interval_ms * 1_000_000;

        // Get bands property
        let bands = properties
            .get("bands")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u32),
                PropertyValue::String(s) => s.parse::<u32>().ok(),
                _ => None,
            })
            .unwrap_or(32);

        // Get threshold property (in dB, negative value)
        let threshold = properties
            .get("threshold")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as i32),
                PropertyValue::String(s) => s.parse::<i32>().ok(),
                _ => None,
            })
            .unwrap_or(-80);

        // Get multi-channel property
        let multi_channel = properties
            .get("multi_channel")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                PropertyValue::String(s) => s.parse::<bool>().ok(),
                _ => None,
            })
            .unwrap_or(false);

        debug!(
            "Spectrum block properties: interval_ms={}, bands={}, threshold={}, multi_channel={}",
            interval_ms, bands, threshold, multi_channel
        );

        // Create the spectrum element
        let spectrum_id = format!("{}:spectrum", instance_id);

        debug!("Creating spectrum element: {}", spectrum_id);

        let spectrum = gst::ElementFactory::make("spectrum")
            .name(&spectrum_id)
            .property("interval", interval_ns as u64)
            .property("post-messages", true)
            .property("bands", bands)
            .property("threshold", threshold)
            .property("multi-channel", multi_channel)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("spectrum: {}", e)))?;

        debug!("Spectrum element created successfully: {}", spectrum_id);

        // Create a bus message handler
        let expected_element_id = spectrum_id.clone();
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_spectrum_message_handler(
                    bus,
                    flow_id,
                    events,
                    expected_element_id.clone(),
                    multi_channel,
                )
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements: vec![(spectrum_id, spectrum)],
            internal_links: vec![],
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}

/// Extract a flat list of f32 values from a GStreamer structure field.
/// Tries GValueArray, gst::Array, and gst::List in order.
fn extract_flat_f32(structure: &gst::StructureRef, field_name: &str) -> Option<Vec<f32>> {
    use gstreamer::glib;

    // Try glib::ValueArray
    if let Ok(array) = structure.get::<glib::ValueArray>(field_name) {
        let result: Vec<f32> = array.iter().filter_map(|v| v.get::<f32>().ok()).collect();
        if !result.is_empty() {
            return Some(result);
        }
    }

    // Try gst::Array (GstValueArray)
    if let Ok(array) = structure.get::<gst::Array>(field_name) {
        let result: Vec<f32> = array.iter().filter_map(|v| v.get::<f32>().ok()).collect();
        if !result.is_empty() {
            return Some(result);
        }
    }

    // Try gst::List (GstValueList)
    if let Ok(list) = structure.get::<gst::List>(field_name) {
        let result: Vec<f32> = list.iter().filter_map(|v| v.get::<f32>().ok()).collect();
        if !result.is_empty() {
            return Some(result);
        }
    }

    None
}

/// Extract multi-channel magnitude data from a spectrum message.
///
/// When `multi-channel=true`, the magnitude field is a nested array:
/// outer dimension = channels, inner dimension = frequency bands.
/// When `multi-channel=false`, it's a flat array (mono mix of all channels).
///
/// Always returns `Vec<Vec<f32>>` where outer = channels, inner = bands.
fn extract_magnitudes(structure: &gst::StructureRef, multi_channel: bool) -> Vec<Vec<f32>> {
    use gstreamer::glib;

    if multi_channel {
        // Multi-channel: try nested GValueArray first, then nested gst::Array
        if let Ok(outer) = structure.get::<glib::ValueArray>("magnitude") {
            let channels: Vec<Vec<f32>> = outer
                .iter()
                .filter_map(|ch_val| {
                    ch_val
                        .get::<glib::ValueArray>()
                        .ok()
                        .map(|inner| inner.iter().filter_map(|v| v.get::<f32>().ok()).collect())
                })
                .collect();
            if !channels.is_empty() && !channels[0].is_empty() {
                return channels;
            }
        }

        // Try nested gst::Array
        if let Ok(outer) = structure.get::<gst::Array>("magnitude") {
            let channels: Vec<Vec<f32>> = outer
                .iter()
                .filter_map(|ch_val| {
                    ch_val
                        .get::<gst::Array>()
                        .ok()
                        .map(|inner| inner.iter().filter_map(|v| v.get::<f32>().ok()).collect())
                })
                .collect();
            if !channels.is_empty() && !channels[0].is_empty() {
                return channels;
            }
        }

        // Try nested gst::List
        if let Ok(outer) = structure.get::<gst::List>("magnitude") {
            let channels: Vec<Vec<f32>> = outer
                .iter()
                .filter_map(|ch_val| {
                    ch_val
                        .get::<gst::List>()
                        .ok()
                        .map(|inner| inner.iter().filter_map(|v| v.get::<f32>().ok()).collect())
                })
                .collect();
            if !channels.is_empty() && !channels[0].is_empty() {
                return channels;
            }
        }

        warn!("multi-channel spectrum: could not extract nested magnitude arrays, falling back to flat");
    }

    // Flat (mono) mode: wrap in a single-channel vec
    if let Some(flat) = extract_flat_f32(structure, "magnitude") {
        return vec![flat];
    }

    // Debug: log what type the field actually has
    if let Ok(value) = structure.value("magnitude") {
        warn!(
            "Could not extract magnitude from spectrum message, value type: {}",
            value.type_()
        );
    } else {
        warn!("Spectrum structure has no field 'magnitude'");
    }

    Vec::new()
}

/// Connect a message handler for spectrum messages from a specific spectrum block.
fn connect_spectrum_message_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    expected_element_id: String,
    multi_channel: bool,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;

    debug!(
        "Connecting spectrum message handler for flow {} element {} (multi_channel={})",
        flow_id, expected_element_id, multi_channel
    );

    bus.add_signal_watch();

    bus.connect_message(None, move |_bus, msg| {
        if let MessageView::Element(element_msg) = msg.view() {
            if let Some(s) = element_msg.structure() {
                if s.name() == "spectrum" {
                    if let Some(source) = msg.src() {
                        let source_element_id = source.name().to_string();

                        if source_element_id != expected_element_id {
                            return;
                        }

                        trace!(
                            "Spectrum message from element: {} (flow {})",
                            source_element_id,
                            flow_id
                        );

                        // Strip ":spectrum" suffix to get the block ID
                        let element_id =
                            if let Some(block_id) = source_element_id.strip_suffix(":spectrum") {
                                block_id.to_string()
                            } else {
                                source_element_id
                            };

                        let magnitudes = extract_magnitudes(s, multi_channel);

                        if !magnitudes.is_empty() {
                            trace!(
                                "Broadcasting SpectrumData for flow {} element {} ({} channels, {} bands)",
                                flow_id,
                                element_id,
                                magnitudes.len(),
                                magnitudes[0].len()
                            );
                            events.broadcast(StromEvent::SpectrumData {
                                flow_id,
                                element_id,
                                magnitudes,
                            });
                        } else {
                            warn!("Magnitude data is empty, not broadcasting SpectrumData");
                        }
                    }
                }
            }
        }
    })
}

/// Get metadata for Spectrum block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![spectrum_definition()]
}

/// Get Spectrum block definition (metadata only).
fn spectrum_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.spectrum".to_string(),
        name: "Spectrum Analyzer".to_string(),
        description:
            "Analyzes audio frequency spectrum. Uses GStreamer spectrum element to perform FFT analysis and display frequency magnitude bars."
                .to_string(),
        category: "Analysis".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "interval".to_string(),
                label: "Update Interval (ms)".to_string(),
                description: "How often spectrum data is sent (lower = more responsive, higher CPU)"
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "50".to_string(), label: Some("50 ms".to_string()) },
                        EnumValue { value: "100".to_string(), label: Some("100 ms".to_string()) },
                        EnumValue { value: "200".to_string(), label: Some("200 ms".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("100".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "interval".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "bands".to_string(),
                label: "FFT Bands".to_string(),
                description: "Number of frequency bands (more = finer resolution, higher CPU)"
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "16".to_string(), label: Some("16 bands".to_string()) },
                        EnumValue { value: "32".to_string(), label: Some("32 bands".to_string()) },
                        EnumValue { value: "64".to_string(), label: Some("64 bands".to_string()) },
                        EnumValue { value: "128".to_string(), label: Some("128 bands".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("32".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bands".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "threshold".to_string(),
                label: "Threshold (dB)".to_string(),
                description: "Minimum magnitude threshold for reported values".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "-80".to_string(), label: Some("-80 dB".to_string()) },
                        EnumValue { value: "-60".to_string(), label: Some("-60 dB".to_string()) },
                        EnumValue { value: "-40".to_string(), label: Some("-40 dB".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("-80".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "threshold".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "multi_channel".to_string(),
                label: "Per-Channel FFT".to_string(),
                description: "When enabled, shows a separate spectrum per audio channel instead of a mono mix"
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "multi_channel".to_string(),
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
                internal_element_id: "spectrum".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "spectrum".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("\u{1f4ca}".to_string()),
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
