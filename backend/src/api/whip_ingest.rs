//! WHIP ingest proxy and page handlers.
//!
//! Each WHIP POST creates a new whipserversrc element (one per client session).
//! PATCH/DELETE requests are routed to the correct session's port via the
//! WhipSessionManager resource_id lookup.

use crate::api::sdp_transform::{
    add_goog_remb, fix_video_bitrate_hints, strip_cvo_extension, strip_redundancy_codecs,
};
use crate::blocks::builtin::whip::{create_whipserversrc_for_session, CreatedSession};
use crate::json_rejection::JsonBody;
use crate::state::AppState;
use crate::whip_session_manager::{
    NewWhipSession, SessionActivity, WhipEndpointConfig, WhipSessionManager,
};
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
#[utoipa::path(
    get,
    path = "/api/whip-endpoints",
    tag = "whip",
    responses(
        (status = 200, description = "List of active WHIP endpoints")
    )
)]
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

/// Receive client-side log messages from any browser page (WHIP, WHEP, clocks, …).
///
/// Accepts a JSON array of log entries and writes them to the server log
/// prefixed with `[CLIENT]` so they can be correlated with server-side events.
#[utoipa::path(
    post,
    path = "/api/client-log",
    tag = "whip",
    responses(
        (status = 204, description = "Log entries accepted")
    )
)]
pub async fn client_log(JsonBody(entries): JsonBody<Vec<ClientLogEntry>>) -> impl IntoResponse {
    for entry in &entries {
        match entry.level.as_deref().unwrap_or("info") {
            "error" => error!("[CLIENT] {}", entry.msg),
            "warning" | "warn" => warn!("[CLIENT] {}", entry.msg),
            "debug" => debug!("[CLIENT] {}", entry.msg),
            _ => info!("[CLIENT] {}", entry.msg),
        }
    }
    StatusCode::NO_CONTENT
}

pub use strom_types::whip::ClientLogEntry;

