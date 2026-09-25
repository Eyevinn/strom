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
//! stays there: everything here goes through Strom's own port, the same way
//! WHEP and WHIP are fronted.
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
//! # The link is a capability, and it says nothing else
//!
//! Minting a link needs Strom's own authentication. The link itself is one
//! opaque key and nothing more — no API token, no Chromium target id, no
//! internal address or port. It is meant to be pasted into a browser or read
//! off a phone screen, so anything in it is something the operator cannot
//! avoid handing over with it.
//!
//! The key is the credential for everything under `/devtools/<key>`: the
//! DevTools application, its files, and the protocol socket. It expires on its
//! own after [`LINK_TTL`] of disuse and can be revoked before that. The only
//! address that appears is the host the operator themselves reached us on,
//! taken from their own request, because the DevTools application has to be
//! told where to open its socket.
//!
//! # This is an instance-wide privilege, not a per-source one
//!
//! One CEF process serves every `cefsrc` in the instance, so there is one
//! debug port and one cookie jar, and each HTML source is a target on that
//! port. A key decides which page a session *starts* on; it bounds nothing
//! after that. From any target the protocol reaches every cookie in the
//! profile (`Storage.getCookies`), navigates that page anywhere including
//! `file://`, and runs whatever JavaScript it likes.
//!
//! So whoever holds a key can reach every HTML source in the instance and
//! everything the browser has ever logged in to. That is fine for an operator
//! running their own instance, and wrong for an instance whose HTML sources
//! belong to different customers: there the isolation has to come from
//! separate Strom instances, which already get separate CEF profiles, and this
//! must stay off.

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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom_types::devtools::{
    DevToolsLink, DevToolsTarget, DevToolsTargets, REMOTE_CONTROL_WARNING,
};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// How long a link survives without being used.
///
/// Long enough to walk to another machine, find the page and work through a
/// login with a second factor; short enough that a link left in a chat log is
/// dead by the time anyone reads it. Every use pushes it out again, so a
/// session in progress does not expire under the operator.
pub const LINK_TTL: Duration = Duration::from_secs(30 * 60);

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

/// One minted link.
struct Link {
    /// The Chromium target the key opens onto.
    target_id: String,
    /// When the key dies if nobody uses it before then.
    expires: Instant,
}

/// The live links, and the configuration they are minted against.
#[derive(Clone)]
pub struct DevToolsState {
    pub config: DevToolsConfig,
    links: Arc<Mutex<HashMap<String, Link>>>,
}

impl DevToolsState {
    pub fn new(config: DevToolsConfig) -> Self {
        Self {
            config,
            links: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Mint a key for a target.
    ///
    /// Two v4 UUIDs, hyphens dropped: 244 random bits from the same source a
    /// token crate would use, without taking a dependency for it.
    fn mint(&self, target_id: String) -> String {
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut links = self.links.lock().unwrap();
        links.retain(|_, l| l.expires > Instant::now());
        links.insert(
            key.clone(),
            Link {
                target_id,
                expires: Instant::now() + LINK_TTL,
            },
        );
        key
    }

    /// Resolve a key to its target, pushing its expiry out.
    ///
    /// Returns `None` for a key that never existed, was revoked, or went
    /// unused for too long — all of which are the same answer to whoever is
    /// asking.
    fn resolve(&self, key: &str) -> Option<String> {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        links.retain(|_, l| l.expires > now);
        let link = links.get_mut(key)?;
        link.expires = now + LINK_TTL;
        Some(link.target_id.clone())
    }

    fn revoke(&self, key: &str) -> bool {
        self.links.lock().unwrap().remove(key).is_some()
    }
}

/// Chromium only ever hands out hex target ids. Anything else is somebody
/// steering the proxy at a path of their choosing, so it never reaches
/// Chromium.
fn valid_target_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Keys are our own hex, and are matched against the table anyway — this only
/// keeps a malformed one from being logged or echoed.
fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit())
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

/// What an expired, revoked or invented key gets. Deliberately the same for
/// all three.
fn no_such_link() -> Response {
    (
        StatusCode::NOT_FOUND,
        "This remote control link is not valid. Links expire when unused; ask for a new one.",
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

/// Fetch a path from Chromium's DevTools endpoint and hand it back as it came.
async fn proxy_get(port: u16, path: &str, query: Option<&str>) -> Response {
    let url = match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("http://127.0.0.1:{}/{}?{}", port, path, q),
        None => format!("http://127.0.0.1:{}/{}", port, path),
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
                    error!("DevTools response broke off mid-body: {}", e);
                    (StatusCode::BAD_GATEWAY, "DevTools endpoint went away").into_response()
                }
            }
        }
        Err(e) => {
            debug!("DevTools endpoint did not answer: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                "DevTools endpoint is not answering - is a flow with an HTML source running?",
            )
                .into_response()
        }
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
pub async fn list_targets(Extension(state): Extension<DevToolsState>) -> Response {
    let Some(port) = state.config.debug_port else {
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

/// Mint a remote control link for one page.
///
/// The caller authenticates as they would for any other endpoint; what comes
/// back is a path anyone can open, so it is handed to a person, not published.
#[utoipa::path(
    post,
    path = "/api/devtools/targets/{target_id}/link",
    tag = "devtools",
    params(("target_id" = String, Path, description = "Chromium target id")),
    responses(
        (status = 200, description = "A link that opens DevTools against this page", body = DevToolsLink),
        (status = 400, description = "Malformed target id"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "Remote control is disabled")
    )
)]
pub async fn create_link(
    Extension(state): Extension<DevToolsState>,
    Path(target_id): Path<String>,
) -> Response {
    if state.config.debug_port.is_none() {
        return disabled();
    }
    if !valid_target_id(&target_id) {
        return (StatusCode::BAD_REQUEST, "Malformed target id").into_response();
    }

    let key = state.mint(target_id);
    info!("Minted a remote control link, valid for {:?}", LINK_TTL);

    Json(DevToolsLink {
        path: format!("/devtools/{}", key),
        expires_in_seconds: LINK_TTL.as_secs(),
        warning: REMOTE_CONTROL_WARNING.to_string(),
    })
    .into_response()
}

/// Revoke a link before it expires.
#[utoipa::path(
    delete,
    path = "/api/devtools/links/{key}",
    tag = "devtools",
    params(("key" = String, Path, description = "The key from the minted path")),
    responses(
        (status = 204, description = "The link is gone"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "No such link")
    )
)]
pub async fn revoke_link(
    Extension(state): Extension<DevToolsState>,
    Path(key): Path<String>,
) -> Response {
    if valid_key(&key) && state.revoke(&key) {
        info!("Revoked a remote control link");
        StatusCode::NO_CONTENT.into_response()
    } else {
        no_such_link()
    }
}

