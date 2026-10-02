//! What a remote control session may say to Chromium.
//!
//! A link carries the page: its picture, clicks and keystrokes into it, its
//! history, and navigation to an address an HTML source is allowed to render
//! (see [`normalize_url`](crate::blocks::builtin::html_input::normalize_url)). Everything outside that — `Runtime.evaluate`,
//! `Storage.getCookies`, the whole `Network` and `Debugger` domains, and any
//! `file:` or `chrome:` page — is what turns a link into control of this host,
//! so the proxy refuses it rather than trusting the page not to ask. The page
//! we serve is only the first user of the link; this filter is what makes the
//! link safe to hand to a second one.

use crate::blocks::builtin::html_input::checked_destination;
use serde_json::{json, Value};

/// The methods a remote control session may send, as Chromium names them.
///
/// `Page.navigate` goes only to an address [`normalize_url`](crate::blocks::builtin::html_input::normalize_url) accepts, so never
/// to the filesystem or Chromium's own pages. It still reaches whatever this
/// server reaches over http, and a `data:` page runs script of the client's
/// choosing, which is why the link's warning says so.
pub const REMOTE_CONTROL_METHODS: &[&str] = &[
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.enable",
    "Page.getNavigationHistory",
    "Page.navigate",
    "Page.navigateToHistoryEntry",
    "Page.reload",
    "Page.screencastFrameAck",
    "Page.startScreencast",
    "Page.stopScreencast",
];

/// Not a Chromium method: the client asks for the block's own page by name,
/// and the proxy supplies the address. The client never gets to choose one.
pub const GO_HOME: &str = "Strom.goHome";

/// Not a Chromium method: show another of the session's windows - its page, or
/// a popup it opened. The proxy checks the target is one of those.
pub const SWITCH_PAGE: &str = "Strom.switchPage";

/// Not a Chromium method: close one of the session's popups. The page the link
/// was minted for cannot be closed this way.
pub const CLOSE_PAGE: &str = "Strom.closePage";

/// Not a Chromium method: load one of the session's popups' address into the
/// session's own page, and close the popup. A login that finishes in a popup
/// leaves the popup logged in and the page on air where it was; the profile is
/// shared, so taking the popup's address is all the page needs.
pub const ADOPT_PAGE: &str = "Strom.adoptPage";

/// Not a Chromium method: make an address the block's own URL, so the page the
/// operator has reached becomes where the source starts. The proxy performs it
/// against Strom, not Chromium.
pub const SET_HOME: &str = "Strom.setHome";

/// What to send to Chromium in place of a message the client sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Forward {
    /// A message rebuilt from the client's, carrying only what the filter
    /// allows. Nothing the client sent goes to Chromium as it arrived.
    Rewritten {
        text: String,
        /// Whether the operator did something - clicked, typed, navigated -
        /// as opposed to the page keeping its screencast going. Only this
        /// keeps a link alive.
        by_operator: bool,
    },
    /// Not for Chromium: set the block's URL to this checked address and
    /// answer the command with this id.
    SetHome { id: Option<i64>, url: String },
    /// Not for Chromium as it stands: go back to the session's own page, and
    /// navigate it to the block's URL.
    GoHome { id: Option<i64> },
    /// Not for Chromium: show this target instead, if it is the session's.
    SwitchPage { id: Option<i64>, target: String },
    /// Not for Chromium: close this target, if it is one of the session's
    /// popups.
    ClosePage { id: Option<i64>, target: String },
    /// Not for Chromium as it stands: load this popup's address into the
    /// session's own page, and close the popup. The address is the proxy's
    /// to look up, never the client's to supply.
    AdoptPage { id: Option<i64>, target: String },
}

/// A navigation of the session's own page to an address the proxy has
/// already checked.
pub fn navigate_message(id: Option<i64>, url: &str) -> String {
    json!({ "id": id, "method": "Page.navigate", "params": { "url": url } }).to_string()
}

/// The navigation [`GO_HOME`] stands for, once the session is back on its own
/// page.
pub fn go_home_message(id: Option<i64>, home_url: &str) -> String {
    navigate_message(id, home_url)
}

/// The protocol's own shape for "no", so the client sees a refusal against the
/// command it sent rather than a socket that silently swallows things.
pub fn refusal(id: Option<i64>, message: &str) -> String {
    json!({
        "id": id,
        "error": { "code": -32601, "message": message }
    })
    .to_string()
}

