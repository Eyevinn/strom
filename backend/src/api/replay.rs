//! Replay control: commands and state per replay group, over REST and a
//! WebSocket, plus the operator page.
//!
//! Both transports take the same commands ([`ReplayCommand`], JSON tagged by
//! `cmd`) and return the same result ([`ReplayCommandResult`]). A malformed
//! command or a value out of range is refused as invalid (HTTP 400), a
//! command that does not apply to the group now as a conflict (HTTP 409).
//! On the WebSocket a command may carry a `seq`, echoed in its `ack` or
//! `error`; the socket also streams the group state every 50 ms (the same
//! object as `GET .../state`) and sends what the group does by itself as
//! `{"type": "event", ...}` ([`strom_types::replay::ReplayEvent`]).
//!
//! A socket does not keep its group alive: it holds the group weakly and
//! closes once the group's players are gone (the flow stopped or was deleted)
//! or a new run replaced the group. A client reconnects to follow the next run.

use crate::blocks::builtin::mediaplayer::replay::timeline::{self, CommandError};
use crate::json_rejection::JsonBody;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use std::sync::{Arc, Weak};
use std::time::Duration;
use strom_types::api::ErrorResponse;
use strom_types::replay::{ReplayCommand, ReplayCommandResult};
use strom_types::FlowId;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn not_found() -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("replay group not found")),
    )
}

fn command_error(e: CommandError) -> ApiError {
    let status = match e {
        CommandError::Invalid(_) => StatusCode::BAD_REQUEST,
        CommandError::Conflict(_) => StatusCode::CONFLICT,
    };
    let title = match e {
        CommandError::Invalid(_) => "Invalid command",
        CommandError::Conflict(_) => "Command does not apply now",
    };
    (
        status,
        Json(ErrorResponse::with_details(title, e.message())),
    )
}

fn group(flow_id: FlowId, group: &str) -> Result<Arc<timeline::ReplayTimeline>, ApiError> {
    timeline::get(flow_id, group).ok_or_else(not_found)
}

/// Send a command to a replay group.
#[utoipa::path(
    post,
    path = "/api/replay/{flow_id}/{group}/command",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    request_body = ReplayCommand,
    responses(
        (status = 200, description = "Command applied", body = ReplayCommandResult),
        (status = 400, description = "Malformed command or value out of range", body = ErrorResponse),
        (status = 404, description = "Replay group not found", body = ErrorResponse),
        (status = 409, description = "The command does not apply to the group now", body = ErrorResponse)
    )
)]
pub async fn replay_command(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
    JsonBody(body): JsonBody<serde_json::Value>,
) -> Result<Json<ReplayCommandResult>, ApiError> {
    let tl = group(flow_id, &group_name)?;
    let cmd = timeline::parse_command(body).map_err(command_error)?;
    let applied = tl.apply(cmd).map_err(command_error)?;
    Ok(Json(tl.result(&applied)))
}

/// The state of a replay group (the object the WebSocket streams).
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}/{group}/state",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    responses(
        (status = 200, description = "Group state", body = Object),
        (status = 404, description = "Replay group not found", body = ErrorResponse)
    )
)]
pub async fn replay_state(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(group(flow_id, &group_name)?.telemetry()))
}

const REPLAY_HTML: &str = include_str!("../../static/replay.html");

/// GET /player/replay/{flow_id}/{group}
pub async fn replay_page(Path((_flow_id, _group)): Path<(String, String)>) -> Html<&'static str> {
    Html(REPLAY_HTML)
}

