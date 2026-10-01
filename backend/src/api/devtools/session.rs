//! One remote control session: the socket to the client, the socket to the
//! page it is showing, and the watch that lets it follow the page's popups.
//!
//! The protocol is text in both directions, and the screencast rides the same
//! socket as everything else: Chromium sends a JPEG per changed frame and
//! waits for the client to acknowledge it before sending the next. That
//! acknowledgement is the flow control, so a slow link costs frame rate rather
//! than an unbounded queue - which is what makes this usable over the
//! internet. Nothing here needs to know that; it just must not buffer.

use super::pages::{BrowserWatch, PageFamily, TargetEvent};
use super::{filter, LinkSource, Session};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

type Upstream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How often a session reports to its link whether the operator did anything.
/// Well inside the link's lifetime, and rare enough that input does not take
/// the table's lock per keystroke.
const HOLD_INTERVAL: Duration = Duration::from_secs(15);
type ClientTx = SplitSink<WebSocket, Message>;

async fn open_page(port: u16, target: &str) -> Option<Upstream> {
    let url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target);
    match tokio_tungstenite::connect_async(&url).await {
        Ok((socket, _)) => Some(socket),
        Err(e) => {
            warn!("Could not reach DevTools for target {}: {}", target, e);
            None
        }
    }
}

async fn tell(client: &mut ClientTx, text: String) -> bool {
    client.send(Message::Text(text.into())).await.is_ok()
}

fn answer(id: Option<i64>) -> String {
    serde_json::json!({ "id": id, "result": {} }).to_string()
}

/// The next change to the browser's targets, or never when nothing is watched.
async fn next_event(
    events: &mut Option<tokio::sync::mpsc::UnboundedReceiver<TargetEvent>>,
) -> Option<TargetEvent> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Make an address the block's own URL, and answer the command that asked.
///
/// This writes the block's configuration from a link, which is what the
/// operator asked for: the page they have logged in to or clicked through to
/// becomes where the source starts, and stays so across a restart. The address
/// has already been through the same check as any other URL for this block.
pub(super) async fn set_home(
    app: &AppState,
    source: &mut LinkSource,
    id: Option<i64>,
    url: String,
) -> String {
    let properties = HashMap::from([(
        crate::blocks::builtin::html_input::URL_PROPERTY.to_string(),
        strom_types::PropertyValue::String(url.clone()),
    )]);
    let result = app
        .update_block_properties(&source.flow_id, &source.block_id, properties, None, None)
        .await;
    let refused = match result {
        Ok((_, rejected)) => rejected.into_values().next(),
        Err(e) => Some(e.to_string()),
    };
    if let Some(reason) = refused {
        warn!(
            "Remote control could not set the start page of block {}: {}",
            source.block_id, reason
        );
        return filter::refusal(id, &reason);
    }

    info!(
        "Remote control set the start page of block {} in flow {}",
        source.block_id, source.flow_id
    );
    app.events()
        .broadcast(strom_types::StromEvent::FlowUpdated {
            flow_id: source.flow_id,
        });
    source.home_url = url.clone();
    serde_json::json!({ "id": id, "result": { "url": url } }).to_string()
}

