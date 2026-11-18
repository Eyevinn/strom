//! The windows a remote control session may show: its page, and the popups
//! that page opened.
//!
//! A login that opens a popup - an OAuth consent screen, "Sign in with..." -
//! gets a window of its own. With the Chrome runtime it is not painted into
//! the source's video, so it never goes on air, but it is also not the page
//! the link was minted for. The session therefore follows it: Chromium reports
//! every target with the target that opened it, and a session may show its
//! own page and anything that page, or its popups, opened. Nothing else - a
//! target outside that family belongs to another HTML source.

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::cef_pages::browser_endpoint;

/// One Chromium target, as the browser endpoint describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    pub id: String,
    pub kind: String,
    pub url: String,
    pub title: String,
    /// The target whose `window.open` created this one, if any.
    pub opener: Option<String>,
}

impl TargetInfo {
    fn from_cdp(info: &Value) -> Option<Self> {
        Some(Self {
            id: info.get("targetId")?.as_str()?.to_string(),
            kind: info.get("type")?.as_str()?.to_string(),
            url: info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            opener: info
                .get("openerId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }
}

/// What the browser endpoint says about targets as they come and go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetEvent {
    /// Created, or changed its URL or title.
    Updated(TargetInfo),
    Destroyed(String),
}

impl TargetEvent {
    /// The event in a `Target.*` notification, if it is one.
    pub fn from_cdp(message: &Value) -> Option<Self> {
        let params = message.get("params")?;
        match message.get("method")?.as_str()? {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                TargetInfo::from_cdp(params.get("targetInfo")?).map(TargetEvent::Updated)
            }
            "Target.targetDestroyed" => Some(TargetEvent::Destroyed(
                params.get("targetId")?.as_str()?.to_string(),
            )),
            _ => None,
        }
    }
}

/// Every target the browser knows of, and which of them one session may show.
#[derive(Debug)]
pub struct PageFamily {
    root: String,
    /// In the order Chromium reported them, so a listing is stable.
    targets: Vec<TargetInfo>,
}

impl PageFamily {
    pub fn new(root: String) -> Self {
        Self {
            root,
            targets: Vec::new(),
        }
    }

    pub fn apply(&mut self, event: TargetEvent) {
        match event {
            TargetEvent::Updated(info) => match self.targets.iter_mut().find(|t| t.id == info.id) {
                Some(existing) => *existing = info,
                None => self.targets.push(info),
            },
            TargetEvent::Destroyed(id) => self.targets.retain(|t| t.id != id),
        }
    }

    fn get(&self, id: &str) -> Option<&TargetInfo> {
        self.targets.iter().find(|t| t.id == id)
    }

    /// Whether `id` is this session's page or a window opened from it.
    ///
    /// Walks the opener chain, so a popup opened by a popup counts. The walk
    /// is bounded by the number of targets, so a cycle - which Chromium does
    /// not produce - cannot hang it.
    pub fn contains(&self, id: &str) -> bool {
        if id == self.root {
            return true;
        }
        let mut current = id;
        for _ in 0..=self.targets.len() {
            let Some(target) = self.get(current) else {
                return false;
            };
            if target.kind != "page" && current == id {
                return false;
            }
            match target.opener.as_deref() {
                Some(opener) if opener == self.root => return true,
                Some(opener) => current = opener,
                None => return false,
            }
        }
        false
    }

    /// The address a popup of this session is showing, for loading it into the
    /// session's own page. `None` for the session's own page, for anything not
    /// opened from it, and for a target Chromium has not described yet.
    pub fn popup_url(&self, id: &str) -> Option<&str> {
        if id == self.root || !self.contains(id) {
            return None;
        }
        self.get(id).map(|t| t.url.as_str())
    }

    /// The pages this session may show: its own first, then its popups in the
    /// order they were opened.
    pub fn pages(&self) -> Vec<&TargetInfo> {
        let root = self.get(&self.root).into_iter();
        let popups = self
            .targets
            .iter()
            .filter(|t| t.id != self.root && t.kind == "page" && self.contains(&t.id));
        root.chain(popups).collect()
    }

    /// Where to go when `id` closes: the page that opened it, if that is still
    /// open and ours, or else the session's own page.
    pub fn fallback_for(&self, id: &str) -> String {
        self.get(id)
            .and_then(|t| t.opener.clone())
            .filter(|opener| self.get(opener).is_some() && self.contains(opener))
            .unwrap_or_else(|| self.root.clone())
    }

    /// What the proxy tells the page about the windows it may switch between.
    pub fn message(&self, current: &str) -> String {
        let pages: Vec<Value> = self
            .pages()
            .into_iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "url": t.url,
                    "title": t.title,
                    "popup": t.id != self.root,
                })
            })
            .collect();
        json!({
            "method": "Strom.pages",
            "params": { "current": current, "pages": pages }
        })
        .to_string()
    }
}

