//! Permanent test verifying that pipeline lifecycle cleanup works correctly.
//!
//! Creates a real flow with PipelineManager, starts it, stops it, drops it,
//! and asserts that all GStreamer objects (pipeline + elements) are fully
//! finalized — no leaked references, no leaked OS resources.

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link};
use tempfile::NamedTempFile;

/// Build a simple flow: audiotestsrc → fakesink
/// Uses only core GStreamer elements to work in CI without plugins-good.
fn build_test_flow(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "is-live".to_string(),
                strom_types::PropertyValue::Bool(true),
            );
            p
        },
        position: [100.0, 200.0].into(),
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
        to: "sink:sink".to_string(),
    });

    flow
}

/// Verify that stopping and dropping a PipelineManager fully finalizes the
/// GStreamer pipeline and all its elements. Any surviving GObject means a
/// strong reference cycle that will leak OS resources (sockets, threads).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pipeline_cleanup_after_stop_and_drop() {
    gstreamer::init().unwrap();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let flow = build_test_flow("lifecycle_test");

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

    let state = manager.start().expect("Failed to start pipeline");
    assert_eq!(state, strom_types::PipelineState::Playing);

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Take weak refs before stop+drop
    let pipeline_weak = manager.pipeline_weak();
    let element_weak_refs = manager.element_weak_refs();
    assert!(!element_weak_refs.is_empty(), "Flow should have elements");
    assert!(
        pipeline_weak.upgrade().is_some(),
        "Pipeline should be alive before drop"
    );

    // Stop and drop — mirrors what stop_flow() does
    manager.stop().expect("Failed to stop pipeline");
    drop(manager);

    // Verify pipeline is fully finalized
    assert!(
        pipeline_weak.upgrade().is_none(),
        "Pipeline still alive after drop — circular reference prevents finalization"
    );

    let leaked: Vec<_> = element_weak_refs
        .iter()
        .filter_map(|(name, weak)| weak.upgrade().map(|_| name.clone()))
        .collect();

    assert!(
        leaked.is_empty(),
        "Elements still alive after drop: {:?}",
        leaked
    );
}

/// Negative test: intentionally create a circular reference by capturing a
/// strong pipeline clone in a signal handler closure. Verify that our weak
/// ref detection catches the leak. If this test ever fails (i.e. no leak
/// detected), it means the detection mechanism is broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_leak_detection_catches_circular_reference() {
    use gstreamer::prelude::*;

    gstreamer::init().unwrap();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let flow = build_test_flow("leak_detection_test");

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

    // Intentionally create a circular reference: connect a signal handler
    // on an element that captures a strong ref to the pipeline.
    let pipeline_strong = manager.pipeline().clone();
    let src = manager.pipeline().by_name("src").expect("src element");
    src.connect_pad_added(move |_elem, _pad| {
        // This closure captures pipeline_strong, creating:
        // pipeline -> src element -> signal handler -> pipeline
        let _ = &pipeline_strong;
    });

    manager.start().expect("Failed to start pipeline");
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let pipeline_weak = manager.pipeline_weak();

    manager.stop().expect("Failed to stop pipeline");
    drop(manager);

    // The pipeline SHOULD still be alive because of the circular reference.
    // If it's not alive, our detection mechanism would miss real leaks.
    assert!(
        pipeline_weak.upgrade().is_some(),
        "Pipeline was finalized despite circular reference — \
         leak detection would miss real leaks!"
    );
}

