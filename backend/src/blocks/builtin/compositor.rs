//! Unified video compositor block supporting both CPU and GPU backends.
//!
//! This block provides video compositing with automatic backend selection:
//! - **GPU (OpenGL)**: Uses `glvideomixerelement` for hardware-accelerated compositing
//! - **CPU (Software)**: Uses `compositor` element for CPU-based compositing
//!
//! Features:
//! - Dynamic number of inputs (1-16)
//! - Per-input positioning (xpos, ypos)
//! - Per-input sizing (width, height)
//! - Per-input alpha blending (0.0-1.0)
//! - Per-input z-ordering
//! - Configurable output canvas size
//! - Multiple background types (black, white, transparent)
//! - Automatic fallback from GPU to CPU when OpenGL is unavailable
//!
//! GPU backend chain: queue -> glupload -> glcolorconvert -> glvideomixerelement -> gldownload -> capsfilter
//! CPU backend chain: queue -> videoconvert -> compositor -> capsfilter

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::*, common_video_resolution_enum_values, element::ElementPadRef, parse_resolution_string,
    PropertyValue, *,
};
use tracing::{info, warn};

/// Backend selection for the compositor.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CompositorBackend {
    /// GPU-accelerated OpenGL compositor (glvideomixerelement)
    OpenGL,
    /// CPU-based software compositor (compositor element)
    Software,
}

/// User preference for compositor backend selection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CompositorPreference {
    /// Automatically select best available backend (GPU first, then CPU)
    Auto,
    /// Only use GPU (OpenGL) backend - fails if unavailable
    GPUOnly,
    /// Only use CPU (software) backend
    CPUOnly,
}

/// Video Compositor block builder supporting CPU and GPU backends.
pub struct CompositorBuilder;

impl BlockBuilder for CompositorBuilder {
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

        // Determine backend to figure out internal element naming
        let preference = parse_compositor_preference(properties);
        let backend = select_compositor(preference).unwrap_or(CompositorBackend::Software);

