//! Regression test: a pipeline that aborts its PAUSED -> PLAYING transition
//! must make `PipelineManager::start()` fail.
//!
//! `gst_element_get_state()` reports a still-running transition as `Ok(Async)`
//! and a failed one as `Err`, whatever the pending state says. `start()` used
//! to treat `Err` with `pending == Playing` as "async still in progress" and
//! return `Ok(Paused)`, so a flow whose pipeline had already posted a fatal
//! error on the bus was reported as started. `start_flow()` then went on to
//! register the flow's WHEP endpoints, but the pipeline never left PAUSED, so
//! whepserversink's signaller never opened its HTTP port and every WHEP offer
//! against that flow answered 502 for as long as it "ran".

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, Link};
use tempfile::NamedTempFile;

/// Build `audiotestsrc → identity error-after=1 → fakesink`.
///
/// `identity` errors out on the very first buffer, which is pushed during
/// preroll. The sink therefore never prerolls, the pipeline never leaves
/// PAUSED, and the transition to PLAYING fails with PLAYING still pending —
/// the same shape a live flow produces when a parser inside `decodebin` posts
/// a fatal error while the pipeline is prerolling.
///
/// Uses only core GStreamer elements plus `audiotestsrc` so it runs in CI.
fn build_failing_flow(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: HashMap::new(),
        position: [100.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "fail".to_string(),
        element_type: "identity".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "error-after".to_string(),
                strom_types::PropertyValue::Int(1),
            );
            p
        },
        position: [250.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [400.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "fail:sink".to_string(),
    });
    flow.links.push(Link {
        from: "fail:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_fails_when_pipeline_cannot_reach_playing() {
    gstreamer::init().unwrap();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let flow = build_failing_flow("start_failure_test");

    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_path,
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("Failed to create PipelineManager");

    let result = manager.start();

    assert!(
        result.is_err(),
        "start() reported success for a pipeline that never reached PLAYING \
         (returned {:?}) — callers register WHEP endpoints for a dead flow",
        result
    );
}

/// Control: the same harness on a healthy flow still starts. Guards against
/// "fix" the failure path by rejecting every async start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_succeeds_for_healthy_pipeline() {
    gstreamer::init().unwrap();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let mut flow = build_failing_flow("start_success_test");
    // Same topology, but identity passes buffers through instead of erroring.
    flow.elements[1].properties.insert(
        "error-after".to_string(),
        strom_types::PropertyValue::Int(-1),
    );

    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_path,
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("Failed to create PipelineManager");

    let state = manager.start().expect("Healthy pipeline failed to start");
    assert_eq!(state, strom_types::PipelineState::Playing);

    manager.stop().expect("Failed to stop pipeline");
}
