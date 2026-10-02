//! Which Chromium page each `cefsrc` renders.
//!
//! Remote control needs the page an HTML source is showing, and Chromium
//! knows a page only by its target id and current URL. The URL says nothing
//! about whose page it is: two sources can show the same one, and a page that
//! has logged in or redirected shows neither of the URLs it was given. The
//! target id, by contrast, stays the same for as long as the page lives,
//! whatever it navigates to.
//!
//! So a page is named when it is born. With the debug port open, every
//! `cefsrc` starts on `about:blank#strom-<token>`, a token nobody else knows.
//! Once Chromium lists a page with that URL, its target id is recorded
//! against the source and the element is pointed at its real URL. The blank
//! start is Chromium's initial empty document; the guard below clears it from
//! the page's history once the page has moved on.
//!
//! This works with any gstcefsrc: it needs nothing from the element but its
//! `url` property.
//!
//! A named page also gets a guard: a DevTools session of Strom's own, held
//! for the page's life, that refuses what an offscreen browser must never do.
//! gstcefsrc leaves these to CEF, where a file chooser is built in-process and
//! has aborted Strom, `print()` never returns, and a download lands on the
//! server's disk. See [`guard`]. From birth on, Strom also loads a new URL
//! into the page itself rather than through the element (see [`load_url`]).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use strom_types::FlowId;
use tracing::{debug, warn};

/// How often Chromium's page list is read while a page is being born.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long after its element has started a page may take to be listed
/// before Strom gives up naming it. `cefsrc` starts only once its browser
/// exists, so the page is normally listed at once; this bounds how long a
/// source shows a blank page when the debug port does not answer.
const BIRTH_TIMEOUT: Duration = Duration::from_secs(5);

/// The URL a page is born on, and the prefix Strom recognizes it by.
const MARKER_PREFIX: &str = "about:blank#strom-";

/// The `cefsrc` property holding the page's URL.
const URL_PROPERTY: &str = "url";

/// The source a page belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PageOwner {
    /// An HTML Input block.
    Block { flow_id: FlowId, block_id: String },
    /// A raw `cefsrc` element in a flow.
    Element { flow_id: FlowId, element_id: String },
}

impl PageOwner {
    fn flow_id(&self) -> &FlowId {
        match self {
            PageOwner::Block { flow_id, .. } | PageOwner::Element { flow_id, .. } => flow_id,
        }
    }
}

static DEBUG_PORT: OnceLock<u16> = OnceLock::new();

/// Target id per source. An entry outlives its page; whoever reads one checks
/// that Chromium still lists the target.
fn pages() -> &'static Mutex<HashMap<PageOwner, String>> {
    static PAGES: OnceLock<Mutex<HashMap<PageOwner, String>>> = OnceLock::new();
    PAGES.get_or_init(Default::default)
}

/// Name pages from now on, through Chromium's debug port on loopback. Set
/// once at startup, when the port in force is known; without it pages are
/// not named, and load their URL straight away.
pub fn set_debug_port(port: u16) {
    let _ = DEBUG_PORT.set(port);
}

/// The target id of the page `owner` renders, if it has been born.
pub fn target_of(owner: &PageOwner) -> Option<String> {
    pages().lock().ok()?.get(owner).cloned()
}

/// The source rendering the page `target_id`, if Strom named it.
pub fn owner_of(target_id: &str) -> Option<PageOwner> {
    pages()
        .lock()
        .ok()?
        .iter()
        .find(|(_, id)| id.as_str() == target_id)
        .map(|(owner, _)| owner.clone())
}

/// Record a page as born, for tests elsewhere that need one.
#[cfg(test)]
pub fn record_for_test(owner: PageOwner, target_id: String) {
    record(owner, target_id);
}

/// The URL each page being born goes to once it is named, by marker. A URL
/// set on air in the meantime replaces the entry instead of reaching the
/// element, so it cannot be overwritten by the one the element was built with.
fn pending() -> &'static Mutex<HashMap<String, String>> {
    static PENDING: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// A `cefsrc` whose page is driven over DevTools rather than through the
