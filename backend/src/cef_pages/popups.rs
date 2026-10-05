//! The guard for windows Strom did not open.
//!
//! A page that calls `window.open`, or follows a link into a new window, gets
//! a page of its own. The guard on the page that opened it does not reach it:
//! auto-attaching from a page session brings in its frames and workers, not
//! the windows it opens. Left alone, such a window could open a file chooser,
//! start a download, call `print()` or ask for the screen - everything
//! [`super::guard`] refuses its page.
//!
//! So while any `cefsrc` is alive, Strom holds one session on the whole
//! browser and has Chromium attach it to every new page and hold that page
//! before it runs anything of its own. Every page Strom did not name is given
//! the same guard as a named one, over that session, and only then let go. A
//! page Strom named is guarded by its own session already and is let go at
//! once.
//!
//! The guard on a page holds only while the session that set it is attached,
//! so the watch stays open for as long as an element is alive or a page it
//! guards is still open, and reconnects if Chromium drops it.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use super::{frame_guard, page_guard, OFFSCREEN_SCRIPT};

/// How often the watch checks whether anything is left for it to guard.
const LIFE_CHECK: Duration = Duration::from_secs(1);

/// How long the watch waits before trying the browser again, when it could
/// not reach it or lost it.
const RETRY: Duration = Duration::from_millis(200);

/// How long a page being born waits for the watch before it loads its URL.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// The elements whose pages may open windows, and whether a watch is running.
#[derive(Default)]
struct Watched {
    elements: Vec<glib::WeakRef<gst::Element>>,
    running: bool,
}

fn watched() -> &'static Mutex<Watched> {
    static WATCHED: OnceLock<Mutex<Watched>> = OnceLock::new();
    WATCHED.get_or_init(Default::default)
}

/// Whether the watch is attached to the browser, so a new page is held.
fn attached() -> &'static watch::Sender<bool> {
    static ATTACHED: OnceLock<watch::Sender<bool>> = OnceLock::new();
    ATTACHED.get_or_init(|| watch::channel(false).0)
}

/// Guard the windows `cefsrc`'s page opens, for as long as it is alive.
///
/// Starts the watch if none is running. The watch holds only a weak reference
/// to the element, so it never keeps a stopped flow alive.
pub(super) fn watch_for(cefsrc: &gst::Element, port: u16, handle: &tokio::runtime::Handle) {
    let mut watched = watched().lock().unwrap_or_else(|e| e.into_inner());
    watched.elements.retain(|e| e.upgrade().is_some());
    watched.elements.push(cefsrc.downgrade());
    if !watched.running {
        watched.running = true;
        handle.spawn(run(port));
    }
}

/// Wait until a page the browser opens from now on is held for the guard,
/// for at most [`READY_TIMEOUT`]. Whether it is.
pub(super) async fn ready() -> bool {
    let mut attached = attached().subscribe();
    let ready = matches!(
        tokio::time::timeout(READY_TIMEOUT, attached.wait_for(|a| *a)).await,
        Ok(Ok(_))
    );
    ready
}

/// Whether the watch has anything left to do. Ends it if not, in the same
/// step, so an element added meanwhile starts a new one.
fn keep_going(guarding: bool) -> bool {
    let mut watched = watched().lock().unwrap_or_else(|e| e.into_inner());
    watched.elements.retain(|e| e.upgrade().is_some());
    if watched.elements.is_empty() && !guarding {
        watched.running = false;
        false
    } else {
        true
    }
}

/// The watch: attached to the browser for as long as there is anything to
/// guard, and again whenever Chromium drops it.
async fn run(port: u16) {
    loop {
        if !keep_going(false) {
            break;
        }
        // Until the first `cefsrc` has started there is no browser to reach.
        if let Some(url) = super::browser_endpoint(port).await {
            if guard_browser(&url, attached(), keep_going).await {
                break;
            }
        }
        tokio::time::sleep(RETRY).await;
    }
    debug!("Browser-wide page guard ended");
}