/// WebSocket to control a replay group and follow its state and events.
///
/// Commands are JSON text frames, as for `POST .../command`, with an optional
/// `seq`. See docs/REPLAY_API.md for the messages.
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}/{group}/ws",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    responses(
        (status = 101, description = "WebSocket connection upgraded"),
        (status = 401, description = "Authentication required"),
        (status = 404, description = "Replay group not found", body = ErrorResponse)
    )
)]
pub async fn replay_ws(
    ws: WebSocketUpgrade,
    Path((flow_id, group)): Path<(FlowId, String)>,
) -> Response {
    let Some(tl) = timeline::get(flow_id, &group) else {
        return not_found().into_response();
    };
    let events = tl.subscribe();
    let held = Arc::downgrade(&tl);
    drop(tl);
    ws.on_upgrade(move |socket| handle(socket, flow_id, group, held, events))
}

/// The group the socket was opened on, while it is still the group's live
/// timeline. `None` once its players are gone, or a new run replaced it. The
/// socket holds only `held`, so it does not keep a stopped or deleted flow's
/// group alive (which kept answering commands and streaming stale state).
fn current(
    flow_id: FlowId,
    group: &str,
    held: &Weak<timeline::ReplayTimeline>,
) -> Option<Arc<timeline::ReplayTimeline>> {
    let tl = timeline::get(flow_id, group)?;
    std::ptr::eq(Arc::as_ptr(&tl), held.as_ptr()).then_some(tl)
}

/// A command from the socket: the reply to send.
fn ws_reply(tl: &timeline::ReplayTimeline, text: &str) -> serde_json::Value {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "type": "error", "seq": null, "code": "invalid",
                "error": format!("not JSON: {}", e),
            })
        }
    };
    let seq = value.get("seq").cloned().unwrap_or(serde_json::Value::Null);
    let applied = timeline::parse_command(value).and_then(|cmd| tl.apply(cmd));
    match applied {
        Ok(a) => {
            let mut ack = serde_json::to_value(tl.result(&a)).unwrap_or_default();
            ack["type"] = "ack".into();
            ack["seq"] = seq;
            ack
        }
        Err(e) => serde_json::json!({
            "type": "error", "seq": seq, "code": e.code(), "error": e.message(),
        }),
    }
}

/// The close frame sent when the socket's group is gone.
fn group_ended() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::AWAY,
        reason: "replay group ended".into(),
    }))
}

/// The reply to a command that came after the socket's group was gone.
fn gone_reply(text: &str) -> serde_json::Value {
    let seq = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("seq").cloned())
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "type": "error", "seq": seq, "code": "gone", "error": "replay group ended",
    })
}

async fn handle(
    socket: WebSocket,
    flow_id: FlowId,
    group: String,
    held: Weak<timeline::ReplayTimeline>,
    events: tokio::sync::broadcast::Receiver<strom_types::replay::ReplayEvent>,
) {
    let (tx, rx) = socket.split();
    run_socket(tx, rx, flow_id, group, held, events).await;
}

