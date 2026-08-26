//! Regression test: a pipeline whose transition to PLAYING is poisoned by a
//! transient startup error must recover, not stay dead.
//!
//! An MPEG-TS feed that re-signals its PMT makes tsdemux drain the previous
//! program, pushing EOS out of the pad it removes. The parser decodebin
//! autoplugged for that short-lived pad has no complete access unit yet and
//! errors fatally, aborting its own state and with it the whole pipeline's
//! transition. The input recovers on its own moments later — only the
//! pipeline's state stays poisoned, so `start()` re-drives the transition.
//!
//! `identity error-after=1` stands in for the doomed parser: it errors on the
//! first buffer, pushed during preroll, then stops being a problem, exactly as
//! decodebin dropping the doomed chain does.
//!
//! The opposite case — a pipeline no retry can rescue must still fail — is
//! guarded by `pipeline_start_failure_test.rs`.

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, Link};
use tempfile::NamedTempFile;

fn build_flow(name: &str, error_after: i64) -> Flow {
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
                strom_types::PropertyValue::Int(error_after),
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

fn manager_for(flow: &Flow) -> PipelineManager {
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    PipelineManager::new(
        flow,
        EventBroadcaster::new(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("Failed to create PipelineManager")
}

/// The guard. `identity` poisons the first transition; the retry inside
/// `start()` must drive the pipeline to PLAYING anyway.
///
/// This fails without the retry loop: the first attempt returns
/// Err(StateChangeError) and `start()` propagates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_recovers_from_a_transient_startup_error() {
    gstreamer::init().unwrap();

    let flow = build_flow("transient_error_recovery", 1);
    let mut manager = manager_for(&flow);

    let state = manager
        .start()
        .expect("start() gave up on a pipeline that a retry recovers");

    assert_eq!(
        state,
        strom_types::PipelineState::Playing,
        "pipeline should be PLAYING after the retry"
    );

    manager.stop().expect("Failed to stop pipeline");
}