        // Create input pads dynamically - map to queue or converter depending on settings
        let mut inputs = Vec::new();
        for i in 0..num_inputs {
            let internal_element_id = if use_queues {
                format!("queue_{}", i)
            } else {
                // Direct connection to converter element
                match backend {
                    CompositorBackend::OpenGL => format!("glupload_{}", i),
                    CompositorBackend::Software => format!("videoconvert_{}", i),
                }
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
        info!("🎬 Building Compositor block instance: {}", instance_id);

        // Parse compositor preference and select backend
        let preference = parse_compositor_preference(properties);
        let backend = select_compositor(preference)?;

        info!(
            "🎬 Selected compositor backend: {:?} (preference: {:?})",
            backend, preference
        );

        // Parse common properties
        let num_inputs = parse_num_inputs(properties);
        let (output_width, output_height) = parse_output_resolution(properties);
        let background = parse_background(properties);
        let use_queues = parse_use_queues(properties);
        let force_live = parse_force_live(properties);
        let gl_output = parse_gl_output(properties);

        info!(
            "🎬 Creating compositor: {} inputs, {}x{} output, background={:?}, backend={:?}",
            num_inputs, output_width, output_height, background, backend
        );

        // Build the pipeline based on selected backend
        match backend {
            CompositorBackend::OpenGL => build_opengl_compositor(
                instance_id,
                properties,
                num_inputs,
                output_width,
                output_height,
                background,
                use_queues,
                force_live,
                gl_output,
            ),
            CompositorBackend::Software => build_software_compositor(
                instance_id,
                properties,
                num_inputs,
                output_width,
                output_height,
                background,
                use_queues,
                force_live,
            ),
        }
    }
}

/// Select compositor backend based on preference and availability.
fn select_compositor(
    preference: CompositorPreference,
) -> Result<CompositorBackend, BlockBuildError> {
    let registry = gst::Registry::get();

    // Check if OpenGL compositor is available
    let has_gl = registry
        .find_feature("glvideomixerelement", gst::ElementFactory::static_type())
        .is_some();

    // Check if software compositor is available (should always be true)
    let has_software = registry
        .find_feature("compositor", gst::ElementFactory::static_type())
        .is_some();

    match preference {
        CompositorPreference::GPUOnly => {
            if has_gl {
                info!("🎬 Using GPU (OpenGL) compositor as requested");
                Ok(CompositorBackend::OpenGL)
            } else {
                Err(BlockBuildError::InvalidConfiguration(
                    "GPU compositor requested but glvideomixerelement is not available".to_string(),
                ))
            }
        }
        CompositorPreference::CPUOnly => {
            if has_software {
                info!("🎬 Using CPU (software) compositor as requested");
                Ok(CompositorBackend::Software)
            } else {
                Err(BlockBuildError::InvalidConfiguration(
                    "CPU compositor requested but compositor element is not available".to_string(),
                ))
            }
        }
        CompositorPreference::Auto => {
            if has_gl {
                info!("🎬 Auto-selected GPU (OpenGL) compositor");
                Ok(CompositorBackend::OpenGL)
            } else if has_software {
                warn!("🎬 GPU compositor unavailable, falling back to CPU compositor");
                Ok(CompositorBackend::Software)
            } else {
                Err(BlockBuildError::InvalidConfiguration(
                    "No compositor backend available (neither glvideomixerelement nor compositor)"
                        .to_string(),
                ))
            }
        }
    }
}

/// Build OpenGL (GPU) compositor pipeline.
#[allow(clippy::too_many_arguments)]
fn build_opengl_compositor(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    num_inputs: usize,
    output_width: u32,
    output_height: u32,
    background: &'static str,
    use_queues: bool,
    force_live: bool,
    gl_output: bool,
) -> Result<BlockBuildResult, BlockBuildError> {
    // Create the main mixer element
    let mixer_id = format!("{}:mixer", instance_id);
    let mixer = gst::ElementFactory::make("glvideomixerelement")
        .name(&mixer_id)
        .property("force-live", force_live)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("glvideomixerelement: {}", e)))?;

    info!("🎬 GL mixer created with force-live={}", force_live);

    // Set mixer properties in NULL state
    mixer.set_property_from_str("background", background);

    // Set latency properties
    set_mixer_latency_properties(&mixer, properties);

    // Request pads and set their properties in NULL state
    info!(
        "🎬 Requesting {} GL mixer sink pads in NULL state",
        num_inputs
    );

    let mut mixer_sink_pads = Vec::new();
    for i in 0..num_inputs {
        let sink_pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
            BlockBuildError::ElementCreation(format!(
                "Failed to request sink pad {} on GL mixer",
                i
            ))
        })?;

        // Set common pad properties
        set_common_pad_properties(&sink_pad, i, properties, output_width, output_height);

        // Set GL-specific pad property: sizing-policy
        let sizing_policy = properties
            .get(&format!("input_{}_sizing_policy", i))
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("keep-aspect-ratio");
        sink_pad.set_property_from_str("sizing-policy", sizing_policy);

        info!(
            "🎬 GL pad {} configured with sizing-policy={}",
            sink_pad.name(),
            sizing_policy
        );

        mixer_sink_pads.push(sink_pad);
    }

    // Create output chain based on gl_output setting
    let capsfilter_id = format!("{}:capsfilter", instance_id);
    let caps_str = if gl_output {
        format!(
            "video/x-raw(memory:GLMemory),format=RGBA,width={},height={}",
            output_width, output_height
        )
    } else {
        format!(
            "video/x-raw,width={},height={}",
            output_width, output_height
        )
    };

    info!("🎬 GL output caps: {}", caps_str);

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

    // Build element list and internal links
    let mut elements = vec![(mixer_id.clone(), mixer)];
    let mut internal_links = Vec::new();

    // Create input chain for each input
    for (i, sink_pad) in mixer_sink_pads.iter().enumerate() {
        let upload_id = format!("{}:glupload_{}", instance_id, i);
        let upload = gst::ElementFactory::make("glupload")
            .name(&upload_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("glupload_{}: {}", i, e)))?;

        // Create glcolorconvert for GPU-based color space conversion (e.g., NV12 -> RGBA)
        let colorconvert_id = format!("{}:glcolorconvert_{}", instance_id, i);
        let colorconvert = gst::ElementFactory::make("glcolorconvert")
            .name(&colorconvert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("glcolorconvert_{}: {}", i, e))
            })?;

        elements.push((upload_id.clone(), upload));
        elements.push((colorconvert_id.clone(), colorconvert));
        let mixer_pad_name = sink_pad.name().to_string();

        if use_queues {
            let queue_id = format!("{}:queue_{}", instance_id, i);
            let queue = create_input_queue(&queue_id, i)?;
            elements.push((queue_id.clone(), queue));

            // Link: queue -> glupload -> glcolorconvert -> mixer
            internal_links.push((
                ElementPadRef::pad(&queue_id, "src"),
                ElementPadRef::pad(&upload_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&upload_id, "src"),
                ElementPadRef::pad(&colorconvert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&colorconvert_id, "src"),
                ElementPadRef::pad(&mixer_id, &mixer_pad_name),
            ));
        } else {
            // Link: glupload -> glcolorconvert -> mixer directly
            internal_links.push((
                ElementPadRef::pad(&upload_id, "src"),
                ElementPadRef::pad(&colorconvert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&colorconvert_id, "src"),
                ElementPadRef::pad(&mixer_id, &mixer_pad_name),
            ));
        }
    }

    // Add output elements and links
    if let Some(ref dl_id) = download_id {
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

        info!("🎬 GL output chain: mixer -> gldownload -> capsfilter");
    } else {
        elements.push((capsfilter_id.clone(), capsfilter));

        internal_links.push((
            ElementPadRef::pad(&mixer_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        ));

        info!("🎬 GL output chain: mixer -> capsfilter (GL memory)");
    }

    info!("🎬 OpenGL compositor created: {} inputs", num_inputs);

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Build software (CPU) compositor pipeline.
#[allow(clippy::too_many_arguments)]
fn build_software_compositor(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    num_inputs: usize,
    output_width: u32,
    output_height: u32,
    background: &'static str,
    use_queues: bool,
    force_live: bool,
) -> Result<BlockBuildResult, BlockBuildError> {
    // Create the main mixer element
    let mixer_id = format!("{}:mixer", instance_id);
    let mixer = gst::ElementFactory::make("compositor")
        .name(&mixer_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("compositor: {}", e)))?;

    info!("🎬 CPU mixer created");

    // Set mixer properties
    // Note: compositor element uses different property names than glvideomixerelement
    if mixer.has_property("background") {
        mixer.set_property_from_str("background", background);
    }

    // Note: compositor element has force-live as read-only (unlike glvideomixerelement)
    // so we don't set it here - it defaults based on whether live sources are connected
    let _ = force_live; // Acknowledge parameter even though we can't use it for CPU backend

    // Set latency properties
    set_mixer_latency_properties(&mixer, properties);

    // Request pads and set their properties
    info!("🎬 Requesting {} CPU mixer sink pads", num_inputs);

    let mut mixer_sink_pads = Vec::new();
    for i in 0..num_inputs {
        let sink_pad = mixer.request_pad_simple("sink_%u").ok_or_else(|| {
            BlockBuildError::ElementCreation(format!(
                "Failed to request sink pad {} on CPU mixer",
                i
            ))
        })?;

        // Set common pad properties
        set_common_pad_properties(&sink_pad, i, properties, output_width, output_height);

        info!("🎬 CPU pad {} configured", sink_pad.name());

        mixer_sink_pads.push(sink_pad);
    }

    // Create output capsfilter
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

    // Build element list and internal links
    let mut elements = vec![(mixer_id.clone(), mixer)];
    let mut internal_links = Vec::new();

    // Create input chain for each input
    for (i, sink_pad) in mixer_sink_pads.iter().enumerate() {
        let convert_id = format!("{}:videoconvert_{}", instance_id, i);
        let convert = gst::ElementFactory::make("videoconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoconvert_{}: {}", i, e)))?;

        elements.push((convert_id.clone(), convert));
        let mixer_pad_name = sink_pad.name().to_string();

        if use_queues {
            let queue_id = format!("{}:queue_{}", instance_id, i);
            let queue = create_input_queue(&queue_id, i)?;
            elements.push((queue_id.clone(), queue));

            // Link: queue -> videoconvert -> mixer
            internal_links.push((
                ElementPadRef::pad(&queue_id, "src"),
                ElementPadRef::pad(&convert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&mixer_id, &mixer_pad_name),
            ));
        } else {
            // Link: videoconvert -> mixer directly
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&mixer_id, &mixer_pad_name),
            ));
        }
    }

    // Add output elements and links
    elements.push((capsfilter_id.clone(), capsfilter));

    internal_links.push((
        ElementPadRef::pad(&mixer_id, "src"),
        ElementPadRef::pad(&capsfilter_id, "sink"),
    ));

    info!("🎬 CPU output chain: mixer -> capsfilter");

    info!("🎬 Software compositor created: {} inputs", num_inputs);

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Create an input queue element with standard settings.
fn create_input_queue(queue_id: &str, index: usize) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("queue")
        .name(queue_id)
        //.property("max-size-buffers", 3u32)
        //.property("max-size-bytes", 0u32)
        //.property("max-size-time", 0u64)
        .property("flush-on-eos", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue_{}: {}", index, e)))
}

