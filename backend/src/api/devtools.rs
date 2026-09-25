//! Reverse proxy for the Chromium DevTools endpoint behind `cefsrc`.
//!
//! An HTML source renders server-side, so an operator cannot click in it: a
//! page behind a login stays on the login screen for as long as the flow runs.
//! Chromium can be driven remotely over the DevTools protocol, and CEF opens
//! that port when `cef.debug_port` is configured — so the operator logs in to
//! the very browser that is on air, and no credential is ever transported.
//!
//! The port itself is unauthenticated and is total control of the browser
//! process, including `file://` reads. Chromium binds it to loopback and it
//! stays there: everything here goes through Strom's own port and Strom's own
//! authentication, the same way WHEP and WHIP are fronted.
//!
//! Three things are proxied, and they are all Chromium's own:
//!
//! - `/json/list`, to find the pages,
//! - `/devtools/*`, the DevTools application, which CEF serves itself,
//! - the per-page WebSocket carrying the protocol.
//!
//! Strom terminates the WebSocket and opens its own to Chromium. That is what
//! keeps the browser's `Origin` check out of the picture: Chromium rejects
//! WebSocket origins it does not know (`--remote-allow-origins`), but our
//! connection carries no origin at all.
//!
//! # This is an instance-wide privilege, not a per-source one
//!
//! One CEF process serves every `cefsrc` in the instance, so there is one
//! debug port and one cookie jar, and each HTML source is a target on that
//! port. The target id decides which page a session *starts* on; it bounds
//! nothing after that. From any target the protocol reaches every cookie in
//! the profile (`Storage.getCookies`), navigates that page anywhere including
//! `file://`, and runs whatever JavaScript it likes.
//!
//! So whoever can open one of these links can reach every HTML source in the
//! instance and everything the browser has ever logged in to. That is fine for
//! an operator running their own instance, and wrong for an instance whose
//! HTML sources belong to different customers: there the isolation has to come
//! from separate Strom instances, which already get separate CEF profiles, and
//! this must stay off.

use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, RawQuery,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use futures::{SinkExt, StreamExt};
use strom_types::devtools::{DevToolsTarget, DevToolsTargets, REMOTE_CONTROL_WARNING};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tower_sessions::Session;
use tracing::{debug, error, info, warn};

/// Where the DevTools endpoint lives, from this instance's configuration.
#[derive(Clone, Copy, Debug)]
pub struct DevToolsConfig {
    /// Chromium's remote debugging port, or `None` when the operator has not
    /// enabled remote control. One port serves every `cefsrc` in the instance,
    /// so two Strom instances on one host need two different ports, the same
    /// way they already need two CEF profile directories.
    pub debug_port: Option<u16>,
    /// Whether this instance terminates TLS itself. Decides whether the
    /// DevTools application is told to open `ws://` or `wss://` back to us —
    /// it will refuse a plaintext socket from a page served over HTTPS.
    pub tls: bool,
}

/// Chromium only ever hands out hex target ids. Anything else is somebody
/// steering the proxy at a path of their choosing, so it never reaches
/// Chromium.
fn valid_target_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// The DevTools application is a fixed set of files under `/devtools/`. Keep
/// the proxy to that subtree: no traversal, no absolute paths, no query of our
/// own making.
fn safe_asset_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path.split('/').any(|seg| seg == "..")
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '@'))
}

/// Answer for every route here when no debug port is configured.
fn disabled() -> Response {
    (
        StatusCode::NOT_FOUND,
        "HTML remote control is disabled. Set cef.debug_port (or \
         STROM_CEF_DEBUG_PORT) to enable it.",
    )
        .into_response()
}

/// The host the client used to reach us, so the DevTools application is told
/// an address that works from where it runs — which is somebody's laptop, not
/// this machine.
fn client_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Whether the client's side of the connection is HTTPS. A reverse proxy in
/// front of us terminates TLS, so its header outranks our own socket.
fn client_is_secure(headers: &HeaderMap, config: &DevToolsConfig) -> bool {
    match headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        Some(proto) => proto.split(',').next().map(str::trim) == Some("https"),
        None => config.tls,
    }
}

