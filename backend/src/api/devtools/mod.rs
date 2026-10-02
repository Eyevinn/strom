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
//! go back into it, along with a way back through the pages it has already
//! shown. The proxy forwards only what [`filter`] allows and answers
//! everything else with a protocol error, so the link is what it looks like
//! rather than what the debug port would otherwise be.
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
//! expires on its own after [`LINK_TTL`] of disuse — nobody opening it, and
//! nobody clicking, typing or navigating in a session on it — and can be
//! revoked before that, as switching the block's Remote Control off also does,
//! and either one also ends a session that is already open — otherwise
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

mod filter;
mod pages;
mod placement;
mod session;

use placement::{page_of, page_targets, running_source, source_of_page};

use crate::state::AppState;
use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, Extension, Path, RawQuery, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use strom_types::devtools::{
    DevToolsLink, DevToolsLinkSummary, DevToolsLinks, DevToolsRevokedLinks, DevToolsTarget,
    DevToolsTargets, REMOTE_CONTROL_WARNING, SCREENCAST_CONTROL_WARNING, SHARED_CONTEXT_WARNING,
};
use strom_types::{FlowId, StromEvent};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// How long a link survives without being used.
///
/// Long enough to walk to another machine, find the page and work through a
/// login with a second factor; short enough that a link left in a chat log is
/// dead by the time anyone reads it. Every use pushes it out again: opening
/// the link, and the operator's input in a session on it, so a session being
/// worked in does not expire under the operator. A tab left open and watched
/// is not use, and is closed when the time runs out.
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
    /// into it, because [`filter`] is all the proxy forwards. On,
    /// a link carries the whole Chrome DevTools Protocol, which is arbitrary
    /// JavaScript, navigation to `file://` and every cookie in the profile.
    /// The two cannot be combined: DevTools needs the domains the filter
    /// exists to refuse, so this is the escape hatch, not a richer mode.
    pub full_devtools: bool,
}

/// What to tell an operator this link hands over, which depends on whether the
/// protocol is filtered and whether each HTML source has a browser context of
/// its own.
///
/// Isolation is asked of the plugin when the link is minted rather than at
/// startup: a link is minted for a page that is rendering, so the plugin is
/// loaded by then, and loading it any earlier is not free.
fn link_warning(config: &DevToolsConfig) -> String {
    warning_for(
        config,
        crate::blocks::builtin::html_input::plugin_isolates(),
    )
}