/// element, once the page is named.
///
/// gstcefsrc forgets its own browser once a popup the page opened closes:
/// the popup's close arrives in the element's handler and is taken for its
/// own. From then on a new `url` is ignored. Strom knows the page, so it
/// navigates it itself and never asks the element again.
struct Steered {
    element: glib::WeakRef<gst::Element>,
    target_id: String,
    /// The URL last loaded, which is what the element's own `url` would say.
    url: String,
}

fn steered() -> &'static Mutex<Vec<Steered>> {
    static STEERED: OnceLock<Mutex<Vec<Steered>>> = OnceLock::new();
    STEERED.get_or_init(Default::default)
}

/// Pages known to be on a URL already, by DevTools target id: the next live
/// write of exactly that URL to the page's element records it without
/// navigating. Set by [`already_showing`] and cleared by [`forget_showing`].
fn showing() -> &'static Mutex<HashMap<String, String>> {
    static SHOWING: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    SHOWING.get_or_init(Default::default)
}

/// Say that `owner`'s page is on `url` already, so that the next live write
/// of that URL makes it the element's URL without loading it again.
///
/// Remote control's "Set start page" pins the page the operator has reached.
/// Navigating to it would reload the page on air, losing a single-page app's
/// state or a form, or breaking a page reached through a one-time redirect.
pub fn already_showing(owner: &PageOwner, url: &str) {
    if let Some(target_id) = target_of(owner) {
        let mut showing = showing().lock().unwrap_or_else(|e| e.into_inner());
        showing.insert(target_id, url.to_string());
    }
}

/// Undo [`already_showing`] if no write used it, so a later write of the same
/// URL navigates as usual.
pub fn forget_showing(owner: &PageOwner) {
    if let Some(target_id) = target_of(owner) {
        let mut showing = showing().lock().unwrap_or_else(|e| e.into_inner());
        showing.remove(&target_id);
    }
}

/// Whether page `target_id` is known to be on `url`; consumes the note.
fn take_showing(target_id: &str, url: &str) -> bool {
    let mut showing = showing().lock().unwrap_or_else(|e| e.into_inner());
    if showing.get(target_id).is_some_and(|shown| shown == url) {
        showing.remove(target_id);
        true
    } else {
        false
    }
}

/// Point a running `cefsrc` at `url`.
///
/// A page still being born keeps its marker until it is named, and loads
/// `url` then. Every live URL write goes through here, so it cannot race the
/// birth.
pub fn load_url(cefsrc: &impl IsA<glib::Object>, url: &str) {
    let mut pending = pending().lock().unwrap_or_else(|e| e.into_inner());
    let current: Option<String> = cefsrc.property(URL_PROPERTY);
    if let Some(next) = current.and_then(|marker| pending.get_mut(&marker)) {
        *next = url.to_string();
        return;
    }
    drop(pending);
    if let (Some(&port), Ok(handle)) = (DEBUG_PORT.get(), tokio::runtime::Handle::try_current()) {
        let mut steered = steered().lock().unwrap_or_else(|e| e.into_inner());
        steered.retain(|s| s.element.upgrade().is_some());
        let this = cefsrc
            .upcast_ref::<glib::Object>()
            .downcast_ref::<gst::Element>();
        if let Some(entry) = steered
            .iter_mut()
            .find(|s| this.is_some() && s.element.upgrade().as_ref() == this)
        {
            entry.url = url.to_string();
            if !take_showing(&entry.target_id, url) {
                handle.spawn(navigate(port, entry.target_id.clone(), url.to_string()));
            }
            return;
        }
    }
    cefsrc.set_property(URL_PROPERTY, url);
}

