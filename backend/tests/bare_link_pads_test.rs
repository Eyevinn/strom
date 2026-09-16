//! Regression tests for links being silently discarded by `POST /api/flows`.
//!
//! `is_pad_valid` used to reject any pad reference without a colon, and
//! `prepare_flow` then dropped those links with `flow.links.retain(...)`. A
//! caller that posted the documented bare form (`{"from": "src0", "to":
//! "caps0"}`) got 201 Created and a stored flow with `"links": []`, and only
//! found out when starting the flow failed with a GStreamer "not-linked" error.
//!
//! These tests call `create_flow` and `update_flow` directly, so reverting the
//! fix in `backend/src/api/flows.rs` turns them red.

use axum::extract::State;
use axum::http::StatusCode;
use std::collections::HashMap;
use strom::api::flows::{create_flow, update_flow};
use strom::json_rejection::JsonBody;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Element, Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

fn new_state() -> AppState {
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let storage = JsonFileStorage::new(storage_file.path());
    AppState::new(
        storage,
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    )
}

fn element(id: &str, element_type: &str, x: f32) -> Element {
    Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties: HashMap::new(),
        pad_properties: HashMap::new(),
        position: (x, 0.0),
    }
}

fn compositor(id: &str, num_inputs: u64, x: f32) -> BlockInstance {
    let mut properties = HashMap::new();
    properties.insert("num_inputs".to_string(), PropertyValue::UInt(num_inputs));
    BlockInstance {
        id: id.to_string(),
        block_definition_id: "builtin.compositor".to_string(),
        name: None,
        properties,
        position: Position { x, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

/// A flow of two elements wired with the bare form documented on `Link`.
fn bare_link_flow(name: &str) -> Flow {
    let mut flow = Flow::new(name);
    flow.elements.push(element("src0", "videotestsrc", 0.0));
    flow.elements.push(element("caps0", "capsfilter", 200.0));
    flow.links.push(Link {
        from: "src0".to_string(),
        to: "caps0".to_string(),
    });
    flow
}

/// The bug: a bare element id is a documented link form, and it must survive
/// being stored. This is the assertion that fails if the fix is reverted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_flow_keeps_links_written_with_bare_element_ids() {
    gstreamer::init().unwrap();
    let state = new_state();

    let flow = bare_link_flow("bare-links");
    let id = flow.id;

    let (status, body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect("a flow with bare links must be accepted");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body.0.flow.links.len(),
        1,
        "the response must carry the link the caller sent"
    );

    let stored = state.get_flow(&id).await.expect("flow must be stored");
    assert_eq!(
        stored.links.len(),
        1,
        "the stored flow must carry the link the caller sent"
    );
    assert_eq!(stored.links[0].from, "src0");
    assert_eq!(stored.links[0].to, "caps0");
}

/// The same form must survive an update, which runs the same preparation step.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_flow_keeps_links_written_with_bare_element_ids() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = bare_link_flow("bare-links-update");
    let id = flow.id;
    flow.links.clear();

    let _created = create_flow(State(state.clone()), JsonBody(flow.clone()))
        .await
        .expect("create should succeed");

    flow.links.push(Link {
        from: "src0".to_string(),
        to: "caps0".to_string(),
    });
    let body = update_flow(
        State(state.clone()),
        axum::extract::Path(id),
        JsonBody(flow),
    )
    .await
    .expect("an update with bare links must be accepted");

    assert_eq!(body.0.flow.links.len(), 1);
    let stored = state.get_flow(&id).await.expect("flow must be stored");
    assert_eq!(
        stored.links.len(),
        1,
        "the update must not discard the link the caller sent"
    );
}

/// A link the server cannot use must fail the request, naming the link. The
/// old behaviour was 201 Created with the link quietly removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_flow_rejects_a_link_to_an_unknown_node() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = bare_link_flow("unknown-node");
    let id = flow.id;
    flow.links.push(Link {
        from: "caps0:src".to_string(),
        to: "ghost:sink".to_string(),
    });

    let (status, body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect_err("a link naming an unknown element must be rejected");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rendered = format!("{:?}", body.0);
    assert!(
        rendered.contains("ghost:sink"),
        "the error must name the offending link, got: {}",
        rendered
    );
    assert!(
        state.get_flow(&id).await.is_none(),
        "a rejected flow must not be stored"
    );
}

/// Update has the same contract: no silent trimming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_flow_rejects_a_link_to_an_unknown_node() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = bare_link_flow("unknown-node-update");
    let id = flow.id;
    let _created = create_flow(State(state.clone()), JsonBody(flow.clone()))
        .await
        .expect("create should succeed");

    flow.links.push(Link {
        from: "caps0:src".to_string(),
        to: "ghost:sink".to_string(),
    });
    let (status, _body) = update_flow(
        State(state.clone()),
        axum::extract::Path(id),
        JsonBody(flow),
    )
    .await
    .expect_err("a link naming an unknown element must be rejected");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let stored = state.get_flow(&id).await.expect("flow must still exist");
    assert_eq!(
        stored.links.len(),
        1,
        "the rejected update must not have replaced the stored flow"
    );
}