/// Guard every new page over the browser endpoint at `url`, until
/// `keep_going` says to stop (true) or the connection is lost (false).
///
/// `keep_going` is told whether a page is still guarded through this
/// connection: closing it would lift that page's guard.
async fn guard_browser(
    url: &str,
    attached: &watch::Sender<bool>,
    mut keep_going: impl FnMut(bool) -> bool,
) -> bool {
    let socket = match tokio_tungstenite::connect_async(url).await {
        Ok((socket, _)) => socket,
        Err(e) => {
            debug!("Could not reach the browser to guard its pages: {}", e);
            return false;
        }
    };
    let (mut tx, mut rx) = socket.split();
    let mut guard = PopupGuard::default();
    let mut stopped = false;
    let mut tick = tokio::time::interval(LIFE_CHECK);
    'watch: {
        for command in guard.start() {
            if tx
                .send(Message::Text(command.to_string().into()))
                .await
                .is_err()
            {
                break 'watch;
            }
        }
        loop {
            tokio::select! {
                incoming = rx.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(_)) => continue,
                        _ => break,
                    };
                    let Ok(message) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let sends = guard.on_message(&message);
                    if guard.attached() {
                        attached.send_if_modified(|a| !std::mem::replace(a, true));
                    }
                    for command in sends {
                        if tx.send(Message::Text(command.to_string().into())).await.is_err() {
                            break 'watch;
                        }
                    }
                }
                _ = tick.tick() => {
                    if !keep_going(guard.guarding()) {
                        stopped = true;
                        break;
                    }
                }
            }
        }
    }
    attached.send_replace(false);
    let _ = tx.close().await;
    stopped
}

/// Whether Strom named page `target_id`, so its own guard already holds it.
///
/// A page being born is on its marker, which nobody else knows, until it is
/// recorded; a page merely on a URL that looks like a marker is not named.
fn named(target_id: &str, url: &str) -> bool {
    super::owner_of(target_id).is_some()
        || super::pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(url)
}

/// What the browser-wide session says, and what Strom answers. No I/O, so
/// what gets guarded can be tested without a browser.
#[derive(Default)]
struct PopupGuard {
    next_id: u64,
    /// The command that has Chromium attach us to new pages.
    start_id: Option<u64>,
    attached: bool,
    /// Targets held until the answer to the last of their guard's commands:
    /// by command id, the target's session and whether it is a page.
    held: HashMap<u64, (String, bool)>,
    /// Sessions of pages guarded over this connection and still attached.
    guarded: HashSet<String>,
}

impl PopupGuard {
    fn command(&mut self, method: &str, params: Value, session: Option<&str>) -> Value {
        self.next_id += 1;
        let mut command = json!({ "id": self.next_id, "method": method, "params": params });
        if let Some(session) = session {
            command["sessionId"] = Value::String(session.to_string());
        }
        command
    }

    /// The commands that start the watch: attach to every page, existing and
    /// new, and hold each new one until it is let go.
    fn start(&mut self) -> Vec<Value> {
        let command = self.command(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            None,
        );
        self.start_id = Some(self.next_id);
        vec![command]
    }

    /// Whether Chromium has agreed to attach us to new pages.
    fn attached(&self) -> bool {
        self.attached
    }

    /// Whether a page is guarded through this connection.
    fn guarding(&self) -> bool {
        !self.guarded.is_empty()
    }

