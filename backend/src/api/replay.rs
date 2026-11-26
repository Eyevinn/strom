//! Replay control: commands and state per replay group, over REST and a
//! WebSocket, plus the operator page and the lab page (latency and per-player
//! telemetry, for measuring the engine).
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
use crate::state::AppState;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
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
const REPLAY_LAB_HTML: &str = include_str!("../../static/replay-lab.html");

/// GET /player/replay/{flow_id}/{group}
pub async fn replay_page(Path((_flow_id, _group)): Path<(String, String)>) -> Html<&'static str> {
    Html(REPLAY_HTML)
}

/// GET /player/replay-lab/{flow_id}/{group}
pub async fn replay_lab_page(
    Path((_flow_id, _group)): Path<(String, String)>,
) -> Html<&'static str> {
    Html(REPLAY_LAB_HTML)
}

/// GET /api/replay/{flow_id}/{group}/ws
pub async fn replay_ws(
    ws: WebSocketUpgrade,
    State(_state): State<AppState>,
    Path((flow_id, group)): Path<(FlowId, String)>,
) -> Response {
    let Some(tl) = timeline::get(flow_id, &group) else {
        return (StatusCode::NOT_FOUND, "replay group not found").into_response();
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

async fn handle(
    socket: WebSocket,
    flow_id: FlowId,
    group: String,
    held: Weak<timeline::ReplayTimeline>,
    mut events: tokio::sync::broadcast::Receiver<strom_types::replay::ReplayEvent>,
) {
    let (mut tx, mut rx) = socket.split();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let mut msg = serde_json::to_value(ev).unwrap_or_default();
                msg["type"] = "event".into();
                if tx.send(Message::Text(msg.to_string().into())).await.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                let Some(tl) = current(flow_id, &group, &held) else {
                    let _ = tx
                        .send(Message::Close(Some(CloseFrame {
                            code: close_code::AWAY,
                            reason: "replay group ended".into(),
                        })))
                        .await;
                    break;
                };
                let msg = tl.telemetry().to_string();
                if tx.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                // The next tick closes the socket if the group is gone.
                let Some(tl) = current(flow_id, &group, &held) else { continue };
                let reply = ws_reply(&tl, &text);
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
}