/// Handle WHIP POST request (SDP offer from client).
///
/// Creates a new whipserversrc element for this session, proxies the SDP offer
/// to it, and registers the session with the WhipSessionManager.
#[utoipa::path(
    post,
    path = "/whip/{endpoint_id}",
    tag = "whip",
    params(
        ("endpoint_id" = String, Path, description = "WHIP endpoint identifier")
    ),
    responses(
        (status = 201, description = "WHIP session created, SDP answer returned", content_type = "application/sdp"),
        (status = 404, description = "WHIP endpoint not found"),
        (status = 502, description = "Proxy error forwarding to internal WHIP server"),
        (status = 503, description = "WHIP element busy, retry in a moment")
    )
)]
pub async fn whip_post(
    State(state): State<AppState>,
    Path(endpoint_id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    // Check that this endpoint is registered
    if !state.whip_registry().contains(&endpoint_id).await {
        warn!("WHIP endpoint not found: {}", endpoint_id);
        return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
    }

    // Get the endpoint config from the session manager
    let config = match state
        .whip_session_manager()
        .get_endpoint_config(&endpoint_id)
    {
        Some(c) => c,
        None => {
            warn!(
                "WHIP endpoint config not found for '{}' in session manager",
                endpoint_id
            );
            return (StatusCode::NOT_FOUND, "WHIP endpoint not configured").into_response();
        }
    };

    // Allocate a slot for this session (pre-allocate with a temporary resource_id,
    // will be updated when we learn the real resource_id from the Location header)
    let temp_resource_id = uuid::Uuid::new_v4().to_string();
    let slot = match state
        .whip_session_manager()
        .allocate_slot_or_take_over(&config, &temp_resource_id)
        .await
    {
        Some(s) => s,
        None => {
            warn!(
                "WHIP endpoint '{}': all {} slots occupied by live sessions, rejecting client",
                endpoint_id, config.max_sessions
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "All session slots are occupied",
            )
                .into_response();
        }
    };

    // Create a new whipserversrc for this session in an isolated pipeline.
    // `cleanup_sent` is handed to both the session's callbacks and (below) the
    // session manager, so that every teardown path can stop the session's
    // inactivity watchdog.
    let config_for_session = config.clone();
    let cleanup_tx = state.whip_session_manager().cleanup_sender();
    let cleanup_sent = Arc::new(AtomicBool::new(false));
    let cleanup_sent_for_session = cleanup_sent.clone();
    let temp_id_for_session = temp_resource_id.clone();
    // Handed over through a channel rather than as the blocking task's result:
    // if this POST is cancelled (the client went away) while the session is
    // built, the task still runs to the end, and the session it built must be
    // given back by it, since nobody is left to receive it.
    let (created_tx, created_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let created = create_whipserversrc_for_session(
            &config_for_session,
            slot,
            cleanup_tx,
            cleanup_sent_for_session.clone(),
        );
        match created_tx.send(created) {
            Ok(()) => {}
            Err(Ok(created)) => drop(PendingSession::new(
                config_for_session,
                slot,
                temp_id_for_session,
                created,
                cleanup_sent_for_session,
            )),
            Err(Err(_)) => {
                config_for_session.release_slot(slot, &temp_id_for_session);
                cleanup_sent_for_session.store(true, Ordering::SeqCst);
            }
        }
    });
    let created = match created_rx.await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            error!("Failed to create whipserversrc for session: {}", e);
            config.release_slot(slot, &temp_resource_id);
            // Abandoned session: stop its inactivity watchdog. Every early return
            // below does the same — a watchdog left running outlives its session.
            cleanup_sent.store(true, Ordering::SeqCst);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create session: {}", e),
            )
                .into_response();
        }
        Err(_) => {
            error!("Creating the whipserversrc for a WHIP session panicked");
            config.release_slot(slot, &temp_resource_id);
            cleanup_sent.store(true, Ordering::SeqCst);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let port = created.port;
    info!(
        "WHIP POST for endpoint '{}': created whipserversrc on port {} (slot {})",
        endpoint_id, port, slot
    );
    // From here on every way out of this handler gives the session back,
    // including the handler being dropped mid-await: see `PendingSession`.
    let pending = PendingSession::new(
        config.clone(),
        slot,
        temp_resource_id,
        created,
        cleanup_sent,
    );

    // Read the request body
    let body_bytes = match axum::body::to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            pending.abandon();
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

    // Retry-loop proxy POST to the new whipserversrc (handles HTTP server startup delay)
    let internal_url = format!("http://127.0.0.1:{}/whip/endpoint", port);

    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create HTTP client: {}", e);
            pending.abandon();
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    // Retry up to 10 times with 200ms backoff (whipserversrc HTTP server needs ~500ms+ to start)
    let max_attempts = 10;
    let mut result = None;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        match forward_whip_post(
            &client,
            &internal_url,
            content_type,
            &body_bytes,
            &auth_header,
        )
        .await
        {
            Ok(tuple) => {
                if attempt > 0 {
                    info!(
                        "WHIP POST proxy succeeded on attempt {} for port {}",
                        attempt + 1,
                        port
                    );
                }
                result = Some(tuple);
                break;
            }
            Err(resp) => {
                debug!(
                    "WHIP POST proxy attempt {} failed for port {}, retrying...",
                    attempt + 1,
                    port
                );
                if attempt == max_attempts - 1 {
                    pending.abandon();
                    warn!(
                        "WHIP: All {} proxy attempts failed for endpoint '{}'",
                        max_attempts, endpoint_id
                    );
                    return *resp;
                }
            }
        }
    }

    let (status, resp_headers, resp_body) = match result {
        Some(tuple) => tuple,
        None => {
            pending.abandon();
            return (StatusCode::SERVICE_UNAVAILABLE, "WHIP element not ready").into_response();
        }
    };

    if status.is_server_error() || status.is_client_error() {
        let body_str = std::str::from_utf8(&resp_body).unwrap_or("<non-utf8>");
        warn!(
            "WHIP: Internal server returned {} for endpoint '{}': {}",
            status, endpoint_id, body_str
        );
        // Teardown element and release slot on any error response — the session
        // cannot be used and would otherwise occupy the slot until the inactivity
        // watchdog fires.
        pending.abandon();
        return build_whip_post_response(
            &endpoint_id,
            status,
            &resp_headers,
            resp_body,
            Some(config.max_video_bitrate_kbps),
        )
        .into_response();
    }

    // Register the session under the resource_id the Location header names.
    if let Err(why) = register_from_location(
        state.whip_session_manager(),
        pending,
        &endpoint_id,
        &resp_headers,
    ) {
        warn!(
            "WHIP: Internal server answered {} for endpoint '{}' with {}, session not registered",
            status, endpoint_id, why
        );
        return (
            StatusCode::BAD_GATEWAY,
            "Internal WHIP server did not name the session",
        )
            .into_response();
    }

    build_whip_post_response(
        &endpoint_id,
        status,
        &resp_headers,
        resp_body,
        Some(config.max_video_bitrate_kbps),
    )
    .into_response()
}

