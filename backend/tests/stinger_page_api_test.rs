//! The stinger API on a mixer whose stinger input is a page (an HTML Input in
//! stinger mode): the page is the library's one entry, cue does nothing, and
//! the clip-library operations are refused. None of it needs the flow running,
//! so this runs without cefsrc.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::stinger::{
    STINGER_MODE_PROPERTY, WEB_STINGER_CUT_POINT_PROPERTY, WEB_STINGER_DURATION_PROPERTY,
};
use strom_types::{Flow, Link, PropertyValue as PV};
use tempfile::NamedTempFile;
use tower::ServiceExt;

const URL: &str = "https://example.com/stinger.html";

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(match body {
            Some(b) => Body::from(b.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn block(id: &str, definition: &str, props: &[(&str, PV)]) -> strom_types::BlockInstance {
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<HashMap<_, _>>(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

/// A stopped flow: a vision mixer with its stinger input fed by a page.
async fn page_flow(url: &str) -> (AppState, strom_types::FlowId, NamedTempFile, NamedTempFile) {
    gstreamer::init().unwrap();
    let mut flow = Flow::new("stinger_page_api");
    flow.blocks.push(block(
        "web",
        strom::blocks::builtin::html_input::BLOCK_ID,
        &[
            ("url", PV::String(url.to_string())),
            (STINGER_MODE_PROPERTY, PV::Bool(true)),
            (WEB_STINGER_DURATION_PROPERTY, PV::UInt(1000)),
            (WEB_STINGER_CUT_POINT_PROPERTY, PV::UInt(500)),
        ],
    ));
    flow.blocks.push(block(
        "vm",
        "builtin.vision_mixer",
        &[
            ("num_inputs", PV::UInt(2)),
            ("enable_stinger", PV::Bool(true)),
        ],
    ));
    flow.links.push(Link {
        from: "web:video_out".to_string(),
        to: "vm:stinger_in".to_string(),
    });
    let storage = NamedTempFile::new().unwrap();
    let blocks = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage.path()),
        blocks.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    let flow_id = flow.id;
    state.upsert_flow(flow).await.unwrap();
    (state, flow_id, storage, blocks)
}

#[tokio::test]
async fn a_page_is_the_one_entry_and_the_library_is_refused() {
    let (state, flow_id, _s, _b) = page_flow(URL).await;
    let app = strom::create_app_with_state(state).await;
    let base = format!("/api/flows/{}/blocks/vm/stinger", flow_id);

    let (status, s) = call(&app, "GET", &base, None).await;
    assert_eq!(status, StatusCode::OK, "{s}");
    assert_eq!(s["source_kind"], "page", "{s}");
    assert_eq!(s["source_block_id"], "web");
    let clips = s["clips"].as_array().unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0]["file"], URL);
    assert_eq!(clips[0]["variant"], "classic");
    assert_eq!(clips[0]["cut_point_ms"], 500);
    assert_eq!(clips[0]["settings"]["premultiplied"], true);
    assert!(
        s["problem"].as_str().unwrap().contains("not running"),
        "a stopped page is not ready: {s}"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("{base}/cue"),
        Some(json!({"index": 0})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cueing the page does nothing, successfully"
    );
    let (status, _) = call(
        &app,
        "POST",
        &format!("{base}/cue"),
        Some(json!({"index": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a page has no entry 1");

    let refused = [
        (
            "POST",
            format!("{base}/clips"),
            Some(json!({"file": "a.mov"})),
        ),
        (
            "PUT",
            format!("{base}/clips/0"),
            Some(json!({"layout": "side_by_side"})),
        ),
        ("DELETE", format!("{base}/clips/0"), None),
        ("POST", format!("{base}/examples"), Some(json!({}))),
    ];
    for (method, uri, body) in refused {
        let (status, e) = call(&app, method, &uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {e}");
        assert!(
            e.to_string()
                .contains("stinger page and has no clip library"),
            "{method} {uri} must say why: {e}"
        );
    }
}

#[tokio::test]
async fn a_page_url_with_its_own_fragment_is_reported() {
    let (state, flow_id, _s, _b) = page_flow("https://example.com/graphics/#/stinger").await;
    let s = state.stinger_state(&flow_id, "vm").await.unwrap();
    assert!(
        s.problem.as_deref().unwrap_or("").contains("#fragment"),
        "{:?}",
        s.problem
    );
}