/// List the pages that `cefsrc` elements are currently rendering.
#[utoipa::path(
    get,
    path = "/api/devtools/targets",
    tag = "devtools",
    responses(
        (status = 200, description = "Pages available for remote control", body = DevToolsTargets),
        (status = 401, description = "Authentication required"),
        (status = 502, description = "The DevTools endpoint did not answer")
    )
)]
pub async fn list_targets(Extension(config): Extension<DevToolsConfig>) -> Response {
    let Some(port) = config.debug_port else {
        return Json(DevToolsTargets {
            enabled: false,
            warning: None,
            targets: Vec::new(),
        })
        .into_response();
    };

    let raw: Vec<serde_json::Value> =
        match reqwest::get(format!("http://127.0.0.1:{}/json/list", port)).await {
            Ok(response) => match response.json().await {
                Ok(json) => json,
                Err(e) => {
                    error!("DevTools target list was not JSON: {}", e);
                    return (
                        StatusCode::BAD_GATEWAY,
                        "DevTools endpoint returned garbage",
                    )
                        .into_response();
                }
            },
            Err(e) => {
                // No cefsrc has started yet, so CEF has not initialized and
                // nothing is listening. That is ordinary, not a fault.
                debug!("DevTools endpoint on port {} did not answer: {}", port, e);
                return Json(DevToolsTargets {
                    enabled: true,
                    warning: Some(REMOTE_CONTROL_WARNING.to_string()),
                    targets: Vec::new(),
                })
                .into_response();
            }
        };

    let targets = raw
        .into_iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .filter_map(|t| {
            let id = t.get("id")?.as_str()?.to_string();
            if !valid_target_id(&id) {
                warn!("Ignoring DevTools target with an unexpected id shape");
                return None;
            }
            Some(DevToolsTarget {
                title: t
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                url: t
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                open_path: format!("/api/devtools/open/{}", id),
                id,
            })
        })
        .collect();

    Json(DevToolsTargets {
        enabled: true,
        warning: Some(REMOTE_CONTROL_WARNING.to_string()),
        targets,
    })
    .into_response()
}

/// Open DevTools against one page.
///
/// This is the link an operator pastes into their own browser. It arrives
/// authenticated — by session, bearer token or `?auth_token=` — and leaves
/// with a session cookie, because what follows is the DevTools application
/// fetching its own files and opening its own socket, and those requests carry
/// nothing but cookies. The token already grants everything the session does,
/// so this widens no one's access; it only makes it last a browser session.
#[utoipa::path(
    get,
    path = "/api/devtools/open/{target_id}",
    tag = "devtools",
    params(("target_id" = String, Path, description = "Chromium target id")),
    responses(
        (status = 303, description = "Redirect to the DevTools application"),
        (status = 400, description = "Malformed target id"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "Remote control is disabled")
    )
)]
pub async fn open_target(
    Extension(config): Extension<DevToolsConfig>,
    Path(target_id): Path<String>,
    session: Session,
    headers: HeaderMap,
) -> Response {
    if config.debug_port.is_none() {
        return disabled();
    }
    if !valid_target_id(&target_id) {
        return (StatusCode::BAD_REQUEST, "Malformed target id").into_response();
    }

    if let Err(e) = session.insert(crate::auth::SESSION_USER_KEY, true).await {
        error!("Could not persist the DevTools session: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, "Session store failed").into_response();
    }

    let Some(host) = client_host(&headers) else {
        return (StatusCode::BAD_REQUEST, "Request carried no Host header").into_response();
    };

    // The DevTools application takes the socket as `ws=host/path` or
    // `wss=host/path` — scheme by parameter name, no scheme in the value.
    let param = if client_is_secure(&headers, &config) {
        "wss"
    } else {
        "ws"
    };

    info!("Opening DevTools for target {}", target_id);
    Redirect::to(&format!(
        "/api/devtools/ui/inspector.html?{}={}/api/devtools/cdp/{}",
        param, host, target_id
    ))
    .into_response()
}

/// Serve the DevTools application from Chromium.
///
/// CEF ships the whole application and serves it under `/devtools/`, so this
/// is a plain pass-through: no rewriting, and nothing fetched from the
/// internet, which matters for an instance that has none.
#[utoipa::path(
    get,
    path = "/api/devtools/ui/{path}",
    tag = "devtools",
    params(("path" = String, Path, description = "File within the DevTools application")),
    responses(
        (status = 200, description = "A file of the DevTools application"),
        (status = 400, description = "Path outside the DevTools application"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "Remote control is disabled"),
        (status = 502, description = "The DevTools endpoint did not answer")
    )
)]
pub async fn proxy_ui(
    Extension(config): Extension<DevToolsConfig>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let Some(port) = config.debug_port else {
        return disabled();
    };
    if !safe_asset_path(&path) {
        return (
            StatusCode::BAD_REQUEST,
            "Path outside the DevTools application",
        )
            .into_response();
    }

    // The DevTools application reads `?ws=` from its own location in the
    // operator's browser, so Chromium never needs the query — but pass it on
    // anyway rather than silently serving a different URL than was asked for.
    let url = match query.as_deref().filter(|q| !q.is_empty()) {
        Some(q) => format!("http://127.0.0.1:{}/devtools/{}?{}", port, path, q),
        None => format!("http://127.0.0.1:{}/devtools/{}", port, path),
    };

    match reqwest::get(&url).await {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            match response.bytes().await {
                Ok(body) => (
                    status,
                    [(header::CONTENT_TYPE, content_type)],
                    Body::from(body),
                )
                    .into_response(),
                Err(e) => {
                    error!("DevTools asset {} broke off mid-body: {}", path, e);
                    (StatusCode::BAD_GATEWAY, "DevTools endpoint went away").into_response()
                }
            }
        }
        Err(e) => {
            debug!("DevTools asset {} could not be fetched: {}", path, e);
            (
                StatusCode::BAD_GATEWAY,
                "DevTools endpoint is not answering - is a flow with an HTML source running?",
            )
                .into_response()
        }
    }
}

