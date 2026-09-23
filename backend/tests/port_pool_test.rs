//! The port pool API end to end: reservation through the router, the flow
//! association round trip, persistence across a restart, and what a server
//! with no pool configured answers.

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use strom::auth::AuthConfig;
use strom::create_app_with_state_and_auth;
use strom::ports::PortReservationStore;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::Flow;
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

async fn app_with_pool(
    state: &AppState,
    ports: impl IntoIterator<Item = u16>,
    dir: &TempDir,
) -> Router {
    let ports: BTreeSet<u16> = ports.into_iter().collect();
    state
        .configure_port_pool(ports, PortReservationStore::new(dir.path()), 600, false)
        .await
        .unwrap();
    state.load_from_storage().await.unwrap();
    // Explicitly unauthenticated: `AuthConfig::from_env()` would pick up a
    // STROM_API_KEY from the developer's shell and turn every assertion below
    // into a 401.
    let auth = AuthConfig {
        admin_user: None,
        admin_password_hash: None,
        api_key: None,
        native_gui_token: None,
        enabled: false,
    };
    create_app_with_state_and_auth(state.clone(), auth).await
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

fn ports_of(v: &Value) -> Vec<u16> {
    v["ports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_u64().unwrap() as u16)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reservations_are_per_owner_and_grow_without_moving() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir);
    let app = app_with_pool(&state, 47100..=47119, &dir).await;

    let (status, a) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-a", "count": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{a}");
    assert_eq!(ports_of(&a), (47100..=47109).collect::<Vec<_>>());

    // The same owner asking again gets the same ports back, renewed.
    let (status, again) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-a", "count": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], a["id"]);
    assert_eq!(ports_of(&again), ports_of(&a));

    let (status, b) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-b", "count": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ports_of(&b), (47110..=47114).collect::<Vec<_>>());

    // Growing prod-a keeps every port it already holds.
    let (status, grown) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-a", "count": 15})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&ports_of(&grown)[..10], &ports_of(&a)[..]);
    assert_eq!(
        &ports_of(&grown)[10..],
        &[47115, 47116, 47117, 47118, 47119]
    );

    // The pool is full now.
    let (status, err) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-c", "count": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");

    let (status, list) = call(&app, Method::GET, "/api/ports/reservations", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // Renew with and without a body.
    let renew = format!(
        "/api/ports/reservations/{}/renew",
        a["id"].as_str().unwrap()
    );
    let (status, _) = call(&app, Method::POST, &renew, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&app, Method::POST, &renew, Some(json!({"ttl_secs": 30}))).await;
    assert_eq!(status, StatusCode::OK);

    // Invalid input is a 400, not a 500.
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "", "count": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flow_association_survives_the_reservation_and_ends_with_the_flow() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir);
    let app = app_with_pool(&state, 47100..=47109, &dir).await;

    let flow = Flow::new("production");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.unwrap();

    let (_, a) = call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-a", "count": 5})),
    )
    .await;
    let id = a["id"].as_str().unwrap().to_string();

    // Declare two of them in use by the flow.
    let (status, assigned) = call(
        &app,
        Method::POST,
        &format!("/api/ports/reservations/{id}/assign"),
        Some(json!({"flow_id": flow_id, "ports": [47100, 47101]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{assigned}");
    assert_eq!(assigned["in_use"].as_array().unwrap().len(), 2);

    // A port the reservation does not hold is a 400.
    let (status, err) = call(
        &app,
        Method::POST,
        &format!("/api/ports/reservations/{id}/assign"),
        Some(json!({"flow_id": flow_id, "ports": [47108]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    // Deleting the reservation leaves the in-use ports held, not free.
    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/ports/reservations/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, pool) = call(&app, Method::GET, "/api/ports", None).await;
    assert_eq!(pool["free"], json!(8), "{pool}");
    let assigned_ports: Vec<u16> = pool["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["state"] == "assigned")
        .map(|e| e["port"].as_u64().unwrap() as u16)
        .collect();
    assert_eq!(assigned_ports, vec![47100, 47101]);

    // Once the flow is gone, so are they.
    state.delete_flow(&flow_id).await.unwrap();
    let (_, pool) = call(&app, Method::GET, "/api/ports", None).await;
    assert_eq!(pool["free"], json!(10), "{pool}");
    assert!(pool["entries"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unassigning_returns_ports_to_the_owner_and_survives_a_restart() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();

    let (id, ports) = {
        let state = new_state(&dir);
        let app = app_with_pool(&state, 47100..=47109, &dir).await;
        let flow = Flow::new("production");
        let flow_id = flow.id;
        state.upsert_flow(flow).await.unwrap();

        let (_, a) = call(
            &app,
            Method::POST,
            "/api/ports/reservations",
            Some(json!({"owner_id": "prod-a", "count": 5})),
        )
        .await;
        let id = a["id"].as_str().unwrap().to_string();
        call(
            &app,
            Method::POST,
            &format!("/api/ports/reservations/{id}/assign"),
            Some(json!({"flow_id": flow_id, "ports": [47100]})),
        )
        .await;

        // Dropping the association gives the port back to the reservation,
        // never to the pool.
        let (status, _) = call(
            &app,
            Method::DELETE,
            &format!("/api/ports/reservations/{id}/assign/{flow_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, after) = call(
            &app,
            Method::GET,
            &format!("/api/ports/reservations/{id}"),
            None,
        )
        .await;
        assert_eq!(ports_of(&after), (47100..=47104).collect::<Vec<_>>());
        assert!(after["in_use"].as_array().unwrap().is_empty());
        (id, ports_of(&after))
    };

    // A new process over the same data directory still knows the reservation,
    // so the owner keeps the numbers its peers are configured for.
    let state = new_state(&dir);
    let app = app_with_pool(&state, 47100..=47109, &dir).await;
    let (status, got) = call(
        &app,
        Method::GET,
        &format!("/api/ports/reservations/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(ports_of(&got), ports);
}

/// A Strom nobody configured a pool on is a first-class answer, not a gap: the
/// routes exist and say what is missing, and `GET /api/ports` answers anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfigured_server_answers_409_and_reports_a_disabled_pool() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir);
    let app = app_with_pool(&state, [], &dir).await;

    let id = uuid::Uuid::new_v4();
    let routes = [
        (Method::GET, "/api/ports/reservations".to_string(), None),
        (
            Method::POST,
            "/api/ports/reservations".to_string(),
            Some(json!({"owner_id": "prod-a", "count": 5})),
        ),
        (Method::GET, format!("/api/ports/reservations/{id}"), None),
        (
            Method::POST,
            format!("/api/ports/reservations/{id}/renew"),
            Some(json!({"ttl_secs": 30})),
        ),
        (
            Method::DELETE,
            format!("/api/ports/reservations/{id}"),
            None,
        ),
        (
            Method::POST,
            format!("/api/ports/reservations/{id}/assign"),
            Some(json!({"flow_id": uuid::Uuid::new_v4(), "ports": [47100]})),
        ),
        (
            Method::DELETE,
            format!(
                "/api/ports/reservations/{id}/assign/{}",
                uuid::Uuid::new_v4()
            ),
            None,
        ),
    ];
    for (method, uri, body) in routes {
        let (status, err) = call(&app, method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {err}");
        // The body names the setting to change, not just the failure.
        let message = err["error"].as_str().unwrap_or_default();
        assert!(message.contains("STROM_PORTS"), "{method} {uri}: {message}");
    }

    let (status, pool) = call(&app, Method::GET, "/api/ports", None).await;
    assert_eq!(status, StatusCode::OK, "{pool}");
    assert_eq!(pool["enabled"], json!(false));
    assert_eq!(pool["total"], json!(0));
    assert!(pool["entries"].as_array().unwrap().is_empty());
}

/// The pool view reports runs, not nine hundred entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pool_view_compacts_runs_and_lists_only_what_is_taken() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir);
    // Two runs with a hole, the shape an operator gets by listing pieces.
    let app = app_with_pool(&state, (47100..=47104).chain(47200..=47204), &dir).await;

    let (status, pool) = call(&app, Method::GET, "/api/ports", None).await;
    assert_eq!(status, StatusCode::OK, "{pool}");
    assert_eq!(pool["enabled"], json!(true));
    assert_eq!(
        pool["ports"],
        json!([{"first": 47100, "last": 47104}, {"first": 47200, "last": 47204}])
    );
    assert_eq!(
        (pool["total"].clone(), pool["free"].clone()),
        (json!(10), json!(10))
    );
    assert!(pool["entries"].as_array().unwrap().is_empty());

    call(
        &app,
        Method::POST,
        "/api/ports/reservations",
        Some(json!({"owner_id": "prod-a", "count": 2})),
    )
    .await;
    let (_, pool) = call(&app, Method::GET, "/api/ports", None).await;
    assert_eq!(pool["free"], json!(8));
    assert_eq!(pool["entries"].as_array().unwrap().len(), 2);
    assert_eq!(pool["entries"][0]["state"], json!("reserved"));
    assert_eq!(pool["entries"][0]["owner_id"], json!("prod-a"));
}
