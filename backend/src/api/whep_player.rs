//! WHEP Player - serves web pages and static assets for playing WHEP streams.
//!
//! URL Structure:
//! - `/player/whep` - HTML page for playing a single WHEP stream
//! - `/player/whep-streams` - HTML page listing all active WHEP streams
//! - `/static/whep.css` - Shared CSS styles
//! - `/static/whep.js` - Shared JavaScript for WebRTC connections
//! - `/whep/{endpoint_id}` - Proxy to internal WHEP servers
//! - `/api/whep-streams` - JSON API listing all active WHEP endpoints

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use crate::assets::WhepAssets;
use crate::state::AppState;

// ============================================================================
// Shared Static Assets (served from embedded files)
// ============================================================================

/// Shared CSS styles for WHEP player pages (egui-inspired dark theme)
pub async fn whep_css() -> impl IntoResponse {
    match WhepAssets::get("whep.css") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/css")
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(content.data))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("CSS not found"))
            .unwrap(),
    }
}

/// Shared JavaScript for WHEP WebRTC connections
pub async fn whep_js() -> impl IntoResponse {
    match WhepAssets::get("whep.js") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/javascript")
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(content.data))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("JavaScript not found"))
            .unwrap(),
    }
}

// ============================================================================
// Player Pages (served from embedded HTML templates)
// ============================================================================

#[derive(Deserialize)]
pub struct WhepPlayerQuery {
    /// The WHEP endpoint URL to connect to (e.g., /whep/my-stream)
    endpoint: Option<String>,
}

/// Serve the WHEP player HTML page.
/// GET /player/whep?endpoint=/whep/my-stream
pub async fn whep_player(Query(params): Query<WhepPlayerQuery>) -> impl IntoResponse {
    let endpoint = params.endpoint.unwrap_or_default();

    match WhepAssets::get("player.html") {
        Some(content) => {
            // Convert to string and replace placeholder
            let html = String::from_utf8_lossy(&content.data);
            let html = html.replace("{{ENDPOINT}}", &endpoint);
            Html(html)
        }
        None => Html("<html><body>Player template not found</body></html>".to_string()),
    }
}

/// Serve the WHEP streams page HTML.
/// GET /player/whep-streams
pub async fn whep_streams_page() -> impl IntoResponse {
    match WhepAssets::get("streams.html") {
        Some(content) => {
            let html = String::from_utf8_lossy(&content.data);
            Html(html.to_string())
        }
        None => Html("<html><body>Streams template not found</body></html>".to_string()),
    }
}

// ============================================================================
// WHEP Proxy (endpoint_id-based routing via WhepRegistry)
// ============================================================================

/// Proxy POST requests to /whep/{endpoint_id}
/// Looks up the internal port from WhepRegistry and forwards to localhost:{port}/whep/endpoint
pub async fn whep_endpoint_proxy(
    State(state): State<AppState>,
    Path(endpoint_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Response {
    // Look up internal port from registry
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/endpoint", port);
    let client = reqwest::Client::new();

    let mut request = client.post(&target_url);

    // Forward content-type header
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        if let Ok(ct) = content_type.to_str() {
            request = request.header(header::CONTENT_TYPE, ct);
        }
    }

    request = request.body(body);

    match request.send().await {
        Ok(response) => {
            let status = response.status();

            // Get Location header for resource URL and rewrite it
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            // Collect all Link headers (WHEP spec: ICE servers sent via Link headers)
            let link_headers: Vec<String> = response
                .headers()
                .get_all(header::LINK)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .collect();

            let body_bytes = response.bytes().await.unwrap_or_default();

            let mut builder = Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
                .header(header::CONTENT_TYPE, "application/sdp")
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                    "POST, DELETE, OPTIONS",
                )
                .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
                .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Location, Link");

            if let Some(loc) = location {
                // Rewrite location from /whep/resource/{id} to /whep/{endpoint_id}/resource/{id}
                let proxy_location = if loc.starts_with("/whep/resource/") {
                    let resource_id = loc.trim_start_matches("/whep/resource/");
                    format!("/whep/{}/resource/{}", endpoint_id, resource_id)
                } else {
                    format!("/whep/{}{}", endpoint_id, loc)
                };
                builder = builder.header(header::LOCATION, proxy_location);
            }

            // Relay all Link headers (for ICE server configuration per WHEP spec)
            for link in link_headers {
                builder = builder.header(header::LINK, link);
            }

            builder.body(Body::from(body_bytes)).unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Proxy DELETE requests to /whep/{endpoint_id}/resource/{resource_id}
pub async fn whep_resource_proxy_delete(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
) -> Response {
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/resource/{}", port, resource_id);
    let client = reqwest::Client::new();

    match client.delete(&target_url).send().await {
        Ok(response) => {
            let status = response.status();
            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK))
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Handle OPTIONS preflight for /whep/{endpoint_id}
pub async fn whep_endpoint_proxy_options() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

/// Proxy PATCH requests to /whep/{endpoint_id}/resource/{resource_id} for ICE candidates
pub async fn whep_resource_proxy_patch(
    State(state): State<AppState>,
    Path((endpoint_id, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let port = match state.whep_registry().get_port(&endpoint_id).await {
        Some(p) => p,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::from(format!(
                    "WHEP endpoint '{}' not found",
                    endpoint_id
                )))
                .unwrap();
        }
    };

    let target_url = format!("http://127.0.0.1:{}/whep/resource/{}", port, resource_id);
    let client = reqwest::Client::new();

    let mut request = client.patch(&target_url);

    // Forward content-type header (typically application/trickle-ice-sdpfrag)
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        if let Ok(ct) = content_type.to_str() {
            request = request.header(header::CONTENT_TYPE, ct);
        }
    }

    request = request.body(body);

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            Response::builder()
                .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::NO_CONTENT))
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .body(Body::empty())
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Body::from(format!("Proxy error: {}", e)))
            .unwrap(),
    }
}

