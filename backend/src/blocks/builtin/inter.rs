//! Inter-pipeline communication blocks using GStreamer intersink/intersrc.
//!
//! These blocks enable sharing media streams between flows:
//! - **InterOutput**: Publishes a stream for other flows to consume
//! - **InterInput**: Subscribes to a stream from another flow

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, MediaType, PropertyValue};

// ============================================================================
// GStreamer Element Creation
// ============================================================================

/// Buffer configuration for inter-pipeline communication.
///
/// Controls how much data is buffered on the subscriber side,
/// allowing each consumer to have independent latency settings.
#[derive(Debug, Clone)]
struct BufferConfig {
    /// Maximum buffer time in nanoseconds (default: 500ms)
    max_time_ns: u64,
}

/// Create a publisher (sink) element for a published output.
///
/// Creates an `intersink` element from the rsinter plugin.
/// The rsinter plugin is format-agnostic - it works with any media type.
fn create_intersink(element_id: &str, channel_name: &str) -> Result<gst::Element, BlockBuildError> {
    let element = gst::ElementFactory::make("intersink")
        .name(element_id)
        .property("producer-name", channel_name)
        .property("sync", false) // Don't sync to clock - allows playing without consumers
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("intersink: {}", e)))?;

    // InterSink is a Bin containing an AppSink. Set async=false on internal appsink
    // to prevent it from waiting for preroll before transitioning to PLAYING.
    // The appsink is named "{element_id}-appsink" inside the bin.
    if let Some(bin) = element.downcast_ref::<gst::Bin>() {
        let appsink_name = format!("{}-appsink", element_id);
        if let Some(appsink) = bin.by_name(&appsink_name) {
            appsink.set_property("async", false);
            tracing::debug!("Set async=false on internal appsink: {}", appsink_name);
        } else {
            tracing::warn!("Could not find internal appsink: {}", appsink_name);
        }
    }

    tracing::debug!(
        element_id = %element_id,
        channel_name = %channel_name,
        "Created intersink publisher"
    );

    Ok(element)
}

/// Create a subscriber (source) element for a subscription.
///
/// Creates an `intersrc` element from the rsinter plugin.
/// The rsinter plugin is format-agnostic - it works with any media type.
fn create_intersrc(
    element_id: &str,
    channel_name: &str,
    buffer_config: &BufferConfig,
) -> Result<gst::Element, BlockBuildError> {
    let mut builder = gst::ElementFactory::make("intersrc")
        .name(element_id)
        .property("producer-name", channel_name);

    // Apply buffer configuration if non-default
    if buffer_config.max_time_ns > 0 {
        builder = builder.property("max-time", buffer_config.max_time_ns);
    }

    let element = builder
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("intersrc: {}", e)))?;

    tracing::debug!(
        element_id = %element_id,
        channel_name = %channel_name,
        max_time_ns = buffer_config.max_time_ns,
        "Created intersrc subscriber"
    );

    Ok(element)
}

// ============================================================================
// Block Builders
// ============================================================================

/// InterOutput block builder - publishes a stream for other flows.
pub struct InterOutputBuilder;

