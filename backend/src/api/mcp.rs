//! MCP Streamable HTTP endpoint handlers.
//!
//! Implements the MCP 2025-03-26 Streamable HTTP transport specification.
//!
//! ## Endpoints
//!
//! - `POST /api/mcp` - Send JSON-RPC requests (returns JSON or SSE)
//! - `GET /api/mcp` - Open SSE stream for server-initiated messages
//! - `DELETE /api/mcp` - Terminate a session

use crate::json_rejection::JsonBody;
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension, Json,
};
use futures::stream::Stream;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_sessions::Session;
use tracing::{debug, info, warn};

use crate::auth::AuthConfig;
use crate::mcp::{
    handler::{JsonRpcRequest, McpHandler},
    session::McpEvent,
    McpSessionManager,
};
use crate::state::AppState;

/// Header name for MCP session ID.
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Validate the Origin header for DNS rebinding protection.
///
/// What this defends against is a page on some other site scripting a browser
/// into driving a Strom instance the victim can reach. So the rule is
/// same-origin: an Origin naming this very host is fine, localhost is fine
/// (the dev case), and anything else is a cross-site caller and rejected.
///
/// Matching only localhost — as this did — locked out every deployment served
/// under a real hostname while letting non-browser callers through untouched,
/// which is the wrong half of the problem.
fn validate_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // No Origin header - not a browser request (the common MCP client case)
        return true;
    };
    let Ok(origin_str) = origin.to_str() else {
        warn!("Rejecting MCP request with a non-UTF-8 Origin header");
        return false;
    };

    let origin_host = origin_str
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin_str);

    // Loopback, for local development. An IPv6 literal keeps its brackets, so
    // the port only starts at a colon outside them.
    let host_only = if let Some(end) = origin_host.find(']') {
        &origin_host[..=end]
    } else {
        origin_host
            .split_once(':')
            .map(|(h, _)| h)
            .unwrap_or(origin_host)
    };
    if host_only == "localhost" || host_only == "127.0.0.1" || host_only == "[::1]" {
        return true;
    }

    // Same origin: the Origin names the host this request was sent to.
    if let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        if origin_host == host {
            return true;
        }
    }

    warn!("Rejecting MCP request from origin: {}", origin_str);
    false
}

/// Extract session ID from headers.
fn get_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Validate MCP authentication.
///
/// Accepts every credential the rest of the API accepts, because a deployment
/// secured one way must not lock MCP out. Checked in order:
///
/// 1. `X-API-Key` header (preferred for MCP clients)
/// 2. `Authorization: Bearer <token>` (API key or native GUI token)
/// 3. The login session cookie
///
/// Authentication is enabled as soon as *any* method is configured (see
/// `AuthConfig::from_env`), so an instance with an admin user but no
/// `STROM_API_KEY` reaches this with only the cookie to offer. Checking the
/// key alone made MCP unreachable on exactly those instances.
///
/// Returns Ok(()) if authenticated or auth is disabled, Err(Response) otherwise.
#[allow(clippy::result_large_err)]
fn validate_mcp_auth(
    auth_config: &AuthConfig,
    headers: &HeaderMap,
    session_authenticated: bool,
) -> Result<(), Response> {
    // If authentication is disabled, allow all requests
    if !auth_config.enabled {
        return Ok(());
    }

    // Check X-API-Key header (preferred for MCP clients)
    if let Some(api_key_header) = headers.get("x-api-key") {
        if let Ok(key) = api_key_header.to_str() {
            if auth_config.verify_api_key(key) {
                return Ok(());
            }
        }
    }

    // Check Authorization: Bearer header
    if let Some(auth_header) = headers.get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                if auth_config.verify_api_key(token) || auth_config.verify_native_gui_token(token) {
                    return Ok(());
                }
            }
        }
    }

    // Check the login session cookie
    if session_authenticated {
        return Ok(());
    }

    // No valid authentication found
    warn!("MCP: Authentication failed - no valid credential provided");
    Err((
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "Authentication required. Provide X-API-Key header, Authorization: Bearer <api-key>, or a logged-in session cookie"})),
    )
        .into_response())
}