/// Handle OPTIONS preflight for /whep/{endpoint_id}/resource/{resource_id}
pub async fn whep_resource_proxy_options() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "PATCH, DELETE, OPTIONS",
        )
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .unwrap()
}

// ============================================================================
// WHEP Streams API (JSON)
// ============================================================================

/// Response structure for a WHEP stream.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct WhepStreamInfo {
    /// Unique identifier for the WHEP endpoint
    pub endpoint_id: String,
    /// Stream mode (e.g., "video", "audio", "video+audio")
    pub mode: String,
    /// Whether the stream includes audio
    pub has_audio: bool,
    /// Whether the stream includes video
    pub has_video: bool,
}

/// Response structure for the streams list endpoint.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct WhepStreamsResponse {
    /// List of active WHEP streams
    pub streams: Vec<WhepStreamInfo>,
}

/// GET /api/whep-streams - List all active WHEP streams (JSON API).
#[utoipa::path(
    get,
    path = "/api/whep-streams",
    tag = "whep",
    responses(
        (status = 200, description = "List of active WHEP streams", body = WhepStreamsResponse)
    )
)]
pub async fn list_whep_streams(State(state): State<AppState>) -> axum::Json<WhepStreamsResponse> {
    let endpoints = state.whep_registry().list_all().await;

    let streams = endpoints
        .into_iter()
        .map(|(endpoint_id, entry)| WhepStreamInfo {
            endpoint_id,
            mode: entry.mode.as_str().to_string(),
            has_audio: entry.mode.has_audio(),
            has_video: entry.mode.has_video(),
        })
        .collect();

    axum::Json(WhepStreamsResponse { streams })
}

// ============================================================================
// ICE Servers API
// ============================================================================

/// Response structure for ICE servers endpoint.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct IceServersResponse {
    /// List of ICE server configurations (STUN/TURN)
    pub ice_servers: Vec<IceServer>,
}

/// ICE server configuration for WebRTC.
/// For TURN servers, username and credential are extracted from the URL.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct IceServer {
    /// ICE server URL (e.g., "stun:stun.l.google.com:19302")
    pub urls: String,
    /// Username for TURN server authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Credential for TURN server authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Parse an ICE server URL into the browser-compatible format.
