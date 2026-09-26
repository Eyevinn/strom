//! What a remote control session may say to Chromium.
//!
//! A link carries the page: its picture, clicks and keystrokes into it, and a
//! way back through what it has already shown. Everything outside that —
//! `Runtime.evaluate`, `Page.navigate`, `Storage.getCookies`, the whole
//! `Network` and `Debugger` domains — is what turns a link into control of
//! this host, so the proxy refuses it rather than trusting the page not to
//! ask. The page we serve is only the first user of the link; this filter is
//! what makes the link safe to hand to a second one.

use serde_json::{json, Value};

/// The methods a remote control session may send, as Chromium names them.
///
/// Navigation is limited to pages the browser has already been on, plus the
/// one the block was pointed at (see [`GO_HOME`]). An address the client
/// chooses is not on this list: from inside the network, `http` alone reaches
/// admin interfaces and metadata services that the link's holder could then
/// read off the screencast.
pub const REMOTE_CONTROL_METHODS: &[&str] = &[
    "Input.dispatchKeyEvent",
    "Input.dispatchMouseEvent",
    "Input.insertText",
    "Page.enable",
    "Page.getNavigationHistory",
    "Page.navigateToHistoryEntry",
    "Page.reload",
    "Page.screencastFrameAck",
    "Page.startScreencast",
    "Page.stopScreencast",
];

/// Not a Chromium method: the client asks for the block's own page by name,
/// and the proxy supplies the address. The client never gets to choose one.
pub const GO_HOME: &str = "Strom.goHome";

/// What to send to Chromium in place of a message the client sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Forward {
    /// The message as it arrived.
    AsIs,
    /// A message rebuilt from the client's, carrying only what the filter
    /// allows.
    Rewritten(String),
}

/// The protocol's own shape for "no", so the client sees a refusal against the
/// command it sent rather than a socket that silently swallows things.
fn refusal(id: Option<i64>, message: &str) -> String {
    json!({
        "id": id,
        "error": { "code": -32601, "message": message }
    })
    .to_string()
}

/// Whether one message from the client may be forwarded, and in what form, or
/// the refusal to send back in its place.
///
/// `home_url` is the page the link was minted for, which [`GO_HOME`] navigates
/// to.
pub fn allows(raw: &str, home_url: &str) -> Result<Forward, String> {
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
        GO_HOME => Ok(Forward::Rewritten(
            json!({ "id": id, "method": "Page.navigate", "params": { "url": home_url } })
                .to_string(),
        )),
        // Page.reload also takes scriptToEvaluateOnLoad, which is arbitrary
        // JavaScript. Only the cache flag goes through.
        "Page.reload" => {
            let ignore_cache = params
                .and_then(|p| p.get("ignoreCache"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(Forward::Rewritten(
                json!({ "id": id, "method": method, "params": { "ignoreCache": ignore_cache } })
                    .to_string(),
            ))
        }
        "Page.navigateToHistoryEntry" => {
            let Some(entry_id) = params
                .and_then(|p| p.get("entryId"))
                .and_then(Value::as_i64)
            else {
                return Err(refusal(id, "Page.navigateToHistoryEntry needs an entryId"));
            };
            Ok(Forward::Rewritten(
                json!({ "id": id, "method": method, "params": { "entryId": entry_id } })
                    .to_string(),
            ))
        }
        _ if REMOTE_CONTROL_METHODS.contains(&method) => Ok(Forward::AsIs),
        _ => Err(refusal(
            id,
            &format!(
                "{} is not available over a remote control link, which carries the page's \
                 picture, clicks, keystrokes and history only",
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
        match allows(raw, HOME).expect("allowed") {
            Forward::Rewritten(text) => serde_json::from_str(&text).expect("valid JSON"),
            Forward::AsIs => panic!("expected a rewrite of {}", raw),
        }
    }

    #[test]
    fn a_link_carries_the_picture_the_input_and_the_history() {
        for method in REMOTE_CONTROL_METHODS {
            let raw = json!({ "id": 7, "method": method, "params": { "entryId": 3 } }).to_string();
            assert!(
                allows(&raw, HOME).is_ok(),
                "{} is what the page needs to work",
                method
            );
        }
    }

    #[test]
    fn a_link_does_not_carry_the_rest_of_the_protocol() {
        // Each of these is on its own enough to turn a link into control of
        // the host: script execution, navigation to an address of the
        // client's choosing, the cookie jar, the network, and the debugger.
        for method in [
            "Runtime.evaluate",
            "Runtime.callFunctionOn",
            "Page.navigate",
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
            let refused = allows(&command(method), HOME)
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
                allows(&command(method), HOME).is_err(),
                "{} is not on the list",
                method
            );
        }
    }

    #[test]
    fn going_home_navigates_to_the_page_the_link_was_minted_for() {
        let raw = json!({ "id": 9, "method": GO_HOME, "params": { "url": "file:///etc/passwd" } })
            .to_string();
        let sent = rewritten(&raw);
        assert_eq!(sent["id"], 9);
        assert_eq!(sent["method"], "Page.navigate");
        // Whatever the client put in, the address is ours.
        assert_eq!(sent["params"], json!({ "url": HOME }));
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
        assert!(allows(&without.to_string(), HOME).is_err());
    }

    #[test]
    fn a_refusal_answers_the_command_that_was_sent() {
        let refused = allows(&command("Runtime.evaluate"), HOME).unwrap_err();
        let parsed: Value = serde_json::from_str(&refused).expect("valid JSON");
        // A client matches answers to commands by id; an answer without one is
        // an answer it will wait for forever.
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], -32601);
    }

    #[test]
    fn anything_that_is_not_a_command_is_refused() {
        assert!(allows("not json at all", HOME).is_err());
        assert!(allows("{}", HOME).is_err());
        assert!(allows(r#"{"id":1}"#, HOME).is_err());
        // A response, not a command - the client has no business sending one.
        assert!(allows(r#"{"id":1,"result":{}}"#, HOME).is_err());
    }
}