/// Load `url` into page `target_id` over DevTools.
async fn navigate(port: u16, target_id: String, url: String) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let endpoint = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target_id);
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(&endpoint).await else {
        warn!("Could not reach page {} to load {}", target_id, url);
        return;
    };
    let command =
        serde_json::json!({ "id": 1, "method": "Page.navigate", "params": { "url": url } });
    if socket
        .send(Message::Text(command.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    // Wait for the answer, so the navigation has started before we hang up.
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
    let _ = socket.close(None).await;
}

/// A string property of `obj` as anyone outside this module should see it:
/// the URL [`shown_url`] reports for a `cefsrc`'s `url`, every other string
/// property as it is. Only `url` is steered; a steered element's `name` or
/// `context-cache-path` must not read back as the page it shows.
pub fn shown_string_property(obj: &glib::Object, property: &str, value: String) -> String {
    if property == URL_PROPERTY {
        shown_url(obj, value)
    } else {
        value
    }
}

/// The URL a `cefsrc` is set to, as anyone outside this module should see it.
///
/// While its page is being born the element holds the marker, which means
/// nothing to an operator and would be saved as the block's URL by a client
/// that reads a property and writes it back. An element Strom steers keeps
/// its first URL; the page is on the one last loaded.
pub fn shown_url(cefsrc: &glib::Object, url: String) -> String {
    if url.starts_with(MARKER_PREFIX) {
        let pending = pending().lock().unwrap_or_else(|e| e.into_inner());
        return pending.get(&url).cloned().unwrap_or(url);
    }
    let Some(this) = cefsrc.downcast_ref::<gst::Element>() else {
        return url;
    };
    let steered = steered().lock().unwrap_or_else(|e| e.into_inner());
    steered
        .iter()
        .find(|s| s.element.upgrade().as_ref() == Some(this))
        .map(|s| s.url.clone())
        .unwrap_or(url)
}

/// Forget the pages of a flow that has stopped. Its pages are closed, and a
/// restart names new ones.
pub fn forget_flow(flow_id: &FlowId) {
    if let Ok(mut pages) = pages().lock() {
        pages.retain(|owner, _| owner.flow_id() != flow_id);
    }
}

/// Load the URL a page being born was waiting for, once and only once.
///
/// With its page named, the element is steered from then on.
fn finish_birth(cefsrc: &impl IsA<glib::Object>, marker: &str, target_id: Option<&str>) {
    let mut pending = pending().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(url) = pending.remove(marker) {
        cefsrc.set_property(URL_PROPERTY, &url);
        let element = cefsrc
            .upcast_ref::<glib::Object>()
            .downcast_ref::<gst::Element>();
        if let (Some(element), Some(target_id)) = (element, target_id) {
            steered()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Steered {
                    element: element.downgrade(),
                    target_id: target_id.to_string(),
                    url,
                });
        }
    }
}

fn record(owner: PageOwner, target_id: String) {
    if let Ok(mut pages) = pages().lock() {
        // A target id belongs to one source only. A restarted page has a new
        // one, so the source's old entry is simply replaced.
        pages.retain(|_, id| *id != target_id);
        pages.insert(owner, target_id);
    }
}

/// Have this `cefsrc` name its page when it is born.
///
/// Call once the element's `url` is final and before it starts. The URL it
/// holds now is what it is pointed at once its page is named. Without a debug
/// port this does nothing.
pub fn name_page(cefsrc: &gst::Element, owner: PageOwner) {
    let Some(&port) = DEBUG_PORT.get() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        warn!(
            "{:?}: no async runtime to watch for its page, so remote control cannot find it",
            owner
        );
        return;
    };
    let real_url: String = cefsrc
        .property::<Option<String>>(URL_PROPERTY)
        .unwrap_or_default();
    let marker = format!("{}{}", MARKER_PREFIX, uuid::Uuid::new_v4().simple());
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(marker.clone(), real_url);
    cefsrc.set_property(URL_PROPERTY, &marker);

    // The task must not keep the element alive: a flow stopped before its
    // page was born would otherwise never be freed.
    let element = cefsrc.downgrade();
    handle.spawn(async move {
        // Until the element starts there is no page to look for, and the
        // flow may be some time getting there; only then does the clock run.
        let mut started: Option<Instant> = None;
        let target_id = loop {
            let Some(cefsrc) = element.upgrade() else {
                pending()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&marker);
                return;
            };
            if started.is_none() && cefsrc.current_state() >= gst::State::Paused {
                started = Some(Instant::now());
            }
            drop(cefsrc);
            if let Some(since) = started {
                if let Some(id) = find_page(port, &marker).await {
                    break Some(id);
                }
                if since.elapsed() > BIRTH_TIMEOUT {
                    break None;
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };
        match &target_id {
            Some(id) => {
                debug!("{:?} renders page {}", owner, id);
                record(owner.clone(), id.clone());
                // Before the page leaves the blank start, so the guard is in
                // place for the first document that could need it.
                guard(port, id.clone(), format!("{:?}", owner)).await;
            }
            None => warn!(
                "{:?}: Chromium did not list its page within {:?} of starting, so remote \
                 control cannot find it. Loading its URL anyway",
                owner, BIRTH_TIMEOUT
            ),
        }
        match element.upgrade() {
            Some(cefsrc) => finish_birth(&cefsrc, &marker, target_id.as_deref()),
            None => {
                pending()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&marker);
            }
        }
    });
}

