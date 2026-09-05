//! Regression test for issue #692.
//!
//! Unmatched `/api/*` paths used to fall through to the SPA catch-all and answer
//! `200 OK` with the frontend HTML document, so an API client that only checks the
//! status code recorded a success for a route that does not exist in the running
//! build. They must return a JSON 404 instead, while non-`/api/` paths keep being
//! served by the SPA fallback so deep links continue to work.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use strom_types::api::ErrorResponse;
use tower::ServiceExt; // for `oneshot`

async fn create_test_app() -> Router {
    use strom::create_app;

    gstreamer::init().unwrap();
    create_app().await
}

#[tokio::test]
async fn unmatched_api_path_returns_json_404() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/this/route/does/not/exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "expected a JSON body, got content-type {content_type}"
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let error: ErrorResponse =
        serde_json::from_slice(&body).expect("body must be an ErrorResponse");
    assert!(!error.error.is_empty());
    // The details carry the full path, not the prefix-stripped one the nested
    // router sees internally.
    assert!(
        error
            .details
            .as_deref()
            .unwrap_or_default()
            .contains("/api/this/route/does/not/exist"),
        "details should name the requested path, got {:?}",
        error.details
    );
}

#[tokio::test]
async fn unmatched_non_api_path_still_serves_the_spa() {
    let app = create_test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/some/deep/spa/link")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // The frontend bundle is only embedded when `backend/dist/` has been built, so
    // accept either the SPA document or the plain 404 from an unbuilt frontend.
    // What must never happen is this path being answered by the API 404 handler.
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    assert!(
        !content_type.starts_with("application/json"),
        "non-API deep links must not be answered by the API 404 handler (status {status}, \
         content-type {content_type})"
    );
}
