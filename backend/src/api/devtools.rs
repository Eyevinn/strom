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
//! # What a link carries
//!
//! Strom serves its own page under the key and terminates the WebSocket
//! itself, so the operator gets the rendered page, and clicks and keystrokes
//! go back into it. The proxy forwards only [`SCREENCAST_METHODS`] and
//! answers everything else with a protocol error, so the link is what it
//! looks like rather than what the debug port would otherwise be.
//!
//! Terminating the socket is also what keeps the browser's `Origin` check out
//! of the picture: Chromium rejects WebSocket origins it does not know
//! (`--remote-allow-origins`), but our own connection carries no origin.
//!
//! # The link is a capability, and it says nothing else
//!
//! Minting a link needs Strom's own authentication, and with no
//! authentication configured the debug port is never opened at all — a door
//! with no lock is worse than no door. The link itself is one opaque key and
//! nothing more: no API token, no Chromium target id, no internal address or
//! port. It is meant to be pasted into a browser or read off a phone screen,
//! so anything in it is something the operator cannot avoid handing over
//! with it.
//!
//! The key is the credential for everything under `/devtools/<key>`. It
//! expires on its own after [`LINK_TTL`] of disuse and can be revoked before
//! that, and either one also ends a session that is already open — otherwise
//! revocation would close the door on the next visitor while the one already
//! inside stayed.
//!
//! Even filtered, a link is not nothing: whoever holds it sees and can type
//! into a page that is on air, for as long as it lives.
//!
//! # The escape hatch, and why it is one
//!
//! `cef.full_devtools` serves Chromium's DevTools application instead and
//! stops filtering. It cannot be a richer mode of the same thing, because
//! DevTools needs precisely the domains the filter exists to refuse:
//! `Runtime`, `Debugger`, `DOM`, `Network`. With it on, a key runs arbitrary
//! JavaScript, navigates anywhere including `file://`, and reads every cookie
//! in the profile.
//!
//! That is also instance-wide. One CEF process serves every `cefsrc`, so
//! there is one debug port and one cookie jar, and a key decides which page a
//! session *starts* on while bounding nothing after that. So with the hatch
//! open, whoever holds a key reaches every HTML source in the instance,
//! everything the browser has ever logged in to, and the files this process
//! can read. That is a debugging setting for an operator on their own
//! instance. For HTML sources belonging to different customers the isolation
//! has to come from separate Strom instances, which already get separate CEF
//! profiles, and this must stay off.

use crate::state::AppState;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Extension, Path, RawQuery, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use strom_types::devtools::{
    DevToolsLink, DevToolsLinkSummary, DevToolsLinks, DevToolsRevokedLinks, DevToolsTarget,
    DevToolsTargets, REMOTE_CONTROL_WARNING, SCREENCAST_CONTROL_WARNING,
};
use strom_types::FlowId;
use tokio::sync::broadcast;
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
    /// Hand out the Chromium DevTools application, with the protocol
    /// unfiltered, instead of the remote control page.
    ///
    /// Off, a link carries the page: its picture, and clicks and keystrokes
    /// into it, because [`SCREENCAST_METHODS`] is all the proxy forwards. On,
    /// a link carries the whole Chrome DevTools Protocol, which is arbitrary
    /// JavaScript, navigation to `file://` and every cookie in the profile.
    /// The two cannot be combined: DevTools needs the domains the filter
    /// exists to refuse, so this is the escape hatch, not a richer mode.
    pub full_devtools: bool,
}

/// What a remote control session may ask Chromium to do.
///
/// A picture, and a way to click and type into it. Everything outside this
/// list — `Runtime.evaluate`, `Page.navigate`, `Storage.getCookies`, the whole
/// `Network` and `Debugger` domains — is what turns a link into control of
/// this host, so the proxy refuses it rather than trusting the page not to
/// ask. The page we serve is only the first user of the link; the filter is
/// what makes the link safe to hand to a second one.
pub const SCREENCAST_METHODS: &[&str] = &[
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.enable",
    "Page.screencastFrameAck",
    "Page.startScreencast",
    "Page.stopScreencast",
];

/// The protocol's own shape for "no", so the client sees a refusal against the
/// command it sent rather than a socket that silently swallows things.
fn cdp_refusal(id: Option<i64>, message: &str) -> String {
    serde_json::json!({
        "id": id,
        "error": { "code": -32601, "message": message }
    })
    .to_string()
}

/// Whether one message from the client may be forwarded, or the refusal to
/// send back in its place.
fn screencast_allows(raw: &str) -> Result<(), String> {
    let Ok(message) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Err(cdp_refusal(None, "Not a DevTools protocol message"));
    };
    let id = message.get("id").and_then(serde_json::Value::as_i64);
    let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
        return Err(cdp_refusal(
            id,
            "A remote control session sends commands, nothing else",
        ));
    };
    if !SCREENCAST_METHODS.contains(&method) {
        return Err(cdp_refusal(
            id,
            &format!(
                "{} is not available over a remote control link, which carries the page's \
                 picture, clicks and keystrokes only",
                method
            ),
        ));
    }
    Ok(())
}

/// What to tell an operator this link hands over, which depends on whether the
/// protocol is filtered.
fn link_warning(config: &DevToolsConfig) -> String {
    if config.full_devtools {
        REMOTE_CONTROL_WARNING.to_string()
    } else {
        SCREENCAST_CONTROL_WARNING.to_string()
    }
}

/// One minted link.
struct Link {
    /// A name for this link that is safe to list, log and hand to a client.
    ///
    /// The key is the credential, so it can never leave this table. An
    /// operator still has to be able to see that a link exists and kill it,
    /// and this is what they name when they do.
    id: String,
    /// The Chromium target the key opens onto.
    target_id: String,
    /// The page the link was minted against, so a listing says which browser
    /// the operator would be revoking.
    target_url: String,
    /// When the key dies if nobody uses it before then.
    expires: Instant,
    /// Fires when the link is revoked or expires.
    ///
    /// Without this, revocation would only close the door to *new* sessions:
    /// a websocket already open on the key would keep full control of the
    /// browser for as long as it liked. Revocation is the emergency stop in
    /// this design, so it has to reach sessions that are already running.
    cancel: broadcast::Sender<()>,
}