/// Run in every document of a guarded page before the page's own script.
///
/// `print()` never returns offscreen. Screen capture would hand the page this
/// machine's display - on a desktop, the operator's real screen - because
/// gstcefsrc runs Chromium with `enable-media-stream`, which grants media
/// requests without asking the element, patched or not. Both are replaced on
/// the prototype, where the page cannot take them back.
const OFFSCREEN_SCRIPT: &str = r#"(() => {
  const lock = (target, name, value) => {
    try { Object.defineProperty(target, name, { value, writable: false, configurable: false }); } catch (e) {}
  };
  lock(window, "print", function () {});
  if (window.MediaDevices) {
    lock(MediaDevices.prototype, "getDisplayMedia", function () {
      return Promise.reject(new DOMException("Screen capture is not available", "NotAllowedError"));
    });
  }
})();"#;

/// What a frame from another site gets: the same as its page, as far as a
/// frame's own session can be told.
fn frame_guard() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("Page.enable", serde_json::json!({})),
        (
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": OFFSCREEN_SCRIPT }),
        ),
        (
            "Target.setAutoAttach",
            serde_json::json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        ),
    ]
}

/// What a page must not do in a browser nobody sits in front of.
///
/// - A file chooser is intercepted: Chromium reports it to this session
///   instead of building a dialog, and nothing more happens.
/// - Downloads are denied.
/// - `print()` does nothing. Offscreen, it never returns.
/// - Screen capture is refused.
/// - `alert`, `confirm` and `prompt` are dismissed as they open.
///
/// It also clears the page's history once the page has left its blank start
/// and finished loading.
/// Attached while the page is on it, the guard makes Chromium keep that start
/// as an entry, and Back would put a blank page on air.
///
/// Each of these holds only while the session that set it is attached, so the
/// guard stays connected for as long as the page lives. It is set up before
/// this returns; the rest of the guard's life runs on its own.
async fn guard(port: u16, target_id: String, who: String) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target_id);
    let mut socket = match tokio_tungstenite::connect_async(&url).await {
        Ok((socket, _)) => socket,
        Err(e) => {
            warn!("{}: could not guard its page: {}", who, e);
            return;
        }
    };
    let setup = [
        ("Page.enable", serde_json::json!({})),
        (
            "Page.setInterceptFileChooserDialog",
            serde_json::json!({ "enabled": true }),
        ),
        (
            "Page.setDownloadBehavior",
            serde_json::json!({ "behavior": "deny" }),
        ),
        (
            "Page.addScriptToEvaluateOnNewDocument",
            serde_json::json!({ "source": OFFSCREEN_SCRIPT }),
        ),
        // A frame from another site is a target of its own, which the page's
        // own commands do not reach. Each is held until it is guarded too.
        (
            "Target.setAutoAttach",
            serde_json::json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        ),
    ];
    let last = setup.len() as u64;
    for (id, (method, params)) in setup.into_iter().enumerate() {
        let command = serde_json::json!({ "id": id + 1, "method": method, "params": params });
        if socket
            .send(Message::Text(command.to_string().into()))
            .await
            .is_err()
        {
            warn!("{}: could not guard its page", who);
            return;
        }
    }
    // Chromium answers in order, so once the last command is answered every
    // one before it is in force, and the page can be sent on its way.
    let answered = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let Ok(answer) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(error) = answer.get("error") {
                warn!("{}: its page guard was refused a command: {}", who, error);
            }
            if answer.get("id").and_then(|i| i.as_u64()) == Some(last) {
                return true;
            }
        }
        false
    })
    .await;
    if !matches!(answered, Ok(true)) {
        warn!("{}: its page guard was not confirmed in time", who);
    }

    tokio::spawn(async move {
        let mut next_id = 100u64;
        // Whether the page has left its blank start, and the command that
        // clears the start from its history once it has finished loading.
        // A command sent while Chromium swaps the page's process is refused,
        // and is tried again at the next load.
        let mut left_start = false;
        let mut history_cleared = false;
        let mut reset_id: Option<u64> = None;
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let id = event.get("id").and_then(|i| i.as_u64());
            if id.is_some() && id == reset_id {
                reset_id = None;
                history_cleared = event.get("error").is_none();
                continue;
            }
            if let Some(error) = event.get("error") {
                // A dialog the plugin has already dismissed is gone by the
                // time ours arrives; that is the plugin doing its job.
                debug!("{}: its page guard was refused a command: {}", who, error);
                continue;
            }
            // Events from a frame of another site carry its session, and so
            // must anything sent back about them.
            let session = event
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            let mut sends: Vec<(&str, serde_json::Value, Option<String>)> = Vec::new();
            match event.get("method").and_then(|m| m.as_str()) {
                Some("Page.javascriptDialogOpening") => sends.push((
                    "Page.handleJavaScriptDialog",
                    serde_json::json!({ "accept": false }),
                    session,
                )),
                Some("Page.fileChooserOpened") => {
                    debug!("{}: refused a file chooser", who);
                }
                Some("Target.attachedToTarget") => {
                    let child = event["params"]["sessionId"].as_str().map(str::to_string);
                    if event["params"]["targetInfo"]["type"] == "iframe" {
                        for (method, params) in frame_guard() {
                            sends.push((method, params, child.clone()));
                        }
                    }
                    // Whatever it is, it was held for us; let it run.
                    sends.push((
                        "Runtime.runIfWaitingForDebugger",
                        serde_json::json!({}),
                        child,
                    ));
                }
                Some("Page.frameNavigated") if session.is_none() && !left_start => {
                    let frame = &event["params"]["frame"];
                    let main = frame.get("parentId").is_none();
                    let url = frame.get("url").and_then(|u| u.as_str()).unwrap_or("");
                    // CDP reports the URL without its fragment, so the
                    // blank start is plain about:blank here.
                    left_start = main && !url.starts_with("about:blank");
                }
                Some("Page.loadEventFired")
                    if session.is_none()
                        && left_start
                        && !history_cleared
                        && reset_id.is_none() =>
                {
                    sends.push(("Page.resetNavigationHistory", serde_json::json!({}), None));
                }
                _ => {}
            }
            let mut gone = false;
            for (method, params, session) in sends {
                next_id += 1;
                if method == "Page.resetNavigationHistory" {
                    reset_id = Some(next_id);
                }
                let mut command =
                    serde_json::json!({ "id": next_id, "method": method, "params": params });
                if let Some(session) = session {
                    command["sessionId"] = serde_json::Value::String(session);
                }
                if socket
                    .send(Message::Text(command.to_string().into()))
                    .await
                    .is_err()
                {
                    gone = true;
                    break;
                }
            }
            if gone {
                break;
            }
        }
        debug!("{}: page guard ended", who);
    });
}

