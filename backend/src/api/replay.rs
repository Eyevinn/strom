//! Replay control: a WebSocket per replay group and the replay control page.
//!
//! Commands are JSON objects tagged by `cmd` (see `replay::timeline::Command`),
//! with an optional `seq` echoed in the ack. The socket also streams the group
//! state every 50 ms.

use crate::blocks::builtin::mediaplayer::replay::timeline::{self, Command};
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use strom_types::FlowId;

const REPLAY_HTML: &str = include_str!("../../static/replay.html");

/// GET /player/replay/{flow_id}/{group}
pub async fn replay_page(Path((_flow_id, _group)): Path<(String, String)>) -> Html<&'static str> {
    Html(REPLAY_HTML)
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
    ws.on_upgrade(move |socket| handle(socket, tl))
}

async fn handle(socket: WebSocket, tl: std::sync::Arc<timeline::ReplayTimeline>) {
    let (mut tx, mut rx) = socket.split();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let msg = tl.telemetry().to_string();
                if tx.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let seq = value.get("seq").cloned().unwrap_or(serde_json::Value::Null);
                let reply = match serde_json::from_value::<Command>(value) {
                    Ok(cmd) => {
                        let (frame, speed) = tl.apply(cmd);
                        serde_json::json!({"type": "ack", "seq": seq, "frame": frame, "speed": speed})
                    }
                    Err(e) => serde_json::json!({"type": "error", "seq": seq, "error": e.to_string()}),
                };
                if tx.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}