fn warning_for(config: &DevToolsConfig, isolated: bool) -> String {
    if config.full_devtools {
        REMOTE_CONTROL_WARNING.to_string()
    } else if isolated {
        SCREENCAST_CONTROL_WARNING.to_string()
    } else {
        format!("{} {}", SCREENCAST_CONTROL_WARNING, SHARED_CONTEXT_WARNING)
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
    /// The HTML source the link was minted for.
    source: LinkSource,
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

/// The HTML source a link was minted for, as the session page presents it.
#[derive(Clone, Debug)]
struct LinkSource {
    flow_id: FlowId,
    block_id: String,
    /// The page the block was pointed at, which the session's home button
    /// navigates to. The client asks for it by name and never supplies it.
    home_url: String,
    flow_name: String,
    block_name: String,
    /// The block's Strict Network Access, which the filter holds navigation
    /// and a new start page to.
    strict: bool,
}

impl LinkSource {
    /// What the proxy tells the page before anything else, so its header can
    /// say which source is under control. Not a Chromium event: the name is
    /// ours, and nothing Chromium sends can collide with it.
    fn context_message(&self) -> String {
        serde_json::json!({
            "method": "Strom.context",
            "params": {
                "flow": self.flow_name,
                "block": self.block_name,
                "home": self.home_url,
            }
        })
        .to_string()
    }
}

/// What a session opened on a link needs to run.
struct Session {
    target_id: String,
    source: LinkSource,
    cancelled: broadcast::Receiver<()>,
    hold: LinkHold,
}

/// A session's hold on the link it was opened on.
///
/// The table is otherwise swept only when something touches it, so without
/// this a session would neither keep its link alive while the operator works
/// in it, nor be ended by its expiry while nobody else asked about links.
struct LinkHold {
    links: Arc<Mutex<HashMap<String, Link>>>,
    key: String,
}

impl LinkHold {
    /// The operator used the link: start its time over.
    fn used(&self) {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        DevToolsState::sweep(&mut links, now);
        if let Some(link) = links.get_mut(&self.key) {
            link.expires = now + LINK_TTL;
        }
    }

    /// Nobody used it: let an expiry take effect.
    fn idle(&self) {
        DevToolsState::sweep(&mut self.links.lock().unwrap(), Instant::now());
    }
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
    fn mint(&self, target_id: String, target_url: String, source: LinkSource) -> MintedLink {
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
                source,
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

    /// Resolve a key to its target, its home page, and a signal that fires
    /// when the link dies.
    ///
    /// A session holds the signal for as long as it is open, so revoking or
    /// expiring the link ends the session rather than only refusing the next
    /// one.
    fn open_session(&self, key: &str) -> Option<Session> {
        let mut links = self.links.lock().unwrap();
        let now = Instant::now();
        Self::sweep(&mut links, now);
        let link = links.get_mut(key)?;
        link.expires = now + LINK_TTL;
        Some(Session {
            target_id: link.target_id.clone(),
            source: link.source.clone(),
            cancelled: link.cancel.subscribe(),
            hold: LinkHold {
                links: self.links.clone(),
                key: key.to_string(),
            },
        })
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

    /// Revoke every link whose block no longer allows one, ending any session
    /// open on it: the flow or block is gone, or Remote Control is off.
    ///
    /// The switch is read when a link is minted, so without this turning it
    /// off would only stop new links, and a link already handed out would
    /// keep control of the page until it went unused for [`LINK_TTL`].
    pub async fn revoke_disallowed(&self, app: &AppState) -> usize {
        let held: Vec<(String, LinkSource)> = {
            let mut links = self.links.lock().unwrap();
            Self::sweep(&mut links, Instant::now());
            links
                .iter()
                .map(|(key, link)| (key.clone(), link.source.clone()))
                .collect()
        };
        let mut disallowed = Vec::new();
        for (key, source) in held {
            if !still_allowed(app, &source).await {
                disallowed.push(key);
            }
        }
        let mut links = self.links.lock().unwrap();
        let mut revoked = 0;
        for key in disallowed {
            if let Some(link) = links.remove(&key) {
                let _ = link.cancel.send(());
                revoked += 1;
            }
        }
        if revoked > 0 {
            info!(
                "Revoked {} remote control link(s) whose HTML source no longer allows one",
                revoked
            );
        }
        revoked
    }

    /// Revoke links as soon as a flow changes under them, rather than at the
    /// next request on them.
    ///
    /// A session already open makes no request of its own that could notice,
    /// so this is what ends it when an operator switches Remote Control off.
    /// Nothing to watch when the debug port is closed: no link can exist.
    pub fn watch_flows(&self, app: AppState) {
        if self.config.debug_port.is_none() {
            return;
        }
        let state = self.clone();
        let mut events = app.events().subscribe();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match events.recv().await {
                    Ok(StromEvent::FlowUpdated { .. } | StromEvent::FlowDeleted { .. }) => {}
                    Ok(_) => continue,
                    // A change may have been among what was missed.
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
                state.revoke_disallowed(&app).await;
            }
        });
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

/// Whether the block a link was minted for still allows one: it still exists,
/// is still an HTML source, and still has Remote Control switched on.
async fn still_allowed(app: &AppState, source: &LinkSource) -> bool {
    let Some(flow) = app.get_flow(&source.flow_id).await else {
        return false;
    };
    flow.blocks.iter().any(|b| {
        b.id == source.block_id
            && b.block_definition_id == crate::blocks::builtin::html_input::BLOCK_ID
            && crate::blocks::builtin::html_input::remote_control_enabled(&b.properties)
    })
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

    // No cefsrc has started yet, so CEF has not initialized and nothing is
    // listening. That is ordinary, not a fault.
    let Some(all) = pages::all_targets(port).await else {
        debug!("DevTools endpoint on port {} did not answer", port);
        return Json(DevToolsTargets {
            enabled: true,
            warning: Some(link_warning(&state.config)),
            targets: Vec::new(),
        })
        .into_response();
    };

    // Popups belong to the page that opened it, and a link to that page shows
    // them; listed on their own they would look like HTML sources.
    let targets = all
        .into_iter()
        .filter(|t| t.kind == "page" && t.opener.is_none())
        .filter_map(|t| {
            if !valid_target_id(&t.id) {
                warn!("Ignoring DevTools target with an unexpected id shape");
                return None;
            }
            Some(DevToolsTarget {
                id: t.id,
                title: t.title,
                url: t.url,
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
fn minted(
    state: &DevToolsState,
    target_id: String,
    target_url: String,
    source: LinkSource,
) -> Response {
    let minted = state.mint(target_id, target_url, source);
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
/// Control switch: the target has to be the page of an HTML source with the
/// switch on, the same page [`create_block_link`] would hand out for it. The
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

    // The switch lives on the block, so the target has to be traced back to the
    // one it was born for before it can be opened.
    let owner = source_of_page(&app, &target.id)
        .await
        .filter(|s| s.remote_control);
    let Some(owner) = owner else {
        return (
            StatusCode::FORBIDDEN,
            "No HTML source with remote control switched on is rendering this page. Turn on \
             the Remote Control property of the block that renders it to hand out a link.",
        )
            .into_response();
    };

    minted(&state, target.id, target.url, owner.link_source())
}

/// Mint a remote control link for one HTML source.
///
/// The block is the name an operator has for a page, so this is the endpoint a
/// client uses; the target id it resolves to is Chromium's business and
/// changes whenever the page is recreated.
///
/// The page is the one the block's `cefsrc` was born with (see
/// [`crate::cef_pages`]), whatever URL it is on now and however many other
/// blocks share its URL.
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
        (status = 404, description = "No such block, or it is not rendering a page yet")
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

    let Some(source) = running_source(&app, &flow_id, &block_id).await else {
        return (
            StatusCode::NOT_FOUND,
            "This block is not rendering a page - is the flow running?",
        )
            .into_response();
    };
    // Starting is the only time a running block has no page: CEF is still
    // initializing, or the page has not been named yet.
    let page = match page_targets(port).await {
        Some(targets) => page_of(&source, &targets),
        None => None,
    };
    let Some(page) = page else {
        return (
            StatusCode::NOT_FOUND,
            "This block's page is still starting. Try again in a moment.",
        )
            .into_response();
    };
    minted(&state, page.id, page.url, source.link_source())
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
    State(app): State<AppState>,
    Extension(state): Extension<DevToolsState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Response {
    if state.config.debug_port.is_none() {
        return disabled();
    }
    if !valid_key(&key) {
        return no_such_link();
    }
    state.revoke_disallowed(&app).await;
    if state.resolve(&key).is_none() {
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
    State(app): State<AppState>,
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
    if !valid_key(&key) {
        return no_such_link();
    }
    state.revoke_disallowed(&app).await;
    if state.resolve(&key).is_none() {
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
    State(app): State<AppState>,
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
    state.revoke_disallowed(&app).await;
    let Some(session) = state.open_session(&key) else {
        return no_such_link();
    };

    let full_devtools = state.config.full_devtools;
    ws.on_upgrade(move |socket| session::pump(socket, port, session, full_devtools, app))
}

#[cfg(test)]
mod tests;
