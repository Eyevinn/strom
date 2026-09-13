//! The WHIP and WHEP proxies set `Access-Control-Expose-Headers` on their
//! responses, but the app-wide CORS layer replaces that header with its own
//! list on any request carrying an `Origin`. Whatever a cross-origin client must
//! read therefore has to be in the layer's list: above all `Location`, the
//! session resource the client needs to end its session.

use axum::{
    body::Body,
    http::{header, Request},
    Router,
};
use tower::ServiceExt; // for `oneshot`

async fn create_test_app() -> Router {
    use strom::create_app;

    gstreamer::init().unwrap();
    create_app().await
}

async fn exposed_headers(method: &str, uri: &str) -> String {
    let response = create_test_app()
        .await
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header(header::ORIGIN, "https://studio.example.com")
                .header(header::CONTENT_TYPE, "application/sdp")
                .body(Body::from("v=0\r\n"))
                .unwrap(),
        )
        .await
        .unwrap();

    response
        .headers()
        .get_all(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",")
        .to_ascii_lowercase()
}

const PROXY_EXPOSED: [&str; 4] = ["location", "link", "accept-patch", "etag"];

fn missing(exposed: &str) -> Vec<&'static str> {
    PROXY_EXPOSED
        .into_iter()
        .filter(|want| !exposed.split(',').any(|h| h.trim() == *want))
        .collect()
}

#[tokio::test]
async fn whip_post_exposes_proxy_headers_cross_origin() {
    let exposed = exposed_headers("POST", "/whip/no-such-endpoint").await;
    assert!(
        missing(&exposed).is_empty(),
        "WHIP POST hides {:?} from other origins; exposed: {exposed:?}",
        missing(&exposed)
    );
}

#[tokio::test]
async fn whep_post_exposes_proxy_headers_cross_origin() {
    let exposed = exposed_headers("POST", "/whep/no-such-endpoint").await;
    assert!(
        missing(&exposed).is_empty(),
        "WHEP POST hides {:?} from other origins; exposed: {exposed:?}",
        missing(&exposed)
    );
}