/// Whether a method is the operator doing something, rather than the page's
/// own upkeep of the screencast and history.
fn by_operator(method: &str) -> bool {
    method.starts_with("Input.")
        || matches!(
            method,
            "Page.navigate" | "Page.navigateToHistoryEntry" | "Page.reload"
        )
}

/// Whether one message from the client may be forwarded, and in what form, or
/// the refusal to send back in its place.
///
/// `strict` is the source's Strict Network Access: navigation and a new start
/// page are then refused an address on the server's own network, the same as
/// the block's URL is.
pub fn allows(raw: &str, strict: bool) -> Result<Forward, String> {
    let Ok(message) = serde_json::from_str::<Value>(raw) else {
        return Err(refusal(None, "Not a DevTools protocol message"));
    };
    let id = message.get("id").and_then(Value::as_i64);
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Err(refusal(
            id,
            "A remote control session sends commands, nothing else",
        ));
    };
    let params = message.get("params");

    match method {
        GO_HOME => Ok(Forward::GoHome { id }),
        SWITCH_PAGE | CLOSE_PAGE | ADOPT_PAGE => {
            let Some(target) = params
                .and_then(|p| p.get("targetId"))
                .and_then(Value::as_str)
            else {
                return Err(refusal(id, &format!("{} needs a targetId", method)));
            };
            let target = target.to_string();
            Ok(match method {
                SWITCH_PAGE => Forward::SwitchPage { id, target },
                CLOSE_PAGE => Forward::ClosePage { id, target },
                _ => Forward::AdoptPage { id, target },
            })
        }
        // Page.reload also takes scriptToEvaluateOnLoad, which is arbitrary
        // JavaScript. Only the cache flag goes through.
        "Page.reload" => {
            let ignore_cache = params
                .and_then(|p| p.get("ignoreCache"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(Forward::Rewritten {
                text:
                    json!({ "id": id, "method": method, "params": { "ignoreCache": ignore_cache } })
                        .to_string(),
                by_operator: true,
            })
        }
        "Page.navigate" | SET_HOME => {
            let Some(raw) = params.and_then(|p| p.get("url")).and_then(Value::as_str) else {
                return Err(refusal(id, &format!("{} needs a url", method)));
            };
            let url = checked_destination(raw, strict).map_err(|reason| refusal(id, &reason))?;
            if method == SET_HOME {
                return Ok(Forward::SetHome { id, url });
            }
            // Only the address goes through: frameId would aim at a subframe,
            // and nothing else the method takes is needed.
            Ok(Forward::Rewritten {
                text: json!({ "id": id, "method": method, "params": { "url": url } }).to_string(),
                by_operator: true,
            })
        }
        "Page.navigateToHistoryEntry" => {
            let Some(entry_id) = params
                .and_then(|p| p.get("entryId"))
                .and_then(Value::as_i64)
            else {
                return Err(refusal(id, "Page.navigateToHistoryEntry needs an entryId"));
            };
            Ok(Forward::Rewritten {
                text: json!({ "id": id, "method": method, "params": { "entryId": entry_id } })
                    .to_string(),
                by_operator: true,
            })
        }
        // Rebuilt from what was parsed, so only the command reaches Chromium:
        // a top-level `sessionId`, say, would aim it at another session on
        // the same socket.
        _ if REMOTE_CONTROL_METHODS.contains(&method) => {
            let mut rebuilt = json!({ "id": id, "method": method });
            if let Some(params) = params {
                rebuilt["params"] = params.clone();
            }
            Ok(Forward::Rewritten {
                text: rebuilt.to_string(),
                by_operator: by_operator(method),
            })
        }
        _ => Err(refusal(
            id,
            &format!(
                "{} is not available over a remote control link, which carries the page's \
                 picture, clicks, keystrokes and navigation only",
                method
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "https://home.example/start";

    fn command(method: &str) -> String {
        json!({ "id": 7, "method": method, "params": {} }).to_string()
    }

    fn rewritten(raw: &str) -> Value {
        match allows(raw, true).expect("allowed") {
            Forward::Rewritten { text, .. } => serde_json::from_str(&text).expect("valid JSON"),
            other => panic!("expected a rewrite of {}, got {:?}", raw, other),
        }
    }

    #[test]
    fn a_link_carries_the_picture_the_input_and_the_history() {
        for method in REMOTE_CONTROL_METHODS {
            let raw = json!({
                "id": 7,
                "method": method,
                "params": { "entryId": 3, "url": "https://example.com" }
            })
            .to_string();
            assert!(
                allows(&raw, true).is_ok(),
                "{} is what the page needs to work",
                method
            );
        }
    }

    #[test]
    fn a_link_does_not_carry_the_rest_of_the_protocol() {
        // Each of these is on its own enough to turn a link into control of
        // the host: script execution, the cookie jar, the network, and the
        // debugger.
        for method in [
            "Runtime.evaluate",
            "Runtime.callFunctionOn",
            "Page.addScriptToEvaluateOnNewDocument",
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
            let refused = allows(&command(method), true)
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
            "Strom.goHomeTo",
        ] {
            assert!(
                allows(&command(method), true).is_err(),
                "{} is not on the list",
                method
            );
        }
    }

    #[test]
    fn going_home_takes_no_address_from_the_client() {
        let raw = json!({ "id": 9, "method": GO_HOME, "params": { "url": "file:///etc/passwd" } })
            .to_string();
        assert_eq!(allows(&raw, true), Ok(Forward::GoHome { id: Some(9) }));
        // The address is the proxy's, whatever the client put in.
        let sent: Value = serde_json::from_str(&go_home_message(Some(9), HOME)).unwrap();
        assert_eq!(sent["method"], "Page.navigate");
        assert_eq!(sent["params"], json!({ "url": HOME }));
    }

    #[test]
    fn switching_and_closing_name_a_target_for_the_proxy_to_check() {
        let switch = json!({ "id": 3, "method": SWITCH_PAGE, "params": { "targetId": "ABCD" } });
        assert_eq!(
            allows(&switch.to_string(), true),
            Ok(Forward::SwitchPage {
                id: Some(3),
                target: "ABCD".to_string()
            })
        );
        let close = json!({ "id": 4, "method": CLOSE_PAGE, "params": { "targetId": "ABCD" } });
        assert_eq!(
            allows(&close.to_string(), true),
            Ok(Forward::ClosePage {
                id: Some(4),
                target: "ABCD".to_string()
            })
        );
        let bare = json!({ "id": 5, "method": SWITCH_PAGE, "params": {} });
        assert!(allows(&bare.to_string(), true).is_err());
    }

    #[test]
    fn adopting_a_popup_names_the_popup_and_never_an_address() {
        let adopt = json!({
            "id": 6,
            "method": ADOPT_PAGE,
            "params": { "targetId": "ABCD", "url": "http://127.0.0.1:9222/json/list" }
        });
        // The address is looked up by the proxy; whatever the client puts
        // next to the target is not carried.
        assert_eq!(
            allows(&adopt.to_string(), true),
            Ok(Forward::AdoptPage {
                id: Some(6),
                target: "ABCD".to_string()
            })
        );
        let bare = json!({ "id": 7, "method": ADOPT_PAGE, "params": {} });
        assert!(allows(&bare.to_string(), true).is_err());
    }

    #[test]
    fn navigation_goes_only_where_an_html_source_may_render() {
        for (raw, sent) in [
            ("https://example.com/a", "https://example.com/a"),
            ("example.com", "https://example.com"),
            ("data:text/html,hi", "data:text/html,hi"),
        ] {
            let message = json!({
                "id": 3,
                "method": "Page.navigate",
                "params": { "url": raw, "frameId": "child", "referrer": "https://x.example" }
            });
            let forwarded = rewritten(&message.to_string());
            assert_eq!(forwarded["params"], json!({ "url": sent }));
        }
        for raw in [
            "file:///etc/passwd",
            "view-source:file:///etc/passwd",
            "chrome://settings",
            "devtools://devtools/bundled/inspector.html",
            "javascript:alert(1)",
        ] {
            let message = json!({ "id": 3, "method": "Page.navigate", "params": { "url": raw } });
            let refused = allows(&message.to_string(), true)
                .expect_err(&format!("{} must not be reached", raw));
            let parsed: Value = serde_json::from_str(&refused).expect("valid JSON");
            assert_eq!(parsed["id"], 3);
        }
    }

    #[test]
    fn setting_home_is_checked_and_handed_to_strom() {
        let message = json!({ "id": 8, "method": SET_HOME, "params": { "url": "example.com/x" } });
        assert_eq!(
            allows(&message.to_string(), true),
            Ok(Forward::SetHome {
                id: Some(8),
                url: "https://example.com/x".to_string()
            })
        );
        let message = json!({ "id": 8, "method": SET_HOME, "params": { "url": "file:///etc" } });
        assert!(allows(&message.to_string(), true).is_err());
    }

    #[test]
    fn a_strict_source_is_not_navigated_onto_the_server() {
        for method in ["Page.navigate", SET_HOME] {
            let message = json!({
                "id": 3,
                "method": method,
                "params": { "url": "http://127.0.0.1:9222/json/list" }
            })
            .to_string();
            let refused = allows(&message, true).expect_err("loopback must be refused");
            let parsed: Value = serde_json::from_str(&refused).expect("valid JSON");
            assert_eq!(parsed["id"], 3);
            assert!(
                allows(&message, false).is_ok(),
                "{} is allowed loose",
                method
            );
        }
    }

    #[test]
    fn a_reload_cannot_carry_a_script() {
        let raw = json!({
            "id": 4,
            "method": "Page.reload",
            "params": { "ignoreCache": true, "scriptToEvaluateOnLoad": "fetch('/secret')" }
        })
        .to_string();
        let sent = rewritten(&raw);
        assert_eq!(sent["params"], json!({ "ignoreCache": true }));
    }

    #[test]
    fn a_history_step_carries_only_its_entry() {
        let raw = json!({
            "id": 5,
            "method": "Page.navigateToHistoryEntry",
            "params": { "entryId": 12, "url": "file:///etc/passwd" }
        })
        .to_string();
        let sent = rewritten(&raw);
        assert_eq!(sent["params"], json!({ "entryId": 12 }));

        let without = json!({ "id": 5, "method": "Page.navigateToHistoryEntry", "params": {} });
        assert!(allows(&without.to_string(), true).is_err());
    }

    #[test]
    fn only_the_parsed_command_is_forwarded() {
        // Whatever else rides along on an allowed method stays behind: a
        // top-level sessionId would route the command to another session.
        let raw = json!({
            "id": 6,
            "method": "Input.dispatchMouseEvent",
            "sessionId": "OTHER",
            "params": { "type": "mousePressed", "x": 1, "y": 2 }
        })
        .to_string();
        let sent = rewritten(&raw);
        assert_eq!(
            sent,
            json!({
                "id": 6,
                "method": "Input.dispatchMouseEvent",
                "params": { "type": "mousePressed", "x": 1, "y": 2 }
            })
        );
    }

    #[test]
    fn only_the_operator_keeps_a_link_alive() {
        let operator = |method: &str| {
            match allows(
            &json!({ "id": 1, "method": method, "params": { "url": "https://example.com", "entryId": 1 } })
                .to_string(),
            true,
        ) {
            Ok(Forward::Rewritten { by_operator, .. }) => by_operator,
            other => panic!("expected {} to be forwarded, got {:?}", method, other),
        }
        };
        for method in [
            "Input.dispatchMouseEvent",
            "Input.dispatchKeyEvent",
            "Input.insertText",
            "Page.navigate",
            "Page.navigateToHistoryEntry",
            "Page.reload",
        ] {
            assert!(operator(method), "{} is the operator at work", method);
        }
        // A tab left open on an animated page acks frames forever; that is
        // not someone using the link.
        for method in [
            "Page.enable",
            "Page.startScreencast",
            "Page.stopScreencast",
            "Page.screencastFrameAck",
            "Page.getNavigationHistory",
        ] {
            assert!(!operator(method), "{} is upkeep, not use", method);
        }
    }

    #[test]
    fn a_refusal_answers_the_command_that_was_sent() {
        let refused = allows(&command("Runtime.evaluate"), true).unwrap_err();
        let parsed: Value = serde_json::from_str(&refused).expect("valid JSON");
        // A client matches answers to commands by id; an answer without one is
        // an answer it will wait for forever.
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], -32601);
    }

    #[test]
    fn anything_that_is_not_a_command_is_refused() {
        assert!(allows("not json at all", true).is_err());
        assert!(allows("{}", true).is_err());
        assert!(allows(r#"{"id":1}"#, true).is_err());
        // A response, not a command - the client has no business sending one.
        assert!(allows(r#"{"id":1,"result":{}}"#, true).is_err());
    }
}