/// Shuttle messages both ways until either side hangs up, or the link dies.
///
/// In filtered mode the session also follows the page's popups. A login that
/// opens a window gets it shown straight away, since that is what the
/// operator just clicked for, and when it closes the session goes back to the
/// window that opened it. Only the session's own page and windows opened from
/// it can be shown or closed; [`PageFamily`] decides which those are.
pub(super) async fn pump(
    client: WebSocket,
    port: u16,
    session: Session,
    full_devtools: bool,
    app: AppState,
) {
    let Session {
        target_id: root,
        mut source,
        mut cancelled,
        hold,
    } = session;
    let Some(mut upstream) = open_page(port, &root).await else {
        return;
    };
    debug!("DevTools session open for target {}", root);

    let (mut client_tx, mut client_rx) = client.split();

    // The DevTools application follows targets itself; our page needs help.
    let (mut events, closer) = match full_devtools {
        true => (None, None),
        false => match BrowserWatch::start(port).await {
            Some(watch) => {
                let (events, closer) = watch.split();
                (Some(events), Some(closer))
            }
            None => (None, None),
        },
    };
    let mut family = PageFamily::new(root.clone());
    let mut current = root.clone();
    let mut hold_tick = tokio::time::interval(HOLD_INTERVAL);
    let mut used = false;

    // Our page is the client in filtered mode, and its header names the
    // source. The DevTools application would have no use for this.
    if !full_devtools && !tell(&mut client_tx, source.context_message()).await {
        return;
    }

    loop {
        // Where to take the session next, decided by whichever branch ran.
        let mut go_to: Option<String> = None;

        tokio::select! {
            incoming = client_rx.next() => {
                let Some(Ok(message)) = incoming else { break };
                let text = match message {
                    Message::Text(t) => t,
                    // The protocol is text. A filtered session has no reason
                    // to send anything else, and a binary frame cannot be
                    // checked against the list, so it does not go.
                    Message::Binary(b) => {
                        if full_devtools && upstream.send(WsMessage::Binary(b)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    Message::Close(_) => break,
                    // Chromium answers our pings; the client's are ours to
                    // answer, and axum has already done it.
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                if full_devtools {
                    // Nothing is parsed in this mode, so anything is use.
                    used = true;
                    if upstream.send(WsMessage::Text(text.as_str().into())).await.is_err() {
                        break;
                    }
                    continue;
                }
                let forward = filter::allows(text.as_str(), source.strict);
                // Every command of Strom's own is the operator asking for it.
                used |= matches!(
                    forward,
                    Ok(filter::Forward::Rewritten { by_operator: true, .. })
                        | Ok(filter::Forward::SetHome { .. })
                        | Ok(filter::Forward::GoHome { .. })
                        | Ok(filter::Forward::SwitchPage { .. })
                        | Ok(filter::Forward::ClosePage { .. })
                        | Ok(filter::Forward::AdoptPage { .. })
                );
                let reply = match forward {
                    Ok(filter::Forward::Rewritten { text, .. }) => {
                        if upstream.send(WsMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                        None
                    }
                    Ok(filter::Forward::SetHome { id, url }) => {
                        Some(set_home(&app, &mut source, id, url).await)
                    }
                    Ok(filter::Forward::GoHome { id }) => {
                        if current != root {
                            match open_page(port, &root).await {
                                Some(page) => {
                                    upstream = page;
                                    current = root.clone();
                                    if !tell(&mut client_tx, switched(&current)).await {
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        let navigate = filter::go_home_message(id, &source.home_url);
                        if upstream.send(WsMessage::Text(navigate.into())).await.is_err() {
                            break;
                        }
                        None
                    }
                    Ok(filter::Forward::SwitchPage { id, target }) => {
                        if family.contains(&target) {
                            go_to = Some(target);
                            Some(answer(id))
                        } else {
                            Some(filter::refusal(id, "That is not one of this session's windows"))
                        }
                    }
                    Ok(filter::Forward::ClosePage { id, target }) => {
                        match &closer {
                            Some(closer) if target != root && family.contains(&target) => {
                                closer.close(&target);
                                Some(answer(id))
                            }
                            _ => Some(filter::refusal(
                                id,
                                "Only a popup this session's page opened can be closed",
                            )),
                        }
                    }
                    Ok(filter::Forward::AdoptPage { id, target }) => {
                        // The address is Chromium's report of the popup, and
                        // goes through the same check as any other the page
                        // may be sent to.
                        let checked = match (&closer, family.popup_url(&target)) {
                            (Some(_), Some(url)) => {
                                crate::blocks::builtin::html_input::checked_destination(
                                    url,
                                    source.strict,
                                )
                            }
                            _ => Err("Only a popup this session's page opened can be shown in \
                                      it"
                            .to_string()),
                        };
                        match checked {
                            Err(reason) => Some(filter::refusal(id, &reason)),
                            Ok(url) => {
                                if current != root {
                                    match open_page(port, &root).await {
                                        Some(page) => {
                                            upstream = page;
                                            current = root.clone();
                                            if !tell(&mut client_tx, switched(&current)).await {
                                                break;
                                            }
                                        }
                                        None => break,
                                    }
                                }
                                info!(
                                    "Remote control loaded a popup's page into block {} in flow {}",
                                    source.block_id, source.flow_id
                                );
                                let navigate = filter::navigate_message(id, &url);
                                if upstream.send(WsMessage::Text(navigate.into())).await.is_err() {
                                    break;
                                }
                                if let Some(closer) = &closer {
                                    closer.close(&target);
                                }
                                None
                            }
                        }
                    }
                    Err(refusal) => {
                        debug!("Refused a method a remote control link does not carry");
                        Some(refusal)
                    }
                };
                if let Some(reply) = reply {
                    if !tell(&mut client_tx, reply).await {
                        break;
                    }
                }
            }

            incoming = upstream.next() => {
                match incoming {
                    Some(Ok(WsMessage::Text(t))) => {
                        if client_tx.send(Message::Text(t.as_str().into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Binary(b))) => {
                        if client_tx.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    // The window on show went away. A popup closing is
                    // normal - an OAuth window closes itself when it is done.
                    // The session's own page going away ends the session.
                    _ => {
                        if current == root {
                            break;
                        }
                        go_to = Some(family.fallback_for(&current));
                        family.apply(TargetEvent::Destroyed(current.clone()));
                    }
                }
            }

            event = next_event(&mut events) => {
                let Some(event) = event else {
                    events = None;
                    continue;
                };
                let (id, opened) = match &event {
                    TargetEvent::Updated(info) => (info.id.clone(), !family.contains(&info.id)),
                    TargetEvent::Destroyed(id) => (id.clone(), false),
                };
                let back = family.fallback_for(&id);
                let closed = matches!(event, TargetEvent::Destroyed(_));
                family.apply(event);
                if opened && id != root && family.contains(&id) {
                    // The operator just clicked for this window.
                    go_to = Some(id);
                } else if closed && id == current && current != root {
                    go_to = Some(back);
                }
                if !tell(&mut client_tx, family.message(&current)).await {
                    break;
                }
            }

            _ = hold_tick.tick() => {
                // A backstop for a change that reached the flow without an
                // event, which is what normally revokes the link.
                if !super::still_allowed(&app, &source).await {
                    info!(
                        "Remote control is no longer allowed for block {}; closing the session",
                        source.block_id
                    );
                    break;
                }
                if std::mem::take(&mut used) {
                    hold.used();
                } else {
                    hold.idle();
                }
            }

            // Revoked or expired. A closed channel means the link is gone
            // too: the sender lives in the table entry, so dropping the entry
            // is itself the signal.
            _ = cancelled.recv() => {
                info!(
                    "Remote control link revoked or expired; closing the session on target {}",
                    root
                );
                break;
            }
        }

        if let Some(target) = go_to.filter(|t| *t != current) {
            let Some(page) = open_page(port, &target).await else {
                // Gone before we got there; stay where we are, unless that
                // is gone too.
                if current == root {
                    continue;
                }
                break;
            };
            upstream = page;
            current = target;
            debug!("Remote control session on {} now shows {}", root, current);
            if !tell(&mut client_tx, switched(&current)).await
                || !tell(&mut client_tx, family.message(&current)).await
            {
                break;
            }
        }
    }

    debug!("DevTools session closed for target {}", root);
}

/// Tells the page it is now looking at another window, so it starts the
/// screencast and history over on it.
fn switched(target: &str) -> String {
    serde_json::json!({ "method": "Strom.switched", "params": { "id": target } }).to_string()
}