/// POST /api/mcp - Handle JSON-RPC requests.
///
/// Accepts JSON-RPC requests and returns either:
/// - `application/json` for simple responses
/// - `text/event-stream` for streaming responses (not implemented yet)
///
/// The `Mcp-Session-Id` header is assigned on initialize and required for subsequent requests.
#[utoipa::path(
    post,
    path = "/api/mcp",
    tag = "mcp",
    request_body = JsonRpcRequest,
    responses(
        (status = 200, description = "JSON-RPC response", content_type = "application/json"),
        (status = 202, description = "Notification accepted (no response body)"),
        (status = 400, description = "Invalid request or session required"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "Invalid origin (DNS rebinding protection)")
    )
)]
pub async fn mcp_post(
    State(state): State<AppState>,
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    session: Session,
    headers: HeaderMap,
    JsonBody(request): JsonBody<JsonRpcRequest>,
) -> Response {
    // Validate authentication
    let session_ok = crate::auth::session_is_authenticated(&session).await;
    if let Err(response) = validate_mcp_auth(&auth_config, &headers, session_ok) {
        return response;
    }

    // Validate origin for DNS rebinding protection
    if !validate_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Invalid origin"})),
        )
            .into_response();
    }

    let session_id = get_session_id(&headers);
    debug!(
        "MCP POST: method={}, session={:?}",
        request.method, session_id
    );

    // Handle initialize specially - create session
    if request.method == "initialize" {
        let new_session_id = sessions.create_session().await;
        info!("MCP: New session initialized: {}", new_session_id);

        if let Some(response) = McpHandler::handle_request(&state, request).await {
            let json_response = serde_json::to_string(&response).unwrap_or_default();
            let mut resp = (StatusCode::OK, json_response).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if let Ok(hv) = HeaderValue::from_str(&new_session_id) {
                resp.headers_mut()
                    .insert(HeaderName::from_static(MCP_SESSION_ID_HEADER), hv);
            }
            return resp;
        }
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // For other methods, validate session exists
    if let Some(ref sid) = session_id {
        if !sessions.session_exists(sid).await {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
        sessions.touch(sid).await;
    }
    // Note: We don't require session for all methods to allow simpler clients

    // Handle the request
    if let Some(response) = McpHandler::handle_request(&state, request).await {
        let json_response = serde_json::to_string(&response).unwrap_or_default();

        let mut resp = (StatusCode::OK, json_response).into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        // Include session ID in response if we have one
        if let Some(sid) = session_id {
            if let Ok(hv) = HeaderValue::from_str(&sid) {
                resp.headers_mut()
                    .insert(HeaderName::from_static(MCP_SESSION_ID_HEADER), hv);
            }
        }

        return resp;
    }

    // Notification - no response needed
    StatusCode::ACCEPTED.into_response()
}

/// GET /api/mcp - Open SSE stream for server-initiated messages.
///
/// Opens a Server-Sent Events stream for receiving server-initiated
/// JSON-RPC messages (notifications, requests from server).
#[utoipa::path(
    get,
    path = "/api/mcp",
    tag = "mcp",
    responses(
        (status = 200, description = "SSE stream for server-initiated messages", content_type = "text/event-stream"),
        (status = 400, description = "Mcp-Session-Id header required"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "Invalid origin"),
        (status = 404, description = "Session not found")
    )
)]
pub async fn mcp_get(
    State(state): State<AppState>,
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    session: Session,
    headers: HeaderMap,
) -> Response {
    // Validate authentication
    let session_ok = crate::auth::session_is_authenticated(&session).await;
    if let Err(response) = validate_mcp_auth(&auth_config, &headers, session_ok) {
        return response;
    }

    // Validate origin
    if !validate_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Invalid origin"})),
        )
            .into_response();
    }

    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Mcp-Session-Id header required for SSE stream"})),
            )
                .into_response();
        }
    };

    // Verify session exists
    if !sessions.session_exists(&session_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Session not found"})),
        )
            .into_response();
    }
    sessions.touch(&session_id).await;

    // Subscribe to session events and Strom events
    let session_rx = match sessions.subscribe(&session_id).await {
        Some(rx) => rx,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Session not found"})),
            )
                .into_response();
        }
    };

    // Also subscribe to Strom's event broadcaster for real-time updates
    let strom_rx = state.events().subscribe();

    info!("MCP: SSE stream opened for session {}", session_id);

    // Create combined stream
    let stream = create_sse_stream(session_id.clone(), session_rx, strom_rx);

    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Create an SSE stream that combines MCP session events and Strom events.
