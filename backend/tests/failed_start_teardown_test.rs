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

use std::collections::HashMap;
use strom::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

const MEDIA_PLAYER_BLOCK_ID: &str = "player";

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

/// The guard. Without the shared teardown on the failed-start path the flow's
/// Media Player stays in the registry, holding its internal pipeline open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_unregisters_the_flows_media_players() {
    gstreamer::init().unwrap();

    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    );

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