/// Set common pad properties (shared between GPU and CPU backends).
fn set_common_pad_properties(
    sink_pad: &gst::Pad,
    index: usize,
    properties: &HashMap<String, PropertyValue>,
    output_width: u32,
    output_height: u32,
) {
    let (default_xpos, default_ypos, default_width, default_height) =
        calculate_default_layout(index, output_width, output_height);

    // XPos
    let xpos = properties
        .get(&format!("input_{}_xpos", index))
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(default_xpos);
    sink_pad.set_property_from_str("xpos", &xpos.to_string());

    // YPos
    let ypos = properties
        .get(&format!("input_{}_ypos", index))
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(default_ypos);
    sink_pad.set_property_from_str("ypos", &ypos.to_string());

    // Width
    let width = properties
        .get(&format!("input_{}_width", index))
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(default_width);
    sink_pad.set_property_from_str("width", &width.to_string());

    // Height
    let height = properties
        .get(&format!("input_{}_height", index))
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(default_height);
    sink_pad.set_property_from_str("height", &height.to_string());

    // Alpha
    let alpha = properties
        .get(&format!("input_{}_alpha", index))
        .and_then(|v| match v {
            PropertyValue::Float(f) => Some(*f),
            _ => None,
        })
        .unwrap_or(1.0);
    sink_pad.set_property_from_str("alpha", &alpha.to_string());

    // Z-Order
    let zorder = properties
        .get(&format!("input_{}_zorder", index))
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as u32),
            PropertyValue::Int(i) if *i >= 0 => Some(*i as u32),
            _ => None,
        })
        .unwrap_or(index as u32);
    sink_pad.set_property_from_str("zorder", &zorder.to_string());

    info!(
        "🎬 Pad {} properties: xpos={}, ypos={}, width={}, height={}, alpha={}, zorder={}",
        sink_pad.name(),
        xpos,
        ypos,
        width,
        height,
        alpha,
        zorder
    );
}