fn create_sse_stream(
    _session_id: String,
    session_rx: tokio::sync::broadcast::Receiver<McpEvent>,
    strom_rx: tokio::sync::broadcast::Receiver<strom_types::StromEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    // Convert session events to SSE events
    let session_stream = BroadcastStream::new(session_rx).filter_map(|result| {
        match result {
            Ok(McpEvent::JsonRpc(json)) => Some(Ok(Event::default().data(json))),
            Err(_) => None, // Lagged or closed
        }
    });

    // Convert Strom events to MCP notifications
    let strom_stream = BroadcastStream::new(strom_rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                // Convert Strom events to MCP notifications
                let notification = match &event {
                    strom_types::StromEvent::FlowCreated { flow_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/flowCreated",
                        "params": { "flow_id": flow_id.to_string() }
                    })),
                    strom_types::StromEvent::FlowUpdated { flow_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/flowUpdated",
                        "params": { "flow_id": flow_id.to_string() }
                    })),
                    strom_types::StromEvent::FlowDeleted { flow_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/flowDeleted",
                        "params": { "flow_id": flow_id.to_string() }
                    })),
                    strom_types::StromEvent::FlowStarted { flow_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/flowStarted",
                        "params": { "flow_id": flow_id.to_string() }
                    })),
                    strom_types::StromEvent::FlowStopped { flow_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/flowStopped",
                        "params": { "flow_id": flow_id.to_string() }
                    })),
                    strom_types::StromEvent::PipelineError { flow_id, error, .. } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/pipelineError",
                        "params": { "flow_id": flow_id.to_string(), "error": error }
                    })),
                    strom_types::StromEvent::PipelineWarning {
                        flow_id, warning, ..
                    } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/strom/pipelineWarning",
                        "params": { "flow_id": flow_id.to_string(), "warning": warning }
                    })),
                    // Everything else is dropped. The mapped events above are
                    // the state changes an assistant acts on; the rest of the
                    // bus is telemetry that arrives many times per second per
                    // flow (loudness, spectrum, QoS, latency, player position,
                    // buffer-age probes, thread and PTP stats). Forwarding it
                    // generically buried the useful notifications and shipped
                    // the payload as a JSON string inside JSON. A client that
                    // wants the full firehose has `WS /api/ws`.
                    _ => None,
                };

                notification.map(|n| {
                    let json_str = serde_json::to_string(&n).unwrap_or_default();
                    Ok(Event::default().data(json_str))
                })
            }
            Err(_) => None, // Lagged or closed
        }
    });

    // Merge both streams
    futures::stream::select(session_stream, strom_stream)
}