/// Open DevTools against the page a key was minted for.
///
/// This is the link an operator pastes into their own browser. Everything the
/// DevTools application then asks for lives under the same key, so the
/// redirect is the only place that has to name an address — and the address it
/// names is the one the operator themselves just used.
pub async fn open_link(
    Extension(state): Extension<DevToolsState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Response {
    if state.config.debug_port.is_none() {
        return disabled();
    }
    if !valid_key(&key) || state.resolve(&key).is_none() {
        return no_such_link();
    }

    let Some(host) = client_host(&headers) else {
        return (StatusCode::BAD_REQUEST, "Request carried no Host header").into_response();
    };

    // The DevTools application takes the socket as `ws=host/path` or
    // `wss=host/path` — scheme by parameter name, no scheme in the value.
    let param = if client_is_secure(&headers, &state.config) {
        "wss"
    } else {
        "ws"
    };

    Redirect::to(&format!(
        "/devtools/{key}/ui/inspector.html?{param}={host}/devtools/{key}/ws"
    ))
    .into_response()
}

/// Serve the DevTools application from Chromium.
///
/// CEF ships the whole application and serves it under `/devtools/`, so this
/// is a plain pass-through: no rewriting, and nothing fetched from the
/// internet, which matters for an instance that has none. Serving it beneath
/// the key is what lets the application's own relative paths keep working
/// without a cookie to carry the credential.
pub async fn proxy_ui(
    Extension(state): Extension<DevToolsState>,
    Path((key, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    let Some(port) = state.config.debug_port else {
        return disabled();
    };
    if !valid_key(&key) || state.resolve(&key).is_none() {
        return no_such_link();
    }
    if !safe_asset_path(&path) {
        return (
            StatusCode::BAD_REQUEST,
            "Path outside the DevTools application",
        )
            .into_response();
    }

    proxy_get(port, &format!("devtools/{}", path), query.as_deref()).await
}

/// Carry the DevTools protocol between the operator's browser and Chromium.
pub async fn proxy_cdp(
    ws: WebSocketUpgrade,
    Extension(state): Extension<DevToolsState>,
    Path(key): Path<String>,
) -> Response {
    let Some(port) = state.config.debug_port else {
        return disabled();
    };
    if !valid_key(&key) {
        return no_such_link();
    }
    let Some(target_id) = state.resolve(&key) else {
        return no_such_link();
    };

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

    fn state() -> DevToolsState {
        DevToolsState::new(DevToolsConfig {
            debug_port: Some(9222),
            tls: false,
        })
    }

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

    #[test]
    fn a_key_carries_the_target_and_nothing_else_does() {
        let s = state();
        let key = s.mint("4A8CD2F2840F8591A6277A7FFFDAB3A9".to_string());
        // The key is what a client sees, so it must not spell out the target.
        assert!(valid_key(&key));
        assert!(!key.contains("4A8CD2F2"));
        assert_eq!(
            s.resolve(&key).as_deref(),
            Some("4A8CD2F2840F8591A6277A7FFFDAB3A9")
        );
    }

    #[test]
    fn keys_are_not_guessable_from_each_other() {
        let s = state();
        let a = s.mint("AAAA".to_string());
        let b = s.mint("AAAA".to_string());
        assert_ne!(a, b);
    }

    #[test]
    fn an_unknown_key_resolves_to_nothing() {
        let s = state();
        assert!(s.resolve(&"f".repeat(64)).is_none());
        assert!(!valid_key("short"));
        assert!(!valid_key(&"z".repeat(64)));
    }

    #[test]
    fn a_revoked_key_stops_working() {
        let s = state();
        let key = s.mint("ABCD".to_string());
        assert!(s.revoke(&key));
        assert!(s.resolve(&key).is_none());
        // Revoking twice is not an error the caller can act on differently.
        assert!(!s.revoke(&key));
    }

    #[test]
    fn an_expired_key_is_gone_even_before_anyone_asks() {
        let s = state();
        let key = s.mint("ABCD".to_string());
        {
            let mut links = s.links.lock().unwrap();
            links.get_mut(&key).unwrap().expires = Instant::now() - Duration::from_secs(1);
        }
        assert!(s.resolve(&key).is_none());
        assert!(s.links.lock().unwrap().is_empty());
    }

    #[test]
    fn using_a_key_pushes_its_expiry_out() {
        let s = state();
        let key = s.mint("ABCD".to_string());
        let first = s.links.lock().unwrap().get(&key).unwrap().expires;
        std::thread::sleep(Duration::from_millis(5));
        assert!(s.resolve(&key).is_some());
        let second = s.links.lock().unwrap().get(&key).unwrap().expires;
        assert!(second > first, "a session in use must not expire under it");
    }
}