/// Say what a stopped flow left open.
///
/// Stopping a `cefsrc` closes its page. Upstream gstcefsrc loses track of its
/// own browser once a popup the page opened closes, and then never closes it:
/// the page keeps running, logged in and on the network, until Strom exits.
/// Strom's build has the fix (popup-close.patch).
///
/// The page is not closed from here. Its browser still answers to the freed
/// element, and closing it runs the element's close handler on freed memory:
/// measured with isolated-context and without popup-close, the next flow
/// start aborted Strom with a corrupted heap.
pub async fn report_leftover_pages(flow_id: &FlowId) {
    let Some(&port) = DEBUG_PORT.get() else {
        return;
    };
    let leftover: Vec<String> = match pages().lock() {
        Ok(pages) => pages
            .iter()
            .filter(|(owner, _)| owner.flow_id() == flow_id)
            .map(|(_, id)| id.clone())
            .collect(),
        Err(_) => return,
    };
    if leftover.is_empty() {
        return;
    }
    let Some(listed) = list_pages(port).await else {
        return;
    };
    for id in leftover {
        if listed.iter().any(|t| t.id == id) {
            warn!(
                "Page {} of flow {} outlived its element and is still running, logged in \
                 and on the network, until Strom restarts. Upstream gstcefsrc does this after \
                 a popup closes; Strom's build does not",
                id, flow_id
            );
        }
    }
}

