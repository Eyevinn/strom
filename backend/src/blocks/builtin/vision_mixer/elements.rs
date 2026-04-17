//! GStreamer element factory helpers for the vision mixer block.

use crate::blocks::BlockBuildError;
use crate::gpu;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, trace};

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
            // Auto: prefer GPU, but only if a real hardware GL renderer is available.
            // Mesa software renderers (llvmpipe) are slower than the CPU compositor.
            if gst::ElementFactory::find("glvideomixerelement").is_some() && gpu::has_hardware_gl()
            {
                info!("Vision mixer: using GPU (OpenGL) backend");
                Ok(CompositorBackend::OpenGL)
            } else {
                info!("Vision mixer: no hardware GL, using CPU backend");
                Ok(CompositorBackend::Software)
            }
        }
    }
}

/// Create the distribution (PGM) compositor element.
pub fn make_dist_compositor(
    backend: CompositorBackend,
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = match backend {
        CompositorBackend::OpenGL => "glvideomixerelement",
        CompositorBackend::Software => "compositor",
    };

    let mixer = gst::ElementFactory::make(element_type)
        .name("mixer")
        .property("force-live", true)
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
    latency_ms: u64,
    min_upstream_latency_ms: u64,
) -> Result<gst::Element, BlockBuildError> {
    let element_type = match backend {
        CompositorBackend::OpenGL => "glvideomixerelement",
        CompositorBackend::Software => "compositor",
    };

    let mixer = gst::ElementFactory::make(element_type)
        .name("mv_comp")
        .property("force-live", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", element_type, e)))?;

    apply_post_build_properties(&mixer, latency_ms, min_upstream_latency_ms);
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
        // Use "zero" instead of "first" to avoid a race condition in GStreamer 1.26:
        // with "first", if the aggregator srcpad task runs before any buffer arrives,
        // it falls through to using the absolute monotonic clock time as start time,
        // causing the compositor to wait for an impossibly far deadline (2× system uptime).
        // With "zero" and force-live=true, running time starts at 0 which is correct
        // for live pipelines using monotonic clock.
        mixer.set_property_from_str("start-time-selection", "zero");
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

/// Create a simple GStreamer element by factory name.
pub fn make_element(factory: &str, name: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
}

/// Suppress upstream latency queries on the sink pad of a queue element.
///
/// The PGM feed from the distribution compositor (mixer) is tee'd into the
/// multiview compositor (mv_comp). Without this probe the latency query from
/// mv_comp traverses back through mixer, causing the two compositors' latencies
/// to stack. The probe answers the LATENCY query directly with min=0 so that
/// mv_comp's peer latency is determined by its direct input paths instead.
pub fn suppress_latency_query(queue: &gst::Element) {
    let pad = queue
        .static_pad("sink")
        .expect("queue must have a sink pad");
    pad.add_probe(gst::PadProbeType::QUERY_UPSTREAM, |_pad, info| {
        if let Some(query) = info.query_mut() {
            if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
                trace!(
                    "Suppressed latency query on PGM->MV path (preventing compositor latency stacking)"
                );
                latency.set(true, gst::ClockTime::ZERO, None::<gst::ClockTime>);
                return gst::PadProbeReturn::Handled;
            }
        }
        gst::PadProbeReturn::Ok
    });
}

fn backend_name(backend: CompositorBackend) -> &'static str {
    match backend {
        CompositorBackend::OpenGL => "OpenGL",
        CompositorBackend::Software => "Software",
    }
}
