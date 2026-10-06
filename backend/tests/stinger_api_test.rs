//! The stinger API as an outside client uses it: editing the library on the
//! mixer, guarding index-addressed calls with the file meant, following a
//! take through its id, and taking through the transition endpoint without
//! naming inputs.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rig::*;
use serde_json::{json, Value};
use strom_types::StromEvent;
use tower::ServiceExt;

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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_library_is_edited_on_the_mixer_and_guarded_by_file() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-lib", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let base = format!("/api/flows/{}/blocks/{}/stinger", r.flow_id, r.mixer());
    let (_, state) = call(&app, "GET", &base, None).await;
    let files: Vec<String> = state["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(files.len(), 3);

    // Add: a fourth clip (a copy of the mask), and adding it again is a no-op.
    let extra = std::path::Path::new(&files[2]).with_file_name("extra.mkv");
    std::fs::copy(&files[2], &extra).unwrap();
    let extra = extra.to_string_lossy().to_string();
    let (status, clip) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": extra})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{clip}");
    assert_eq!(clip["index"], 3);
    let (status, again) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": extra})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["index"], 3, "adding the same file twice returns it");
    let (status, _) = call(
        &app,
        "POST",
        &format!("{base}/clips"),
        Some(json!({"file": "/nonexistent/stinger.mkv"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a missing file is refused");

    // Guarded settings: the right file is accepted, a wrong one is a conflict.
    let q = |f: &str| urlencoding(f);
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/clips/3?file={}", q(&extra)),
        Some(json!({"invert_matte": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &app,
        "PUT",
        &format!("{base}/clips/3?file={}", q(&files[0])),
        Some(json!({"invert_matte": true})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Remove clip 0: the rest move up, and a client still holding the old
    // index for clip 1 gets a conflict instead of editing another clip.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/clips/0?file={}", q(&files[0])),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = call(&app, "GET", &base, None).await;
    let now: Vec<&str> = state["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        now,
        vec![files[1].as_str(), files[2].as_str(), extra.as_str()]
    );
    assert_eq!(
        state["clips"][2]["settings"]["invert_matte"], true,
        "settings follow their file"
    );
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/clips/1?file={}", q(&files[1])),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_take_is_followed_by_its_id() {
    if !common::plugins_available(CODEC_ELEMENTS) {
        return;
    }
    let r = start("api-take", "cpu").await;
    let app = strom::create_app_with_state(r.state.clone()).await;
    let mut events = r.state.events().subscribe();
    let blocks = format!("/api/flows/{}/blocks/{}", r.flow_id, r.mixer());

    // A guarded take with the wrong file does not start.
    let (status, _) = call(
        &app,
        "POST",
        &format!("{blocks}/stinger/take"),
        Some(json!({"index": 0, "file": "not-this.mkv"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Through the transition endpoint, naming no inputs.
    let (status, body) = call(
        &app,
        "POST",
        &format!("{blocks}/transition"),
        Some(json!({"transition_type": "stinger", "stinger_clip": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let report = r.wait_for_report(0).await;
    let first = report.take_id;

    // The take endpoint returns the id the events carry.
    r.wait_until_parked().await;
    let (status, take) = call(
        &app,
        "POST",
        &format!("{blocks}/stinger/take"),
        Some(json!({"index": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{take}");
    let id = take["take_id"].as_u64().unwrap();
    assert_ne!(id, first);
    assert!(take["file"].as_str().unwrap().ends_with("classic.mkv"));
    let (mut started, mut completed) = (None, None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while completed.is_none() && std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await {
            Ok(Ok(StromEvent::StingerStarted { take_id, .. })) if take_id == id => {
                started = Some(take_id)
            }
            Ok(Ok(StromEvent::StingerCompleted { report, .. })) if report.take_id == id => {
                completed = Some(report.take_id)
            }
            _ => {}
        }
    }
    assert_eq!(started, Some(id));
    assert_eq!(completed, Some(id));

    // Any other transition still needs its inputs.
    let (status, _) = call(
        &app,
        "POST",
        &format!("{blocks}/transition"),
        Some(json!({"transition_type": "cut"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    r.state.stop_flow(&r.flow_id).await.unwrap();
}

/// Percent-encode a query value.
fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}