/// DELETE /api/mcp - Terminate a session.
///
/// Terminates the session identified by the `Mcp-Session-Id` header.
#[utoipa::path(
    delete,
    path = "/api/mcp",
    tag = "mcp",
    responses(
        (status = 204, description = "Session terminated successfully"),
        (status = 400, description = "Mcp-Session-Id header required"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "Invalid origin"),
        (status = 404, description = "Session not found")
    )
)]
pub async fn mcp_delete(
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    session: Session,
    headers: HeaderMap,
) -> Response {
    // Validate authentication
    let session_ok = crate::auth::session_is_authenticated(&session).await;
    if let Err(response) = validate_mcp_auth(&auth_config, &headers, session_ok) {
        return response;
    }

    // Validate origin
    if !validate_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let session_id = match get_session_id(&headers) {
        Some(id) => id,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    if sessions.terminate(&session_id).await {
        info!("MCP: Session terminated: {}", session_id);
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn password_only_config() -> AuthConfig {
        // An instance secured with an admin login and no API key. This is the
        // shape that used to lock MCP out entirely.
        AuthConfig {
            admin_user: Some("admin".to_string()),
            admin_password_hash: Some("not-a-real-hash".to_string()),
            api_key: None,
            native_gui_token: None,
            enabled: true,
        }
    }

    fn api_key_config() -> AuthConfig {
        AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: Some("secret".to_string()),
            native_gui_token: None,
            enabled: true,
        }
    }

    #[test]
    fn no_origin_header_is_accepted() {
        // Non-browser MCP clients send no Origin.
        assert!(validate_origin(&headers(&[])));
    }

    #[test]
    fn localhost_origins_are_accepted() {
        for origin in [
            "http://localhost",
            "http://localhost:8080",
            "https://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            assert!(
                validate_origin(&headers(&[("origin", origin)])),
                "expected {} to be accepted",
                origin
            );
        }
    }

    #[test]
    fn same_host_origin_is_accepted() {
        // A Strom served under a real hostname: the UI's own origin must work.
        assert!(validate_origin(&headers(&[
            ("origin", "https://strom.example.com"),
            ("host", "strom.example.com"),
        ])));
        assert!(validate_origin(&headers(&[
            ("origin", "https://strom.example.com:8443"),
            ("host", "strom.example.com:8443"),
        ])));
    }

    #[test]
    fn cross_site_origin_is_rejected() {
        // The actual DNS rebinding case: another site driving this instance.
        assert!(!validate_origin(&headers(&[
            ("origin", "https://evil.example.net"),
            ("host", "strom.example.com"),
        ])));
        // A host mismatch on port alone is still cross-origin.
        assert!(!validate_origin(&headers(&[
            ("origin", "https://strom.example.com:9999"),
            ("host", "strom.example.com:8443"),
        ])));
    }

    #[test]
    fn auth_disabled_accepts_anything() {
        let config = AuthConfig {
            admin_user: None,
            admin_password_hash: None,
            api_key: None,
            native_gui_token: None,
            enabled: false,
        };
        assert!(validate_mcp_auth(&config, &headers(&[]), false).is_ok());
    }

    #[test]
    fn api_key_is_accepted_in_either_header() {
        let config = api_key_config();
        assert!(validate_mcp_auth(&config, &headers(&[("x-api-key", "secret")]), false).is_ok());
        assert!(validate_mcp_auth(
            &config,
            &headers(&[("authorization", "Bearer secret")]),
            false
        )
        .is_ok());
        assert!(validate_mcp_auth(&config, &headers(&[("x-api-key", "wrong")]), false).is_err());
        assert!(validate_mcp_auth(&config, &headers(&[]), false).is_err());
    }

    #[test]
    fn a_logged_in_session_is_accepted_without_an_api_key() {
        // The regression this guards: authentication is enabled by the admin
        // user alone, so requiring an API key left no usable credential and
        // every MCP request came back 401.
        let config = password_only_config();
        assert!(!config.has_api_key_auth());

        assert!(validate_mcp_auth(&config, &headers(&[]), true).is_ok());
        assert!(validate_mcp_auth(&config, &headers(&[]), false).is_err());
    }

    #[test]
    fn native_gui_token_is_accepted() {
        let mut config = api_key_config();
        let token = config.generate_native_gui_token();
        assert!(validate_mcp_auth(
            &config,
            &headers(&[("authorization", &format!("Bearer {}", token))]),
            false
        )
        .is_ok());
    }
}
