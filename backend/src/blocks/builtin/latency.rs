//! Audio latency measurement block using GStreamer audiolatency element.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, PropertyValue, StromEvent, *};
use tracing::{debug, trace, warn};

/// Audio Latency block builder.
pub struct LatencyBuilder;

impl BlockBuilder for LatencyBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        tracing::info!("Building Latency block instance: {}", instance_id);

        // Get samplesperbuffer property (number of samples in each buffer)
        let samples_per_buffer = properties
            .get("samplesperbuffer")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as i32),
                PropertyValue::String(s) => s.parse::<i32>().ok(),
                _ => None,
            })
            .unwrap_or(240); // Default 240 samples

        // Get print_latency property (whether to print to stdout)
        let print_latency = properties
            .get("print_latency")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        tracing::info!(
            "Latency block properties: samplesperbuffer={}, print_latency={}",
            samples_per_buffer,
            print_latency
        );

        // Create the audiolatency element
        let audiolatency_id = format!("{}:audiolatency", instance_id);

        tracing::info!("Creating audiolatency element: {}", audiolatency_id);

        let audiolatency = gst::ElementFactory::make("audiolatency")
            .name(&audiolatency_id)
            .property("samplesperbuffer", samples_per_buffer)
            .property("print-latency", print_latency)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audiolatency: {}", e)))?;

        tracing::info!(
            "Audiolatency element created successfully: {}",
            audiolatency_id
        );

        // Create a bus message handler that will be called when the pipeline starts
        let bus_message_handler = Some(Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_latency_message_handler(bus, flow_id, events)
            },
        ) as crate::blocks::BusMessageConnectFn);

        Ok(BlockBuildResult {
            elements: vec![(audiolatency_id, audiolatency)],
            internal_links: vec![], // No internal links - it's a single element
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}

/// Connect a message handler for latency messages from the audiolatency element.
/// This is called when the pipeline starts and uses `connect_message` which
/// allows multiple handlers (unlike `add_watch` which only allows one).
fn connect_latency_message_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;

    debug!("Connecting latency message handler via connect_message");

    // First ensure signal watch is enabled (this is ref-counted, safe to call multiple times)
    bus.add_signal_watch();

    // Connect to message signal - this allows multiple handlers unlike add_watch
    bus.connect_message(None, move |_bus, msg| {
        // Only handle element messages with "latency" structure
        if let MessageView::Element(element_msg) = msg.view() {
            if let Some(s) = element_msg.structure() {
                let structure_name = s.name();

                if structure_name == "latency" {
                    trace!("Received 'latency' message from GStreamer bus!");

                    // Extract element ID from the source
                    if let Some(source) = msg.src() {
                        let full_element_id = source.name().to_string();
                        trace!("Latency message from element: {}", full_element_id);

                        // Strip ":audiolatency" suffix to get the block ID
                        let element_id =
                            if let Some(block_id) = full_element_id.strip_suffix(":audiolatency") {
                                block_id.to_string()
                            } else {
                                full_element_id
                            };
                        trace!("Using block ID for lookup: {}", element_id);

                        // Extract latency values from the message structure
                        // Values are in microseconds (i64)
                        let last_latency_us = s.get::<i64>("last-latency").unwrap_or(0);
                        let average_latency_us = s.get::<i64>("average-latency").unwrap_or(0);

                        trace!(
                            "Extracted latency: last={}us, avg={}us",
                            last_latency_us,
                            average_latency_us
                        );

                        // Broadcast latency data event
                        events.broadcast(StromEvent::LatencyData {
                            flow_id,
                            element_id,
                            last_latency_us,
                            average_latency_us,
                        });
                    } else {
                        warn!("Latency message has no source element");
                    }
                }
            }
        }
    })
}

/// Get metadata for Latency block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![latency_definition()]
}

/// Get Latency block definition (metadata only).
fn latency_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.latency".to_string(),
        name: "Audio Latency".to_string(),
        description:
            "Measures audio latency by sending periodic ticks and measuring round-trip time. Uses GStreamer audiolatency element."
                .to_string(),
        category: "Analysis".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "samplesperbuffer".to_string(),
                label: "Samples Per Buffer".to_string(),
                description: "Number of samples in each outgoing buffer".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(240)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "samplesperbuffer".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "print_latency".to_string(),
                label: "Print to Console".to_string(),
                description: "Print measured latencies to stdout".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "print_latency".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audiolatency".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audiolatency".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("⏱".to_string()),
            width: Some(1.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