/// Carry the DevTools protocol between the operator's browser and Chromium.
#[utoipa::path(
    get,
    path = "/api/devtools/cdp/{target_id}",
    tag = "devtools",
    params(("target_id" = String, Path, description = "Chromium target id")),
    responses(
        (status = 101, description = "WebSocket connection upgraded"),
        (status = 400, description = "Malformed target id"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "Remote control is disabled")
    )
)]
pub async fn proxy_cdp(
    ws: WebSocketUpgrade,
    Extension(config): Extension<DevToolsConfig>,
    Path(target_id): Path<String>,
) -> Response {
    let Some(port) = config.debug_port else {
        return disabled();
    };
    if !valid_target_id(&target_id) {
        return (StatusCode::BAD_REQUEST, "Malformed target id").into_response();
    }

    let upstream = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target_id);
    ws.on_upgrade(move |socket| pump(socket, upstream, target_id))
}

/// Shuttle messages both ways until either side hangs up.
///
/// The protocol is text in both directions, and the screencast rides the same
/// socket as everything else: Chromium sends a JPEG per changed frame and
/// waits for the client to acknowledge it before sending the next. That
/// acknowledgement is the flow control, so a slow link costs frame rate rather
/// than an unbounded queue — which is what makes this usable over the
/// internet. Nothing here needs to know that; it just must not buffer.
async fn pump(client: WebSocket, upstream_url: String, target_id: String) {
    let (upstream, _) = match tokio_tungstenite::connect_async(&upstream_url).await {
        Ok(pair) => pair,
        Err(e) => {
            warn!("Could not reach DevTools for target {}: {}", target_id, e);
            return;
        }
    };

    debug!("DevTools session open for target {}", target_id);

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    let to_upstream = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let forwarded = match msg {
                Message::Text(t) => WsMessage::Text(t.as_str().into()),
                Message::Binary(b) => WsMessage::Binary(b),
                Message::Close(_) => break,
                // Chromium answers our pings; the client's are ours to answer,
                // and axum has already done it.
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if upstream_tx.send(forwarded).await.is_err() {
                break;
            }
        }
    };

    let to_client = async {
        while let Some(Ok(msg)) = upstream_rx.next().await {
            let forwarded = match msg {
                WsMessage::Text(t) => Message::Text(t.as_str().into()),
                WsMessage::Binary(b) => Message::Binary(b),
                WsMessage::Close(_) => break,
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            };
            if client_tx.send(forwarded).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = to_upstream => {}
        _ = to_client => {}
    }

    debug!("DevTools session closed for target {}", target_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_ids_are_hex_only() {
        assert!(valid_target_id("4A8CD2F2840F8591A6277A7FFFDAB3A9"));
        assert!(valid_target_id("cf033111-6dfb-4027-909c-a39d9ba6f825"));
        assert!(!valid_target_id(""));
        assert!(!valid_target_id("../../json/version"));
        assert!(!valid_target_id("page/ABC?x=1"));
        assert!(!valid_target_id(&"a".repeat(65)));
    }

    #[test]
    fn asset_paths_stay_inside_the_application() {
        assert!(safe_asset_path("inspector.html"));
        assert!(safe_asset_path("entrypoints/inspector/inspector.js"));
        assert!(safe_asset_path("core/common/common.js"));
        assert!(!safe_asset_path("../json/version"));
        assert!(!safe_asset_path("/etc/passwd"));
        assert!(!safe_asset_path("a/../../b"));
        assert!(!safe_asset_path(""));
    }

    #[test]
    fn forwarded_proto_outranks_our_own_socket() {
        let plain = DevToolsConfig {
            debug_port: Some(9222),
            tls: false,
        };
        let mut headers = HeaderMap::new();
        assert!(!client_is_secure(&headers, &plain));

        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(client_is_secure(&headers, &plain));

        // A proxy chain lists the client's protocol first.
        headers.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert!(client_is_secure(&headers, &plain));

        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        let tls = DevToolsConfig {
            debug_port: Some(9222),
            tls: true,
        };
        assert!(!client_is_secure(&headers, &tls));
    }
}