/// Set latency properties on a mixer element.
fn set_mixer_latency_properties(mixer: &gst::Element, properties: &HashMap<String, PropertyValue>) {
    // Set latency if provided
    if let Some(latency_value) = properties.get("latency") {
        let latency_ms = match latency_value {
            PropertyValue::UInt(u) => *u,
            PropertyValue::Int(i) if *i >= 0 => *i as u64,
            _ => 0,
        };
        if latency_ms > 0 {
            let latency_ns = latency_ms * 1_000_000;
            info!(
                "🎬 Setting mixer latency to {}ms ({}ns)",
                latency_ms, latency_ns
            );
            mixer.set_property_from_str("latency", &latency_ns.to_string());
        }
    }

    // Set min-upstream-latency if provided
    if let Some(min_upstream_latency_value) = properties.get("min_upstream_latency") {
        let min_upstream_latency_ms = match min_upstream_latency_value {
            PropertyValue::UInt(u) => *u,
            PropertyValue::Int(i) if *i >= 0 => *i as u64,
            _ => 0,
        };
        if min_upstream_latency_ms > 0 && mixer.has_property("min-upstream-latency") {
            let min_upstream_latency_ns = min_upstream_latency_ms * 1_000_000;
            info!(
                "🎬 Setting mixer min-upstream-latency to {}ms ({}ns)",
                min_upstream_latency_ms, min_upstream_latency_ns
            );
            mixer.set_property_from_str(
                "min-upstream-latency",
                &min_upstream_latency_ns.to_string(),
            );
        }
    }
}

