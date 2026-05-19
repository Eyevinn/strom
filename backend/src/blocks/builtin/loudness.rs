//! EBU R128 loudness meter block using GStreamer ebur128level element.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue, StromEvent, *};
use tracing::{debug, trace};

/// EBU R128 Loudness Meter block builder.
pub struct LoudnessBuilder;

impl BlockBuilder for LoudnessBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building Loudness block instance: {}", instance_id);

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

        debug!(
            "Loudness block properties: interval_ms={}, interval_ns={}",
            interval_ms, interval_ns
        );

        // Create the ebur128level element
        let element_id = format!("{}:ebur128level", instance_id);

        debug!("Creating ebur128level element: {}", element_id);

        let ebur128 = gst::ElementFactory::make("ebur128level")
            .name(&element_id)
            .property("interval", interval_ns as u64)
            .property("post-messages", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("ebur128level: {}", e)))?;

        debug!("ebur128level element created successfully: {}", element_id);

        // Create a bus message handler
        let expected_element_id = element_id.clone();
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_loudness_message_handler(bus, flow_id, events, expected_element_id.clone())
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements: vec![(element_id, ebur128)],
            internal_links: vec![],
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}

/// Extract f64 values from a gst::Array field in a GStreamer structure.
fn extract_array_values(structure: &gst::StructureRef, field_name: &str) -> Vec<f64> {
    if let Ok(array) = structure.get::<gst::Array>(field_name) {
        array.iter().filter_map(|v| v.get::<f64>().ok()).collect()
    } else {
        Vec::new()
    }
}

/// Connect a message handler for ebur128-level messages from a specific loudness block.
fn connect_loudness_message_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    expected_element_id: String,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;

    debug!(
        "Connecting loudness message handler for flow {} element {}",
        flow_id, expected_element_id
    );

    bus.add_signal_watch();

    bus.connect_message(None, move |_bus, msg| {
        if let MessageView::Element(element_msg) = msg.view() {
            if let Some(s) = element_msg.structure() {
                if s.name() == "ebur128-level" {
                    if let Some(source) = msg.src() {
                        let source_element_id = source.name().to_string();

                        if source_element_id != expected_element_id {
                            return;
                        }

                        trace!(
                            "Loudness message from element: {} (flow {})",
                            source_element_id,
                            flow_id
                        );

                        // Strip ":ebur128level" suffix to get the block ID
                        let element_id = if let Some(block_id) =
                            source_element_id.strip_suffix(":ebur128level")
                        {
                            block_id.to_string()
                        } else {
                            source_element_id
                        };

                        let momentary_raw = s.get::<f64>("momentary-loudness").ok();
                        let shortterm = s.get::<f64>("shortterm-loudness").ok().filter(|v| v.is_finite());
                        let integrated = s.get::<f64>("global-loudness").ok().filter(|v| v.is_finite());
                        let loudness_range = s.get::<f64>("loudness-range").ok().filter(|v| v.is_finite());
                        let true_peak = extract_array_values(s, "true-peak");

                        // Only broadcast if we have valid momentary data
                        if let Some(momentary) = momentary_raw.filter(|v| v.is_finite()) {
                            trace!(
                                "Broadcasting LoudnessData for flow {} element {}: M={:.1} S={:?} I={:?}",
                                flow_id, element_id, momentary, shortterm, integrated
                            );
                            events.broadcast(StromEvent::LoudnessData {
                                flow_id,
                                element_id,
                                momentary,
                                shortterm,
                                integrated,
                                loudness_range,
                                true_peak,
                            });
                        } else {
                            trace!("Momentary loudness is not finite, not broadcasting LoudnessData");
                        }
                    }
                }
            }
        }
    })
}

/// Get metadata for Loudness block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![loudness_definition()]
}

/// Get Loudness block definition (metadata only).
fn loudness_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.loudness".to_string(),
        name: "Loudness Meter".to_string(),
        description:
            "EBU R128 loudness meter. Measures momentary, short-term, integrated loudness, loudness range, and true peak."
                .to_string(),
        category: "Analysis".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "interval".to_string(),
            label: "Update Interval (ms)".to_string(),
            description: "How often loudness values are sent (lower = more responsive, higher CPU)"
                .to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue { value: "100".to_string(), label: Some("100 ms".to_string()) },
                    EnumValue { value: "200".to_string(), label: Some("200 ms".to_string()) },
                    EnumValue { value: "500".to_string(), label: Some("500 ms".to_string()) },
                    EnumValue { value: "1000".to_string(), label: Some("1000 ms".to_string()) },
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
        }],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "ebur128level".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "ebur128level".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📊".to_string()),
            width: Some(2.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