impl Link {
    fn summary(&self, now: Instant) -> DevToolsLinkSummary {
        DevToolsLinkSummary {
            id: self.id.clone(),
            target_url: self.target_url.clone(),
            expires_in_seconds: self.expires.saturating_duration_since(now).as_secs(),
        }
    }
}

/// The live links, and the configuration they are minted against.
#[derive(Clone)]
pub struct DevToolsState {
    pub config: DevToolsConfig,
    links: Arc<Mutex<HashMap<String, Link>>>,
}

/// What a caller gets back when a link is minted: the credential, and the
/// non-secret name for it.
struct MintedLink {
    key: String,
    id: String,
}

impl DevToolsState {
    pub fn new(config: DevToolsConfig) -> Self {
        Self {
            config,
            links: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Drop every link that has run out of time, telling any session still
    /// open on one to stop.
    ///
    /// Called from every path that touches the table, so an expiry takes
    /// effect whether or not anyone asks about that particular link.
    fn sweep(links: &mut HashMap<String, Link>, now: Instant) {
        links.retain(|_, l| {
            if l.expires > now {
                true
            } else {
                // Nobody may be listening; that is not an error.
                let _ = l.cancel.send(());
                false
            }
        });
    }

    /// Mint a key for a target.
    ///
    /// Two v4 UUIDs, hyphens dropped: 244 random bits from the same source a
    /// token crate would use, without taking a dependency for it.
    fn mint(&self, target_id: String, target_url: String) -> MintedLink {
        let key = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let id = Uuid::new_v4().simple().to_string();
        let (cancel, _) = broadcast::channel(1);
        let mut links = self.links.lock().unwrap();
        Self::sweep(&mut links, Instant::now());
        links.insert(
            key.clone(),
            Link {
                id: id.clone(),
                target_id,
                target_url,
                expires: Instant::now() + LINK_TTL,
                cancel,
            },
        );
        MintedLink { key, id }
    }

    /// Resolve a key to its target, pushing its expiry out.
    ///
    /// Returns `None` for a key that never existed, was revoked, or went
    /// unused for too long — all of which are the same answer to whoever is
    /// asking.
    fn resolve(&self, key: &str) -> Option<String> {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        Self::sweep(&mut links, now);
        let link = links.get_mut(key)?;
        link.expires = now + LINK_TTL;
        Some(link.target_id.clone())
    }

    /// Resolve a key to its target and a signal that fires when the link dies.
    ///
    /// A session holds the signal for as long as it is open, so revoking or
    /// expiring the link ends the session rather than only refusing the next
    /// one.
    fn open_session(&self, key: &str) -> Option<(String, broadcast::Receiver<()>)> {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        Self::sweep(&mut links, now);
        let link = links.get_mut(key)?;
        link.expires = now + LINK_TTL;
        Some((link.target_id.clone(), link.cancel.subscribe()))
    }

    /// Revoke one link by its non-secret id, ending any session open on it.
    fn revoke_by_id(&self, id: &str) -> bool {
        let mut links = self.links.lock().unwrap();
        Self::sweep(&mut links, Instant::now());
        let Some(key) = links
            .iter()
            .find(|(_, l)| l.id == id)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        if let Some(link) = links.remove(&key) {
            let _ = link.cancel.send(());
        }
        true
    }

    /// Revoke every link, ending every session open on one. The panic button.
    fn revoke_all(&self) -> usize {
        let mut links = self.links.lock().unwrap();
        let count = links.len();
        for link in links.values() {
            let _ = link.cancel.send(());
        }
        links.clear();
        count
    }

    /// The live links, newest expiry last. Never the keys.
    fn list(&self) -> Vec<DevToolsLinkSummary> {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        Self::sweep(&mut links, now);
        let mut out: Vec<_> = links.values().map(|l| l.summary(now)).collect();
        out.sort_by_key(|a| a.expires_in_seconds);
        out
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

/// Link ids are our own hex too — one UUID, so half a key's length. Keeping
/// the two lengths apart is what lets the log redaction tell a credential from
/// a name for one.
fn valid_link_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

/// What replaces a remote control key wherever a path is written down.
const REDACTED: &str = "<redacted>";

/// Take any remote control key out of a request path before it is logged.
///
/// A key is the whole credential, and it is worth as much in a log file as it
/// is in the operator's hand — more, because the log keeps it. The DevTools
/// application fetches around a hundred files under `/devtools/<key>/ui/`, so
/// without this every page load writes a working key to the access log a
/// hundred times over, and its sliding TTL keeps it working for as long as
/// anyone keeps reading.
///
/// Matching is by shape, not by route, so it covers the proxy's own paths and
/// any future one that happens to carry a key. Link ids are half the length
/// and survive — they are not credentials, and an operator needs to see them.
pub fn redact_path(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.split('/').any(valid_key) {
        return std::borrow::Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len());
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        if valid_key(segment) {
            out.push_str(REDACTED);
        } else {
            out.push_str(segment);
        }
    }
    std::borrow::Cow::Owned(out)
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

/// Whether a header value is a bare `host` or `host:port` and nothing else.
///
/// `X-Forwarded-Host` is whatever a client sent unless a proxy overwrote it,
/// and `HeaderValue::to_str` happily passes `&`, `?` and `#` through. The host
/// ends up inside the inspector URL's query string, so a value carrying any of
/// those could append parameters of its own and point the DevTools
/// application's websocket at a host of the sender's choosing. Nothing but a
/// host and an optional port is allowed through, and userinfo (`user@host`) is
/// not a host.
fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    // Split an optional `:port` off the end. An IPv6 literal is bracketed, so
    // its own colons sit inside the brackets and are never taken for a port.
    let (name, port) = match host.rfind(':') {
        Some(i) if !host[i..].contains(']') => (&host[..i], Some(&host[i + 1..])),
        _ => (host, None),
    };
    if let Some(port) = port {
        if port.is_empty() || port.len() > 5 || !port.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    if let Some(inner) = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
        return !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'));
    }
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The host the client used to reach us, so the DevTools application is told
/// an address that works from where it runs — which is somebody's laptop, not
/// this machine.
///
/// Rejects anything that is not a plain `host[:port]`; see [`valid_host`].
fn client_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| valid_host(h))
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

/// One HTTP client for everything this module asks Chromium.
///
/// `reqwest::get` builds a whole client per call — connection pool, resolver,
/// TLS configuration — and drops it again. The DevTools application is around
/// a hundred files per load, so that would be a hundred throwaway clients and
/// no connection reuse at all against loopback.
fn devtools_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Fetch a path from Chromium's DevTools endpoint and hand it back as it came.
async fn proxy_get(port: u16, path: &str, query: Option<&str>) -> Response {
    let url = match query.filter(|q| !q.is_empty()) {
        Some(q) => format!("http://127.0.0.1:{}/{}?{}", port, path, q),
        None => format!("http://127.0.0.1:{}/{}", port, path),
    };

    match devtools_client().get(&url).send().await {
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

    let raw: Vec<serde_json::Value> = match devtools_client()
        .get(format!("http://127.0.0.1:{}/json/list", port))
        .send()
        .await
    {
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
                warning: Some(link_warning(&state.config)),
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
        warning: Some(link_warning(&state.config)),
        targets,
    })
    .into_response()
}

/// Build the answer for a successful mint.
fn minted(state: &DevToolsState, target_id: String, target_url: String) -> Response {
    let minted = state.mint(target_id, target_url);
    // The key is the credential, so it is never logged - only the id is.
    info!(
        "Minted remote control link {}, valid for {:?}",
        minted.id, LINK_TTL
    );
    Json(DevToolsLink {
        id: minted.id,
        path: format!("/devtools/{}", minted.key),
        expires_in_seconds: LINK_TTL.as_secs(),
        warning: link_warning(&state.config),
    })
    .into_response()
}

/// Mint a remote control link for one page.
///
/// The caller authenticates as they would for any other endpoint; what comes
/// back is a path anyone can open, so it is handed to a person, not published.
///
/// Naming Chromium's target directly does not get around the block's Remote
/// Control switch: the target still has to be one that some HTML source with
/// the switch on resolves to, by the same rule [`create_block_link`] uses. The
/// two endpoints are different ways of naming the same page, so they cannot be
/// allowed to disagree about whether it may be opened.
#[utoipa::path(
    post,
    path = "/api/devtools/targets/{target_id}/link",
    tag = "devtools",
    params(("target_id" = String, Path, description = "Chromium target id")),
    responses(
        (status = 200, description = "A link that opens DevTools against this page", body = DevToolsLink),
        (status = 400, description = "Malformed target id"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "No HTML source with remote control on is rendering this page"),
        (status = 404, description = "Remote control is disabled, or no such page")
    )
)]
pub async fn create_link(
    State(app): State<AppState>,
    Extension(state): Extension<DevToolsState>,
    Path(target_id): Path<String>,
) -> Response {
    let Some(port) = state.config.debug_port else {
        return disabled();
    };
    if !valid_target_id(&target_id) {
        return (StatusCode::BAD_REQUEST, "Malformed target id").into_response();
    }

    let Some(targets) = page_targets(port).await else {
        return (
            StatusCode::NOT_FOUND,
            "No page is being rendered yet - is the flow running?",
        )
            .into_response();
    };
    let Some(target) = targets.iter().find(|t| t.id == target_id).cloned() else {
        return (StatusCode::NOT_FOUND, "No such page").into_response();
    };

    // The switch lives on the block, so the target has to be traced back to one
    // before it can be opened.
    let sources = html_sources(&app).await;
    let owner = sources.iter().find(|s| {
        s.remote_control
            && resolve_target(&targets, &sources, s) == Resolution::Target(target.id.clone())
    });
    if owner.is_none() {
        return (
            StatusCode::FORBIDDEN,
            "No HTML source with remote control switched on is rendering this page. Turn on \
             the Remote Control property of the block that renders it to hand out a link.",
        )
            .into_response();
    }

    minted(&state, target.id, target.url)
}

/// Two URLs naming the same page. Chromium reports what it navigated to, which
/// is the operator's URL with a trailing slash added on an empty path, so a
/// literal comparison would miss a page the operator would say is theirs.
fn same_page(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The `scheme://host:port` a URL belongs to, when it has one.
///
/// `data:` and anything else without an authority has no origin, and gets
/// `None` rather than a guess. This is deliberately textual: it is used only
/// to decide that two URLs came from the same site, never to reach anything.
fn origin(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&url[..scheme.len() + 3 + authority_len])
}

/// Same site, whatever path either of them is on now.
fn same_origin(a: &str, b: &str) -> bool {
    match (origin(a), origin(b)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

/// One page Chromium is rendering right now.
#[derive(Clone, Debug)]
struct PageTarget {
    id: String,
    url: String,
}

/// One HTML source in this instance, and the page it was pointed at.
///
/// Every `cefsrc` in the instance shares the one CEF process, so working out
/// which page belongs to which block is an instance-wide question: a block in
/// another flow can be the reason this one's page is ambiguous.
#[derive(Clone, Debug)]
struct HtmlSource {
    flow_id: FlowId,
    block_id: String,
    /// The page the operator pointed the block at, not necessarily the one it
    /// is showing now.
    url: String,
    remote_control: bool,
}

impl HtmlSource {
    fn is(&self, flow_id: &FlowId, block_id: &str) -> bool {
        self.flow_id == *flow_id && self.block_id == block_id
    }
}

/// What asking "which page is this block rendering?" can come back with.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// Exactly one page can be this block's.
    Target(String),
    /// More than one could be, so picking would be guessing.
    Ambiguous,
    /// None of the pages can be this block's.
    Unknown,
}

/// Decide which Chromium page belongs to one HTML source.
///
/// Chromium exposes a page's *current* URL and nothing else, so an exact match
/// against the block's configured URL is only right until the page navigates —
/// and the first thing a page behind a login does is redirect to the login,
/// which is exactly when an operator wants this feature. So matching widens in
/// steps, and every step keeps the rule that an answer is only given when it
/// is the only possible one:
///
/// 1. **Exact URL.** The page is still on the URL the block names.
/// 2. **Same origin.** The page redirected or was clicked through within the
///    site the block names. Pages another block claims exactly are its, not
///    ours, and another source pointed at the same site makes this a tie.
/// 3. **Sole survivor.** Exactly one source has no page and exactly one page
///    has no source. They can only be each other — this is what carries a
///    cross-origin login redirect.
///
/// Anything short of that is [`Resolution::Ambiguous`] or
/// [`Resolution::Unknown`], never a guess.
fn resolve_target(targets: &[PageTarget], sources: &[HtmlSource], want: &HtmlSource) -> Resolution {
    // 1. Still on the URL it was given.
    let exact: Vec<&PageTarget> = targets
        .iter()
        .filter(|t| same_page(&t.url, &want.url))
        .collect();
    match exact.len() {
        1 => return Resolution::Target(exact[0].id.clone()),
        0 => {}
        _ => return Resolution::Ambiguous,
    }

    // A source with an exact match has its page; that page is not up for grabs.
    let settled = |s: &HtmlSource| targets.iter().any(|t| same_page(&t.url, &s.url));
    let claimed_exactly = |t: &PageTarget| sources.iter().any(|s| same_page(&t.url, &s.url));

    // 2. Same site, different path - a redirect or a click.
    let candidates: Vec<&PageTarget> = targets
        .iter()
        .filter(|t| !claimed_exactly(t) && same_origin(&t.url, &want.url))
        .collect();
    let rivals = sources
        .iter()
        .filter(|s| {
            !s.is(&want.flow_id, &want.block_id) && !settled(s) && same_origin(&s.url, &want.url)
        })
        .count();
    if !candidates.is_empty() && (candidates.len() > 1 || rivals > 0) {
        return Resolution::Ambiguous;
    }
    if candidates.len() == 1 {
        return Resolution::Target(candidates[0].id.clone());
    }

    // 3. One page left over, one source left over. A cross-origin login
    //    redirect lands here: the URL shares nothing with what was configured,
    //    but there is nothing else it could be.
    let spoken_for = |t: &PageTarget| {
        claimed_exactly(t)
            || sources
                .iter()
                .any(|s| !settled(s) && same_origin(&t.url, &s.url))
    };
    let unclaimed: Vec<&PageTarget> = targets.iter().filter(|t| !spoken_for(t)).collect();
    let homeless: Vec<&HtmlSource> = sources
        .iter()
        .filter(|s| !settled(s) && !targets.iter().any(|t| same_origin(&t.url, &s.url)))
        .collect();
    if unclaimed.len() == 1 && homeless.len() == 1 && homeless[0].is(&want.flow_id, &want.block_id)
    {
        return Resolution::Target(unclaimed[0].id.clone());
    }

    Resolution::Unknown
}

/// Ask Chromium which pages exist right now.
async fn page_targets(port: u16) -> Option<Vec<PageTarget>> {
    let raw: Vec<serde_json::Value> = devtools_client()
        .get(format!("http://127.0.0.1:{}/json/list", port))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(
        raw.into_iter()
            .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
            .filter_map(|t| {
                let id = t.get("id")?.as_str()?.to_string();
                let url = t.get("url")?.as_str()?.to_string();
                valid_target_id(&id).then_some(PageTarget { id, url })
            })
            .collect(),
    )
}

/// Every HTML source in the instance, because one CEF process serves them all.
async fn html_sources(app: &AppState) -> Vec<HtmlSource> {
    app.get_flows()
        .await
        .into_iter()
        .flat_map(|flow| {
            let flow_id = flow.id;
            flow.blocks
                .into_iter()
                .filter(|b| b.block_definition_id == crate::blocks::builtin::html_input::BLOCK_ID)
                .map(move |b| HtmlSource {
                    flow_id,
                    // A block with no `url` property still renders the block's
                    // default, so ask the block rather than the stored map.
                    url: crate::blocks::builtin::html_input::url(&b.properties),
                    remote_control: crate::blocks::builtin::html_input::remote_control_enabled(
                        &b.properties,
                    ),
                    block_id: b.id,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Mint a remote control link for one HTML source.
///
/// The block is the name an operator has for a page, so this is the endpoint a
/// client uses; the target id it resolves to is Chromium's business and
/// changes whenever the page is recreated.
///
/// Which page belongs to which block is decided from the URL, because that is
/// all Chromium exposes about a browser — see [`resolve_target`] for how far
/// that stretches once the page has navigated. Two blocks that cannot be told
/// apart are refused with a reason rather than guessed between.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/devtools/link",
    tag = "devtools",
    params(
        ("flow_id" = String, Path, description = "Flow id"),
        ("block_id" = String, Path, description = "Block instance id")
    ),
    responses(
        (status = 200, description = "A link that opens DevTools against this page", body = DevToolsLink),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "The block has remote control switched off"),
        (status = 404, description = "No such block, or it is not rendering a page yet"),
        (status = 409, description = "Another block is showing the same URL")
    )
)]
pub async fn create_block_link(
    State(app): State<AppState>,
    Extension(state): Extension<DevToolsState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Response {
    let Some(port) = state.config.debug_port else {
        return disabled();
    };

    let Some(flow) = app.get_flow(&flow_id).await else {
        return (StatusCode::NOT_FOUND, "Flow not found").into_response();
    };
    let Some(block) = flow.blocks.iter().find(|b| b.id == block_id) else {
        return (StatusCode::NOT_FOUND, "Block not found").into_response();
    };
    if block.block_definition_id != crate::blocks::builtin::html_input::BLOCK_ID {
        return (
            StatusCode::NOT_FOUND,
            "Only an HTML source can be controlled remotely",
        )
            .into_response();
    }

    if !crate::blocks::builtin::html_input::remote_control_enabled(&block.properties) {
        return (
            StatusCode::FORBIDDEN,
            "This HTML source has remote control switched off. Turn on its Remote Control \
             property to hand out a link.",
        )
            .into_response();
    }

    let Some(targets) = page_targets(port).await else {
        return (
            StatusCode::NOT_FOUND,
            "No page is being rendered yet - is the flow running?",
        )
            .into_response();
    };

    let sources = html_sources(&app).await;
    let Some(want) = sources.iter().find(|s| s.is(&flow_id, &block_id)) else {
        return (StatusCode::NOT_FOUND, "Block not found").into_response();
    };

    match resolve_target(&targets, &sources, want) {
        Resolution::Target(target_id) => {
            let url = targets
                .iter()
                .find(|t| t.id == target_id)
                .map(|t| t.url.clone())
                .unwrap_or_else(|| want.url.clone());
            minted(&state, target_id, url)
        }
        Resolution::Unknown => (
            StatusCode::NOT_FOUND,
            "This block is not rendering a page yet - is the flow running?",
        )
            .into_response(),
        Resolution::Ambiguous => (
            StatusCode::CONFLICT,
            "More than one HTML source could be showing this page, so which browser the link \
             would open cannot be decided. Give them different URLs.",
        )
            .into_response(),
    }
}

/// List the links that are alive right now.
///
/// Without this an operator who has lost track of a link can only wait out its
/// TTL or restart the instance, and one link is control of every browser in it.
/// The key is never in the answer — it is the credential, and the point of the
/// listing is to revoke a link, not to recover one.
#[utoipa::path(
    get,
    path = "/api/devtools/links",
    tag = "devtools",
    responses(
        (status = 200, description = "The live remote control links", body = DevToolsLinks),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn list_links(Extension(state): Extension<DevToolsState>) -> Response {
    Json(DevToolsLinks {
        links: state.list(),
    })
    .into_response()
}

/// Revoke every link at once, and end every session open on one.
///
/// The panic button: one call after which nobody is driving any browser in
/// this instance, whether or not anyone still knows which links exist.
#[utoipa::path(
    delete,
    path = "/api/devtools/links",
    tag = "devtools",
    responses(
        (status = 200, description = "How many links were revoked", body = DevToolsRevokedLinks),
        (status = 401, description = "Authentication required")
    )
)]
pub async fn revoke_all_links(Extension(state): Extension<DevToolsState>) -> Response {
    let revoked = state.revoke_all();
    if revoked > 0 {
        warn!(
            "Revoked all {} remote control links; any session open on one is closed",
            revoked
        );
    }
    Json(DevToolsRevokedLinks { revoked }).into_response()
}

/// Revoke a link before it expires, and end any session already open on it.
///
/// Takes the link's non-secret id, not its key: an operator revoking a link
/// they handed out should not have to keep a copy of the credential to do it,
/// and the id is the only part of a link that is safe to write down.
#[utoipa::path(
    delete,
    path = "/api/devtools/links/{id}",
    tag = "devtools",
    params(("id" = String, Path, description = "The link's non-secret id, as returned when it was minted")),
    responses(
        (status = 204, description = "The link is gone, and so is any session on it"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "No such link")
    )
)]
pub async fn revoke_link(
    Extension(state): Extension<DevToolsState>,
    Path(id): Path<String>,
) -> Response {
    if valid_link_id(&id) && state.revoke_by_id(&id) {
        info!("Revoked remote control link {}", id);
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

    if !state.config.full_devtools {
        // Our own page, served from under the key. It needs no address of its
        // own - the socket is one path along from wherever this was reached -
        // so nothing on this path has to trust a forwarded header.
        return match crate::assets::RemoteControlAssets::get("index.html") {
            Some(page) => {
                Html(String::from_utf8_lossy(page.data.as_ref()).into_owned()).into_response()
            }
            None => {
                error!("The remote control page is missing from this binary");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "The remote control page is missing from this build",
                )
                    .into_response()
            }
        };
    }

    let Some(host) = client_host(&headers) else {
        return (
            StatusCode::BAD_REQUEST,
            "Request carried no usable Host header",
        )
            .into_response();
    };

    // The DevTools application takes the socket as `ws=host/path` or
    // `wss=host/path` — scheme by parameter name, no scheme in the value.
    let param = if client_is_secure(&headers, &state.config) {
        "wss"
    } else {
        "ws"
    };

    // The host has already been checked to be a bare `host[:port]`; encoding
    // it as well means nothing in it can be read as query syntax even if that
    // check is ever loosened. The path after it stays literal, because the
    // application wants `host/path` and reads the value back decoded.
    let host = urlencoding::encode(&host);
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
    if !state.config.full_devtools {
        return (
            StatusCode::NOT_FOUND,
            "This instance does not serve the DevTools application. A remote control link \
             carries the page itself; set cef.full_devtools to serve DevTools instead.",
        )
            .into_response();
    }
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
    // Take the link's cancellation signal along with the target. The session
    // holds it for as long as it is open, so revoking the link or letting it
    // expire ends this session too — otherwise revocation would only refuse
    // the *next* one, and whoever already had the socket would keep control of
    // the browser for as long as they cared to hold it.
    let Some((target_id, cancelled)) = state.open_session(&key) else {
        return no_such_link();
    };

    let upstream = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target_id);
    let full_devtools = state.config.full_devtools;
    ws.on_upgrade(move |socket| pump(socket, upstream, target_id, cancelled, full_devtools))
}

/// Shuttle messages both ways until either side hangs up, or the link dies.
///
/// The protocol is text in both directions, and the screencast rides the same
/// socket as everything else: Chromium sends a JPEG per changed frame and
/// waits for the client to acknowledge it before sending the next. That
/// acknowledgement is the flow control, so a slow link costs frame rate rather
/// than an unbounded queue — which is what makes this usable over the
/// internet. Nothing here needs to know that; it just must not buffer.
async fn pump(
    client: WebSocket,
    upstream_url: String,
    target_id: String,
    mut cancelled: broadcast::Receiver<()>,
    full_devtools: bool,
) {
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

    // A refusal has to reach the client, whose sink belongs to the other half
    // of this pump, so it travels the same way Chromium's own answers do.
    let (refusals, mut refused) = tokio::sync::mpsc::unbounded_channel::<String>();

    let to_upstream = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let forwarded = match msg {
                Message::Text(t) => {
                    if !full_devtools {
                        if let Err(refusal) = screencast_allows(t.as_str()) {
                            debug!("Refused a method a remote control link does not carry");
                            if refusals.send(refusal).is_err() {
                                break;
                            }
                            continue;
                        }
                    }
                    WsMessage::Text(t.as_str().into())
                }
                // The protocol is text. A filtered session has no reason to
                // send anything else, and a binary frame cannot be checked
                // against the list, so it does not go.
                Message::Binary(b) => {
                    if !full_devtools {
                        continue;
                    }
                    WsMessage::Binary(b)
                }
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
        loop {
            let forwarded = tokio::select! {
                incoming = upstream_rx.next() => match incoming {
                    Some(Ok(WsMessage::Text(t))) => Message::Text(t.as_str().into()),
                    Some(Ok(WsMessage::Binary(b))) => Message::Binary(b),
                    Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_))) => {
                        continue
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                },
                refusal = refused.recv() => match refusal {
                    Some(text) => Message::Text(text.into()),
                    None => break,
                },
            };
            if client_tx.send(forwarded).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = to_upstream => {}
        _ = to_client => {}
        // Revoked or expired. A closed channel means the link is gone too:
        // the sender lives in the table entry, so dropping the entry is
        // itself the signal.
        _ = cancelled.recv() => {
            info!(
                "Remote control link revoked or expired; closing the session on target {}",
                target_id
            );
        }
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
            full_devtools: false,
        })
    }

    fn target(id: &str, url: &str) -> PageTarget {
        PageTarget {
            id: id.to_string(),
            url: url.to_string(),
        }
    }

    fn source(block_id: &str, url: &str) -> HtmlSource {
        HtmlSource {
            flow_id: FlowId::new_v4(),
            block_id: block_id.to_string(),
            url: url.to_string(),
            remote_control: true,
        }
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
            full_devtools: false,
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
            full_devtools: false,
        };
        assert!(!client_is_secure(&headers, &tls));
    }

    #[test]
    fn a_key_carries_the_target_and_nothing_else_does() {
        let s = state();
        let minted = s.mint(
            "4A8CD2F2840F8591A6277A7FFFDAB3A9".to_string(),
            "https://example.com/".to_string(),
        );
        // The key is what a client sees, so it must not spell out the target.
        assert!(valid_key(&minted.key));
        assert!(!minted.key.contains("4A8CD2F2"));
        // The id is a name for the link, not a second copy of the credential.
        assert!(valid_link_id(&minted.id));
        assert_ne!(minted.id, minted.key);
        assert!(!minted.key.contains(&minted.id));
        assert_eq!(
            s.resolve(&minted.key).as_deref(),
            Some("4A8CD2F2840F8591A6277A7FFFDAB3A9")
        );
    }

    #[test]
    fn a_trailing_slash_does_not_make_it_a_different_page() {
        assert!(same_page("https://example.com", "https://example.com/"));
        assert!(same_page("file:///demo/a.html", "file:///demo/a.html"));
        assert!(!same_page("https://example.com/a", "https://example.com/b"));
    }

    #[test]
    fn keys_are_not_guessable_from_each_other() {
        let s = state();
        let a = s.mint("AAAA".to_string(), "https://a.example".to_string());
        let b = s.mint("AAAA".to_string(), "https://a.example".to_string());
        assert_ne!(a.key, b.key);
        assert_ne!(a.id, b.id);
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
        let minted = s.mint("ABCD".to_string(), "https://a.example".to_string());
        assert!(s.revoke_by_id(&minted.id));
        assert!(s.resolve(&minted.key).is_none());
        // Revoking twice is not an error the caller can act on differently.
        assert!(!s.revoke_by_id(&minted.id));
    }

    #[test]
    fn an_expired_key_is_gone_even_before_anyone_asks() {
        let s = state();
        let minted = s.mint("ABCD".to_string(), "https://a.example".to_string());
        {
            let mut links = s.links.lock().unwrap();
            links.get_mut(&minted.key).unwrap().expires = Instant::now() - Duration::from_secs(1);
        }
        assert!(s.resolve(&minted.key).is_none());
        assert!(s.links.lock().unwrap().is_empty());
    }

    #[test]
    fn using_a_key_pushes_its_expiry_out() {
        let s = state();
        let minted = s.mint("ABCD".to_string(), "https://a.example".to_string());
        let first = s.links.lock().unwrap().get(&minted.key).unwrap().expires;
        std::thread::sleep(Duration::from_millis(5));
        assert!(s.resolve(&minted.key).is_some());
        let second = s.links.lock().unwrap().get(&minted.key).unwrap().expires;
        assert!(second > first, "a session in use must not expire under it");
    }

    // --- Revocation has to reach sessions that are already open ---

    #[tokio::test]
    async fn revoking_a_link_ends_the_session_already_open_on_it() {
        // Without this, revocation only closes the door to *new* sessions and
        // whoever already holds the socket keeps control of the browser.
        let s = state();
        let minted = s.mint("ABCD".to_string(), "https://a.example".to_string());
        let (_target, mut cancelled) = s.open_session(&minted.key).expect("session opens");

        assert!(s.revoke_by_id(&minted.id));

        // The signal arrives; a session selecting on it stops.
        let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
            .await
            .expect("the live session must be told within the second");
    }

    #[tokio::test]
    async fn revoking_everything_ends_every_open_session() {
        let s = state();
        let a = s.mint("A".to_string(), "https://a.example".to_string());
        let b = s.mint("B".to_string(), "https://b.example".to_string());
        let (_, mut first) = s.open_session(&a.key).expect("session opens");
        let (_, mut second) = s.open_session(&b.key).expect("session opens");

        assert_eq!(s.revoke_all(), 2);

        let _ = tokio::time::timeout(Duration::from_secs(1), first.recv())
            .await
            .expect("first session told");
        let _ = tokio::time::timeout(Duration::from_secs(1), second.recv())
            .await
            .expect("second session told");
        assert!(s.list().is_empty());
    }

    #[tokio::test]
    async fn an_expiring_link_ends_the_session_on_it() {
        // The TTL has to bite on a socket that is already open, the same way
        // revocation does - otherwise an established session never expires.
        let s = state();
        let minted = s.mint("ABCD".to_string(), "https://a.example".to_string());
        let (_, mut cancelled) = s.open_session(&minted.key).expect("session opens");
        {
            let mut links = s.links.lock().unwrap();
            links.get_mut(&minted.key).unwrap().expires = Instant::now() - Duration::from_secs(1);
        }
        // Any call that touches the table sweeps it.
        assert!(s.list().is_empty());
        let _ = tokio::time::timeout(Duration::from_secs(1), cancelled.recv())
            .await
            .expect("the expired session must be told");
    }

    #[test]
    fn a_listing_names_links_without_handing_the_key_back() {
        let s = state();
        let minted = s.mint("ABCD".to_string(), "https://a.example/login".to_string());
        let listed = s.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, minted.id);
        assert_eq!(listed[0].target_url, "https://a.example/login");
        assert!(listed[0].expires_in_seconds > 0);
        // Whatever else a listing carries, it is not the credential.
        let rendered = serde_json::to_string(&listed[0]).unwrap();
        assert!(
            !rendered.contains(&minted.key),
            "a listing must never carry the key"
        );
    }

    // --- Keys must not reach the log ---

    #[test]
    fn a_key_in_a_path_is_redacted_before_it_is_logged() {
        let key = "a".repeat(64);
        let path = format!("/devtools/{}/ui/core/common/common.js", key);
        let redacted = redact_path(&path);
        assert!(!redacted.contains(&key), "got {}", redacted);
        assert_eq!(redacted, "/devtools/<redacted>/ui/core/common/common.js");

        // The websocket path carries it too.
        assert_eq!(
            redact_path(&format!("/devtools/{}/ws", key)),
            "/devtools/<redacted>/ws"
        );
    }

    #[test]
    fn a_path_without_a_key_is_left_exactly_as_it_was() {
        for path in [
            "/api/flows",
            "/devtools/targets",
            // A link id is not a credential and an operator needs to see it.
            "/api/devtools/links/0123456789abcdef0123456789abcdef",
            "/",
        ] {
            assert_eq!(redact_path(path), path, "path {} was rewritten", path);
        }
    }

    // --- The host that ends up in the redirect ---

    #[test]
    fn only_a_bare_host_and_port_are_accepted() {
        assert!(valid_host("example.com"));
        assert!(valid_host("example.com:8080"));
        assert!(valid_host("192.0.2.10:8080"));
        assert!(valid_host("[2001:db8::1]"));
        assert!(valid_host("[2001:db8::1]:8080"));
        assert!(valid_host("localhost"));
    }

    #[test]
    fn a_host_carrying_query_syntax_is_refused() {
        // These land inside the inspector URL's query string, where an `&`
        // would append parameters of the sender's choosing and could point the
        // DevTools websocket at a host they control.
        for host in [
            "example.com&ws=attacker.example/x",
            "example.com?x=1",
            "example.com#frag",
            "example.com/path",
            "user@example.com",
            "example.com:notaport",
            "",
            " ",
        ] {
            assert!(!valid_host(host), "{} should be refused", host);
        }
    }

    #[test]
    fn a_forwarded_host_is_filtered_the_same_way_as_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "strom.example:8080".parse().unwrap());
        assert_eq!(client_host(&headers).as_deref(), Some("strom.example:8080"));

        // A crafted forwarded host does not win by being crafted - it is
        // dropped, and the Host header is not consulted as a fallback because
        // the client chose to send a forwarded host at all.
        headers.insert(
            "x-forwarded-host",
            "evil.example&ws=evil.example/x".parse().unwrap(),
        );
        assert!(client_host(&headers).is_none());
    }

    // --- Working out which page belongs to which block ---

    #[test]
    fn an_origin_is_the_scheme_host_and_port() {
        assert_eq!(
            origin("https://example.com/a/b?c"),
            Some("https://example.com")
        );
        assert_eq!(
            origin("https://example.com:8443/a"),
            Some("https://example.com:8443")
        );
        assert_eq!(origin("https://example.com"), Some("https://example.com"));
        assert_eq!(origin("data:text/html,hi"), None);
        assert!(same_origin("https://a.example/x", "https://a.example/y"));
        assert!(!same_origin("https://a.example/x", "https://b.example/x"));
        // Without an origin there is nothing to compare, so nothing matches.
        assert!(!same_origin("data:text/html,a", "data:text/html,a"));
    }

    #[test]
    fn a_page_still_on_its_url_is_matched_exactly() {
        let targets = vec![
            target("AAAA", "https://a.example/"),
            target("BBBB", "https://b.example/"),
        ];
        let sources = vec![
            source("one", "https://a.example"),
            source("two", "https://b.example"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Target("BBBB".to_string())
        );
    }

    #[test]
    fn a_same_site_redirect_still_resolves() {
        // The page was pointed at /dashboard and the server sent it to /login.
        // This is the ordinary shape of the case the feature exists for.
        let targets = vec![
            target("AAAA", "https://app.example/login?next=/dashboard"),
            target("BBBB", "https://other.example/"),
        ];
        let sources = vec![
            source("one", "https://app.example/dashboard"),
            source("two", "https://other.example/"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
    }

    #[test]
    fn a_cross_origin_login_redirect_resolves_when_nothing_else_could_be_it() {
        // The single HTML source in the instance was sent to an identity
        // provider on a different host. Its URL now shares nothing with what
        // was configured, and there is still only one page it can be.
        let targets = vec![target("AAAA", "https://login.idp.example/?redirect=x")];
        let sources = vec![source("one", "https://app.example/dashboard")];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Target("AAAA".to_string())
        );
    }

    #[test]
    fn a_cross_origin_redirect_with_a_second_stranded_source_is_refused() {
        // Two sources have both wandered off their configured URLs. Either
        // page could be either source's, so neither gets an answer.
        let targets = vec![
            target("AAAA", "https://login.idp.example/"),
            target("BBBB", "https://sso.other.example/"),
        ];
        let sources = vec![
            source("one", "https://app.example/dashboard"),
            source("two", "https://intranet.example/home"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Unknown
        );
    }

    #[test]
    fn two_blocks_on_one_url_are_refused_rather_than_guessed_between() {
        let targets = vec![
            target("AAAA", "https://a.example/"),
            target("BBBB", "https://a.example/"),
        ];
        let sources = vec![
            source("one", "https://a.example"),
            source("two", "https://a.example"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn two_blocks_on_one_site_are_refused_once_both_have_navigated() {
        // Both were pointed into the same site and both redirected, so the one
        // page left cannot be attributed. Widening the match must not turn
        // this into a guess.
        let targets = vec![target("AAAA", "https://app.example/login")];
        let sources = vec![
            source("one", "https://app.example/a"),
            source("two", "https://app.example/b"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_page_another_block_is_sitting_on_exactly_is_not_up_for_grabs() {
        // "two" is exactly where it said it would be, so its page is its own;
        // "one" must not be handed it just because they share a site.
        let targets = vec![target("BBBB", "https://app.example/b")];
        let sources = vec![
            source("one", "https://app.example/a"),
            source("two", "https://app.example/b"),
        ];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
        assert_eq!(
            resolve_target(&targets, &sources, &sources[1]),
            Resolution::Target("BBBB".to_string())
        );
    }

    #[test]
    fn a_block_whose_flow_is_not_running_gets_no_page() {
        let targets: Vec<PageTarget> = Vec::new();
        let sources = vec![source("one", "https://a.example")];
        assert_eq!(
            resolve_target(&targets, &sources, &sources[0]),
            Resolution::Unknown
        );
    }

    fn command(method: &str) -> String {
        serde_json::json!({ "id": 7, "method": method, "params": {} }).to_string()
    }

    #[test]
    fn a_link_carries_the_picture_and_the_input_for_it() {
        for method in SCREENCAST_METHODS {
            assert!(
                screencast_allows(&command(method)).is_ok(),
                "{} is what the page needs to work",
                method
            );
        }
    }

    #[test]
    fn a_link_does_not_carry_the_rest_of_the_protocol() {
        // Each of these is on its own enough to turn a link into control of
        // the host: script execution, navigation to the filesystem, the
        // cookie jar, the network, and the debugger.
        for method in [
            "Runtime.evaluate",
            "Runtime.callFunctionOn",
            "Page.navigate",
            "Page.captureSnapshot",
            "Storage.getCookies",
            "Network.getAllCookies",
            "Network.setRequestInterception",
            "Debugger.enable",
            "Target.createTarget",
            "Browser.getVersion",
            "DOM.getDocument",
            "Emulation.setDeviceMetricsOverride",
        ] {
            let refused = screencast_allows(&command(method))
                .expect_err(&format!("{} must not be forwarded", method));
            assert!(
                refused.contains(method),
                "the refusal has to name what was refused, got {}",
                refused
            );
        }
    }

    #[test]
    fn a_near_miss_is_not_waved_through() {
        // Substring matching would let all of these past.
        for method in [
            "Page.startScreencastEvil",
            "XPage.enable",
            "page.enable",
            "Page.Enable",
            "Input.dispatchMouseEvent.extra",
        ] {
            assert!(
                screencast_allows(&command(method)).is_err(),
                "{} is not on the list",
                method
            );
        }
    }

    #[test]
    fn a_refusal_answers_the_command_that_was_sent() {
        let refused = screencast_allows(&command("Runtime.evaluate")).unwrap_err();
        let parsed: serde_json::Value = serde_json::from_str(&refused).expect("valid JSON");
        // A client matches answers to commands by id; an answer without one is
        // an answer it will wait for forever.
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], -32601);
    }

    #[test]
    fn anything_that_is_not_a_command_is_refused() {
        assert!(screencast_allows("not json at all").is_err());
        assert!(screencast_allows("{}").is_err());
        assert!(screencast_allows(r#"{"id":1}"#).is_err());
        // A response, not a command - the client has no business sending one.
        assert!(screencast_allows(r#"{"id":1,"result":{}}"#).is_err());
    }

    #[test]
    fn the_remote_control_page_is_in_the_binary() {
        // open_link serves this; without it a link opens on an error page and
        // the whole feature is dead in a release build.
        assert!(crate::assets::RemoteControlAssets::get("index.html").is_some());
    }

    #[test]
    fn the_warning_says_which_of_the_two_modes_this_is() {
        let filtered = DevToolsConfig {
            debug_port: Some(9222),
            tls: false,
            full_devtools: false,
        };
        let unfiltered = DevToolsConfig {
            full_devtools: true,
            ..filtered
        };
        assert_eq!(link_warning(&filtered), SCREENCAST_CONTROL_WARNING);
        assert_eq!(link_warning(&unfiltered), REMOTE_CONTROL_WARNING);
        assert_ne!(link_warning(&filtered), link_warning(&unfiltered));
    }
}