    /// Guard `session`'s target with `commands`, and hold it until the last
    /// of them is answered.
    fn guard_target(
        &mut self,
        session: &str,
        page: bool,
        commands: Vec<(&'static str, Value)>,
    ) -> Vec<Value> {
        let sends: Vec<Value> = commands
            .into_iter()
            .map(|(method, params)| self.command(method, params, Some(session)))
            .collect();
        self.held.insert(self.next_id, (session.to_string(), page));
        sends
    }

    /// Let a held target run.
    fn release(&mut self, session: &str, page: bool) -> Vec<Value> {
        let mut sends =
            vec![self.command("Runtime.runIfWaitingForDebugger", json!({}), Some(session))];
        if page {
            // The window's first, empty document was there before the guard,
            // and its opener can reach into it: lock it as well.
            sends.push(self.command(
                "Runtime.evaluate",
                json!({ "expression": OFFSCREEN_SCRIPT }),
                Some(session),
            ));
        }
        sends
    }

    /// What to send in answer to one message from the browser endpoint.
    fn on_message(&mut self, message: &Value) -> Vec<Value> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let error = message.get("error");
            if Some(id) == self.start_id {
                match error {
                    Some(error) => warn!(
                        "Chromium refused to hold new pages for their guard, so a window a \
                         page opens is not guarded: {}",
                        error
                    ),
                    None => self.attached = true,
                }
            } else if let Some((session, page)) = self.held.remove(&id) {
                if let Some(error) = error {
                    warn!("A window's guard was refused a command: {}", error);
                }
                return self.release(&session, page);
            } else if let Some(error) = error {
                // A dialog its page has already closed, an evaluation in a
                // window that has gone.
                debug!(
                    "The browser-wide page guard was refused a command: {}",
                    error
                );
            }
            return Vec::new();
        }
        let params = &message["params"];
        let session = message.get("sessionId").and_then(Value::as_str);
        match message.get("method").and_then(Value::as_str) {
            Some("Target.attachedToTarget") => {
                let Some(child) = params["sessionId"].as_str() else {
                    return Vec::new();
                };
                let info = &params["targetInfo"];
                let id = info["targetId"].as_str().unwrap_or("");
                let url = info["url"].as_str().unwrap_or("");
                let opener = info["openerId"].as_str().filter(|o| !o.is_empty());
                match info["type"].as_str() {
                    // A window a page opened is never one Strom named.
                    Some("page") if opener.is_some() || !named(id, url) => {
                        debug!("Guarding page {}, opened by {:?}", id, opener);
                        self.guarded.insert(child.to_string());
                        self.guard_target(child, true, page_guard())
                    }
                    Some("iframe") => self.guard_target(child, false, frame_guard()),
                    // A page Strom named has its own guard; a worker has
                    // nothing to guard. Whatever it is, it was held for us.
                    _ => self.release(child, false),
                }
            }
            Some("Target.detachedFromTarget") => {
                if let Some(child) = params["sessionId"].as_str() {
                    self.guarded.remove(child);
                }
                Vec::new()
            }
            Some("Page.javascriptDialogOpening") if session.is_some() => vec![self.command(
                "Page.handleJavaScriptDialog",
                json!({ "accept": false }),
                session,
            )],
            Some("Page.fileChooserOpened") => {
                debug!("A window's guard refused a file chooser");
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cef_pages::{forget_flow, record, PageOwner, MARKER_PREFIX};
    use strom_types::FlowId;

    fn attached_to(session: Option<&str>, child: &str, info: Value) -> Value {
        let mut message = json!({
            "method": "Target.attachedToTarget",
            "params": { "sessionId": child, "targetInfo": info, "waitingForDebugger": true }
        });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        message
    }

    fn popup(id: &str, opener: &str) -> Value {
        json!({ "targetId": id, "type": "page", "url": "", "openerId": opener, "attached": true })
    }

    fn methods(sends: &[Value]) -> Vec<&str> {
        sends
            .iter()
            .map(|s| s["method"].as_str().unwrap())
            .collect()
    }

    fn answer(command: &Value) -> Value {
        json!({ "id": command["id"], "result": {} })
    }

    fn started() -> PopupGuard {
        let mut guard = PopupGuard::default();
        let start = guard.start();
        assert_eq!(methods(&start), ["Target.setAutoAttach"]);
        assert_eq!(start[0]["params"]["waitForDebuggerOnStart"], true);
        assert_eq!(start[0]["params"]["flatten"], true);
        assert!(start[0].get("sessionId").is_none(), "on the browser itself");
        assert!(!guard.attached());
        assert!(guard.on_message(&answer(&start[0])).is_empty());
        assert!(guard.attached());
        guard
    }

    /// The point of the watch: a window a page opens gets the page guard, on
    /// its own session, and runs only once that guard is in force.
    #[test]
    fn a_popup_is_guarded_before_it_runs() {
        let mut guard = started();
        let sends = guard.on_message(&attached_to(None, "S-POP", popup("POP", "MAIN")));
        let expected: Vec<&str> = page_guard().iter().map(|(m, _)| *m).collect();
        assert_eq!(methods(&sends), expected);
        assert!(methods(&sends).contains(&"Page.setInterceptFileChooserDialog"));
        assert!(methods(&sends).contains(&"Page.setDownloadBehavior"));
        assert!(sends.iter().all(|s| s["sessionId"] == "S-POP"));
        assert!(guard.guarding());

        // Answers before the last one do not let it go.
        for command in &sends[..sends.len() - 1] {
            assert!(guard.on_message(&answer(command)).is_empty());
        }
        let released = guard.on_message(&answer(sends.last().unwrap()));
        assert_eq!(
            methods(&released),
            ["Runtime.runIfWaitingForDebugger", "Runtime.evaluate"]
        );
        assert!(released.iter().all(|s| s["sessionId"] == "S-POP"));
        assert_eq!(released[1]["params"]["expression"], OFFSCREEN_SCRIPT);
    }

    /// A popup with no opener - `noopener`, or a page Strom never heard of -
    /// is guarded too. Only a page Strom named is left to its own guard.
    #[test]
    fn every_page_strom_did_not_name_is_guarded() {
        let flow = FlowId::new_v4();
        record(
            PageOwner::Block {
                flow_id: flow,
                block_id: "html".to_string(),
            },
            "NAMED-ROOT".to_string(),
        );
        let mut guard = started();

        let unknown = json!({ "targetId": "LONE", "type": "page", "url": "https://example.com/" });
        let sends = guard.on_message(&attached_to(None, "S-LONE", unknown));
        assert_eq!(methods(&sends)[0], "Page.enable");

        let root =
            json!({ "targetId": "NAMED-ROOT", "type": "page", "url": "https://example.com/" });
        let sends = guard.on_message(&attached_to(None, "S-ROOT", root));
        assert_eq!(methods(&sends), ["Runtime.runIfWaitingForDebugger"]);

        // Looking like a page being born is not being one.
        let fake = json!({
            "targetId": "FAKE", "type": "page", "url": format!("{}guess", MARKER_PREFIX)
        });
        let sends = guard.on_message(&attached_to(None, "S-FAKE", fake));
        assert_eq!(methods(&sends)[0], "Page.enable");

        // A named page's own popup is guarded, whatever it shows.
        let sends = guard.on_message(&attached_to(None, "S-P", popup("P", "NAMED-ROOT")));
        assert_eq!(methods(&sends)[0], "Page.enable");
        forget_flow(&flow);
    }

    #[test]
    fn a_page_being_born_is_left_to_its_own_guard() {
        let marker = format!("{}{}", MARKER_PREFIX, uuid::Uuid::new_v4().simple());
        crate::cef_pages::pending()
            .lock()
            .unwrap()
            .insert(marker.clone(), "https://example.com/".to_string());
        let mut guard = started();
        let born = json!({ "targetId": "BORN", "type": "page", "url": marker });
        let sends = guard.on_message(&attached_to(None, "S-BORN", born));
        assert_eq!(methods(&sends), ["Runtime.runIfWaitingForDebugger"]);
        crate::cef_pages::pending().lock().unwrap().remove(&marker);
    }

    /// A frame from another site inside a popup is a target of the popup's
    /// session, and gets what a frame in a named page gets.
    #[test]
    fn a_frame_in_a_popup_is_guarded_and_a_worker_just_runs() {
        let mut guard = started();
        let frame = json!({ "targetId": "FRAME", "type": "iframe", "url": "" });
        let sends = guard.on_message(&attached_to(Some("S-POP"), "S-FRAME", frame));
        let expected: Vec<&str> = frame_guard().iter().map(|(m, _)| *m).collect();
        assert_eq!(methods(&sends), expected);
        let released = guard.on_message(&answer(sends.last().unwrap()));
        assert_eq!(methods(&released), ["Runtime.runIfWaitingForDebugger"]);
        assert_eq!(released[0]["sessionId"], "S-FRAME");

        let worker = json!({ "targetId": "SW", "type": "service_worker", "url": "" });
        let sends = guard.on_message(&attached_to(None, "S-SW", worker));
        assert_eq!(methods(&sends), ["Runtime.runIfWaitingForDebugger"]);
        assert!(!guard.guarding(), "a worker is not a page to keep guarding");
    }

    #[test]
    fn a_dialog_in_a_popup_is_dismissed_on_its_session() {
        let mut guard = started();
        let dialog = json!({
            "method": "Page.javascriptDialogOpening",
            "sessionId": "S-POP",
            "params": { "type": "alert", "message": "hi" }
        });
        let sends = guard.on_message(&dialog);
        assert_eq!(methods(&sends), ["Page.handleJavaScriptDialog"]);
        assert_eq!(sends[0]["sessionId"], "S-POP");
        assert_eq!(sends[0]["params"]["accept"], false);
    }

    #[test]
    fn a_closed_popup_is_no_longer_guarded() {
        let mut guard = started();
        guard.on_message(&attached_to(None, "S-POP", popup("POP", "MAIN")));
        assert!(guard.guarding());
        guard.on_message(&json!({
            "method": "Target.detachedFromTarget",
            "params": { "sessionId": "S-POP", "targetId": "POP" }
        }));
        assert!(!guard.guarding());
    }

    type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn recv(ws: &mut Socket) -> Option<Value> {
        loop {
            match ws.next().await? {
                Ok(Message::Text(t)) => return serde_json::from_str(t.as_str()).ok(),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    async fn send(ws: &mut Socket, message: Value) {
        ws.send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
    }

    /// The watch over a real WebSocket, against a stand-in for Chromium's
    /// browser endpoint: it asks to hold new pages, guards a popup and lets
    /// it run, and closes the connection once there is nothing to guard.
    #[tokio::test]
    async fn the_watch_guards_a_popup_over_the_browser_endpoint() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "ws://{}/devtools/browser/test",
            listener.local_addr().unwrap()
        );
        let done = Arc::new(AtomicBool::new(false));

        let chromium = {
            let done = done.clone();
            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let start = recv(&mut ws).await.unwrap();
                assert_eq!(start["method"], "Target.setAutoAttach");
                send(&mut ws, answer(&start)).await;
                send(&mut ws, attached_to(None, "S-POP", popup("POP", "MAIN"))).await;

                let mut seen = Vec::new();
                loop {
                    let command = recv(&mut ws)
                        .await
                        .expect("the watch hung up before letting the popup run");
                    assert_eq!(command["sessionId"], "S-POP");
                    let method = command["method"].as_str().unwrap().to_string();
                    if method == "Runtime.runIfWaitingForDebugger" {
                        seen.push(method);
                        break;
                    }
                    seen.push(method);
                    send(&mut ws, answer(&command)).await;
                }
                // The popup closes; nothing is left to guard.
                send(
                    &mut ws,
                    json!({
                        "method": "Target.detachedFromTarget",
                        "params": { "sessionId": "S-POP", "targetId": "POP" }
                    }),
                )
                .await;
                done.store(true, Ordering::SeqCst);
                // The watch hangs up.
                while let Some(Ok(message)) = ws.next().await {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
                seen
            })
        };

        let (attached, mut seen_attached) = watch::channel(false);
        let watch = tokio::spawn(async move {
            let stopped = guard_browser(&url, &attached, |guarding| {
                guarding || !done.load(Ordering::SeqCst)
            })
            .await;
            (stopped, *attached.borrow())
        });

        tokio::time::timeout(Duration::from_secs(5), seen_attached.wait_for(|a| *a))
            .await
            .expect("the watch never said it was attached")
            .unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(10), chromium)
            .await
            .expect("the popup was never let go")
            .unwrap();
        assert!(seen.contains(&"Page.setInterceptFileChooserDialog".to_string()));
        assert!(seen.contains(&"Page.setDownloadBehavior".to_string()));
        assert_eq!(seen.last().unwrap(), "Runtime.runIfWaitingForDebugger");

        let (stopped, still_attached) = tokio::time::timeout(Duration::from_secs(5), watch)
            .await
            .expect("the watch did not end once nothing was left to guard")
            .unwrap();
        assert!(stopped);
        assert!(!still_attached);
    }
}
