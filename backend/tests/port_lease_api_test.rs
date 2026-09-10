//! The port lease API end to end: allocation through the router, persistence
//! across a restart, and the guard that keeps a lease off ports a flow
//! already listens on.

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use strom::create_app_with_state;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Flow, PortLease, PortRange, PropertyValue};
use tempfile::TempDir;
use tower::ServiceExt;

fn new_state(dir: &TempDir) -> AppState {
    AppState::new(
        JsonFileStorage::new(dir.path().join("flows.json")),
        dir.path().join("blocks.json"),
        dir.path(),
        vec![],
        "all".to_string(),
        vec![],
    )
}

async fn app_with_pool(state: &AppState, first: u16, last: u16) -> Router {
    state
        .set_port_lease_range(PortRange::new(first, last).unwrap())
        .await;
    state.load_from_storage().await.unwrap();
    create_app_with_state(state.clone()).await
}

async fn call(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

fn lease(v: &Value) -> PortLease {
    serde_json::from_value(v.clone()).unwrap()
}

fn srt_listener_flow(port: u16) -> Flow {
    let mut flow = Flow::new("ingest");
    let mut properties = HashMap::new();
    properties.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://:{port}?mode=listener")),
    );
    flow.blocks.push(BlockInstance {
        id: "srt_in".to_string(),
        block_definition_id: "builtin.mpegtssrt_input".to_string(),
        name: None,
        properties,
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_are_allocated_per_client_and_idempotent() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir);
    let app = app_with_pool(&state, 47100, 47139).await;

    let (status, a) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-a", "size": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{a}");
    let a = lease(&a);
    assert_eq!((a.first_port, a.last_port), (47100, 47119));

    // The same client asking again gets the same block back, renewed.
    let (status, again) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-a", "size": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lease(&again).id, a.id);

    let (status, b) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-b", "size": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let b = lease(&b);
    assert_eq!((b.first_port, b.last_port), (47120, 47139));

    // The pool is now full.
    let (status, err) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-c", "size": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");

    let (status, list) = call(&app, Method::GET, "/api/port-leases", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // Renew with and without a body.
    let renew_uri = format!("/api/port-leases/{}/renew", a.id);
    let (status, renewed) = call(&app, Method::POST, &renew_uri, None).await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    let (status, renewed) = call(
        &app,
        Method::POST,
        &renew_uri,
        Some(json!({"ttl_secs": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{renewed}");
    assert_eq!(lease(&renewed).id, a.id);

    // Release b and the block is free again.
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/port-leases/{}", b.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        Method::GET,
        &format!("/api/port-leases/{}", b.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, c) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-c", "size": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(lease(&c).first_port, 47120);

    // Invalid input is a 400, not a 500.
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "", "size": 20})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leases_survive_a_restart_and_avoid_ports_flows_listen_on() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();

    let first_lease = {
        let state = new_state(&dir);
        let app = app_with_pool(&state, 47100, 47119).await;
        // A flow already listens on 47100: the block must start after it.
        state.upsert_flow(srt_listener_flow(47100)).await.unwrap();
        let (status, a) = call(
            &app,
            Method::POST,
            "/api/port-leases",
            Some(json!({"client_id": "open-live-a", "size": 5})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{a}");
        let a = lease(&a);
        assert_eq!((a.first_port, a.last_port), (47101, 47105));
        a
    };

    // A new process over the same data directory still knows the lease, so
    // the client gets its block back rather than a new one.
    let state = new_state(&dir);
    let app = app_with_pool(&state, 47100, 47119).await;
    let (status, got) = call(
        &app,
        Method::GET,
        &format!("/api/port-leases/{}", first_lease.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(lease(&got), first_lease);

    let (status, again) = call(
        &app,
        Method::POST,
        "/api/port-leases",
        Some(json!({"client_id": "open-live-a", "size": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(lease(&again).first_port, 47101);
}
