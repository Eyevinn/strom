//! OpenGL video compositor block for combining multiple video inputs.
//!
//! This block uses GStreamer's `glvideomixerelement` to composite multiple video streams
//! with hardware-accelerated OpenGL rendering. Each input can be positioned, sized, and
//! blended independently with configurable properties.
//!
//! Features:
//! - Dynamic number of inputs (1-16)
//! - Per-input positioning (xpos, ypos)
//! - Per-input sizing (width, height)
//! - Per-input alpha blending (0.0-1.0)
//! - Per-input z-ordering
//! - Configurable output canvas size
//! - Multiple background types (checker, black, white, transparent)
//!
//! The block creates a chain: glupload (per input) -> glvideomixerelement -> gldownload -> capsfilter

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::info;

/// OpenGL Video Compositor block builder.
pub struct GLCompositorBuilder;

impl BlockBuilder for GLCompositorBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        // Get number of inputs from properties
        let num_inputs = properties
            .get("num_inputs")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(2)
            .clamp(1, 16);

        // Create input pads dynamically - map directly to glupload (no videoconvert)
        let mut inputs = Vec::new();
        for i in 0..num_inputs {
            inputs.push(ExternalPad {
                name: format!("video_in_{}", i),
                media_type: MediaType::Video,
                internal_element_id: format!("glupload_{}", i),
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("🎬 Building GLCompositor block instance: {}", instance_id);

        // Parse properties
        let num_inputs = parse_num_inputs(properties);
        let output_width = parse_output_width(properties);
        let output_height = parse_output_height(properties);
        let background = parse_background(properties);

        info!(
            "🎬 Creating compositor: {} inputs, {}x{} output, background={:?}",
            num_inputs, output_width, output_height, background
        );
        info!(
            "🎬 Block properties: {:?}",
            properties.keys().collect::<Vec<_>>()
        );

        // Create the main mixer element
        let mixer_id = format!("{}:mixer", instance_id);

        // Get force_live property (construction-time only, default true for live mixing)
        let force_live = properties
            .get("force_live")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true); // Default to true for live mixing behavior

        let mixer = gst::ElementFactory::make("glvideomixerelement")
            .name(&mixer_id)
            .property("force-live", force_live)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("glvideomixerelement: {}", e)))?;

        info!("🎬 Mixer created with force-live={}", force_live);

        // Set mixer properties in NULL state
        mixer.set_property_from_str("background", background);

        // Set latency if provided
        if let Some(latency_value) = properties.get("latency") {
            let latency_ms = match latency_value {
                PropertyValue::UInt(u) => *u,
                PropertyValue::Int(i) if *i >= 0 => *i as u64,
                _ => 0,
            };
            let latency_ns = latency_ms * 1_000_000; // Convert ms to nanoseconds
            info!(
                "🎬 Setting mixer latency to {}ms ({}ns)",
                latency_ms, latency_ns
            );
            mixer.set_property_from_str("latency", &latency_ns.to_string());
        }

        // Set min-upstream-latency if provided
        if let Some(min_upstream_latency_value) = properties.get("min_upstream_latency") {
            let min_upstream_latency_ms = match min_upstream_latency_value {
                PropertyValue::UInt(u) => *u,
                PropertyValue::Int(i) if *i >= 0 => *i as u64,
                _ => 0,
            };
            let min_upstream_latency_ns = min_upstream_latency_ms * 1_000_000; // Convert ms to nanoseconds
            info!(
                "🎬 Setting mixer min-upstream-latency to {}ms ({}ns)",
                min_upstream_latency_ms, min_upstream_latency_ns
            );
            mixer.set_property_from_str(
                "min-upstream-latency",
                &min_upstream_latency_ns.to_string(),
            );
        }

        // Request pads and set their properties in NULL state (before adding to pipeline)
        // This is the key insight from test-glvideomixer: configure everything in NULL state
        info!(
            "🎬 Requesting {} mixer sink pads and setting properties in NULL state",
            num_inputs
        );
        info!("🎬 Mixer element state: {:?}", mixer.current_state());
        info!("🎬 Mixer element name: {}", mixer.name());

        let mut mixer_sink_pads = Vec::new();
        for i in 0..num_inputs {
            // Request pad in NULL state
            info!(
                "🎬 Attempting to request pad {} using template 'sink_%u'...",
                i
            );
            let sink_pad = mixer.request_pad_simple("sink_%u")
                .ok_or_else(|| {
                    BlockBuildError::ElementCreation(
                        format!("Failed to request sink pad {} on mixer (element introspection disabled to avoid segfault)", i)
                    )
                })?;

            info!("🎬 Requested pad: {}", sink_pad.name());

            // Set pad properties in NULL state
            // Get per-input properties from block properties, with sensible defaults
            let default_xpos = if i == 0 { 0 } else { 960 };
            let xpos = properties
                .get(&format!("input_{}_xpos", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(default_xpos);
            sink_pad.set_property_from_str("xpos", &xpos.to_string());

            let ypos = properties
                .get(&format!("input_{}_ypos", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(0);
            sink_pad.set_property_from_str("ypos", &ypos.to_string());

            let width = properties
                .get(&format!("input_{}_width", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(-1);
            sink_pad.set_property_from_str("width", &width.to_string());

            let height = properties
                .get(&format!("input_{}_height", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(-1);
            sink_pad.set_property_from_str("height", &height.to_string());

            let alpha = properties
                .get(&format!("input_{}_alpha", i))
                .and_then(|v| match v {
                    PropertyValue::Float(f) => Some(*f),
                    _ => None,
                })
                .unwrap_or(1.0);
            sink_pad.set_property_from_str("alpha", &alpha.to_string());

            let zorder = properties
                .get(&format!("input_{}_zorder", i))
                .and_then(|v| match v {
                    PropertyValue::UInt(u) => Some(*u as u32),
                    PropertyValue::Int(i) if *i >= 0 => Some(*i as u32),
                    _ => None,
                })
                .unwrap_or(i as u32);
            sink_pad.set_property_from_str("zorder", &zorder.to_string());

            // Get sizing policy (default: keep-aspect-ratio)
            let sizing_policy = properties
                .get(&format!("input_{}_sizing_policy", i))
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("keep-aspect-ratio");
            sink_pad.set_property_from_str("sizing-policy", sizing_policy);

            info!("🎬 Pad {} properties set: xpos={}, ypos={}, width={}, height={}, alpha={}, zorder={}, sizing-policy={}",
                  sink_pad.name(), xpos, ypos, width, height, alpha, zorder, sizing_policy);

            mixer_sink_pads.push(sink_pad);
        }

        // Create gldownload for output
        let download_id = format!("{}:gldownload", instance_id);
        let download = gst::ElementFactory::make("gldownload")
            .name(&download_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("gldownload: {}", e)))?;

        // Create capsfilter with output dimensions
        let capsfilter_id = format!("{}:capsfilter", instance_id);
        let caps_str = format!(
            "video/x-raw,width={},height={}",
            output_width, output_height
        );
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        // Create glupload elements for each input
        let mut elements = vec![(mixer_id.clone(), mixer.clone())];
        let mut internal_links = Vec::new();

        for (i, sink_pad) in mixer_sink_pads.iter().enumerate() {
            // Create glupload for hardware-accelerated format conversion
            // Note: videoconvert removed - it's a CPU bottleneck for live video!
            // glupload can handle format conversion directly with GPU acceleration
            let upload_id = format!("{}:glupload_{}", instance_id, i);
            let upload = gst::ElementFactory::make("glupload")
                .name(&upload_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("glupload_{}: {}", i, e)))?;

            elements.push((upload_id.clone(), upload));

            // Link glupload -> mixer (using pre-created pad)
            // We already requested and configured the pad above in NULL state
            let mixer_pad_name = sink_pad.name().to_string();
            internal_links.push((
                ElementPadRef::pad(&upload_id, "src"),
                ElementPadRef::pad(&mixer_id, &mixer_pad_name),
            ));
        }

        // Add output elements
        elements.push((download_id.clone(), download));
        elements.push((capsfilter_id.clone(), capsfilter));

        // Link mixer -> gldownload -> capsfilter (pad-level linking)
        internal_links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&download_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&download_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        ));

        info!(
            "🎬 GLCompositor block created: {} inputs with pads pre-configured in NULL state",
            num_inputs
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(), // No pad properties needed - already set in NULL state
        })
    }
}

/// Parse number of inputs from properties.
fn parse_num_inputs(properties: &HashMap<String, PropertyValue>) -> usize {
    properties
        .get("num_inputs")
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as usize),
            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
            _ => None,
        })
        .unwrap_or(2)
        .clamp(1, 16)
}

