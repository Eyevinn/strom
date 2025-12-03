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

        // Check if queues are enabled (default true)
        let use_queues = properties
            .get("use_queues")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // Create input pads dynamically - map to queue or glupload depending on use_queues
        let mut inputs = Vec::new();
        for i in 0..num_inputs {
            let internal_element_id = if use_queues {
                format!("queue_{}", i)
            } else {
                format!("glupload_{}", i)
            };

            inputs.push(ExternalPad {
                name: format!("video_in_{}", i),
                media_type: MediaType::Video,
                internal_element_id,
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

        // Get gl_output property (default false = include gldownload for compatibility)
        let gl_output = properties
            .get("gl_output")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false); // Default to false (include gldownload)

        // Get use_queues property (default true = use queues for latency buffering)
        let use_queues = properties
            .get("use_queues")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true); // Default to true (use queues)

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
            // Get per-input properties from block properties, with computed defaults
            // based on the output canvas resolution
            let (default_xpos, default_ypos, default_width, default_height) =
                calculate_default_layout(i, output_width, output_height);

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
                .unwrap_or(default_ypos);
            sink_pad.set_property_from_str("ypos", &ypos.to_string());

            let width = properties
                .get(&format!("input_{}_width", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(default_width);
            sink_pad.set_property_from_str("width", &width.to_string());

            let height = properties
                .get(&format!("input_{}_height", i))
                .and_then(|v| match v {
                    PropertyValue::Int(i) => Some(*i),
                    _ => None,
                })
                .unwrap_or(default_height);
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

        // Create output chain based on gl_output setting
        let capsfilter_id = format!("{}:capsfilter", instance_id);
        let caps_str = if gl_output {
            // GL output: keep in GL memory
            // glvideomixerelement only outputs RGBA format in GL memory
            // Note: texture-target is automatically negotiated, don't restrict it
            format!(
                "video/x-raw(memory:GLMemory),format=RGBA,width={},height={}",
                output_width, output_height
            )
        } else {
            // System memory output: regular video/x-raw
            format!(
                "video/x-raw,width={},height={}",
                output_width, output_height
            )
        };

        info!("🎬 Output caps: {}", caps_str);

        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        // Optionally create gldownload if not outputting GL memory
        let download_id = if !gl_output {
            Some(format!("{}:gldownload", instance_id))
        } else {
            None
        };

        let download = if let Some(ref id) = download_id {
            Some(
                gst::ElementFactory::make("gldownload")
                    .name(id)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("gldownload: {}", e)))?,
            )
        } else {
            None
        };

        // Create input chain elements for each input
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

            let mixer_pad_name = sink_pad.name().to_string();

            if use_queues {
                // Create queue for latency buffering (solves "Impossible to configure latency" errors)
                // Queue provides buffering needed when sources have low/zero max latency
                let queue_id = format!("{}:queue_{}", instance_id, i);
                let queue = gst::ElementFactory::make("queue")
                    .name(&queue_id)
                    .property("max-size-buffers", 3u32) // Buffer up to 3 frames
                    .property("max-size-bytes", 0u32) // No byte limit
                    .property("max-size-time", 0u64) // No time limit
                    .property("flush-on-eos", true)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("queue_{}: {}", i, e)))?;

                elements.push((queue_id.clone(), queue));

                // Link queue -> glupload -> mixer
                internal_links.push((
                    ElementPadRef::pad(&queue_id, "src"),
                    ElementPadRef::pad(&upload_id, "sink"),
                ));
                internal_links.push((
                    ElementPadRef::pad(&upload_id, "src"),
                    ElementPadRef::pad(&mixer_id, &mixer_pad_name),
                ));
            } else {
                // Link glupload -> mixer directly (no queue for lower latency)
                internal_links.push((
                    ElementPadRef::pad(&upload_id, "src"),
                    ElementPadRef::pad(&mixer_id, &mixer_pad_name),
                ));
            }
        }

        // Add output elements and create links based on gl_output setting
        if let Some(ref dl_id) = download_id {
            // gl_output=false: mixer -> gldownload -> capsfilter
            elements.push((dl_id.clone(), download.unwrap()));
            elements.push((capsfilter_id.clone(), capsfilter));

            internal_links.push((
                ElementPadRef::pad(&mixer_id, "src"),
                ElementPadRef::pad(dl_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(dl_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ));

            info!("🎬 Output chain: mixer -> gldownload -> capsfilter (system memory)");
        } else {
            // gl_output=true: mixer -> capsfilter (stay in GL memory)
            elements.push((capsfilter_id.clone(), capsfilter));

            internal_links.push((
                ElementPadRef::pad(&mixer_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ));

            info!("🎬 Output chain: mixer -> capsfilter (GL memory)");
        }

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

/// Calculate default position and size for an input based on output resolution.
///
/// Creates a 3-row tiered layout:
/// - Row 1: Inputs 0-1 as large halves (50% width, 50% height)
/// - Row 2: Inputs 2-5 side by side (25% width, 25% height)
/// - Row 3: Inputs 6-15 small tiles (10% width, 10% height)
///
/// With 30px vertical spacing between rows for labels.
fn calculate_default_layout(
    input_index: usize,
    canvas_width: u32,
    canvas_height: u32,
) -> (i64, i64, i64, i64) {
    let w = canvas_width as i64;
    let h = canvas_height as i64;

    // Row heights (as percentage of canvas height)
    let row1_h = h / 2; // 50% height for row 1
    let row2_h = h / 4; // 25% height for row 2
    let row3_h = h / 10; // 10% height for row 3

    // Vertical positions with 30px spacing
    let row1_y = 0;
    let row2_y = row1_h + 30;
    let row3_y = row2_y + row2_h + 30;

    match input_index {
        // Row 1: Two large halves
        0 => (0, row1_y, w / 2, row1_h),
        1 => (w / 2, row1_y, w / 2, row1_h),

        // Row 2: Four medium tiles
        2 => (0, row2_y, w / 4, row2_h),
        3 => (w / 4, row2_y, w / 4, row2_h),
        4 => (w / 2, row2_y, w / 4, row2_h),
        5 => (w * 3 / 4, row2_y, w / 4, row2_h),

        // Row 3: Small tiles (10 tiles at 10% width each)
        n => {
            let tile_w = w / 10;
            let tile_x = ((n - 6) as i64) * tile_w;
            (tile_x, row3_y, tile_w, row3_h)
        }
    }
}

/// Get metadata for GLCompositor block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![glcompositor_definition()]
}

/// Get GLCompositor block definition (metadata only).
fn glcompositor_definition() -> BlockDefinition {
    const MAX_INPUTS: usize = 16;

    let mut exposed_properties = vec![
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
            ExposedProperty {
                name: "gl_output".to_string(),
                label: "GL Memory Output".to_string(),
                description: "Output in OpenGL memory (true) for chaining GL elements, or system memory (false, default) for compatibility with encoders. When true, skips gldownload for better performance with GL processing chains.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "gl_output".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "use_queues".to_string(),
                label: "Use Input Queues".to_string(),
                description: "Add queue elements on inputs for latency buffering (true, default = use queues to handle sources with varying latency). Disable for direct connection if you need absolute lowest latency and all sources have proper max latency reporting.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "use_queues".to_string(),
                    transform: None,
                },
            },
        ];

    // Dynamically generate per-input properties for all possible inputs (0 to MAX_INPUTS-1)
    // Default layout is computed based on default canvas size (1920x1080)
    for i in 0..MAX_INPUTS {
        let (default_xpos, default_ypos, default_width, default_height) =
            calculate_default_layout(i, 1920, 1080);

        // XPos
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_xpos", i),
            label: format!("Input {} X Position", i),
            description: format!("X position of input {} on the canvas", i),
            property_type: PropertyType::Int,
            default_value: Some(PropertyValue::Int(default_xpos)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_xpos", i),
                transform: None,
            },
        });

        // YPos
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_ypos", i),
            label: format!("Input {} Y Position", i),
            description: format!("Y position of input {} on the canvas", i),
            property_type: PropertyType::Int,
            default_value: Some(PropertyValue::Int(default_ypos)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_ypos", i),
                transform: None,
            },
        });

        // Width
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_width", i),
            label: format!("Input {} Width", i),
            description: format!("Width of input {} (-1 = source width)", i),
            property_type: PropertyType::Int,
            default_value: Some(PropertyValue::Int(default_width)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_width", i),
                transform: None,
            },
        });

        // Height
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_height", i),
            label: format!("Input {} Height", i),
            description: format!("Height of input {} (-1 = source height)", i),
            property_type: PropertyType::Int,
            default_value: Some(PropertyValue::Int(default_height)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_height", i),
                transform: None,
            },
        });

        // Alpha
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_alpha", i),
            label: format!("Input {} Alpha", i),
            description: format!("Alpha/transparency of input {} (0.0-1.0)", i),
            property_type: PropertyType::Float,
            default_value: Some(PropertyValue::Float(1.0)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_alpha", i),
                transform: None,
            },
        });

        // Z-Order
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_zorder", i),
            label: format!("Input {} Z-Order", i),
            description: format!("Z-order of input {} (higher = on top)", i),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(i as u64)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: format!("input_{}_zorder", i),
                transform: None,
            },
        });

        // Sizing Policy
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_sizing_policy", i),
            label: format!("Input {} Sizing Policy", i),
            description: format!("How to scale input {}: 'none' (stretch to fill) or 'keep-aspect-ratio' (preserve aspect with padding)", i),
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
                property_name: format!("input_{}_sizing_policy", i),
                transform: None,
            },
        });
    }

    BlockDefinition {
        id: "builtin.glcompositor".to_string(),
        name: "OpenGL Video Compositor".to_string(),
        description: "Hardware-accelerated OpenGL video compositor for combining multiple video inputs with positioning, scaling, and alpha blending. Each input can be independently positioned and sized on the output canvas.".to_string(),
        category: "Video".to_string(),
        exposed_properties,
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
