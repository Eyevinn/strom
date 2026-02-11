//! WHIP ingest proxy and page handlers.
//!
//! Proxies WHIP POST/PATCH/DELETE requests from external clients to internal
//! whipserversrc instances, similar to how whep_player.rs proxies for WHEP.
//!
//! Also serves the WHIP ingest HTML page for browser-based camera/mic sending.

use crate::api::sdp_transform::{
    add_goog_remb, fix_video_bitrate_hints, strip_cvo_extension, strip_redundancy_codecs,
};
use crate::state::AppState;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use tracing::{debug, error, info, warn};

/// Serve the WHIP ingest page.
pub async fn whip_ingest_page(State(state): State<AppState>) -> impl IntoResponse {
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
pub async fn list_whip_endpoints(State(state): State<AppState>) -> impl IntoResponse {
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

/// Receive client-side log messages from the WHIP ingest page.
///
/// Accepts a JSON array of log entries and writes them to the server log
/// prefixed with `[WHIP-CLIENT]` so they can be correlated with server-side events.
pub async fn client_log(Json(entries): Json<Vec<ClientLogEntry>>) -> impl IntoResponse {
    for entry in &entries {
        match entry.level.as_deref().unwrap_or("info") {
            "error" => error!("[WHIP-CLIENT] {}", entry.msg),
            "warning" | "warn" => warn!("[WHIP-CLIENT] {}", entry.msg),
            "debug" => debug!("[WHIP-CLIENT] {}", entry.msg),
            _ => info!("[WHIP-CLIENT] {}", entry.msg),
        }
    }
    StatusCode::NO_CONTENT
}

#[derive(serde::Deserialize)]
pub struct ClientLogEntry {
    pub msg: String,
    pub level: Option<String>,
}

/// Handle WHIP POST request (SDP offer from client).
///
/// Proxies the SDP offer to the internal whipserversrc HTTP server and returns
/// the SDP answer, rewriting the Location header to use the proxy path.
///
/// If the internal server returns 500 or times out (stale session), we recreate
/// the whipserversrc element in the background and return the error immediately.
/// The client's own retry logic will resend the offer to the fresh element.
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

    // 5s timeout: a healthy whipserversrc responds in <1s. If it takes longer,
    // the element is stuck (zombie session) and needs recreation.
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

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

    // Strip RED/RTX/ULPFEC from SDP offer. Disabled: decodebin3 handles these
    // fine in current gst-plugins-rs. Re-enable if "No streams to output" errors return.
    #[allow(unreachable_code)]
    let body_bytes = if false {
        if content_type.contains("sdp") {
            if let Ok(sdp_str) = std::str::from_utf8(&body_bytes) {
                let cleaned = strip_redundancy_codecs(sdp_str);
                debug!("WHIP: SDP after stripping redundancy codecs:\n{}", cleaned);
                axum::body::Bytes::from(cleaned)
            } else {
                body_bytes
            }
        } else {
            body_bytes
        }
    } else {
        body_bytes
    };

    let auth_header = headers.get(header::AUTHORIZATION).cloned();

    let result = forward_whip_post(
        &client,
        &internal_url,
        content_type,
        &body_bytes,
        &auth_header,
    )
    .await;

    // Handle proxy errors (timeout, connection refused, etc.)
    let (status, resp_headers, resp_body) = match result {
        Ok(tuple) => tuple,
        Err(_) => {
            // Proxy failed (likely timeout) - recreate element in background so it's
            // ready when the client retries, then return error immediately.
            warn!(
                "WHIP: Proxy request to whipserversrc timed out for endpoint '{}' -- triggering recreation",
                endpoint_id
            );
            let eid = endpoint_id.clone();
            tokio::task::spawn(async move {
                match tokio::task::spawn_blocking(move || {
                    crate::blocks::builtin::whip::recreate_whipserversrc(&eid)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("WHIP: Failed to recreate element: {}", e),
                    Err(e) => error!("WHIP: Recreation task panicked: {:?}", e),
                }
            });
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "WHIP element busy, retry in a moment",
            )
                .into_response();
        }
    };

    // If internal server returned 500, the element has a stale session.
    // Recreate in background and return error so the client retries against
    // the fresh element (no server-side retry to avoid creating zombie sessions).
    if status.is_server_error() {
        let body_str = std::str::from_utf8(&resp_body).unwrap_or("<non-utf8>");
        warn!(
            "WHIP: Internal server returned {} for endpoint '{}': {} -- triggering recreation",
            status, endpoint_id, body_str
        );

        let eid = endpoint_id.clone();
        tokio::task::spawn(async move {
            match tokio::task::spawn_blocking(move || {
                crate::blocks::builtin::whip::recreate_whipserversrc(&eid)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("WHIP: Failed to recreate element: {}", e),
                Err(e) => error!("WHIP: Recreation task panicked: {:?}", e),
            }
        });
    } else if status.is_client_error() {
        let body_str = std::str::from_utf8(&resp_body).unwrap_or("<non-utf8>");
        warn!(
            "WHIP: Internal server returned {} for endpoint '{}': {}",
            status, endpoint_id, body_str
        );
    }

    build_whip_post_response(&endpoint_id, status, &resp_headers, resp_body).into_response()
}

/// Forward a WHIP POST request to the internal whipserversrc.
async fn forward_whip_post(
    client: &reqwest::Client,
    internal_url: &str,
    content_type: &str,
    body_bytes: &axum::body::Bytes,
    auth_header: &Option<axum::http::HeaderValue>,
) -> Result<
    (
        reqwest::StatusCode,
        reqwest::header::HeaderMap,
        axum::body::Bytes,
    ),
    Response,
> {
    let mut req = client
        .post(internal_url)
        .header(header::CONTENT_TYPE, content_type)
        .body(body_bytes.clone());

    if let Some(auth) = auth_header {
        req = req.header(header::AUTHORIZATION, auth.clone());
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!("Failed to proxy WHIP POST to {}: {}", internal_url, e);
            return Err((StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response());
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read proxy response: {}", e);
            return Err((StatusCode::BAD_GATEWAY, "Failed to read response").into_response());
        }
    };

    Ok((
        status,
        resp_headers,
        axum::body::Bytes::from(resp_body.to_vec()),
    ))
}

/// Build the final WHIP POST response with SDP patching and header rewriting.
fn build_whip_post_response(
    endpoint_id: &str,
    status: reqwest::StatusCode,
    resp_headers: &reqwest::header::HeaderMap,
    resp_body: axum::body::Bytes,
) -> Response {
    // Patch the SDP answer for better Chrome bandwidth estimation:
    // 1. Add goog-remb as fallback bandwidth estimation
    // 2. Add x-google bitrate hints to the video fmtp line so Chrome
    //    starts at a reasonable bitrate (webrtcbin strips these from
    //    fmtp and puts them as standalone a=x-google-* attributes that
    //    Chrome ignores for bandwidth estimation)
    //
    // NOTE: We intentionally do NOT rewrite extmap IDs in the answer.
    // webrtcbin assigns its own extmap IDs internally.
    let resp_body = if let Ok(answer_str) = std::str::from_utf8(&resp_body) {
        let patched = add_goog_remb(answer_str);
        let patched = fix_video_bitrate_hints(&patched);
        let patched = strip_cvo_extension(&patched);
        debug!("WHIP: SDP answer:\n{}", patched);
        axum::body::Bytes::from(patched)
    } else {
        resp_body
    };

    // Build the response with rewritten headers
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));

    // Rewrite Location header: /whip/resource/{id} -> /whip/{endpoint_id}/resource/{id}
    if let Some(location) = resp_headers.get(header::LOCATION) {
        if let Ok(loc_str) = location.to_str() {
            let rewritten = if let Some(path_after_whip) = loc_str.strip_prefix("/whip/") {
                format!("/whip/{}/{}", endpoint_id, path_after_whip)
            } else {
                loc_str.to_string()
            };
            info!("WHIP: Rewriting Location: {} -> {}", loc_str, rewritten);
            builder = builder.header(header::LOCATION, &rewritten);
        }
    }

    // Forward relevant headers
    for (name, value) in resp_headers {
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
        .header(
            "Access-Control-Allow-Methods",
            "POST, PATCH, DELETE, OPTIONS",
        )
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, If-Match",
        )
        .header(
            "Access-Control-Expose-Headers",
            "Location, Link, Accept-Patch, ETag",
        );

    match builder.body(Body::from(resp_body)) {
        Ok(resp) => resp,
        Err(e) => {
            error!("Failed to build response: {}", e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Response build error"))
                .unwrap()
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

    let internal_url = format!("http://127.0.0.1:{}/whip/resource/{}", port, resource_id);

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
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read PATCH proxy response: {}", e);
            axum::body::Bytes::new()
        }
    };

    let builder = Response::builder()
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
///
/// After forwarding DELETE to whipserversrc, we trigger async element
/// recreation. The whipserversrc element is treated as single-use: it handles
/// one session, then gets destroyed and replaced with a fresh element.
/// The pad-removed callback also triggers recreation, but the AtomicBool
/// idempotency flag in WhipServerContext prevents double-recreation.
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

    let internal_url = format!("http://127.0.0.1:{}/whip/resource/{}", port, resource_id);

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let resp_status = match client.delete(&internal_url).send().await {
        Ok(r) => r.status(),
        Err(e) => {
            error!("Failed to proxy WHIP DELETE: {}", e);
            return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response();
        }
    };

    // Recreate the whipserversrc element synchronously before returning the
    // DELETE response. This ensures the new element is ready to accept the next
    // client connection immediately. We use spawn_blocking + await because
    // recreate_whipserversrc does GStreamer operations that must not run on
    // the tokio runtime. The short sleep gives whipserversrc time to finish
    // its internal teardown before we destroy the element.
    let endpoint_id_for_recreate = endpoint_id.clone();
    let recreate_result = tokio::task::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        crate::blocks::builtin::whip::recreate_whipserversrc(&endpoint_id_for_recreate)
    })
    .await;

    match recreate_result {
        Ok(Ok(())) => info!("WHIP DELETE: Recreated whipserversrc for '{}'", endpoint_id),
        Ok(Err(e)) => warn!(
            "WHIP DELETE: Failed to recreate for '{}': {}",
            endpoint_id, e
        ),
        Err(e) => error!(
            "WHIP DELETE: Recreation task panicked for '{}': {:?}",
            endpoint_id, e
        ),
    }

    Response::builder()
        .status(
            StatusCode::from_u16(resp_status.as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        )
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

/// Handle CORS preflight for WHIP endpoints.
pub async fn whip_options() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Methods",
            "POST, PATCH, DELETE, OPTIONS",
        )
        .header(
            "Access-Control-Allow-Headers",
            "Content-Type, Authorization, If-Match",
        )
        .header(
            "Access-Control-Expose-Headers",
            "Location, Link, Accept-Patch, ETag",
        )
        .body(Body::empty())
        .unwrap()
}

/// Handle CORS preflight for WHIP resource endpoints.
pub async fn whip_resource_options() -> impl IntoResponse {
    whip_options().await
}