/// A session created for a POST and not registered yet: everything the POST
/// has to give back if it does not end in a registered session.
///
/// Dropping it gives the session back, so every way out of `whip_post` does,
/// including the handler future being dropped while it awaits (the client
/// disconnected during the proxy's retries, say). Only `into_parts`, for
/// registration, takes the session out without giving it back.
struct PendingSession(Option<PendingParts>);

struct PendingParts {
    config: Arc<WhipEndpointConfig>,
    slot: usize,
    /// The id the slot was claimed under until the answer names the session.
    temp_resource_id: String,
    element: gstreamer::Element,
    session_pipeline: gstreamer::Pipeline,
    port: u16,
    cleanup_sent: Arc<AtomicBool>,
    activity: Arc<SessionActivity>,
}

impl PendingSession {
    fn new(
        config: Arc<WhipEndpointConfig>,
        slot: usize,
        temp_resource_id: String,
        created: CreatedSession,
        cleanup_sent: Arc<AtomicBool>,
    ) -> Self {
        let CreatedSession {
            element,
            session_pipeline,
            port,
            activity,
        } = created;
        Self(Some(PendingParts {
            config,
            slot,
            temp_resource_id,
            element,
            session_pipeline,
            port,
            cleanup_sent,
            activity,
        }))
    }

    /// Give the session up: release its slot, stop its watchdog, and take its
    /// pipeline to NULL. Dropping a session pipeline that is still PLAYING
    /// leaves its whipserversrc's sockets and threads behind, and a slot
    /// left held under the temporary id is never released by anyone.
    fn abandon(self) {
        drop(self);
    }

    /// Take the session out for registration; it is no longer given back.
    fn into_parts(mut self) -> PendingParts {
        self.0
            .take()
            .expect("a PendingSession is consumed only once")
    }
}

impl Drop for PendingSession {
    fn drop(&mut self) {
        let Some(PendingParts {
            config,
            slot,
            temp_resource_id,
            element,
            session_pipeline,
            cleanup_sent,
            ..
        }) = self.0.take()
        else {
            return;
        };
        config.release_slot(slot, &temp_resource_id);
        cleanup_sent.store(true, Ordering::SeqCst);
        let store = config.dynamic_webrtcbin_store.clone();
        let block_id = config.instance_id.clone();
        let teardown = move || {
            WhipSessionManager::teardown_session_pipeline(&session_pipeline);
            // Kept alive until its pipeline has reached NULL.
            drop(element);
            // Drop the abandoned session's webrtcbin from the stats store.
            WhipSessionManager::cleanup_dynamic_webrtcbin_store(&store, &block_id);
        };
        // A NULL transition blocks, so it runs off the async runtime. The
        // blocking pool needs a runtime; a drop outside one (a blocking
        // thread, a runtime shutting down) gets a thread of its own.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(teardown);
            }
            Err(_) => {
                std::thread::spawn(teardown);
            }
        }
    }
}