// ============================================================================
// Property Parsing Helpers
// ============================================================================

/// Parse compositor preference from properties.
fn parse_compositor_preference(
    properties: &HashMap<String, PropertyValue>,
) -> CompositorPreference {
    properties
        .get("compositor_preference")
        .and_then(|v| match v {
            PropertyValue::String(s) => match s.as_str() {
                "gpu" => Some(CompositorPreference::GPUOnly),
                "cpu" => Some(CompositorPreference::CPUOnly),
                _ => Some(CompositorPreference::Auto),
            },
            _ => None,
        })
        .unwrap_or(CompositorPreference::Auto)
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

/// Parse output resolution from properties.
fn parse_output_resolution(properties: &HashMap<String, PropertyValue>) -> (u32, u32) {
    if let Some(PropertyValue::String(res)) = properties.get("output_resolution") {
        if let Some((w, h)) = parse_resolution_string(res) {
            return (w.clamp(1, 7680), h.clamp(1, 4320));
        }
    }
    (1920, 1080)
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
            "black" => Some("black"),
            "white" => Some("white"),
            "transparent" => Some("transparent"),
            _ => None,
        })
        .unwrap_or("black")
}

/// Parse use_queues from properties.
fn parse_use_queues(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("use_queues")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true)
}

/// Parse force_live from properties.
fn parse_force_live(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("force_live")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(true)
}

/// Parse gl_output from properties (GPU only, ignored for CPU).
fn parse_gl_output(properties: &HashMap<String, PropertyValue>) -> bool {
    properties
        .get("gl_output")
        .and_then(|v| match v {
            PropertyValue::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false)
}

/// Calculate default position and size for an input based on output resolution.
///
/// Creates a 3-row tiered layout:
/// - Row 1: Inputs 0-1 as large halves (50% width, 50% height)
/// - Row 2: Inputs 2-5 side by side (25% width, 25% height)
/// - Row 3: Inputs 6-15 small tiles (10% width, 10% height)
fn calculate_default_layout(
    input_index: usize,
    canvas_width: u32,
    canvas_height: u32,
) -> (i64, i64, i64, i64) {
    let w = canvas_width as i64;
    let h = canvas_height as i64;

    let row1_h = h / 2;
    let row2_h = h / 4;
    let row3_h = h / 10;

    let row1_y = 0;
    let row2_y = row1_h + 30;
    let row3_y = row2_y + row2_h + 30;

    match input_index {
        0 => (0, row1_y, w / 2, row1_h),
        1 => (w / 2, row1_y, w / 2, row1_h),
        2 => (0, row2_y, w / 4, row2_h),
        3 => (w / 4, row2_y, w / 4, row2_h),
        4 => (w / 2, row2_y, w / 4, row2_h),
        5 => (w * 3 / 4, row2_y, w / 4, row2_h),
        n => {
            let tile_w = w / 10;
            let tile_x = ((n - 6) as i64) * tile_w;
            (tile_x, row3_y, tile_w, row3_h)
        }
    }
}

// ============================================================================
// Block Definition
// ============================================================================

/// Get metadata for Compositor block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![compositor_definition()]
}

