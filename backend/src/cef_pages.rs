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
//! start is Chromium's initial empty document, which the first navigation
//! replaces, so it leaves no entry in the page's history.
//!
//! This works with any gstcefsrc: it needs nothing from the element but its
//! `url` property.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::FlowId;
use tracing::{debug, warn};

/// How often Chromium's page list is read while a page is being born.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long to wait for a page to appear before giving up on naming it. The
/// first `cefsrc` in a process initializes CEF, which takes seconds; after
/// that, a page appears within milliseconds of its element starting.
const BIRTH_TIMEOUT: Duration = Duration::from_secs(60);

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
    cefsrc.set_property(URL_PROPERTY, &marker);

    // The task must not keep the element alive: a flow stopped before its
    // page was born would otherwise never be freed.
    let element = cefsrc.downgrade();
    handle.spawn(async move {
        let started = Instant::now();
        let target_id = loop {
            if element.upgrade().is_none() {
                return;
            }
            if let Some(id) = find_page(port, &marker).await {
                break Some(id);
            }
            if started.elapsed() > BIRTH_TIMEOUT {
                break None;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };
        match &target_id {
            Some(id) => {
                debug!("{:?} renders page {}", owner, id);
                record(owner.clone(), id.clone());
            }
            None => warn!(
                "{:?}: its page did not appear within {:?}, so remote control cannot find it",
                owner, BIRTH_TIMEOUT
            ),
        }
        let Some(element) = element.upgrade() else {
            return;
        };
        // A URL set on air while the page was being born has already been
        // loaded, and wins over the one the element was built with.
        let current: Option<String> = element.property(URL_PROPERTY);
        if current.as_deref() == Some(marker.as_str()) {
            element.set_property(URL_PROPERTY, &real_url);
        }
    });
}

/// The target id of the page Chromium lists on `url`, if exactly one is.
async fn find_page(port: u16, url: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Listed {
        id: String,
        #[serde(rename = "type")]
        kind: String,
        url: String,
    }
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap_or_default()
    });
    let listed: Vec<Listed> = client
        .get(format!("http://127.0.0.1:{}/json/list", port))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let mut matching = listed
        .into_iter()
        .filter(|t| t.kind == "page" && t.url == url);
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
    fn a_target_belongs_to_one_source_only() {
        let flow = FlowId::new_v4();
        record(block(flow, "a"), "SAME".to_string());
        record(block(flow, "b"), "SAME".to_string());
        assert_eq!(owner_of("SAME"), Some(block(flow, "b")));
        assert_eq!(target_of(&block(flow, "a")), None);
    }
}
