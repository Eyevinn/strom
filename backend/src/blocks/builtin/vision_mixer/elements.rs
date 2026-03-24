//! GStreamer element factory helpers for the vision mixer block.

use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info};

/// Compositor backend selection result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompositorBackend {
    OpenGL,
    Software,
}

/// Determine which compositor backend to use.
pub fn select_backend(preference: &str) -> Result<CompositorBackend, BlockBuildError> {
    match preference {
        "gpu" => {
            if gst::ElementFactory::find("glvideomixerelement").is_some() {
                Ok(CompositorBackend::OpenGL)
            } else {
                Err(BlockBuildError::ElementCreation(
                    "GPU backend requested but glvideomixerelement not available".to_string(),
                ))
            }
        }
        "cpu" => Ok(CompositorBackend::Software),
        _ => {
            // Auto: prefer GPU, fallback to CPU
            if gst::ElementFactory::find("glvideomixerelement").is_some() {
                info!("Vision mixer: using GPU (OpenGL) backend");
                Ok(CompositorBackend::OpenGL)
            } else {
                info!("Vision mixer: GPU unavailable, falling back to CPU backend");
                Ok(CompositorBackend::Software)
            }
        }
    }
}

/// Create the distribution (PGM) compositor element.
pub fn make_dist_compositor(
    backend: CompositorBackend,
    force_live: bool,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = match backend {
        CompositorBackend::OpenGL => "glvideomixerelement",
        CompositorBackend::Software => "compositor",
    };

    let mixer = gst::ElementFactory::make(element_type)
        .name("mixer")
        .property("force-live", force_live)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", element_type, e)))?;

    apply_post_build_properties(&mixer, latency_ms, min_upstream_latency_ms);
    if mixer.find_property("background").is_some() {
        mixer.set_property_from_str("background", "black");
    }
    debug!(
        "Created distribution compositor: {} ({})",
        element_type,
        backend_name(backend)
    );
    Ok(mixer)
}

/// Create the multiview compositor element.
pub fn make_mv_compositor(
    backend: CompositorBackend,
    force_live: bool,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = match backend {
        CompositorBackend::OpenGL => "glvideomixerelement",
        CompositorBackend::Software => "compositor",
    };

    let mixer = gst::ElementFactory::make(element_type)
        .name("mv_comp")
        .property("force-live", force_live)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", element_type, e)))?;

    apply_post_build_properties(&mixer, latency_ms, min_upstream_latency_ms);
    // Black background for multiview
    if mixer.find_property("background").is_some() {
        mixer.set_property_from_str("background", "black");
    }

    debug!(
        "Created multiview compositor: {} ({})",
        element_type,
        backend_name(backend)
    );
    Ok(mixer)
}

/// Apply compositor properties that can be set after construction.
/// Note: force-live is construct-only and must be set via ElementFactory::make().property().
fn apply_post_build_properties(
    mixer: &gst::Element,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) {
    if mixer.find_property("latency").is_some() {
        let latency_ns = latency_ms * 1_000_000;
        mixer.set_property("latency", latency_ns);
    }
    if mixer.find_property("min-upstream-latency").is_some() {
        let latency_ns = min_upstream_latency_ms * 1_000_000;
        mixer.set_property("min-upstream-latency", latency_ns);
    }
    if mixer.find_property("start-time-selection").is_some() {
        mixer.set_property_from_str("start-time-selection", "first");
    }
}

/// Create a tee element for splitting input to multiple consumers.
pub fn make_tee(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("tee")
        .name(name)
        .property("allow-not-linked", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("tee: {}", e)))
}

/// Create a queue element.
pub fn make_queue(name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make("queue")
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue: {}", e)))
}

/// Create a capsfilter with video/x-raw caps at the given resolution.
pub fn make_capsfilter(
    name: &str,
    width: u32,
    height: u32,
) -> Result<gst::Element, BlockBuildError> {
    let caps = gst::Caps::builder("video/x-raw")
        .field("width", width as i32)
        .field("height", height as i32)
        .build();

    gst::ElementFactory::make("capsfilter")
        .name(name)
        .property("caps", &caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))
}

/// Create a simple GStreamer element by factory name.
pub fn make_element(factory: &str, name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
}

fn backend_name(backend: CompositorBackend) -> &'static str {
    match backend {
        CompositorBackend::OpenGL => "OpenGL",
        CompositorBackend::Software => "Software",
    }
}