#[derive(serde::Deserialize)]
struct Listed {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default()
    })
}

/// The pages Chromium lists, popups included.
async fn list_pages(port: u16) -> Option<Vec<Listed>> {
    let listed: Vec<Listed> = client()
        .get(format!("http://127.0.0.1:{}/json/list", port))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(listed.into_iter().filter(|t| t.kind == "page").collect())
}

/// The target id of the page Chromium lists on `url`, if exactly one is.
async fn find_page(port: u16, url: &str) -> Option<String> {
    let mut matching = list_pages(port).await?.into_iter().filter(|t| t.url == url);
    let page = matching.next()?;
    matching.next().is_none().then_some(page.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(flow_id: FlowId, id: &str) -> PageOwner {
        PageOwner::Block {
            flow_id,
            block_id: id.to_string(),
        }
    }

    #[test]
    fn a_page_is_found_by_its_source_and_its_source_by_the_page() {
        let flow = FlowId::new_v4();
        record(block(flow, "a"), "AAAA".to_string());
        record(block(flow, "b"), "BBBB".to_string());
        assert_eq!(target_of(&block(flow, "a")).as_deref(), Some("AAAA"));
        assert_eq!(owner_of("BBBB"), Some(block(flow, "b")));
    }

    #[test]
    fn a_restarted_page_replaces_the_old_one() {
        let flow = FlowId::new_v4();
        record(block(flow, "a"), "OLD1".to_string());
        record(block(flow, "a"), "NEW1".to_string());
        assert_eq!(target_of(&block(flow, "a")).as_deref(), Some("NEW1"));
        assert_eq!(owner_of("OLD1"), None);
    }

    #[test]
    fn a_stopped_flow_forgets_its_pages_and_no_one_elses() {
        let stopped = FlowId::new_v4();
        let running = FlowId::new_v4();
        record(block(stopped, "a"), "STOP".to_string());
        record(block(running, "a"), "RUNS".to_string());
        forget_flow(&stopped);
        assert_eq!(target_of(&block(stopped, "a")), None);
        assert_eq!(target_of(&block(running, "a")).as_deref(), Some("RUNS"));
    }

    #[test]
    fn a_page_being_born_shows_the_url_it_will_load() {
        let marker = format!("{}{}", MARKER_PREFIX, "f00d");
        pending()
            .lock()
            .unwrap()
            .insert(marker.clone(), "https://example.com/".to_string());
        let element = fake::FakeCefSrc::with_url(&marker);
        let object = element.upcast_ref::<glib::Object>();
        assert_eq!(shown_url(object, marker.clone()), "https://example.com/");
        assert_eq!(
            shown_url(object, "https://other.example/".to_string()),
            "https://other.example/"
        );
        pending().lock().unwrap().remove(&marker);
    }

    /// "Set start page" pins the page the operator has reached; it must not
    /// reload it. The note covers one write of exactly that URL, and is gone
    /// once used or forgotten, so a later write navigates.
    #[test]
    fn a_page_already_showing_a_url_is_not_sent_there_again() {
        let flow = FlowId::new_v4();
        record(block(flow, "home"), "PAGEHOME".to_string());
        let owner = block(flow, "home");

        already_showing(&owner, "https://example.com/logged-in");
        assert!(!take_showing("PAGEHOME", "https://example.com/other"));
        assert!(take_showing("PAGEHOME", "https://example.com/logged-in"));
        assert!(
            !take_showing("PAGEHOME", "https://example.com/logged-in"),
            "the note is used once"
        );

        already_showing(&owner, "https://example.com/logged-in");
        forget_showing(&owner);
        assert!(!take_showing("PAGEHOME", "https://example.com/logged-in"));
        forget_flow(&flow);
    }

    /// Only `url` is mapped. Every string property of a steered element used
    /// to read back as the page URL, so a client reading and writing back the
    /// element's properties wrote that URL into `name` and the rest.
    #[test]
    fn only_the_url_property_reads_back_as_the_shown_url() {
        let marker = format!("{}{}", MARKER_PREFIX, "beef");
        pending()
            .lock()
            .unwrap()
            .insert(marker.clone(), "https://example.com/".to_string());
        let element = fake::FakeCefSrc::with_url(&marker);
        let object = element.upcast_ref::<glib::Object>();
        assert_eq!(
            shown_string_property(object, URL_PROPERTY, marker.clone()),
            "https://example.com/"
        );
        for other in ["name", "context-cache-path"] {
            assert_eq!(
                shown_string_property(object, other, marker.clone()),
                marker,
                "{} read back as the page URL",
                other
            );
        }
        pending().lock().unwrap().remove(&marker);
    }

    /// Stands in for a `cefsrc`: all a page's birth touches is its `url`.
    mod fake {
        use gstreamer::glib;
        use gstreamer::glib::subclass::prelude::*;
        use gstreamer::prelude::*;
        use std::cell::RefCell;

        #[derive(Default)]
        pub struct Imp {
            url: RefCell<Option<String>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for Imp {
            const NAME: &'static str = "StromFakeCefSrc";
            type Type = FakeCefSrc;
        }

        impl ObjectImpl for Imp {
            fn properties() -> &'static [glib::ParamSpec] {
                static PROPS: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                    std::sync::OnceLock::new();
                PROPS.get_or_init(|| vec![glib::ParamSpecString::builder("url").build()])
            }
            fn set_property(&self, _id: usize, value: &glib::Value, _pspec: &glib::ParamSpec) {
                *self.url.borrow_mut() = value.get().unwrap();
            }
            fn property(&self, _id: usize, _pspec: &glib::ParamSpec) -> glib::Value {
                self.url.borrow().to_value()
            }
        }

        glib::wrapper! {
            pub struct FakeCefSrc(ObjectSubclass<Imp>);
        }

        impl FakeCefSrc {
            pub fn with_url(url: &str) -> Self {
                glib::Object::builder().property("url", url).build()
            }
            pub fn url(&self) -> Option<String> {
                self.property("url")
            }
        }
    }

    /// Put a fake element on a marker, as `name_page` does.
    fn being_born(real: &str) -> (fake::FakeCefSrc, String) {
        let marker = format!("{}{}", MARKER_PREFIX, uuid::Uuid::new_v4().simple());
        pending()
            .lock()
            .unwrap()
            .insert(marker.clone(), real.to_string());
        (fake::FakeCefSrc::with_url(&marker), marker)
    }

    #[test]
    fn a_page_goes_to_its_url_once_it_is_named() {
        let (element, marker) = being_born("https://built.example/");
        finish_birth(&element, &marker, None);
        assert_eq!(element.url().as_deref(), Some("https://built.example/"));
    }

    #[test]
    fn a_url_set_on_air_during_birth_is_the_one_loaded() {
        let (element, marker) = being_born("https://built.example/");
        load_url(&element, "https://live.example/");
        // Still on the marker, so Chromium can still find the page.
        assert_eq!(element.url().as_deref(), Some(marker.as_str()));
        finish_birth(&element, &marker, None);
        assert_eq!(element.url().as_deref(), Some("https://live.example/"));
    }

    #[test]
    fn a_url_set_on_air_after_birth_goes_straight_to_the_element() {
        let (element, marker) = being_born("https://built.example/");
        finish_birth(&element, &marker, None);
        load_url(&element, "https://live.example/");
        assert_eq!(element.url().as_deref(), Some("https://live.example/"));
        // A late second finish changes nothing.
        finish_birth(&element, &marker, None);
        assert_eq!(element.url().as_deref(), Some("https://live.example/"));
    }

    #[test]
    fn a_target_belongs_to_one_source_only() {
        let flow = FlowId::new_v4();
        record(block(flow, "a"), "SAME".to_string());
        record(block(flow, "b"), "SAME".to_string());
        assert_eq!(owner_of("SAME"), Some(block(flow, "b")));
        assert_eq!(target_of(&block(flow, "a")), None);
    }
}