/// Get Compositor block definition (metadata only).
fn compositor_definition() -> BlockDefinition {
    const MAX_INPUTS: usize = 16;

    let mut exposed_properties = vec![
        // Backend preference
        ExposedProperty {
            name: "compositor_preference".to_string(),
            label: "Backend".to_string(),
            description: "Compositor backend: Auto (GPU first, fallback to CPU), GPU Only (OpenGL), or CPU Only (software)".to_string(),
            property_type: PropertyType::Enum {
                values: vec![
                    EnumValue {
                        value: "auto".to_string(),
                        label: Some("Auto (GPU first, then CPU)".to_string()),
                    },
                    EnumValue {
                        value: "gpu".to_string(),
                        label: Some("GPU Only (OpenGL)".to_string()),
                    },
                    EnumValue {
                        value: "cpu".to_string(),
                        label: Some("CPU Only (Software)".to_string()),
                    },
                ],
            },
            default_value: Some(PropertyValue::String("auto".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "compositor_preference".to_string(),
                transform: None,
            },
        },
        // Number of inputs
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
        // Output resolution
        ExposedProperty {
            name: "output_resolution".to_string(),
            label: "Output Resolution".to_string(),
            description: "Output canvas resolution".to_string(),
            property_type: PropertyType::Enum {
                values: common_video_resolution_enum_values(false),
            },
            default_value: Some(PropertyValue::String("1920x1080".to_string())),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "output_resolution".to_string(),
                transform: None,
            },
        },
        // Background
        ExposedProperty {
            name: "background".to_string(),
            label: "Background".to_string(),
            description: "Background color for the canvas".to_string(),
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
        // Latency
        ExposedProperty {
            name: "latency".to_string(),
            label: "Latency (ms)".to_string(),
            description: "Additional latency in milliseconds for the mixer aggregator".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(200)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "latency".to_string(),
                transform: None,
            },
        },
        // Min upstream latency
        ExposedProperty {
            name: "min_upstream_latency".to_string(),
            label: "Min Upstream Latency (ms)".to_string(),
            description: "Minimum upstream latency reported to upstream elements".to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(200)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "min_upstream_latency".to_string(),
                transform: None,
            },
        },
        // Force live mode
        ExposedProperty {
            name: "force_live".to_string(),
            label: "Force Live Mode".to_string(),
            description: "Always operate in live mode. Construction-time only.".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(true)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "force_live".to_string(),
                transform: None,
            },
        },
        // Use queues
        ExposedProperty {
            name: "use_queues".to_string(),
            label: "Use Input Queues".to_string(),
            description: "Add queue elements on inputs for latency buffering".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(true)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "use_queues".to_string(),
                transform: None,
            },
        },
        // GL output (GPU only)
        ExposedProperty {
            name: "gl_output".to_string(),
            label: "GL Memory Output (GPU only)".to_string(),
            description: "Output in OpenGL memory for chaining GL elements. Only applies to GPU backend.".to_string(),
            property_type: PropertyType::Bool,
            default_value: Some(PropertyValue::Bool(false)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "gl_output".to_string(),
                transform: None,
            },
        },
    ];

    // Generate per-input properties
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

        // Sizing Policy (GPU only)
        exposed_properties.push(ExposedProperty {
            name: format!("input_{}_sizing_policy", i),
            label: format!("Input {} Sizing Policy (GPU only)", i),
            description: format!(
                "How to scale input {}: stretch or keep aspect ratio. Only applies to GPU backend.",
                i
            ),
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
        id: "builtin.compositor".to_string(),
        name: "Video Compositor".to_string(),
        description: "Video compositor supporting both GPU (OpenGL) and CPU backends. Combines multiple video inputs with positioning, scaling, and alpha blending.".to_string(),
        category: "Video".to_string(),
        exposed_properties,
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    name: "video_in_0".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "queue_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    name: "video_in_1".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "queue_1".to_string(),
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
            width: Some(2.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