/// Regression test for the delete-without-stop leak: deleting a flow that has
/// an active pipeline must fully release the pipeline (encoders, sockets,
/// element refs). Before the fix in delete_flow, the PipelineManager stayed
/// in AppState.pipelines after deletion and orphaned NVENC sessions / sockets
/// would accumulate until process exit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_delete_running_flow_releases_pipeline() {
    gstreamer::init().unwrap();

    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let storage = JsonFileStorage::new(storage_file.path());

    let state = AppState::new(
        storage,
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    );

    let flow = build_test_flow("delete_running_flow_test");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");

    state.start_flow(&flow_id).await.expect("start_flow failed");

    // Capture weak refs to the running pipeline before deletion
    let (pipeline_weak, element_weak_refs) = {
        let pipelines = state.pipelines_read().await;
        let manager = pipelines
            .get(&flow_id)
            .expect("Pipeline should be active after start_flow");
        (manager.pipeline_weak(), manager.element_weak_refs())
    };
    assert!(
        pipeline_weak.upgrade().is_some(),
        "Pipeline should be alive before delete"
    );
    assert!(
        !element_weak_refs.is_empty(),
        "Pipeline should have elements"
    );

    // Delete WITHOUT calling stop_flow first — this is the path the UI takes
    let deleted = state
        .delete_flow(&flow_id)
        .await
        .expect("delete_flow failed");
    assert!(deleted, "delete_flow should report the flow was deleted");

    // Pipeline must no longer be tracked in AppState
    {
        let pipelines = state.pipelines_read().await;
        assert!(
            !pipelines.contains_key(&flow_id),
            "Pipeline still in AppState.pipelines after delete — orphaned manager"
        );
    }

    // And every GStreamer object must be finalized — surviving refs would
    // mean a leaked encoder / socket / thread that no API path can release.
    assert!(
        pipeline_weak.upgrade().is_none(),
        "Pipeline still alive after delete_flow — resources leaked"
    );
    let leaked: Vec<_> = element_weak_refs
        .iter()
        .filter_map(|(name, weak)| weak.upgrade().map(|_| name.clone()))
        .collect();
    assert!(
        leaked.is_empty(),
        "Elements still alive after delete_flow: {:?}",
        leaked
    );
}

/// Regression test for the WHIP session-teardown P0: an element removed from
/// its parent bin while it is still above NULL is disposed without ever running
/// its READY→NULL transition, leaking every socket, thread and fd it holds.
///
/// `whipserversrc` removes each session's internal bin from a thread of its own,
/// so the removal races the session pipeline's descent — see
/// `strom::gst::orphan_guard`. This stands the same shape up with core elements
/// and replays the two halves of that race in a fixed order.
#[test]
fn test_removed_child_is_driven_to_null() {
    use gstreamer::prelude::*;

    gstreamer::init().unwrap();

    let pipeline = gstreamer::Pipeline::new();
    // Stand-ins for whipserversrc and the session bin it removes.
    let parent = gstreamer::Bin::builder().name("parent").build();
    let session = gstreamer::Bin::builder().name("session").build();

    let src = gstreamer::ElementFactory::make("fakesrc")
        .property("is-live", true)
        .build()
        .expect("fakesrc is part of gstreamer core");
    let sink = gstreamer::ElementFactory::make("fakesink")
        .build()
        .expect("fakesink is part of gstreamer core");
    session.add_many([&src, &sink]).unwrap();
    src.link(&sink).unwrap();
    parent.add(&session).unwrap();
    pipeline.add(&parent).unwrap();

    strom::gst::orphan_guard::install(&parent);

    pipeline
        .set_state(gstreamer::State::Playing)
        .expect("pipeline should reach Playing");
    let (result, current, _) = pipeline.state(gstreamer::ClockTime::from_seconds(5));
    assert!(result.is_ok(), "pipeline state change failed");
    assert_eq!(current, gstreamer::State::Playing);
    assert_eq!(session.current_state(), gstreamer::State::Playing);

    // A removal that lands before the parent's descent has reached the child.
    parent.remove(&session).unwrap();
    assert_eq!(
        session.current_state(),
        gstreamer::State::Null,
        "removed bin left above NULL — its OS resources are leaked and GStreamer \
         will fire 'Trying to dispose element ... instead of the NULL state'"
    );

    // The other half of the race: the parent's own descent reaches the child
    // after it has gone to NULL. A bin is never skipped for being below the
    // target state, so without the state lock this raises it back to PAUSED and
    // reopens everything it holds. Re-adding makes that deterministic; in
    // production the two threads simply overlap.
    parent.add(&session).unwrap();
    parent.set_state(gstreamer::State::Ready).unwrap();
    assert_eq!(
        session.current_state(),
        gstreamer::State::Null,
        "parent raised a removed bin back out of NULL"
    );

    let session_weak = session.downgrade();
    let src_weak = src.downgrade();
    drop(src);
    drop(sink);

    parent.remove(&session).unwrap();
    pipeline.set_state(gstreamer::State::Null).unwrap();
    drop(session);
    drop(parent);
    drop(pipeline);

    assert!(
        session_weak.upgrade().is_none(),
        "removed bin still alive after drop"
    );
    assert!(
        src_weak.upgrade().is_none(),
        "element inside the removed bin still alive after drop"
    );
}