/// Every target in the browser, with its opener.
///
/// `/json/list` leaves the opener out, and that is what tells a popup from
/// the page an HTML source renders.
pub async fn all_targets(port: u16) -> Option<Vec<TargetInfo>> {
    let url = browser_endpoint(port).await?;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.ok()?;
    socket
        .send(WsMessage::Text(
            json!({ "id": 1, "method": "Target.getTargets" })
                .to_string()
                .into(),
        ))
        .await
        .ok()?;
    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(Ok(message)) = socket.next().await {
            if let WsMessage::Text(text) = message {
                let value: Value = serde_json::from_str(text.as_str()).ok()?;
                if value.get("id").and_then(Value::as_i64) == Some(1) {
                    return Some(value);
                }
            }
        }
        None
    })
    .await
    .ok()??;
    let _ = socket.close(None).await;
    Some(
        answer
            .get("result")?
            .get("targetInfos")?
            .as_array()?
            .iter()
            .filter_map(TargetInfo::from_cdp)
            .collect(),
    )
}

/// A watch on the browser's targets, and a way to close one.
pub struct BrowserWatch {
    pub events: mpsc::UnboundedReceiver<TargetEvent>,
    close: mpsc::UnboundedSender<String>,
}

impl BrowserWatch {
    /// Start watching. Chromium reports every existing target first, then
    /// every change, for as long as the watch is held.
    pub async fn start(port: u16) -> Option<Self> {
        let url = browser_endpoint(port).await?;
        let (socket, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("Could not watch the browser's targets: {}", e);
                return None;
            }
        };
        let (mut tx, mut rx) = socket.split();
        tx.send(WsMessage::Text(
            json!({ "id": 1, "method": "Target.setDiscoverTargets", "params": { "discover": true } })
                .to_string()
                .into(),
        ))
        .await
        .ok()?;

