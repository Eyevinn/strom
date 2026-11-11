//! `srt+relay://<id>` in an SRT block's `srt_uri` reaches the block's
//! `srtsrc`/`srtsink` as a caller URI to the configured SRT relay, with the
//! block's own direction in the streamid. Each test builds the real block.

use crate::common;

use std::collections::HashMap;
use strom::blocks::builtin::get_builder;
use strom::blocks::{BlockBuildContext, BlockBuildError};
use strom_types::PropertyValue;

use gstreamer::prelude::*;

const RELAY: &str = "127.0.0.1:9000";

/// The SRT blocks, the element in each that carries the URI, and the role
/// the block takes on the relay.
/// The EFP blocks exist only with the `efp` feature, which CI builds with.
const BLOCKS: &[(&str, &str, &str)] = &[
    ("builtin.mpegtssrt_input", "srtsrc", "request"),
    ("builtin.mpegtssrt_output", "srtsink", "publish"),
    #[cfg(feature = "efp")]
    ("builtin.efpsrt_input", "srtsrc", "request"),
    #[cfg(feature = "efp")]
    ("builtin.efpsrt_output", "srtsink", "publish"),
];

fn build(
    definition_id: &str,
    srt_uri: &str,
    relay: Option<&str>,
) -> Result<strom::blocks::BlockBuildResult, BlockBuildError> {
    let mut properties = HashMap::new();
    properties.insert(
        "srt_uri".to_string(),
        PropertyValue::String(srt_uri.to_string()),
    );
    let ctx =
        BlockBuildContext::new(vec![], "all".to_string()).with_srt_relay(relay.map(str::to_string));
    get_builder(definition_id)
        .unwrap_or_else(|| panic!("no builder for {definition_id}"))
        .build("relay_block", &properties, &ctx)
}

/// The `uri` property of the block's SRT element, split into its address and
/// its query parameters. GStreamer rebuilds the URI string from its parsed
/// form, so the parameter order is not kept.
fn srt_uri_of(
    result: &strom::blocks::BlockBuildResult,
    factory: &str,
) -> (String, HashMap<String, String>) {
    let element = result
        .elements
        .iter()
        .map(|(_, e)| e)
        .find(|e| e.factory().is_some_and(|f| f.name() == factory))
        .unwrap_or_else(|| panic!("block has no {factory}"));
    let uri: String = element.property("uri");
    let (address, query) = uri.split_once('?').unwrap_or((&uri, ""));
    let params = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    (address.to_string(), params)
}

/// Percent-decode the few escapes a streamid uses.
fn decode(value: &str) -> String {
    value
        .replace("%23", "#")
        .replace("%3A", ":")
        .replace("%3a", ":")
        .replace("%3D", "=")
        .replace("%3d", "=")
        .replace("%2C", ",")
        .replace("%2c", ",")
}

#[test]
fn relay_uri_reaches_the_srt_element_as_a_caller_uri() {
    common::init_static_plugins();
    for (definition_id, factory, role) in BLOCKS {
        let result = build(
            definition_id,
            "srt+relay://cam-1?passphrase=0123456789abcdef&pbkeylen=32",
            Some(RELAY),
        )
        .unwrap_or_else(|e| panic!("{definition_id}: {e}"));
        let (address, params) = srt_uri_of(&result, factory);
        assert_eq!(address, format!("srt://{RELAY}"), "{definition_id}");
        assert_eq!(params.get("mode").map(String::as_str), Some("caller"));
        assert_eq!(
            params.get("streamid").map(|s| decode(s)),
            Some(format!("#!::r=cam-1,m={role}")),
            "{definition_id}"
        );
        assert_eq!(
            params.get("passphrase").map(String::as_str),
            Some("0123456789abcdef"),
            "{definition_id}"
        );
        assert_eq!(params.get("pbkeylen").map(String::as_str), Some("32"));
    }
}

/// `strom_types::srt_relay::SRT_RELAY_BLOCKS` names exactly the blocks with
/// an `srt_uri` property, so the frontend helper and the backend resolution
/// cover the same blocks.
#[test]
fn every_block_with_an_srt_uri_is_a_relay_block() {
    use strom_types::srt_relay::{srt_relay_role, SRT_RELAY_BLOCKS};
    let with_srt_uri: Vec<String> = strom::blocks::builtin::get_all_builtin_blocks()
        .into_iter()
        .filter(|def| def.exposed_properties.iter().any(|p| p.name == "srt_uri"))
        .map(|def| def.id)
        .collect();
    for id in &with_srt_uri {
        assert!(
            srt_relay_role(id).is_some(),
            "{id} is missing from SRT_RELAY_BLOCKS"
        );
    }
    for (id, _) in SRT_RELAY_BLOCKS {
        // The EFP blocks exist only with the `efp` feature.
        if id.starts_with("builtin.efpsrt") && !cfg!(feature = "efp") {
            continue;
        }
        assert!(with_srt_uri.iter().any(|b| b == id), "{id} has no srt_uri");
    }
}

