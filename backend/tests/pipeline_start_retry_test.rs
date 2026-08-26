//! Regression test: a pipeline whose transition to PLAYING is poisoned by a
//! transient startup error must recover, not stay dead — and "recovered" has to
//! mean data moves, not just that the state reads PLAYING.
//!
//! An MPEG-TS feed that re-signals its PMT makes tsdemux drain the previous
//! program, pushing EOS out of the pad it removes. The parser decodebin
//! autoplugged for that short-lived pad has no complete access unit yet and
//! errors fatally, aborting its own state and with it the whole pipeline's
//! transition. The input recovers on its own moments later — decodebin drops
//! the doomed chain and exposes the new program's pads — so only the pipeline's
//! state stays poisoned and `start()` re-drives the transition.
//!
//! `identity error-after=1` stands in for the doomed parser: it errors on the
//! first buffer, pushed during preroll, then stops being a problem. The
//! recovering flow puts that doomed chain *alongside* a healthy one, which is
//! the shape of the real failure: the graph keeps a live data path across the
//! transient error. A single doomed chain has no such path — the source pauses
//! its task and pushes EOS, the EOS prerolls the sink, and the re-drive then
//! reports Success on a pipeline that never carries another buffer. That case
//! must fail, and is guarded below.
//!
//! The persistent case — a pipeline no retry can rescue must still fail — is
//! guarded by `pipeline_start_failure_test.rs`.

use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, Link};
use tempfile::NamedTempFile;

fn element(id: &str, element_type: &str, x: f32) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties: HashMap::new(),
        position: [x, 200.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// `audiotestsrc -> identity error-after=N -> fakesink`: one chain, and the
/// error takes it down with it.
fn build_doomed_flow(name: &str, error_after: i64) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(element("src", "audiotestsrc", 100.0));
    let mut fail = element("fail", "identity", 250.0);
    fail.properties.insert(
        "error-after".to_string(),
        strom_types::PropertyValue::Int(error_after),
    );
    flow.elements.push(fail);
    flow.elements.push(element("sink", "fakesink", 400.0));

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

/// The doomed chain plus a healthy one, both in the same pipeline. The error
/// still aborts the pipeline's transition, but the graph keeps a live data path
/// — so the re-drive has something real to recover.
fn build_recovering_flow(name: &str) -> Flow {
    let mut flow = build_doomed_flow(name, 1);

    flow.elements
        .push(element("src_live", "audiotestsrc", 100.0));
    flow.elements.push(element("sink_live", "fakesink", 400.0));
    flow.links.push(Link {
        from: "src_live:src".to_string(),
        to: "sink_live:sink".to_string(),
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

/// Count buffers arriving at one named sink element over `window`.
fn buffers_at(manager: &PipelineManager, sink_name: &str, window: std::time::Duration) -> u64 {
    let element = manager
        .pipeline()
        .by_name(sink_name)
        .unwrap_or_else(|| panic!("no element named '{}' in the pipeline", sink_name));
    let pad = element.static_pad("sink").expect("sink pad");

    let count = Arc::new(AtomicU64::new(0));
    let counter = count.clone();
    let probe = pad
        .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gstreamer::PadProbeReturn::Ok
        })
        .expect("failed to attach probe");

    std::thread::sleep(window);
    let seen = count.load(Ordering::Relaxed);
    pad.remove_probe(probe);
    seen
}

/// The guard. `identity` poisons the first transition; the retry inside
/// `start()` must drive the pipeline to PLAYING anyway *and* the surviving
/// chain must still be passing buffers afterwards.
///
/// This fails without the retry loop: the first attempt returns
/// Err(StateChangeError) and `start()` propagates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_recovers_from_a_transient_startup_error() {
    gstreamer::init().unwrap();

    let flow = build_recovering_flow("transient_error_recovery");
    let mut manager = manager_for(&flow);

    let state = manager
        .start()
        .expect("start() gave up on a pipeline that a retry recovers");

    assert_eq!(
        state,
        strom_types::PipelineState::Playing,
        "pipeline should be PLAYING after the retry"
    );

    // The point of the recovery: data moves again. Without this the test is
    // satisfied by a pipeline that reads PLAYING and carries nothing.
    let seen = buffers_at(&manager, "sink_live", std::time::Duration::from_millis(300));
    assert!(
        seen > 0,
        "no buffers reached the surviving sink after the retry — the pipeline \
         reads PLAYING but its data path is dead"
    );

    manager.stop().expect("Failed to stop pipeline");
}

/// The other half of the guard: when the transient error kills the *only* data
/// path, the re-driven transition still reports Success — the source pushed EOS
/// when it took the flow error, and that EOS prerolled the sink. `start()` must
/// not report such a pipeline as started, or `start_flow()` goes on to register
/// its WHEP endpoints, mark it auto-restart and leave a flow that produces
/// nothing for the rest of the process's life.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_fails_when_the_transient_error_killed_the_only_data_path() {
    gstreamer::init().unwrap();

    let flow = build_doomed_flow("transient_error_no_data_path", 1);
    let mut manager = manager_for(&flow);

    let result = manager.start();

    assert!(
        result.is_err(),
        "start() reported {:?} for a pipeline whose only data path is EOS",
        result
    );

    manager.stop().expect("Failed to stop pipeline");
}
