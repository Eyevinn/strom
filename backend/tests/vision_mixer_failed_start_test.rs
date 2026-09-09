//! Regression test for the overlay timer threads that survived a failed flow
//! start (issue #727).
//!
//! `start_flow` installs the bus watch — and with it the element-setup closures
//! that spawn `overlay-timer-*` — before any state change, so a pipeline that
//! fails on its way to PLAYING has already started the thread. That thread's
//! only exit condition is its renderer leaving the overlay registry, and that
//! unregistration used to live in `stop_flow` alone, on a path a failed start
//! never reaches. Each survivor rendered the overlay clock at full framerate
//! for the life of the process: six of them were measured burning ~6.5 CPU
//! hours between them, with no flow running.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::vision_mixer::overlay;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vm-failed-start";

/// A vision mixer that builds and starts fine, plus an unrelated branch that
/// refuses to go PLAYING. `fakesink state-error=paused-to-playing` fails that
/// one transition; `async=false` keeps it from waiting for a preroll it would
/// otherwise never get, so the failure actually happens. Both elements are core
/// GStreamer, and the CPU compositor backend needs no GL context — this runs
/// anywhere CI does.
///
/// The vision mixer's inputs are left unlinked, as in `vision_mixer_fx_test`:
/// force-live compositors output regardless.
fn build_failing_vm_flow(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PropertyValue::String("cpu".to_string()),
            );
            p.insert("num_inputs".to_string(), PropertyValue::UInt(2));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert("is-live".to_string(), PropertyValue::Bool(true));
            p
        },
        position: [100.0, 400.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert("async".to_string(), PropertyValue::Bool(false));
            p.insert(
                "state-error".to_string(),
                PropertyValue::String("paused-to-playing".to_string()),
            );
            p
        },
        position: [400.0, 400.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow
}

/// A flow start that fails must leave no overlay behind: no registry entry (the
/// API would keep serving a block whose pipeline is gone) and no timer thread.
///
/// Both assertions are needed. The registry check is what proves the teardown
/// path actually unregisters; the thread check alone would pass on the timer's
/// orphaned-appsrc backstop, which fires once the pipeline finalizes whether or
/// not anything unregistered it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_leaves_no_overlay_timer() {
    gstreamer::init().unwrap();
    // The vision mixer's converters ask for the detected GPU mode, which panics
    // if nothing has probed for it — `main` does this at startup.
    strom::gpu::detect_gpu_capabilities();

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

    let timers_before = overlay::overlay_timers_running();

    let flow = build_failing_vm_flow("vm_failed_start_test");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");

    let started = state.start_flow(&flow_id).await;
    assert!(
        started.is_err(),
        "test setup is wrong: the flow was expected to fail its PLAYING \
         transition, but start_flow returned {:?}",
        started
    );

    assert!(
        overlay::get_overlay_state(BLOCK_ID).is_none(),
        "overlay state survived a failed start — the API still sees a block \
         whose pipeline is gone"
    );
    assert!(
        overlay::get_overlay_renderer(BLOCK_ID).is_none(),
        "overlay renderer survived a failed start — this is what leaves the \
         overlay-timer-* thread with no exit condition"
    );

    // The thread notices at its next tick, so give it a moment. It also has to
    // outlive nothing else: a survivor here is permanent.
    let deadline = Instant::now() + Duration::from_secs(5);
    while overlay::overlay_timers_running() > timers_before && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        overlay::overlay_timers_running(),
        timers_before,
        "an overlay-timer-* thread outlived the flow that created it — it \
         renders at full framerate for the life of the process"
    );
}