/// Parse output width from properties.
fn parse_output_width(properties: &HashMap<String, PropertyValue>) -> u32 {
    properties
        .get("output_width")
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as u32),
            PropertyValue::Int(i) if *i > 0 => Some(*i as u32),
            _ => None,
        })
        .unwrap_or(1920)
        .clamp(1, 7680) // Max 8K width
}

/// Parse output height from properties.
fn parse_output_height(properties: &HashMap<String, PropertyValue>) -> u32 {
    properties
        .get("output_height")
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as u32),
            PropertyValue::Int(i) if *i > 0 => Some(*i as u32),
            _ => None,
        })
        .unwrap_or(1080)
        .clamp(1, 4320) // Max 8K height
}

/// Parse background type from properties.
fn parse_background(properties: &HashMap<String, PropertyValue>) -> &'static str {
    properties
        .get("background")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .and_then(|s| match s {
            "checker" => Some("checker"),
            "black" => Some("black"),
            "white" => Some("white"),
            "transparent" => Some("transparent"),
            _ => None,
        })
        .unwrap_or("black")
}

/// Get metadata for GLCompositor block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![glcompositor_definition()]
}

/// Get GLCompositor block definition (metadata only).
fn glcompositor_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.glcompositor".to_string(),
        name: "OpenGL Video Compositor".to_string(),
        description: "Hardware-accelerated OpenGL video compositor for combining multiple video inputs with positioning, scaling, and alpha blending. Each input can be independently positioned and sized on the output canvas.".to_string(),
        category: "Video".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_inputs".to_string(),
                label: "Number of Inputs".to_string(),
                description: "Number of video inputs to composite (1-16)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(2)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_inputs".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "output_width".to_string(),
                label: "Output Width".to_string(),
                description: "Width of the output canvas in pixels (1-7680)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1920)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "output_width".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "output_height".to_string(),
                label: "Output Height".to_string(),
                description: "Height of the output canvas in pixels (1-4320)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1080)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "output_height".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "background".to_string(),
                label: "Background".to_string(),
                description: "Background type for the canvas".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "black".to_string(),
                            label: Some("Black".to_string()),
                        },
                        EnumValue {
                            value: "white".to_string(),
                            label: Some("White".to_string()),
                        },
                        EnumValue {
                            value: "checker".to_string(),
                            label: Some("Checker Pattern".to_string()),
                        },
                        EnumValue {
                            value: "transparent".to_string(),
                            label: Some("Transparent".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("black".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "background".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "latency".to_string(),
                label: "Latency (ms)".to_string(),
                description: "Additional latency in milliseconds for the mixer aggregator".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "latency".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "min_upstream_latency".to_string(),
                label: "Min Upstream Latency (ms)".to_string(),
                description: "Minimum upstream latency in milliseconds that is reported to upstream elements".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "min_upstream_latency".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "force_live".to_string(),
                label: "Force Live Mode".to_string(),
                description: "Always operate in live mode and aggregate on timeout regardless of whether any live sources are linked upstream. Construction-time only - cannot be changed after block creation.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "force_live".to_string(),
                    transform: None,
                },
            },
            // Note: Per-input properties (input_N_xpos, etc.) are dynamically generated
            // based on num_inputs. For now, we expose common defaults for 2 inputs.
            // TODO: Generate these dynamically in the UI based on num_inputs value.
            ExposedProperty {
                name: "input_0_xpos".to_string(),
                label: "Input 0 X Position".to_string(),
                description: "X position of input 0 on the canvas".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_xpos".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_ypos".to_string(),
                label: "Input 0 Y Position".to_string(),
                description: "Y position of input 0 on the canvas".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_ypos".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_width".to_string(),
                label: "Input 0 Width".to_string(),
                description: "Width of input 0 (-1 = source width)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(-1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_width".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_height".to_string(),
                label: "Input 0 Height".to_string(),
                description: "Height of input 0 (-1 = source height)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(-1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_height".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_alpha".to_string(),
                label: "Input 0 Alpha".to_string(),
                description: "Alpha/transparency of input 0 (0.0-1.0)".to_string(),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(1.0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_alpha".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_zorder".to_string(),
                label: "Input 0 Z-Order".to_string(),
                description: "Z-order of input 0 (higher = on top)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_zorder".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_0_sizing_policy".to_string(),
                label: "Input 0 Sizing Policy".to_string(),
                description: "How to scale input 0: 'none' (stretch to fill) or 'keep-aspect-ratio' (preserve aspect with padding)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "none".to_string(),
                            label: Some("None (Stretch to Fill)".to_string()),
                        },
                        EnumValue {
                            value: "keep-aspect-ratio".to_string(),
                            label: Some("Keep Aspect Ratio".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("keep-aspect-ratio".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_0_sizing_policy".to_string(),
                    transform: None,
                },
            },
            // Input 1 properties
            ExposedProperty {
                name: "input_1_xpos".to_string(),
                label: "Input 1 X Position".to_string(),
                description: "X position of input 1 on the canvas".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(960)),  // Right half by default
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_xpos".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_ypos".to_string(),
                label: "Input 1 Y Position".to_string(),
                description: "Y position of input 1 on the canvas".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_ypos".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_width".to_string(),
                label: "Input 1 Width".to_string(),
                description: "Width of input 1 (-1 = source width)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(-1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_width".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_height".to_string(),
                label: "Input 1 Height".to_string(),
                description: "Height of input 1 (-1 = source height)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(-1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_height".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_alpha".to_string(),
                label: "Input 1 Alpha".to_string(),
                description: "Alpha/transparency of input 1 (0.0-1.0)".to_string(),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(1.0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_alpha".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_zorder".to_string(),
                label: "Input 1 Z-Order".to_string(),
                description: "Z-order of input 1 (higher = on top)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_zorder".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "input_1_sizing_policy".to_string(),
                label: "Input 1 Sizing Policy".to_string(),
                description: "How to scale input 1: 'none' (stretch to fill) or 'keep-aspect-ratio' (preserve aspect with padding)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "none".to_string(),
                            label: Some("None (Stretch to Fill)".to_string()),
                        },
                        EnumValue {
                            value: "keep-aspect-ratio".to_string(),
                            label: Some("Keep Aspect Ratio".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("keep-aspect-ratio".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "input_1_sizing_policy".to_string(),
                    transform: None,
                },
            },
        ],
        // External pads are computed dynamically based on num_inputs
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    name: "video_in_0".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videoconvert_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    name: "video_in_1".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videoconvert_1".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎬".to_string()),
            color: Some("#9C27B0".to_string()), // Purple for compositor
            width: Some(2.0),
            height: Some(2.5),
        }),
    }
}
