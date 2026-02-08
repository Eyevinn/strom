//! WHIP ingest proxy and page handlers.
//!
//! Proxies WHIP POST/PATCH/DELETE requests from external clients to internal
//! whipserversrc instances, similar to how whep_player.rs proxies for WHEP.
//!
//! Also serves the WHIP ingest HTML page for browser-based camera/mic sending.

use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use tracing::{debug, error, info, warn};

/// Serve the WHIP ingest page.
pub async fn whip_ingest_page(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let endpoints = state.whip_registry().list_all().await;
    let _ = endpoints; // Page fetches endpoints via JS

    match crate::assets::WhipAssets::get("ingest.html") {
        Some(content) => {
            let html = std::str::from_utf8(content.data.as_ref()).unwrap_or("");
            Html(html.to_string()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Ingest page not found").into_response(),
    }
}

/// List active WHIP endpoints (public API, no auth required).
pub async fn list_whip_endpoints(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let endpoints = state.whip_registry().list_all().await;
    let list: Vec<serde_json::Value> = endpoints
        .into_iter()
        .map(|(id, entry)| {
            serde_json::json!({
                "endpoint_id": id,
                "mode": entry.mode.as_str(),
            })
        })
        .collect();
    axum::Json(list).into_response()
}

/// Handle WHIP POST request (SDP offer from client).
///
/// Proxies the SDP offer to the internal whipserversrc HTTP server and returns
/// the SDP answer, rewriting the Location header to use the proxy path.
pub async fn whip_post(
    State(state): State<AppState>,
    Path(endpoint_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    debug!("WHIP POST for endpoint: {}", endpoint_id);

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            warn!("WHIP endpoint not found: {}", endpoint_id);
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    // Forward the request to the internal whipserversrc
    let internal_url = format!("http://127.0.0.1:{}/whip/endpoint", port);

    let client = match reqwest::Client::builder()
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    // Build the forwarded request
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return (StatusCode::BAD_REQUEST, "Failed to read body").into_response();
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/sdp");

    let mut req = client
        .post(&internal_url)
        .header(header::CONTENT_TYPE, content_type)
        .body(body_bytes);

    // Forward Authorization header if present
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        req = req.header(header::AUTHORIZATION, auth.clone());
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to proxy WHIP POST to {}: {}", internal_url, e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read proxy response: {}", e);
            return (StatusCode::BAD_GATEWAY, "Failed to read response").into_response();
        }
    };

    // Build the response with rewritten headers
    let mut builder = Response::builder().status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));

    // Rewrite Location header: /whip/resource/{id} -> /whip/{endpoint_id}/resource/{id}
    if let Some(location) = resp_headers.get(header::LOCATION) {
        if let Ok(loc_str) = location.to_str() {
            let rewritten = if loc_str.starts_with("/whip/") {
                let path_after_whip = &loc_str[6..]; // after "/whip/"
                format!("/whip/{}/{}", endpoint_id, path_after_whip)
            } else {
                loc_str.to_string()
            };
            info!("WHIP: Rewriting Location: {} -> {}", loc_str, rewritten);
            builder = builder.header(header::LOCATION, &rewritten);
        }
    }

    // Forward relevant headers
    for (name, value) in &resp_headers {
        let name_str = name.as_str().to_lowercase();
        match name_str.as_str() {
            "content-type" | "link" | "accept-patch" | "etag" => {
                builder = builder.header(name, value);
            }
            _ => {}
        }
    }

    // Add CORS headers
    builder = builder
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "POST, PATCH, DELETE, OPTIONS")
        .header("Access-Control-Allow-Headers", "Content-Type, Authorization, If-Match")
        .header("Access-Control-Expose-Headers", "Location, Link, Accept-Patch, ETag");

    match builder.body(Body::from(resp_body)) {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            error!("Failed to build response: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Response build error").into_response()
        }
    }
}

/// Handle WHIP PATCH request (ICE trickle from client).
pub async fn whip_resource_patch(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    debug!(
        "WHIP PATCH for endpoint: {}, resource: {}",
        endpoint_id, resource_id
    );

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    let internal_url = format!(
        "http://127.0.0.1:{}/whip/resource/{}",
        port, resource_id
    );

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read PATCH body: {}", e);
            return (StatusCode::BAD_REQUEST, "Failed to read body").into_response();
        }
    };

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/trickle-ice-sdpfrag");

    let response = match client
        .patch(&internal_url)
        .header(header::CONTENT_TYPE, content_type)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to proxy WHIP PATCH: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    let status = response.status();
    let resp_body = response.bytes().await.unwrap_or_default();

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("Access-Control-Allow-Origin", "*");

    match builder.body(Body::from(resp_body)) {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            error!("Failed to build PATCH response: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Response error").into_response()
        }
    }
}

/// Handle WHIP DELETE request (client disconnect).
pub async fn whip_resource_delete(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
) -> impl IntoResponse {
    info!(
        "WHIP DELETE for endpoint: {}, resource: {}",
        endpoint_id, resource_id
    );

    let port = match state.whip_registry().get_port(&endpoint_id).await {
        Some(port) => port,
        None => {
            return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
        }
    };

    let internal_url = format!(
        "http://127.0.0.1:{}/whip/resource/{}",
        port, resource_id
    );

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    match client.delete(&internal_url).send().await {
        Ok(r) => {
            let status = r.status();
            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
                .header("Access-Control-Allow-Origin", "*")
                .body(Body::empty())
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap()
                })
                .into_response()
        }
        Err(e) => {
            error!("Failed to proxy WHIP DELETE: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response()
        }
    }
}

/// Handle CORS preflight for WHIP endpoints.
pub async fn whip_options() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Allow-Methods", "POST, PATCH, DELETE, OPTIONS")
        .header("Access-Control-Allow-Headers", "Content-Type, Authorization, If-Match")
        .header("Access-Control-Expose-Headers", "Location, Link, Accept-Patch, ETag")
        .body(Body::empty())
        .unwrap()
}

/// Handle CORS preflight for WHIP resource endpoints.
pub async fn whip_resource_options() -> impl IntoResponse {
    whip_options().await
}