/// The socket's loop, on any sink and stream of messages (tests drive it
/// with channels). The group is held strongly only to build a message, never
/// across a send: a slow client must not keep a stopped flow's group alive.
async fn run_socket<T, R, E>(
    mut tx: T,
    mut rx: R,
    flow_id: FlowId,
    group: String,
    held: Weak<timeline::ReplayTimeline>,
    mut events: tokio::sync::broadcast::Receiver<strom_types::replay::ReplayEvent>,
) where
    T: futures::Sink<Message> + Unpin,
    R: futures::Stream<Item = Result<Message, E>> + Unpin,
{
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    // A slow client gets the latest state, not a burst of the ticks it missed.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            ev = events.recv() => {
                let msg = match ev {
                    Ok(ev) => {
                        let mut msg = serde_json::to_value(ev).unwrap_or_default();
                        msg["type"] = "event".into();
                        msg
                    }
                    // The socket fell behind and lost events: say so, so a
                    // client waiting for one reads the state instead.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        serde_json::json!({"type": "lagged", "missed": n})
                    }
                    // The timeline was dropped (its flow stopped): close
                    // the socket the same way as when a new run replaced it.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let _ = tx.send(group_ended()).await;
                        break;
                    }
                };
                if tx.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                let Some(tl) = current(flow_id, &group, &held) else {
                    let _ = tx.send(group_ended()).await;
                    break;
                };
                let msg = tl.telemetry().to_string();
                drop(tl);
                if tx.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                let Some(tl) = current(flow_id, &group, &held) else {
                    // Answer the command, then close.
                    let reply = gone_reply(&text);
                    let _ = tx.send(Message::Text(reply.to_string().into())).await;
                    let _ = tx.send(group_ended()).await;
                    break;
                };
                let reply = ws_reply(&tl, &text);
                drop(tl);
                if tx.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket's hold does not keep the group: once the players are gone, the
    /// group is gone for REST (404) and for the socket, which then closes.
    #[test]
    fn a_socket_does_not_keep_its_group_alive() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let held = Arc::downgrade(&player);
        assert!(current(flow, "g", &held).is_some());
        drop(player);
        assert!(current(flow, "g", &held).is_none());
        assert!(timeline::get(flow, "g").is_none());
    }

    /// A new run makes a new group: a socket on the old one does not follow
    /// it with the old run's subscription.
    #[test]
    fn a_new_run_ends_the_old_socket() {
        let flow = uuid::Uuid::new_v4();
        let first = timeline::get_or_create(flow, "g");
        let held = Arc::downgrade(&first);
        drop(first);
        let second = timeline::get_or_create(flow, "g");
        assert!(current(flow, "g", &held).is_none());
        assert!(current(flow, "g", &Arc::downgrade(&second)).is_some());
    }

    #[test]
    fn another_group_is_not_the_held_one() {
        let flow = uuid::Uuid::new_v4();
        let a = timeline::get_or_create(flow, "a");
        let _b = timeline::get_or_create(flow, "b");
        assert!(current(flow, "b", &Arc::downgrade(&a)).is_none());
    }

    /// A sink that takes messages until one matches `stall_on`, then never
    /// finishes sending it: a client that stopped reading.
    struct StallingSink {
        stall_on: fn(&str) -> bool,
        stalled: Option<tokio::sync::oneshot::Sender<()>>,
        stalling: bool,
    }

    impl futures::Sink<Message> for StallingSink {
        type Error = std::convert::Infallible;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(mut self: std::pin::Pin<&mut Self>, m: Message) -> Result<(), Self::Error> {
            if let Message::Text(t) = &m {
                if (self.stall_on)(t) {
                    self.stalling = true;
                    if let Some(s) = self.stalled.take() {
                        let _ = s.send(());
                    }
                }
            }
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            if self.stalling {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(Ok(()))
            }
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            self.poll_flush(cx)
        }
    }

    /// Start a socket loop on `flow`/`g` whose client stalls on the first
    /// message matching `stall_on`; returns once it has stalled.
    async fn stalled_socket(
        flow: FlowId,
        player: &Arc<timeline::ReplayTimeline>,
        stall_on: fn(&str) -> bool,
        command: Option<&str>,
    ) -> tokio::task::JoinHandle<()> {
        let (stalled_tx, stalled_rx) = tokio::sync::oneshot::channel();
        let sink = StallingSink {
            stall_on,
            stalled: Some(stalled_tx),
            stalling: false,
        };
        let (to_server, from_client) =
            futures::channel::mpsc::unbounded::<Result<Message, std::convert::Infallible>>();
        if let Some(c) = command {
            to_server
                .unbounded_send(Ok(Message::Text(c.to_string().into())))
                .unwrap();
        }
        let task = tokio::spawn(run_socket(
            sink,
            from_client,
            flow,
            "g".to_string(),
            Arc::downgrade(player),
            player.subscribe(),
        ));
        tokio::time::timeout(Duration::from_secs(10), stalled_rx)
            .await
            .expect("the socket never sent")
            .unwrap();
        // Keep the command stream open while the socket is stalled.
        std::mem::forget(to_server);
        task
    }

    /// A client that stops reading while the state is being sent does not
    /// keep the group: once the players are gone, the group is gone.
    #[tokio::test]
    async fn a_stalled_state_send_does_not_keep_the_group() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let task = stalled_socket(flow, &player, |_| true, None).await;
        drop(player);
        assert!(timeline::get(flow, "g").is_none());
        task.abort();
    }

    /// The same while the reply to a command is being sent.
    #[tokio::test]
    async fn a_stalled_reply_does_not_keep_the_group() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let task = stalled_socket(
            flow,
            &player,
            |t| t.contains("\"seq\":4"),
            Some(r#"{"cmd":"jog","frames":1,"seq":4}"#),
        )
        .await;
        drop(player);
        assert!(timeline::get(flow, "g").is_none());
        task.abort();
    }

    /// Drive a socket loop with channels: (commands in, messages out, task).
    fn channel_socket(
        flow: FlowId,
        player: &Arc<timeline::ReplayTimeline>,
    ) -> (
        futures::channel::mpsc::UnboundedSender<Result<Message, std::convert::Infallible>>,
        futures::channel::mpsc::UnboundedReceiver<Message>,
        tokio::task::JoinHandle<()>,
    ) {
        let (to_server, from_client) = futures::channel::mpsc::unbounded();
        let (to_client, from_server) = futures::channel::mpsc::unbounded::<Message>();
        let task = tokio::spawn(run_socket(
            to_client,
            from_client,
            flow,
            "g".to_string(),
            Arc::downgrade(player),
            player.subscribe(),
        ));
        (to_server, from_server, task)
    }

    /// The next message other than a state frame.
    async fn next_reply(rx: &mut futures::channel::mpsc::UnboundedReceiver<Message>) -> Message {
        loop {
            let m = tokio::time::timeout(Duration::from_secs(10), rx.next())
                .await
                .expect("the socket stalled")
                .expect("the socket closed");
            if let Message::Text(t) = &m {
                let v: serde_json::Value = serde_json::from_str(t).unwrap();
                if v.get("type").is_none() || v["type"] == "state" {
                    continue;
                }
            }
            return m;
        }
    }

    fn json(m: &Message) -> serde_json::Value {
        match m {
            Message::Text(t) => serde_json::from_str(t).unwrap(),
            other => panic!("not text: {:?}", other),
        }
    }

    fn is_group_ended(m: &Message) -> bool {
        matches!(m, Message::Close(Some(f)) if f.code == close_code::AWAY)
    }

    /// Each command's reply echoes its `seq`, with an `invalid` or
    /// `conflict` code on a refusal.
    #[tokio::test]
    async fn the_socket_echoes_seq() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let (tx, mut rx, task) = channel_socket(flow, &player);
        let send = |t: &str| {
            tx.unbounded_send(Ok(Message::Text(t.to_string().into())))
                .unwrap()
        };
        send(r#"{"cmd":"jog","frames":1,"seq":7}"#);
        let r = json(&next_reply(&mut rx).await);
        assert_eq!(r["type"], "error");
        assert_eq!(r["seq"], 7);
        assert_eq!(r["code"], "conflict");
        send(r#"{"cmd":"nope","seq":"a"}"#);
        let r = json(&next_reply(&mut rx).await);
        assert_eq!(
            (&r["type"], &r["seq"], &r["code"]),
            (&"error".into(), &"a".into(), &"invalid".into())
        );
        send("not json");
        let r = json(&next_reply(&mut rx).await);
        assert_eq!(
            (&r["seq"], &r["code"]),
            (&serde_json::Value::Null, &"invalid".into())
        );
        drop(tx);
        task.await.unwrap();
    }

    /// An applied command is acked with its result and `seq`.
    #[test]
    fn an_applied_command_is_acked_with_seq() {
        use std::sync::atomic::Ordering;
        let flow = uuid::Uuid::new_v4();
        let tl = timeline::get_or_create(flow, "g");
        let p = Arc::new(timeline::PlayerStats::new("cam0"));
        let base: i64 = 1_800_000_000_000_000_000;
        p.start_tai.store(base, Ordering::Relaxed);
        p.end_tai.store(base + 60_000_000_000, Ordering::Relaxed);
        p.init_grid(50, 1, base);
        tl.register_player(&p);
        let r = ws_reply(&tl, r#"{"cmd":"live","seq":12}"#);
        assert_eq!(r["type"], "ack", "{}", r);
        assert_eq!(r["seq"], 12);
        assert_eq!(r["live"], true);
        assert!(
            r.get("speed").is_some() && r.get("cursor_tai").is_some(),
            "{}",
            r
        );
        tl.unregister_player(&p);
    }

    /// When the group goes, the socket closes with 1001 (going away).
    #[tokio::test]
    async fn the_socket_closes_with_1001_when_the_group_goes() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let (_tx, mut rx, task) = channel_socket(flow, &player);
        drop(player);
        let m = next_reply(&mut rx).await;
        assert!(is_group_ended(&m), "{:?}", m);
        task.await.unwrap();
    }

    /// A command after the group is gone gets an error with its `seq`
    /// (code `gone`) before the close. The loop picks its branches at
    /// random, so run it until it has read the command.
    #[tokio::test]
    async fn a_command_after_the_group_is_answered() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut answered = 0;
        for _ in 0..100 {
            let flow = uuid::Uuid::new_v4();
            let player = timeline::get_or_create(flow, "g");
            let held = Arc::downgrade(&player);
            let events = player.subscribe();
            drop(player);
            let read = Arc::new(AtomicBool::new(false));
            let flag = read.clone();
            let cmd: Result<Message, std::convert::Infallible> =
                Ok(Message::Text(r#"{"cmd":"jog","frames":1,"seq":3}"#.into()));
            let from_client = futures::stream::iter(vec![cmd])
                .chain(futures::stream::pending())
                .inspect(move |_| flag.store(true, Ordering::SeqCst));
            let (to_client, mut from_server) = futures::channel::mpsc::unbounded::<Message>();
            run_socket(to_client, from_client, flow, "g".to_string(), held, events).await;
            let first = next_reply(&mut from_server).await;
            if !read.load(Ordering::SeqCst) {
                // The tick or the closed events won: the command was not read.
                assert!(is_group_ended(&first), "{:?}", first);
                continue;
            }
            let r = json(&first);
            assert_eq!(
                (&r["type"], &r["seq"], &r["code"]),
                (&"error".into(), &3.into(), &"gone".into())
            );
            assert!(is_group_ended(&next_reply(&mut from_server).await));
            answered += 1;
        }
        assert!(answered > 0, "the socket never read the command");
    }

    /// The socket reports lost events instead of dropping them silently.
    #[tokio::test]
    async fn a_lagging_socket_is_told() {
        let flow = uuid::Uuid::new_v4();
        let player = timeline::get_or_create(flow, "g");
        let events = player.subscribe();
        let (_tx, from_client) =
            futures::channel::mpsc::unbounded::<Result<Message, std::convert::Infallible>>();
        let (to_client, mut from_server) = futures::channel::mpsc::unbounded::<Message>();
        for _ in 0..70 {
            player.emit(strom_types::replay::ReplayEvent::Ended {
                at: timeline::instant(0),
            });
        }
        let task = tokio::spawn(run_socket(
            to_client,
            from_client,
            flow,
            "g".to_string(),
            Arc::downgrade(&player),
            events,
        ));
        let r = json(&next_reply(&mut from_server).await);
        assert_eq!(r["type"], "lagged", "{}", r);
        assert_eq!(r["missed"], 6);
        drop(player);
        task.await.unwrap();
    }
}