/// A bare reference to a block resolves to that block's only pad on that side,
/// so it is stored in the explicit form the pipeline builder resolves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_flow_resolves_a_bare_block_reference_to_its_only_pad() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = Flow::new("bare-block");
    let id = flow.id;
    flow.elements.push(element("src0", "videotestsrc", 0.0));
    flow.elements.push(element("sink0", "fakesink", 400.0));
    // One input, one output: both sides are unambiguous.
    flow.blocks.push(compositor("b0", 1, 200.0));
    flow.links.push(Link {
        from: "src0:src".to_string(),
        to: "b0".to_string(),
    });
    flow.links.push(Link {
        from: "b0".to_string(),
        to: "sink0:sink".to_string(),
    });

    let _created = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect("bare block references must be accepted");

    let stored = state.get_flow(&id).await.expect("flow must be stored");
    assert_eq!(stored.links.len(), 2, "both links must be stored");
    assert_eq!(
        stored.links[0].to, "b0:video_in_0",
        "a bare block on the destination side resolves to its only input pad"
    );
    assert_eq!(
        stored.links[1].from, "b0:video_out",
        "a bare block on the source side resolves to its only output pad"
    );
}

/// An ambiguous bare block reference is a caller error, not something to guess
/// at or drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_flow_rejects_an_ambiguous_bare_block_reference() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = Flow::new("ambiguous-block");
    let id = flow.id;
    flow.elements.push(element("src0", "videotestsrc", 0.0));
    flow.blocks.push(compositor("b0", 2, 200.0));
    flow.links.push(Link {
        from: "src0:src".to_string(),
        to: "b0".to_string(),
    });

    let (status, body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect_err("a bare reference to a multi-input block must be rejected");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rendered = format!("{:?}", body.0);
    assert!(
        rendered.contains("video_in_0") && rendered.contains("video_in_1"),
        "the error must name the pads to choose from, got: {}",
        rendered
    );
    assert!(state.get_flow(&id).await.is_none());
}

/// Pruning still has a job: a block's pads follow its properties, so a link to
/// a pad the block no longer produces is drift rather than a caller mistake.
/// That link is dropped and the request succeeds - the case the old blanket
/// prune existed for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_flow_prunes_a_link_to_a_pad_the_block_no_longer_has() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = Flow::new("stale-block-pad");
    let id = flow.id;
    flow.elements.push(element("src0", "videotestsrc", 0.0));
    flow.elements.push(element("src1", "videotestsrc", 0.0));
    // Two inputs, so video_in_2 belongs to an older, wider compositor.
    flow.blocks.push(compositor("b0", 2, 200.0));
    flow.links.push(Link {
        from: "src0:src".to_string(),
        to: "b0:video_in_0".to_string(),
    });
    flow.links.push(Link {
        from: "src1:src".to_string(),
        to: "b0:video_in_2".to_string(),
    });

    let (status, _body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect("a stale block pad must not fail the request");

    assert_eq!(status, StatusCode::CREATED);
    let stored = state.get_flow(&id).await.expect("flow must be stored");
    assert_eq!(
        stored.links.len(),
        1,
        "only the link to the vanished pad is dropped"
    );
    assert_eq!(stored.links[0].to, "b0:video_in_0");
}
