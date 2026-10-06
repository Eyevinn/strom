//! Regression test: a flow that fails to start must release what a stopped
//! flow releases.
//!
//! `start_flow()` can fail in four places, and each used to clean up its own
//! subset. The worst gap was the Media Player registry: it holds an `Arc` per
//! instance, and that `Arc`'s `Drop` is the only thing that takes the block's
//! *internal* pipeline to NULL. A Media Player flow that failed to start left a
//! decoding pipeline — and its file descriptors — running for the life of the
//! process, once per attempt.
//!
//! The registry is the part of that teardown a test can observe from outside;
//! the bus watch, the thread-priority handler and the CPU allocation are
//! released on the same path, by the same call.
//!
//! Removing the registry entry is not the same as freeing what it stands for,
//! so the second test asserts the internal pipeline itself is gone. It takes
//! the stop path rather than the failed-start path because that is the only
//! one from which a test can hold the pipeline's `WeakRef` — both run the same
//! `teardown_flow()`.
//!
//! `vision_mixer_overlay` covers the other thing a failed start used to leak:
//! the vision mixer's overlay timer thread.

use gstreamer::prelude::ObjectExt;
use std::collections::HashMap;
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

const MEDIA_PLAYER_BLOCK_ID: &str = "player";

/// An `AppState` on throwaway storage. The temp files back its flow and block
/// storage and must outlive it.
fn new_app_state() -> (AppState, [NamedTempFile; 2]) {
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    (state, [storage_file, blocks_file])
}

/// A flow with a Media Player block and a chain that can never reach PLAYING.
///
/// `fakesink state-error=paused-to-playing` fails that transition on every
/// attempt, so the start fails for a reason that has nothing to do with the
/// Media Player — which is the point. The block registered itself while the
/// pipeline was being built, and the failing start has to undo that.
fn build_flow_that_cannot_start(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: HashMap::new(),
        position: [100.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "state-error".to_string(),
                PropertyValue::String("paused-to-playing".to_string()),
            );
            p
        },
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "sink:sink".to_string(),
    });

    // Empty playlist: the block still builds its internal pipeline and
    // registers itself, which is all this test needs from it.
    flow.blocks.push(strom_types::BlockInstance {
        id: MEDIA_PLAYER_BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "playlist".to_string(),
                PropertyValue::String("[]".to_string()),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 400.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    flow
}

/// The same flow, minus the failing state change: this one reaches PLAYING.
fn build_flow_that_starts(name: &str) -> Flow {
    let mut flow = build_flow_that_cannot_start(name);
    for element in &mut flow.elements {
        if element.id == "sink" {
            element.properties.remove("state-error");
        }
    }
    flow
}

/// The guard. Without the shared teardown on the failed-start path the flow's
/// Media Player stays in the registry, holding its internal pipeline open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_unregisters_the_flows_media_players() {
    gstreamer::init().unwrap();

    let (state, _files) = new_app_state();

    let flow = build_flow_that_cannot_start("failed_start_teardown");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");

    let key = MediaPlayerKey {
        flow_id,
        block_id: MEDIA_PLAYER_BLOCK_ID.to_string(),
    };

    let result = state.start_flow(&flow_id).await;
    assert!(
        result.is_err(),
        "start_flow reported {:?} for a pipeline that can never reach PLAYING — \
         the test bed is wrong, not the teardown",
        result
    );

    assert!(
        !MEDIA_PLAYER_REGISTRY.contains(&key),
        "the flow's Media Player is still registered after a failed start. The \
         registry holds the only Arc whose Drop takes the block's internal \
         pipeline to NULL, so that pipeline and its file descriptors now run \
         for the life of the process — once per start attempt."
    );

    // Nothing was inserted into the live pipeline map either: a flow that
    // failed to start must not look startable-again-only-after-stop.
    assert!(
        !state.pipelines_read().await.contains_key(&flow_id),
        "a flow that failed to start is still in the pipeline map"
    );
}

/// The resource the registry entry stands for.
///
/// Unregistering drops the registry's `Arc`, but the signal watch on the
/// internal bus holds another one — and the closure behind it owns the state
/// that owns the pipeline that owns the bus. Until that handler is
/// disconnected, `MediaPlayerState::drop` never runs: the internal pipeline
/// keeps decoding, and keeps its file descriptors, for the life of the
/// process, once per start.
///
/// The registry assertion above cannot see that, since the entry is gone
/// either way. This one holds a `WeakRef` to the pipeline across teardown, so
/// it fails if the explicit shutdown is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_releases_the_media_players_internal_pipeline() {
    gstreamer::init().unwrap();

    let (state, _files) = new_app_state();

    let flow = build_flow_that_starts("teardown_releases_internal_pipeline");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");

    state.start_flow(&flow_id).await.expect("start_flow failed");

    let key = MediaPlayerKey {
        flow_id,
        block_id: MEDIA_PLAYER_BLOCK_ID.to_string(),
    };
    let player = MEDIA_PLAYER_REGISTRY
        .get(&key)
        .expect("the Media Player did not register while the flow was running");
    let internal_pipeline = player
        .internal_pipeline
        .read()
        .expect("internal pipeline lock poisoned")
        .as_ref()
        .expect("the Media Player built no internal pipeline")
        .downgrade();
    drop(player);

    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    // The pipeline is dropped inside teardown, but a queued bus message can
    // hold the last reference for a moment after the call returns. Give it a
    // bounded window: a reference kept by a live signal handler never goes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while internal_pipeline.upgrade().is_some() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(
        internal_pipeline.upgrade().is_none(),
        "the Media Player's internal pipeline survived teardown. Its bus watch \
         still holds an Arc on the state that owns it, so the state is never \
         dropped and the pipeline decodes on — with its file descriptors — for \
         the life of the process."
    );
}

/// Regression test for the overlay timer threads that survived a failed flow
/// start (issue #727).
///
/// `start_flow` installs the bus watch — and with it the element-setup closures
/// that spawn `overlay-timer-*` — before any state change, so a pipeline that
/// fails on its way to PLAYING has already started the thread. That thread's
/// only exit condition is its renderer leaving the overlay registry, and that
/// unregistration used to live in `stop_flow` alone, on a path a failed start
/// never reaches. Each survivor rendered the overlay clock at full framerate
/// for the life of the process: six of them were measured burning ~6.5 CPU
/// hours between them, with no flow running.
mod vision_mixer_overlay {
    use super::*;
    use std::time::{Duration, Instant};
    use strom::blocks::builtin::vision_mixer::overlay;

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

        let (state, _files) = new_app_state();

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
            overlay::get_overlay_state(&flow_id, BLOCK_ID).is_none(),
            "overlay state survived a failed start — the API still sees a block \
             whose pipeline is gone"
        );
        assert!(
            overlay::get_overlay_renderer(&flow_id, BLOCK_ID).is_none(),
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
}
