//! Audio meter block using GStreamer level element.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue, StromEvent, *};
use tracing::{debug, warn};

/// Audio Meter block builder.
pub struct MeterBuilder;

impl BlockBuilder for MeterBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        tracing::info!("📊 Building Meter block instance: {}", instance_id);

        // Get interval property (in milliseconds, convert to nanoseconds for GStreamer)
        let interval_ms = properties
            .get("interval")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i),
                PropertyValue::String(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(100); // Default 100ms

        let interval_ns = interval_ms * 1_000_000; // Convert ms to ns

        tracing::info!(
            "📊 Meter block properties: interval_ms={}, interval_ns={}",
            interval_ms,
            interval_ns
        );

        // Create the level element
        let level_id = format!("{}:level", instance_id);

        tracing::info!("📊 Creating level element: {}", level_id);
        tracing::info!("📊 Setting post-messages=true on level element");

        let level = gst::ElementFactory::make("level")
            .name(&level_id)
            .property("interval", interval_ns as u64)
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("level: {}", e)))?;

        tracing::info!("📊 Level element created successfully: {}", level_id);

        // Create a bus message handler that will be called when the pipeline starts
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_level_message_handler(bus, flow_id, events)
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements: vec![(level_id, level)],
            internal_links: vec![], // No internal links - it's a single element
            bus_message_handler,
        })
    }
}

/// Extract f64 values from a GValueArray field in a GStreamer structure.
/// Used to parse RMS, peak, and decay values from level messages.
fn extract_level_values(structure: &gst::StructureRef, field_name: &str) -> Vec<f64> {
    use gstreamer::glib;

    // Try to get the field as a GValueArray
    if let Ok(array) = structure.get::<glib::ValueArray>(field_name) {
        // Extract each value from the array
        array.iter().filter_map(|v| v.get::<f64>().ok()).collect()
    } else {
        Vec::new()
    }
}

/// Connect a message handler for level messages from the meter block.
/// This is called when the pipeline starts and uses `connect_message` which
/// allows multiple handlers (unlike `add_watch` which only allows one).
fn connect_level_message_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;

    debug!("📊 Connecting level message handler via connect_message");

    // First ensure signal watch is enabled (this is ref-counted, safe to call multiple times)
    bus.add_signal_watch();

    // Connect to message signal - this allows multiple handlers unlike add_watch
    bus.connect_message(None, move |_bus, msg| {
        // Only handle element messages with "level" structure
        if let MessageView::Element(element_msg) = msg.view() {
            if let Some(s) = element_msg.structure() {
                let structure_name = s.name();

                if structure_name == "level" {
                    debug!("📊 Received 'level' message from GStreamer bus!");

                    // Extract element ID from the source
                    if let Some(source) = msg.src() {
                        let full_element_id = source.name().to_string();
                        debug!("📊 Level message from element: {}", full_element_id);

                        // Strip ":level" suffix to get the block ID
                        // Meter blocks create elements like "block_id:level", but UI looks up by "block_id"
                        let element_id =
                            if let Some(block_id) = full_element_id.strip_suffix(":level") {
                                block_id.to_string()
                            } else {
                                full_element_id
                            };
                        debug!("📊 Using block ID for lookup: {}", element_id);

                        // Extract RMS, peak, and decay values from the message structure
                        // These are GValueArrays containing one f64 per channel
                        let rms = extract_level_values(s, "rms");
                        let peak = extract_level_values(s, "peak");
                        let decay = extract_level_values(s, "decay");

                        debug!(
                            "📊 Extracted values: rms={:?}, peak={:?}, decay={:?}",
                            rms, peak, decay
                        );

                        if !rms.is_empty() {
                            debug!(
                                "📊 Broadcasting MeterData event for flow {} element {}",
                                flow_id, element_id
                            );
                            // Broadcast meter data event
                            events.broadcast(StromEvent::MeterData {
                                flow_id,
                                element_id,
                                rms,
                                peak,
                                decay,
                            });
                        } else {
                            warn!("📊 RMS array is empty, not broadcasting MeterData");
                        }
                    } else {
                        warn!("📊 Level message has no source element");
                    }
                }
            }
        }
    })
}

/// Get metadata for Meter block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![meter_definition()]
}

/// Get Meter block definition (metadata only).
fn meter_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.meter".to_string(),
        name: "Audio Meter".to_string(),
        description:
            "Analyzes audio levels. Uses GStreamer level element to report RMS and peak per channel."
                .to_string(),
        category: "Analysis".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "interval".to_string(),
            label: "Update Interval (ms)".to_string(),
            description: "How often meter values are sent (lower = more responsive, higher CPU)"
                .to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue { value: "10".to_string(), label: Some("10 ms".to_string()) },
                    EnumValue { value: "20".to_string(), label: Some("20 ms".to_string()) },
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
        }],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "level".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "level".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📊".to_string()),
            color: Some("#9C27B0".to_string()),
            width: Some(1.5),
            height: Some(2.0),
        }),
    }
}