        let (events_tx, events) = mpsc::unbounded_channel();
        let (close, mut close_rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut next_id = 2;
            loop {
                tokio::select! {
                    incoming = rx.next() => {
                        let Some(Ok(WsMessage::Text(text))) = incoming else {
                            match incoming {
                                Some(Ok(_)) => continue,
                                _ => break,
                            }
                        };
                        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
                            continue;
                        };
                        if let Some(event) = TargetEvent::from_cdp(&value) {
                            if events_tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    target = close_rx.recv() => {
                        let Some(target) = target else { break };
                        let command = json!({
                            "id": next_id,
                            "method": "Target.closeTarget",
                            "params": { "targetId": target }
                        });
                        next_id += 1;
                        if tx.send(WsMessage::Text(command.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.close().await;
            debug!("Browser target watch ended");
        });
        Some(Self { events, close })
    }

    /// The events, and a handle to close targets with, apart - a session
    /// waits on one while using the other.
    pub fn split(self) -> (mpsc::UnboundedReceiver<TargetEvent>, TargetCloser) {
        (self.events, TargetCloser(self.close))
    }
}

/// Closes targets through a [`BrowserWatch`].
pub struct TargetCloser(mpsc::UnboundedSender<String>);

impl TargetCloser {
    /// Ask Chromium to close a target. The caller has checked it is one the
    /// session may touch.
    pub fn close(&self, target: &str) {
        let _ = self.0.send(target.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, opener: Option<&str>) -> TargetEvent {
        TargetEvent::Updated(TargetInfo {
            id: id.to_string(),
            kind: "page".to_string(),
            url: format!("https://{}.example/", id.to_lowercase()),
            title: String::new(),
            opener: opener.map(str::to_string),
        })
    }

    fn family() -> PageFamily {
        let mut f = PageFamily::new("MAIN".to_string());
        f.apply(page("MAIN", None));
        f.apply(page("OTHER", None));
        f
    }

    #[test]
    fn a_popup_from_our_page_is_ours_and_another_source_is_not() {
        let mut f = family();
        f.apply(page("LOGIN", Some("MAIN")));
        f.apply(page("THEIRS", Some("OTHER")));
        assert!(f.contains("MAIN"));
        assert!(f.contains("LOGIN"));
        // Another HTML source, and a popup it opened, are out of reach.
        assert!(!f.contains("OTHER"));
        assert!(!f.contains("THEIRS"));
        assert!(!f.contains("UNKNOWN"));
    }

    #[test]
    fn a_popup_opened_by_our_popup_is_ours_too() {
        let mut f = family();
        f.apply(page("LOGIN", Some("MAIN")));
        f.apply(page("MFA", Some("LOGIN")));
        assert!(f.contains("MFA"));
        let ids: Vec<&str> = f.pages().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["MAIN", "LOGIN", "MFA"]);
    }

    #[test]
    fn a_service_worker_opened_by_our_page_is_not_a_page_to_show() {
        let mut f = family();
        f.apply(TargetEvent::Updated(TargetInfo {
            id: "WORKER".to_string(),
            kind: "service_worker".to_string(),
            url: String::new(),
            title: String::new(),
            opener: Some("MAIN".to_string()),
        }));
        assert!(!f.contains("WORKER"));
        assert_eq!(f.pages().len(), 1);
    }

    #[test]
    fn only_our_own_popups_can_be_adopted() {
        let mut family = family();
        family.apply(page("POPUP", Some("MAIN")));
        family.apply(page("THEIRS", Some("OTHER")));
        assert_eq!(family.popup_url("POPUP"), Some("https://popup.example/"));
        assert_eq!(
            family.popup_url("MAIN"),
            None,
            "the page is not its own popup"
        );
        assert_eq!(family.popup_url("OTHER"), None, "another source's page");
        assert_eq!(family.popup_url("THEIRS"), None, "another source's popup");
        assert_eq!(family.popup_url("GONE"), None);
    }

    #[test]
    fn closing_a_popup_goes_back_to_whoever_opened_it() {
        let mut f = family();
        f.apply(page("LOGIN", Some("MAIN")));
        f.apply(page("MFA", Some("LOGIN")));
        assert_eq!(f.fallback_for("MFA"), "LOGIN");
        f.apply(TargetEvent::Destroyed("LOGIN".to_string()));
        // Its opener is gone as well, so the session's own page it is.
        assert_eq!(f.fallback_for("MFA"), "MAIN");
    }

    #[test]
    fn a_popup_that_closes_leaves_the_listing() {
        let mut f = family();
        f.apply(page("LOGIN", Some("MAIN")));
        f.apply(TargetEvent::Destroyed("LOGIN".to_string()));
        assert!(!f.contains("LOGIN"));
        let sent: Value = serde_json::from_str(&f.message("MAIN")).unwrap();
        assert_eq!(sent["method"], "Strom.pages");
        assert_eq!(sent["params"]["pages"].as_array().unwrap().len(), 1);
        assert_eq!(sent["params"]["pages"][0]["popup"], false);
    }

    #[test]
    fn target_events_are_read_from_chromium_notifications() {
        let created = json!({
            "method": "Target.targetCreated",
            "params": { "targetInfo": {
                "targetId": "LOGIN", "type": "page", "url": "https://login.example/",
                "title": "Sign in", "openerId": "MAIN", "attached": false
            }}
        });
        assert_eq!(
            TargetEvent::from_cdp(&created),
            Some(TargetEvent::Updated(TargetInfo {
                id: "LOGIN".to_string(),
                kind: "page".to_string(),
                url: "https://login.example/".to_string(),
                title: "Sign in".to_string(),
                opener: Some("MAIN".to_string()),
            }))
        );
        let destroyed =
            json!({ "method": "Target.targetDestroyed", "params": { "targetId": "LOGIN" } });
        assert_eq!(
            TargetEvent::from_cdp(&destroyed),
            Some(TargetEvent::Destroyed("LOGIN".to_string()))
        );
        assert_eq!(
            TargetEvent::from_cdp(&json!({ "id": 1, "result": {} })),
            None
        );
    }
}