#[test]
fn relay_uri_without_a_relay_fails_the_build() {
    common::init_static_plugins();
    for (definition_id, _, _) in BLOCKS {
        let err = match build(definition_id, "srt+relay://cam-1", None) {
            Ok(_) => panic!("{definition_id} built without a relay"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("STROM_SRT_RELAY"), "{definition_id}: {err}");
    }
}

#[test]
fn relay_uri_with_mode_fails_the_build() {
    common::init_static_plugins();
    for (definition_id, _, _) in BLOCKS {
        let err = match build(
            definition_id,
            "srt+relay://cam-1?mode=listener",
            Some(RELAY),
        ) {
            Ok(_) => panic!("{definition_id} built with mode in the relay uri"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("mode"), "{definition_id}: {err}");
    }
}

#[test]
fn plain_uri_is_untouched_with_a_relay_configured() {
    common::init_static_plugins();
    for (definition_id, factory, _) in BLOCKS {
        let result = build(definition_id, "srt://:5000?mode=listener", Some(RELAY))
            .unwrap_or_else(|e| panic!("{definition_id}: {e}"));
        let (address, params) = srt_uri_of(&result, factory);
        assert_eq!(address, "srt://:5000", "{definition_id}");
        assert_eq!(params.get("mode").map(String::as_str), Some("listener"));
        assert!(!params.contains_key("streamid"), "{definition_id}");
    }
}

/// A flow with one MPEG-TS/SRT Input block on `srt_uri`, its outputs unlinked.
fn relay_input_flow(srt_uri: &str) -> strom_types::Flow {
    let mut flow = strom_types::Flow::new("srt_relay_flow");
    flow.blocks.push(strom_types::BlockInstance {
        id: "relay_in".to_string(),
        block_definition_id: "builtin.mpegtssrt_input".to_string(),
        name: None,
        properties: HashMap::from([(
            "srt_uri".to_string(),
            PropertyValue::String(srt_uri.to_string()),
        )]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

/// The relay configured on the server reaches the blocks of a flow it starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_started_flow_dials_the_configured_relay() {
    common::init_static_plugins();
    // A local UDP port nobody answers on: the caller just keeps retrying.
    let relay = std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.to_string())
        .unwrap();
    let state = common::state::new();
    state.configure_srt_relay(
        strom::srt_relay::SrtRelayConfig::from_settings(Some(relay.clone()), None, None).unwrap(),
    );
    let flow = relay_input_flow("srt+relay://cam1?latency=200");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    state.start_flow(&flow_id).await.expect("start_flow");

    let props = state
        .get_element_properties(&flow_id, "relay_in:srtsrc")
        .await
        .expect("srtsrc properties");
    let Some(PropertyValue::String(uri)) = props.get("uri") else {
        panic!("srtsrc has no uri: {:?}", props.get("uri"));
    };
    state.stop_flow(&flow_id).await.unwrap();

    assert!(uri.starts_with(&format!("srt://{relay}?")), "{uri}");
    assert!(uri.contains("mode=caller"), "{uri}");
    assert!(
        decode(uri).contains("streamid=#!::r=cam1,m=request"),
        "{uri}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flow_with_a_relay_uri_fails_to_start_without_a_relay() {
    common::init_static_plugins();
    let state = common::state::new();
    let flow = relay_input_flow("srt+relay://cam1");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    let err = state
        .start_flow(&flow_id)
        .await
        .expect_err("a relay uri without a relay must not start");
    assert!(err.to_string().contains("STROM_SRT_RELAY"), "{err}");
}

mod api {
    use axum::{body::Body, http::Request, http::StatusCode, Router};
    use serde_json::{json, Value};
    use strom::auth::AuthConfig;
    use strom::srt_relay::SrtRelayConfig;
    use strom::state::AppState;
    use tower::ServiceExt;

    fn auth(api_key: Option<&str>) -> AuthConfig {
        AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: api_key.map(str::to_string),
            native_gui_token: None,
            enabled: api_key.is_some(),
            session_key: strom::auth::SessionKey::random(),
            session_cookie_secure: false,
        }
    }

    async fn app(state: &AppState, api_key: Option<&str>) -> Router {
        strom::create_app_with_state_and_auth(state.clone(), auth(api_key)).await
    }

    async fn get(app: &Router, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut req = Request::builder().uri("/api/srt-relay");
        if let Some(token) = bearer {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn unconfigured_server_reports_no_relay() {
        gstreamer::init().unwrap();
        let state = crate::common::state::new();
        let (status, body) = get(&app(&state, None).await, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"configured": false, "relay": null, "public": null, "error": null})
        );
    }

    #[tokio::test]
    async fn configured_relay_with_public_override() {
        gstreamer::init().unwrap();
        let state = crate::common::state::new();
        state.configure_srt_relay(
            SrtRelayConfig::from_settings(
                Some("srt-splice:9000".into()),
                None,
                Some("srt.example.com:9000".into()),
            )
            .unwrap(),
        );
        let (status, body) = get(&app(&state, None).await, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "configured": true,
                "relay": "srt-splice:9000",
                "public": "srt.example.com:9000",
                "error": null
            })
        );
    }

    #[tokio::test]
    async fn the_route_requires_authentication() {
        gstreamer::init().unwrap();
        let state = crate::common::state::new();
        let app = app(&state, Some("test-key")).await;
        assert_eq!(get(&app, None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(get(&app, Some("test-key")).await.0, StatusCode::OK);
    }
}
