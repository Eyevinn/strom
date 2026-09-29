//! A `PipelineManager` built the way `start_flow` builds one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::{PipelineError, PipelineManager};
use strom_types::Flow;

/// Build `flow` with no ICE servers, no WHIP registry, and media under the
/// system temp dir. Initialises GStreamer, but not GPU detection: a test whose
/// flow has a vision mixer or video encoder calls
/// `strom::gpu::detect_gpu_capabilities()` itself.
pub fn build(flow: &Flow) -> Result<PipelineManager, PipelineError> {
    build_with(
        flow,
        EventBroadcaster::with_capacity(10),
        std::env::temp_dir(),
    )
}

/// [`build`] with the caller's event broadcaster and media path.
pub fn build_with(
    flow: &Flow,
    events: EventBroadcaster,
    media_path: PathBuf,
) -> Result<PipelineManager, PipelineError> {
    gstreamer::init().expect("GStreamer initialises");
    // `PipelineManager::new` takes a registry but does not read it.
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());
    PipelineManager::new(
        flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_path,
        Arc::new(Mutex::new(HashMap::new())),
    )
}
