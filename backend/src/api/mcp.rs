//! MCP Streamable HTTP endpoint handlers.
//!
//! Implements the MCP 2025-03-26 Streamable HTTP transport specification.
//!
//! ## Endpoints
//!
//! - `POST /api/mcp` - Send JSON-RPC requests (returns JSON or SSE)
//! - `GET /api/mcp` - Open SSE stream for server-initiated messages
//! - `DELETE /api/mcp` - Terminate a session

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

/// Validate Origin header for DNS rebinding protection.
fn validate_origin(headers: &HeaderMap) -> bool {
    // For local development, we accept requests without Origin
    // or from localhost origins
    if let Some(origin) = headers.get(header::ORIGIN) {
        if let Ok(origin_str) = origin.to_str() {
            // Accept localhost origins
            if origin_str.starts_with("http://localhost")
                || origin_str.starts_with("https://localhost")
                || origin_str.starts_with("http://127.0.0.1")
                || origin_str.starts_with("https://127.0.0.1")
            {
                return true;
            }
            // Reject other origins for security
            warn!("Rejecting MCP request from origin: {}", origin_str);
            return false;
        }
    }
    // No Origin header - accept (common for non-browser clients)
    true
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
/// Checks for API key in:
/// 1. `X-API-Key` header (preferred for MCP)
/// 2. `Authorization: Bearer <token>` header (standard HTTP auth)
///
/// Returns Ok(()) if authenticated or auth is disabled, Err(Response) otherwise.
#[allow(clippy::result_large_err)]
fn validate_mcp_auth(auth_config: &AuthConfig, headers: &HeaderMap) -> Result<(), Response> {
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
                if auth_config.verify_api_key(token) {
                    return Ok(());
                }
            }
        }
    }

    // No valid authentication found
    warn!("MCP: Authentication failed - no valid API key provided");
    Err((
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "Authentication required. Provide X-API-Key header or Authorization: Bearer <api-key>"})),
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
pub async fn mcp_post(
    State(state): State<AppState>,
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    // Validate authentication
    if let Err(response) = validate_mcp_auth(&auth_config, &headers) {
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
pub async fn mcp_get(
    State(state): State<AppState>,
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    headers: HeaderMap,
) -> Response {
    // Validate authentication
    if let Err(response) = validate_mcp_auth(&auth_config, &headers) {
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
                    // Skip high-frequency events to avoid overwhelming the client
                    strom_types::StromEvent::SystemStats(_) => None,
                    strom_types::StromEvent::MeterData { .. } => None,
                    strom_types::StromEvent::Ping => None,
                    // Include other events
                    _ => {
                        // Generic serialization for other events
                        if let Ok(json_str) = serde_json::to_string(&event) {
                            Some(json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/strom/event",
                                "params": { "event": json_str }
                            }))
                        } else {
                            None
                        }
                    }
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
pub async fn mcp_delete(
    Extension(sessions): Extension<McpSessionManager>,
    Extension(auth_config): Extension<Arc<AuthConfig>>,
    headers: HeaderMap,
) -> Response {
    // Validate authentication
    if let Err(response) = validate_mcp_auth(&auth_config, &headers) {
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