impl BlockBuilder for InterOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        tracing::info!("Building InterOutput block instance: {}", instance_id);

        // Get flow_id and block_id (injected by expand_blocks)
        let flow_id = properties
            .get("_flow_id")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidConfiguration("_flow_id not provided".to_string())
            })?;

        let block_id = properties
            .get("_block_id")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidConfiguration("_block_id not provided".to_string())
            })?;

        // Generate unique channel name from flow_id and block_id
        let channel_name = format!("strom_{}_{}", flow_id, block_id);

        let element_id = format!("{}:intersink", instance_id);

        // Create the intersink element (rsinter is format-agnostic)
        let intersink = create_intersink(&element_id, &channel_name)?;

        tracing::info!(
            "InterOutput block created: {} -> channel '{}'",
            instance_id,
            channel_name
        );

        Ok(BlockBuildResult {
            elements: vec![(element_id, intersink)],
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// InterInput block builder - subscribes to a stream from another flow.
pub struct InterInputBuilder;

impl BlockBuilder for InterInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        tracing::info!("Building InterInput block instance: {}", instance_id);

        // Get channel name property
        let channel_name = properties
            .get("channel")
            .and_then(|v| match v {
                PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidConfiguration("channel name is required".to_string())
            })?;

        // Get buffer configuration
        let max_time_ms = properties
            .get("max_time")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u64),
                PropertyValue::String(s) => s.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(500); // Default 500ms

        let buffer_config = BufferConfig {
            max_time_ns: max_time_ms * 1_000_000, // Convert ms to ns
        };

        let element_id = format!("{}:intersrc", instance_id);

        // Create the intersrc element (rsinter is format-agnostic)
        let intersrc = create_intersrc(&element_id, &channel_name, &buffer_config)?;

        tracing::info!(
            "InterInput block created: {} <- channel '{}' (buffer: {}ms)",
            instance_id,
            channel_name,
            max_time_ms
        );

        Ok(BlockBuildResult {
            elements: vec![(element_id, intersrc)],
            internal_links: vec![],
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

// ============================================================================
// Block Definitions
// ============================================================================

/// Get metadata for Inter blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![inter_output_definition(), inter_input_definition()]
}

/// InterOutput block definition.
fn inter_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.inter_output".to_string(),
        name: "Inter Output".to_string(),
        description: "Publishes a stream for other flows to consume. The channel name is automatically generated from the flow and block IDs.".to_string(),
        category: "Inter-Pipeline".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "description".to_string(),
            label: "Description".to_string(),
            description: "Description for this output (shown in Inter Input dropdowns)"
                .to_string(),
            property_type: PropertyType::String,
            default_value: None,
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "description".to_string(),
                transform: None,
            },
        }],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "sink".to_string(),
                media_type: MediaType::Generic,
                internal_element_id: "intersink".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![], // No outputs - this is a sink
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📤".to_string()),
            width: Some(1.5),
            height: Some(1.5),
            // Orange/amber color scheme for inter-pipeline blocks
            light_fill_color: Some("#FEF0E0".to_string()),
            light_stroke_color: Some("#C07020".to_string()),
            dark_fill_color: Some("#352A1E".to_string()),
            dark_stroke_color: Some("#E8A050".to_string()),
            ..Default::default()
        }),
    }
}

/// InterInput block definition.
fn inter_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.inter_input".to_string(),
        name: "Inter Input".to_string(),
        description: "Subscribes to a stream from another flow. Select an Inter Output from the dropdown.".to_string(),
        category: "Inter-Pipeline".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "channel".to_string(),
                label: "Source".to_string(),
                description: "Select an Inter Output block to subscribe to".to_string(),
                property_type: PropertyType::String,
                default_value: None,
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "channel".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "max_time".to_string(),
                label: "Buffer Time (ms)".to_string(),
                description: "Maximum buffer time for this subscriber. Higher values add latency but handle jitter better.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "100".to_string(), label: Some("100 ms".to_string()) },
                        EnumValue { value: "200".to_string(), label: Some("200 ms".to_string()) },
                        EnumValue { value: "500".to_string(), label: Some("500 ms (default)".to_string()) },
                        EnumValue { value: "1000".to_string(), label: Some("1000 ms".to_string()) },
                        EnumValue { value: "2000".to_string(), label: Some("2000 ms".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("500".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_time".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![], // No inputs - this is a source
            outputs: vec![ExternalPad {
                name: "src".to_string(),
                media_type: MediaType::Generic,
                internal_element_id: "intersrc".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📥".to_string()),
            width: Some(1.5),
            height: Some(1.5),
            // Orange/amber color scheme for inter-pipeline blocks
            light_fill_color: Some("#FEF0E0".to_string()),
            light_stroke_color: Some("#C07020".to_string()),
            dark_fill_color: Some("#352A1E".to_string()),
            dark_stroke_color: Some("#E8A050".to_string()),
            ..Default::default()
        }),
    }
}
