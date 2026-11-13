//! `GET /api/capabilities` end to end (#1257): what it reports by default,
//! with inbound reachability off and ports configured, and that it never
//! exposes ICE/TURN URLs, credentials or internal relay addresses.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use strom::auth::AuthConfig;
use strom::capabilities::ReachabilitySettings;
use strom::create_app_with_state_and_auth;
use strom::ports::PortReservationStore;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use tempfile::TempDir;
use tower::ServiceExt;

fn new_state(dir: &TempDir, ice_servers: Vec<String>) -> AppState {
    AppState::new(
        JsonFileStorage::new(dir.path().join("flows.json")),
        dir.path().join("blocks.json"),
        dir.path(),
        ice_servers,
        "all".to_string(),
        vec![],
        false,
        false,
    )
}

async fn app(state: &AppState) -> Router {
    // Explicitly unauthenticated, so a STROM_API_KEY in the developer's shell
    // cannot turn the assertions into a 401.
    let auth = AuthConfig {
        admin_user: None,
        admin_password_hash: None,
        api_key: None,
        native_gui_token: None,
        enabled: false,
        session_key: strom::auth::SessionKey::random(),
        session_cookie_secure: false,
    };
    create_app_with_state_and_auth(state.clone(), auth).await
}

async fn get_capabilities(app: &Router) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn defaults_report_every_mode_and_any_port() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir, vec![]);
    let app = app(&state).await;

    let (status, body) = get_capabilities(&app).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value,
        json!({
            "network": { "inbound_reachable": true, "ports": null },
            "protocols": {
                "srt": {
                    "modes": { "listener": true, "caller": true, "rendezvous": true },
                    "relays": []
                },
                "webrtc": { "ice_servers": false, "ice_transport_policy": "all" }
            }
        })
    );
}

#[tokio::test]
async fn unreachable_with_ports_reports_caller_only_and_hides_ice_urls() {
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(
        &dir,
        vec![
            "stun:stun.example.com:3478".to_string(),
            "turn:user:secret@turn.example.com:3478".to_string(),
        ],
    );
    let ports: BTreeSet<u16> = (47100..=47199).chain([47250]).collect();
    state
        .configure_port_pool(ports, PortReservationStore::new(dir.path()), 600, false)
        .await
        .unwrap();
    state.set_reachability(ReachabilitySettings {
        inbound_reachable: false,
    });
    let app = app(&state).await;

    let (status, body) = get_capabilities(&app).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value,
        json!({
            "network": {
                "inbound_reachable": false,
                "ports": [
                    { "first": 47100, "last": 47199 },
                    { "first": 47250, "last": 47250 }
                ]
            },
            "protocols": {
                "srt": {
                    "modes": { "listener": false, "caller": true, "rendezvous": false },
                    "relays": []
                },
                "webrtc": { "ice_servers": true, "ice_transport_policy": "all" }
            }
        })
    );
    for secret in ["example.com", "secret", "turn:", "stun:"] {
        assert!(
            !body.contains(secret),
            "capabilities leak '{secret}': {body}"
        );
    }
}

/// Configured SRT relays are reported by name and public address, `default`
/// first; their internal addresses never leave the server.
#[tokio::test]
async fn relays_are_reported_without_internal_addresses() {
    use strom::relay::{RelayEntry, Relays, RelaysSection};
    gstreamer::init().unwrap();
    let dir = TempDir::new().unwrap();
    let state = new_state(&dir, vec![]);
    let entry = |public: &str, internal: &str| RelayEntry {
        public: Some(public.to_string()),
        internal: Some(internal.to_string()),
    };
    let section: RelaysSection = [(
        "srt".to_string(),
        [
            (
                "us".to_string(),
                entry("srt-us.example.com:9000", "splice-us.internal:9000"),
            ),
            (
                "default".to_string(),
                entry("srt.example.com:9000", "srt-splice.internal:9000"),
            ),
        ]
        .into(),
    )]
    .into();
    state.configure_relays(Relays::from_settings(&section, |_| None).unwrap());
    let app = app(&state).await;

    let (status, body) = get_capabilities(&app).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value["protocols"]["srt"]["relays"],
        json!([
            { "name": "default", "public": "srt.example.com:9000" },
            { "name": "us", "public": "srt-us.example.com:9000" }
        ])
    );
    assert!(
        !body.contains(".internal"),
        "internal address leaked: {body}"
    );
}
