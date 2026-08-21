//! Integration tests for the Strom API.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::json;
use strom_types::api::FlowListResponse;
use strom_types::Flow;
use tower::ServiceExt; // for `oneshot`

/// Helper to create a test app instance.
async fn create_test_app() -> Router {
    // Import from the backend crate
    use strom::create_app;

    gstreamer::init().unwrap();
    create_app().await
}

#[tokio::test]
async fn test_health_check() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_list_flows_empty() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let flows: FlowListResponse = serde_json::from_slice(&body).unwrap();

    assert_eq!(flows.flows.len(), 0);
}

#[tokio::test]
async fn test_create_flow() {
    let app = create_test_app().await;

    let flow = Flow::new("Test Flow".to_string());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&flow).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(response_json["flow"]["name"], "Test Flow");

    // The backend must keep the id the caller supplied (see #672). It used to
    // overwrite it, which made the required `id` field of the request schema
    // meaningless and stopped callers starting a flow by an id they chose.
    let returned_id = response_json["flow"]["id"].as_str().unwrap();
    assert_eq!(returned_id, flow.id.to_string());

    // Runtime state must be cleared
    assert_eq!(response_json["flow"]["running"], false);
    assert!(response_json["flow"]["gst_state"].is_null());
}

#[tokio::test]
async fn test_create_flow_empty_name() {
    let app = create_test_app().await;

    let flow = Flow::new("".to_string());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&flow).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_flow_name_too_long() {
    let app = create_test_app().await;

    let flow = Flow::new("x".repeat(256));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&flow).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_list_elements() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/elements")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let elements = response_json["elements"].as_array().unwrap();
    assert!(
        !elements.is_empty(),
        "Should discover some GStreamer elements"
    );
}

#[tokio::test]
async fn test_get_specific_element() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/elements/videotestsrc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(response_json["element"]["name"], "videotestsrc");
    assert!(!response_json["element"]["description"].is_null());
}

#[tokio::test]
async fn test_get_nonexistent_element() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/elements/nonexistent_element_12345")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// gst-launch-1.0 API Tests
// ============================================================================

#[tokio::test]
async fn test_parse_gst_launch_simple() {
    let app = create_test_app().await;

    let request_body = json!({
        "pipeline": "videotestsrc ! fakesink"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/parse")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let elements = response_json["elements"].as_array().unwrap();
    assert_eq!(elements.len(), 2);

    // Check that we have both element types
    let types: Vec<&str> = elements
        .iter()
        .map(|e| e["element_type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"videotestsrc"));
    assert!(types.contains(&"fakesink"));

    // Check that links are extracted
    let links = response_json["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
}

#[tokio::test]
async fn test_parse_gst_launch_with_properties() {
    let app = create_test_app().await;

    let request_body = json!({
        "pipeline": "videotestsrc pattern=ball num-buffers=100 ! fakesink"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/parse")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Find videotestsrc element and check properties
    let elements = response_json["elements"].as_array().unwrap();
    let vts = elements
        .iter()
        .find(|e| e["element_type"] == "videotestsrc")
        .expect("Should have videotestsrc");

    // pattern=ball should be converted to human-readable string "ball"
    assert_eq!(vts["properties"]["pattern"], "ball");
    assert_eq!(vts["properties"]["num-buffers"], 100);
}

#[tokio::test]
async fn test_parse_gst_launch_invalid_pipeline() {
    let app = create_test_app().await;

    let request_body = json!({
        "pipeline": "this_element_does_not_exist ! fakesink"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/parse")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(response_json["error"].as_str().unwrap().contains("Invalid"));
}

#[tokio::test]
async fn test_export_gst_launch_simple() {
    let app = create_test_app().await;

    let request_body = json!({
        "elements": [
            {
                "id": "src",
                "element_type": "videotestsrc",
                "properties": {},
                "pad_properties": {},
                "position": [0.0, 0.0]
            },
            {
                "id": "sink",
                "element_type": "fakesink",
                "properties": {},
                "pad_properties": {},
                "position": [100.0, 0.0]
            }
        ],
        "links": [
            { "from": "src", "to": "sink" }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/export")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let pipeline = response_json["pipeline"].as_str().unwrap();
    assert_eq!(pipeline, "videotestsrc ! fakesink");
}

#[tokio::test]
async fn test_export_gst_launch_with_properties() {
    let app = create_test_app().await;

    let request_body = json!({
        "elements": [
            {
                "id": "src",
                "element_type": "videotestsrc",
                "properties": { "pattern": 18 },
                "pad_properties": {},
                "position": [0.0, 0.0]
            },
            {
                "id": "sink",
                "element_type": "fakesink",
                "properties": { "sync": false },
                "pad_properties": {},
                "position": [100.0, 0.0]
            }
        ],
        "links": [
            { "from": "src", "to": "sink" }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/export")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let pipeline = response_json["pipeline"].as_str().unwrap();
    assert!(pipeline.contains("videotestsrc"));
    assert!(pipeline.contains("pattern=18"));
    assert!(pipeline.contains("fakesink"));
    assert!(pipeline.contains("sync=false"));
}

// ============================================================================
// Buffer Age Probe API Tests
// ============================================================================

#[tokio::test]
async fn test_list_probes_nonexistent_flow() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows/00000000-0000-0000-0000-000000000001/probes")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_activate_probe_nonexistent_flow() {
    let app = create_test_app().await;

    let request_body = json!({
        "element_id": "some-element"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows/00000000-0000-0000-0000-000000000001/probes")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_activate_probe_invalid_flow_id() {
    let app = create_test_app().await;

    let request_body = json!({
        "element_id": "some-element"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows/not-a-uuid/probes")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_deactivate_probe_nonexistent_flow() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows/00000000-0000-0000-0000-000000000001/probes/some-probe-id")
                .method("DELETE")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_export_gst_launch_empty() {
    let app = create_test_app().await;

    let request_body = json!({
        "elements": [],
        "links": []
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/gst-launch/export")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
