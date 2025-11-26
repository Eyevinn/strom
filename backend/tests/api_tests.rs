//! Integration tests for the Strom API.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use serde_json::json;
use strom_types::api::FlowListResponse;
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

    let request_body = json!({
        "name": "Test Flow"
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/flows")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request_body).unwrap()))
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