/// Register `pending` under the resource_id in the internal server's Location
/// header (`/whip/resource/{resource_id}`). Returns the resource_id, or, when
/// the header is missing or unusable, abandons the session and says why.
fn register_from_location(
    manager: &WhipSessionManager,
    pending: PendingSession,
    endpoint_id: &str,
    headers: &reqwest::header::HeaderMap,
) -> Result<String, String> {
    let resource_id = match headers.get(header::LOCATION).map(|l| l.to_str()) {
        None => Err("no Location header".to_string()),
        Some(Err(_)) => Err("a Location header that is not valid UTF-8".to_string()),
        Some(Ok(location)) => location
            .strip_prefix("/whip/resource/")
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("an unexpected Location '{}'", location)),
    };
    let resource_id = match resource_id {
        Ok(id) => id,
        Err(why) => {
            pending.abandon();
            return Err(why);
        }
    };

    let PendingParts {
        config,
        slot,
        temp_resource_id,
        element,
        session_pipeline,
        port,
        cleanup_sent,
        activity,
    } = pending.into_parts();
    // Update slot assignment from temp_resource_id to real resource_id
    if !config.rename_slot_holder(slot, &temp_resource_id, &resource_id) {
        warn!(
            "WHIP: Slot {} on endpoint '{}' is no longer held by this POST (session '{}')",
            slot, endpoint_id, resource_id
        );
    }
    info!(
        "WHIP: Registering session resource_id='{}' on port {} for endpoint '{}' (slot {})",
        resource_id, port, endpoint_id, slot
    );
    let registered = manager.register_session(NewWhipSession {
        resource_id: resource_id.clone(),
        port,
        element,
        session_pipeline,
        endpoint_id: endpoint_id.to_string(),
        slot,
        config,
        cleanup_sent,
        activity,
    });
    if !registered {
        warn!(
            "WHIP: Session '{}' was cleaned up instead of registered (ICE failed early, or its flow stopped while the POST was in flight)",
            resource_id
        );
    }
    Ok(resource_id)
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
    // Boxed: an axum `Response` Err-variant trips clippy::result_large_err.
    Box<Response>,
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
            return Err(Box::new(
                (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response(),
            ));
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();
    let resp_body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to read proxy response: {}", e);
            return Err(Box::new(
                (StatusCode::BAD_GATEWAY, "Failed to read response").into_response(),
            ));
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
    max_video_bitrate_kbps: Option<u32>,
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
        let patched = fix_video_bitrate_hints(&patched, max_video_bitrate_kbps);
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
///
/// Looks up the session's port by resource_id and proxies the PATCH.
#[utoipa::path(
    patch,
    path = "/whip/{endpoint_id}/resource/{resource_id}",
    tag = "whip",
    params(
        ("endpoint_id" = String, Path, description = "WHIP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHIP resource/session identifier")
    ),
    responses(
        (status = 204, description = "ICE candidates accepted"),
        (status = 404, description = "WHIP endpoint or session not found"),
        (status = 502, description = "Proxy error forwarding to internal WHIP server")
    )
)]
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

    // Look up the session's port by resource_id
    let port = match state.whip_session_manager().get_session_port(&resource_id) {
        Some(port) => port,
        None => {
            // Fall back to endpoint-level check for better error messages
            if !state.whip_registry().contains(&endpoint_id).await {
                return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
            }
            return (StatusCode::NOT_FOUND, "WHIP session not found").into_response();
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
/// Proxies the DELETE to the session's whipserversrc, then tears down the element
/// and removes the session from the session manager.
#[utoipa::path(
    delete,
    path = "/whip/{endpoint_id}/resource/{resource_id}",
    tag = "whip",
    params(
        ("endpoint_id" = String, Path, description = "WHIP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHIP resource/session identifier")
    ),
    responses(
        (status = 200, description = "WHIP session deleted"),
        (status = 404, description = "WHIP endpoint or session not found"),
        (status = 502, description = "Proxy error forwarding to internal WHIP server")
    )
)]
pub async fn whip_resource_delete(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
) -> impl IntoResponse {
    info!(
        "WHIP DELETE for endpoint: {}, resource: {}",
        endpoint_id, resource_id
    );

    // Look up and remove the session (returns element, session_pipeline, endpoint_id, port, slot)
    let (element, session_pipeline, session_endpoint_id, _port, slot) =
        match state.whip_session_manager().remove_session(&resource_id) {
            Some(tuple) => tuple,
            None => {
                if !state.whip_registry().contains(&endpoint_id).await {
                    return (StatusCode::NOT_FOUND, "WHIP endpoint not found").into_response();
                }
                // Session already removed (e.g., pad-removed cleanup) - return OK
                info!(
                    "WHIP DELETE: session '{}' not found (may already be cleaned up)",
                    resource_id
                );
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }
        };

    // Release the slot so new sessions can use it
    let webrtcbin_store = if let Some(config) = state
        .whip_session_manager()
        .get_endpoint_config(&session_endpoint_id)
    {
        config.release_slot(slot, &resource_id);
        Some((
            config.dynamic_webrtcbin_store.clone(),
            config.instance_id.clone(),
        ))
    } else {
        None
    };

    // Tear down the session pipeline directly — no need to proxy the DELETE since
    // set_state(Null) will clean up the whipserversrc and its WebRTC session.
    // Proxying the DELETE first would cause a race: whipserversrc starts internal
    // teardown (puts bins in PAUSED) before our set_state(Null) can cascade properly.
    let _ = tokio::task::spawn_blocking(move || {
        WhipSessionManager::teardown_session_pipeline(&session_pipeline);
        drop(element);
        // Remove stale webrtcbin entries so frontend stops showing dead stats
        if let Some((store, block_id)) = webrtcbin_store {
            WhipSessionManager::cleanup_dynamic_webrtcbin_store(&store, &block_id);
        }
    })
    .await;

    info!(
        "WHIP DELETE: session '{}' for endpoint '{}' cleaned up (slot {} released)",
        resource_id, session_endpoint_id, slot
    );

    Response::builder()
        .status(StatusCode::OK)
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
#[utoipa::path(
    options,
    path = "/whip/{endpoint_id}",
    tag = "whip",
    params(
        ("endpoint_id" = String, Path, description = "WHIP endpoint identifier")
    ),
    responses(
        (status = 204, description = "CORS preflight response")
    )
)]
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
#[utoipa::path(
    options,
    path = "/whip/{endpoint_id}/resource/{resource_id}",
    tag = "whip",
    params(
        ("endpoint_id" = String, Path, description = "WHIP endpoint identifier"),
        ("resource_id" = String, Path, description = "WHIP resource/session identifier")
    ),
    responses(
        (status = 204, description = "CORS preflight response")
    )
)]
pub async fn whip_resource_options() -> impl IntoResponse {
    whip_options().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whip_session_manager::SlotOutput;
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use std::time::{Duration, Instant};

    /// A manager with one single-slot endpoint, its slot claimed under a
    /// temporary id by a POST whose session pipeline is PLAYING.
    fn pending_post() -> (WhipSessionManager, PendingSession, gst::Pipeline) {
        gst::init().expect("gstreamer init");
        let manager = WhipSessionManager::new();
        manager.register_endpoint(
            "endpoint".to_string(),
            WhipEndpointConfig::for_tests("endpoint", 1),
        );
        let config = manager.get_endpoint_config("endpoint").unwrap();
        assert_eq!(config.allocate_slot("temp-id"), Some(0));

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("fakesrc")
            .property("is-live", true)
            .build()
            .expect("fakesrc is part of gstreamer core");
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink is part of gstreamer core");
        pipeline.add_many([&src, &sink]).unwrap();
        src.link(&sink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let pending = PendingSession(Some(PendingParts {
            config,
            slot: 0,
            temp_resource_id: "temp-id".to_string(),
            element: src,
            session_pipeline: pipeline.clone(),
            port: 40100,
            cleanup_sent: Arc::new(AtomicBool::new(false)),
            activity: Arc::new(SessionActivity::new(
                Instant::now(),
                Arc::new(SlotOutput::new(Instant::now())),
            )),
        }));
        (manager, pending, pipeline)
    }

    /// The bug this guards: an answer the session cannot be registered from
    /// dropped the session pipeline while PLAYING and left its slot held
    /// under the temporary id, which nothing ever releases.
    async fn assert_abandoned(headers: reqwest::header::HeaderMap) {
        let (manager, pending, pipeline) = pending_post();
        let (config, cleanup_sent) = parts_of(&pending);

        let result = register_from_location(&manager, pending, "endpoint", &headers);

        assert!(result.is_err(), "nothing to register from: {:?}", result);
        assert_given_back(&config, &cleanup_sent, &pipeline).await;
    }

    fn parts_of(pending: &PendingSession) -> (Arc<WhipEndpointConfig>, Arc<AtomicBool>) {
        let parts = pending.0.as_ref().expect("not consumed yet");
        (parts.config.clone(), parts.cleanup_sent.clone())
    }

    async fn assert_given_back(
        config: &WhipEndpointConfig,
        cleanup_sent: &AtomicBool,
        pipeline: &gst::Pipeline,
    ) {
        assert_eq!(
            config.slot_assignments.read().unwrap()[0],
            None,
            "the slot must be released"
        );
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "the session's watchdog must be stopped"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while pipeline.current_state() != gst::State::Null && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            pipeline.current_state(),
            gst::State::Null,
            "the session pipeline must be taken down to NULL"
        );
    }

    /// A POST whose client goes away mid-await (during the proxy's retries,
    /// say) has its handler future dropped, and with it the pending session.
    /// No error path runs, so the drop itself must give the session back.
    #[tokio::test]
    async fn a_dropped_pending_session_gives_the_session_up() {
        let (_manager, pending, pipeline) = pending_post();
        let (config, cleanup_sent) = parts_of(&pending);

        drop(pending);

        assert_given_back(&config, &cleanup_sent, &pipeline).await;
    }

    /// Dropped outside any tokio runtime (a blocking thread, a runtime
    /// shutting down), it still gives the session back.
    #[test]
    fn a_pending_session_dropped_outside_a_runtime_gives_the_session_up() {
        let (_manager, pending, pipeline) = pending_post();
        let (config, cleanup_sent) = parts_of(&pending);

        drop(pending);

        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(assert_given_back(&config, &cleanup_sent, &pipeline));
    }

    #[tokio::test]
    async fn an_answer_without_a_location_gives_the_session_up() {
        assert_abandoned(reqwest::header::HeaderMap::new()).await;
    }

    #[tokio::test]
    async fn an_answer_with_an_unexpected_location_gives_the_session_up() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(header::LOCATION, "/somewhere/else".parse().unwrap());
        assert_abandoned(headers).await;
    }

    #[tokio::test]
    async fn an_answer_with_a_non_utf8_location_gives_the_session_up() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            header::LOCATION,
            reqwest::header::HeaderValue::from_bytes(b"/whip/resource/\xff").unwrap(),
        );
        assert_abandoned(headers).await;
    }

    /// The counterpart: a usable Location registers the session and moves
    /// the slot to its resource_id.
    #[tokio::test]
    async fn an_answer_with_a_location_registers_the_session() {
        let (manager, pending, pipeline) = pending_post();
        let (config, cleanup_sent) = parts_of(&pending);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(header::LOCATION, "/whip/resource/abc".parse().unwrap());

        let result = register_from_location(&manager, pending, "endpoint", &headers);

        assert_eq!(result.as_deref(), Ok("abc"));
        assert_eq!(manager.get_session_port("abc"), Some(40100));
        assert_eq!(
            config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("abc")
        );
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a registered session keeps its watchdog"
        );
        // The fixture's live pipeline may still be completing its async
        // change to PLAYING; wait for it rather than read where it is.
        let (result, state, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        assert_eq!(
            (result, state),
            (Ok(gst::StateChangeSuccess::Success), gst::State::Playing),
            "a registered session's pipeline is not torn down"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }
}
