//! Regression tests for `POST /api/flows` discarding the client-supplied id.
//!
//! `create_flow` used to run `flow.id = FlowId::new_v4()` unconditionally, even
//! though `id` is a required field of the request schema. A caller that
//! pre-generated an id could not create a flow and then start it by that id —
//! it had to read the assigned id back out of the response first.
//!
//! These tests call `create_flow` directly, so reverting the fix in
//! `backend/src/api/flows.rs` turns `creates_flow_with_the_supplied_id` red.

use axum::extract::State;
use axum::http::StatusCode;
use strom::api::flows::create_flow;
use strom::json_rejection::JsonBody;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::Flow;
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

/// The id the caller sends is the id the flow gets, and the id it is stored
/// under. This is the assertion that fails if the unconditional overwrite
/// returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creates_flow_with_the_supplied_id() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = Flow::new("supplied-id");
    let chosen = Flow::new("scratch").id; // a fresh, known uuid
    flow.id = chosen;

    let (status, body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect("create_flow should succeed");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body.0.flow.id, chosen,
        "the server must keep the id the caller supplied"
    );
    assert!(
        state.get_flow(&chosen).await.is_some(),
        "the flow must be retrievable by the supplied id"
    );
}

/// Reusing an existing id is a conflict, not a silent overwrite of the other
/// flow. Before this change the second create simply got a different id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_a_duplicate_id_with_conflict() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut first = Flow::new("first");
    let shared = Flow::new("scratch").id;
    first.id = shared;
    let _first = create_flow(State(state.clone()), JsonBody(first))
        .await
        .expect("first create should succeed");

    let mut second = Flow::new("second");
    second.id = shared;
    let err = create_flow(State(state.clone()), JsonBody(second))
        .await
        .expect_err("a duplicate id must be rejected");

    assert_eq!(err.0, StatusCode::CONFLICT);

    let stored = state
        .get_flow(&shared)
        .await
        .expect("the original flow must still exist");
    assert_eq!(
        stored.name, "first",
        "the conflicting create must not have overwritten the original flow"
    );
}

/// A nil uuid means "no id supplied" — the server assigns one rather than
/// storing a flow keyed on all-zeros.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assigns_an_id_when_the_caller_sends_nil() {
    gstreamer::init().unwrap();
    let state = new_state();

    let mut flow = Flow::new("nil-id");
    flow.id = Default::default(); // uuid nil
    assert!(
        flow.id.is_nil(),
        "precondition: the request carries a nil id"
    );

    let (status, body) = create_flow(State(state.clone()), JsonBody(flow))
        .await
        .expect("create_flow should succeed");

    assert_eq!(status, StatusCode::CREATED);
    assert!(
        !body.0.flow.id.is_nil(),
        "a nil id must be replaced with a generated one"
    );
    assert!(state.get_flow(&body.0.flow.id).await.is_some());
}