/// TURN URLs with embedded credentials (turn:user:pass@host:port) are parsed
/// into separate urls, username, and credential fields.
///
/// Handles both standard URI format (turn:user:pass@host) and
/// GStreamer format (turn://user:pass@host).
fn parse_ice_server(url: &str) -> IceServer {
    // Check if it's a TURN URL with credentials
    if url.starts_with("turn:") || url.starts_with("turns:") {
        // Determine scheme and strip optional // after scheme
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("turns://") {
            ("turns:", rest)
        } else if let Some(rest) = url.strip_prefix("turn://") {
            ("turn:", rest)
        } else if let Some(rest) = url.strip_prefix("turns:") {
            ("turns:", rest)
        } else if let Some(rest) = url.strip_prefix("turn:") {
            ("turn:", rest)
        } else {
            // Shouldn't happen given the outer if, but be safe
            return IceServer {
                urls: url.to_string(),
                username: None,
                credential: None,
            };
        };

        if let Some(at_pos) = rest.rfind('@') {
            // Has credentials: user:pass@host:port
            let credentials = &rest[..at_pos];
            let host_port = &rest[at_pos + 1..];

            // Split credentials on first ':' (username:password)
            if let Some(colon_pos) = credentials.find(':') {
                let username = &credentials[..colon_pos];
                let password = &credentials[colon_pos + 1..];

                return IceServer {
                    urls: format!("{}{}", scheme, host_port),
                    username: Some(username.to_string()),
                    credential: Some(password.to_string()),
                };
            }
        }
    }

    // STUN server or TURN without embedded credentials
    // Normalize stun:// to stun: for browser compatibility
    let normalized_url = if let Some(rest) = url.strip_prefix("stun://") {
        format!("stun:{}", rest)
    } else if let Some(rest) = url.strip_prefix("turn://") {
        if !url.contains('@') {
            format!("turn:{}", rest)
        } else {
            url.to_string()
        }
    } else if let Some(rest) = url.strip_prefix("turns://") {
        if !url.contains('@') {
            format!("turns:{}", rest)
        } else {
            url.to_string()
        }
    } else {
        url.to_string()
    };

    IceServer {
        urls: normalized_url,
        username: None,
        credential: None,
    }
}

/// GET /api/ice-servers - Get configured ICE servers for WebRTC connections.
#[utoipa::path(
    get,
    path = "/api/ice-servers",
    tag = "whep",
    responses(
        (status = 200, description = "List of configured ICE servers", body = IceServersResponse)
    )
)]
pub async fn get_ice_servers(State(state): State<AppState>) -> axum::Json<IceServersResponse> {
    let ice_servers = state
        .ice_servers()
        .iter()
        .map(|url| parse_ice_server(url))
        .collect();

    axum::Json(IceServersResponse { ice_servers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stun_server() {
        let server = parse_ice_server("stun:stun.l.google.com:19302");
        assert_eq!(server.urls, "stun:stun.l.google.com:19302");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    #[test]
    fn test_parse_turn_server_with_credentials() {
        let server = parse_ice_server("turn:myuser:mypassword@turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert_eq!(server.username, Some("myuser".to_string()));
        assert_eq!(server.credential, Some("mypassword".to_string()));
    }

    #[test]
    fn test_parse_turns_server_with_credentials() {
        let server = parse_ice_server("turns:user:pass@turn.example.com:5349");
        assert_eq!(server.urls, "turns:turn.example.com:5349");
        assert_eq!(server.username, Some("user".to_string()));
        assert_eq!(server.credential, Some("pass".to_string()));
    }

    #[test]
    fn test_parse_turn_server_without_credentials() {
        let server = parse_ice_server("turn:turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    // Tests for GStreamer-style URLs with ://

    #[test]
    fn test_parse_stun_server_with_slashes() {
        let server = parse_ice_server("stun://stun.l.google.com:19302");
        assert_eq!(server.urls, "stun:stun.l.google.com:19302");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }

    #[test]
    fn test_parse_turn_server_with_slashes_and_credentials() {
        let server = parse_ice_server("turn://myuser:mypassword@turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert_eq!(server.username, Some("myuser".to_string()));
        assert_eq!(server.credential, Some("mypassword".to_string()));
    }

    #[test]
    fn test_parse_turns_server_with_slashes_and_credentials() {
        let server = parse_ice_server("turns://user:pass@turn.example.com:5349");
        assert_eq!(server.urls, "turns:turn.example.com:5349");
        assert_eq!(server.username, Some("user".to_string()));
        assert_eq!(server.credential, Some("pass".to_string()));
    }

    #[test]
    fn test_parse_turn_server_with_slashes_without_credentials() {
        let server = parse_ice_server("turn://turn.example.com:3478");
        assert_eq!(server.urls, "turn:turn.example.com:3478");
        assert!(server.username.is_none());
        assert!(server.credential.is_none());
    }
}
