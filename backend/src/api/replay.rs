//! Replay control (API version 2): discovery, commands, state and
//! diagnostics per replay group, over REST and a WebSocket, plus the operator
//! page and the diagnostics page.
//!
//! Both transports take the same commands ([`ReplayRequest`], JSON tagged by
//! `cmd`, with an optional `client_ref`) and return the same result
//! ([`ReplayCommandResult`]). A malformed command or a value out of range is
//! refused as `invalid` (HTTP 400), a command that does not apply to the
//! group now as `conflict` (HTTP 409), a range wholly outside the media as
//! `out_of_range` (HTTP 409). On REST the code is the error's `error` field.
//! The WebSocket sends [`ReplayWsMessage`]s: the state every 50 ms (or
//! `?state_ms=`), events, `lagged` when it dropped events, diagnostics with
//! `?diagnostics=1`, and an `ack` or `error` for each command, carrying the
//! command's `seq`.
//!
//! A socket does not keep its group alive: it holds the group weakly and
//! closes once the group's players are gone (the flow stopped or was deleted)
//! or a new run replaced the group. A client reconnects to follow the next run.

use crate::blocks::builtin::mediaplayer::replay::timeline::{self, CommandError};
use crate::json_rejection::JsonBody;
use crate::state::AppState;
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::{Arc, Weak};
use std::time::Duration;
use strom_types::api::ErrorResponse;
use strom_types::replay::{
    ReplayAck, ReplayCommandResult, ReplayDiagnostics, ReplayErrorCode, ReplayEvent,
    ReplayGroupInfo, ReplayIndex, ReplayRequest, ReplayState, ReplayWsError, ReplayWsMessage,
    MAX_STATE_MS, MIN_STATE_MS,
};
use strom_types::FlowId;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn not_found() -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("replay group not found")),
    )
}

/// A refused command: the code (`invalid`, `conflict`, `out_of_range`) as
/// `error`, the reason as `details`.
fn command_error(e: CommandError) -> ApiError {
    let status = match e.code() {
        ReplayErrorCode::Invalid => StatusCode::BAD_REQUEST,
        ReplayErrorCode::Conflict | ReplayErrorCode::OutOfRange => StatusCode::CONFLICT,
    };
    (
        status,
        Json(ErrorResponse::with_details(e.code().as_str(), e.message())),
    )
}

fn group(flow_id: FlowId, group: &str) -> Result<Arc<timeline::ReplayTimeline>, ApiError> {
    timeline::get(flow_id, group).ok_or_else(not_found)
}

fn index(flow: Option<FlowId>) -> ReplayIndex {
    ReplayIndex::new(
        timeline::running(flow)
            .iter()
            .map(|(f, g, tl)| tl.info(*f, g))
            .collect(),
    )
}

/// The replay groups of every running flow, with the API version and
/// features.
#[utoipa::path(
    get,
    path = "/api/replay",
    tag = "replay",
    responses((status = 200, description = "Replay groups", body = ReplayIndex))
)]
pub async fn replay_index() -> Json<ReplayIndex> {
    Json(index(None))
}

/// The replay groups of a flow (empty when it does not run).
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}",
    tag = "replay",
    params(("flow_id" = String, Path, description = "Flow ID (UUID)")),
    responses((status = 200, description = "The flow's replay groups", body = ReplayIndex))
)]
pub async fn replay_flow_index(Path(flow_id): Path<FlowId>) -> Json<ReplayIndex> {
    Json(index(Some(flow_id)))
}

/// One replay group, for discovery.
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}/{group}",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    responses(
        (status = 200, description = "The group", body = ReplayGroupInfo),
        (status = 404, description = "Replay group not found", body = ErrorResponse)
    )
)]
pub async fn replay_group(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
) -> Result<Json<ReplayGroupInfo>, ApiError> {
    Ok(Json(
        group(flow_id, &group_name)?.info(flow_id, &group_name),
    ))
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
    request_body = ReplayRequest,
    responses(
        (status = 200, description = "Command applied", body = ReplayCommandResult),
        (status = 400, description = "invalid: malformed command or value out of range", body = ErrorResponse),
        (status = 404, description = "Replay group not found", body = ErrorResponse),
        (status = 409, description = "conflict: the command does not apply now; out_of_range: the range is outside the media", body = ErrorResponse)
    )
)]
pub async fn replay_command(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
    JsonBody(body): JsonBody<serde_json::Value>,
) -> Result<Json<ReplayCommandResult>, ApiError> {
    let tl = group(flow_id, &group_name)?;
    let req = timeline::parse_command(body).map_err(command_error)?;
    let applied = tl.apply(req).map_err(command_error)?;
    Ok(Json(tl.result(&applied)))
}

/// The state of a replay group (what the WebSocket streams as `state`).
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}/{group}/state",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    responses(
        (status = 200, description = "Group state", body = ReplayState),
        (status = 404, description = "Replay group not found", body = ErrorResponse)
    )
)]
pub async fn replay_state(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
) -> Result<Json<ReplayState>, ApiError> {
    Ok(Json(group(flow_id, &group_name)?.state()))
}

/// Engine diagnostics of a replay group.
#[utoipa::path(
    get,
    path = "/api/replay/{flow_id}/{group}/diagnostics",
    tag = "replay",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("group" = String, Path, description = "Replay group")
    ),
    responses(
        (status = 200, description = "Group diagnostics", body = ReplayDiagnostics),
        (status = 404, description = "Replay group not found", body = ErrorResponse)
    )
)]
pub async fn replay_diagnostics(
    Path((flow_id, group_name)): Path<(FlowId, String)>,
) -> Result<Json<ReplayDiagnostics>, ApiError> {
    Ok(Json(group(flow_id, &group_name)?.diagnostics()))
}

const REPLAY_HTML: &str = include_str!("../../static/replay.html");
const REPLAY_DIAGNOSTICS_HTML: &str = include_str!("../../static/replay-diagnostics.html");

/// GET /player/replay/{flow_id}/{group}
pub async fn replay_page(Path((_flow_id, _group)): Path<(String, String)>) -> Html<&'static str> {
    Html(REPLAY_HTML)
}

/// GET /player/replay-diagnostics/{flow_id}/{group}
pub async fn replay_diagnostics_page(
    Path((_flow_id, _group)): Path<(String, String)>,
) -> Html<&'static str> {
    Html(REPLAY_DIAGNOSTICS_HTML)
}

/// GET /player/replay-lab/{flow_id}/{group}: the old name of the
/// diagnostics page.
pub async fn replay_lab_page(Path((flow_id, group)): Path<(String, String)>) -> Redirect {
    Redirect::permanent(&format!(
        "/player/replay-diagnostics/{}/{}",
        urlencoding::encode(&flow_id),
        urlencoding::encode(&group)
    ))
}

/// What a socket asks for.
#[derive(Debug, Default, Deserialize)]
pub struct WsParams {
    /// `1` or `true`: send `diagnostics` with each state.
    pub diagnostics: Option<String>,
    /// State every this many ms, 50 .. 1000 (clamped).
    pub state_ms: Option<u64>,
}

impl WsParams {
    fn diagnostics(&self) -> bool {
        matches!(self.diagnostics.as_deref(), Some("1") | Some("true"))
    }

    fn state_every(&self) -> Duration {
        Duration::from_millis(
            self.state_ms
                .unwrap_or(MIN_STATE_MS)
                .clamp(MIN_STATE_MS, MAX_STATE_MS),
        )
    }
}

/// GET /api/replay/{flow_id}/{group}/ws?diagnostics=1&state_ms=200
pub async fn replay_ws(
    ws: WebSocketUpgrade,
    State(_state): State<AppState>,
    Path((flow_id, group)): Path<(FlowId, String)>,
    Query(params): Query<WsParams>,
) -> Response {
    let Some(tl) = timeline::get(flow_id, &group) else {
        return (StatusCode::NOT_FOUND, "replay group not found").into_response();
    };
    let events = tl.subscribe();
    let held = Arc::downgrade(&tl);
    drop(tl);
    ws.on_upgrade(move |socket| handle(socket, flow_id, group, held, events, params))
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
fn ws_reply(tl: &timeline::ReplayTimeline, text: &str) -> ReplayWsMessage {
    let mut value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return ReplayWsMessage::Error(ReplayWsError {
                seq: serde_json::Value::Null,
                code: ReplayErrorCode::Invalid,
                error: format!("not JSON: {}", e),
                client_ref: None,
            })
        }
    };
    let seq = value.get("seq").cloned().unwrap_or(serde_json::Value::Null);
    let client_ref = timeline::take_client_ref(&mut value.clone()).ok().flatten();
    let applied = timeline::parse_command(std::mem::take(&mut value)).and_then(|r| tl.apply(r));
    match applied {
        Ok(a) => ReplayWsMessage::Ack(ReplayAck {
            seq,
            result: tl.result(&a),
        }),
        Err(e) => ReplayWsMessage::Error(ReplayWsError {
            seq,
            code: e.code(),
            error: e.message().to_string(),
            client_ref,
        }),
    }
}

fn text(m: &ReplayWsMessage) -> Message {
    Message::Text(serde_json::to_string(m).unwrap_or_default().into())
}

/// The close frame sent when the socket's group is gone.
fn group_ended() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::AWAY,
        reason: "replay group ended".into(),
    }))
}

async fn handle(
    socket: WebSocket,
    flow_id: FlowId,
    group: String,
    held: Weak<timeline::ReplayTimeline>,
    events: tokio::sync::broadcast::Receiver<ReplayEvent>,
    params: WsParams,
) {
    let (tx, rx) = socket.split();
    run_socket(tx, rx, flow_id, group, held, events, params).await;
}

/// The socket's loop, on any sink and stream of messages (a test drives it
/// with channels). A command's `ack` goes out before the events it caused:
/// they are sent on a later turn of the loop.
async fn run_socket<T, R, E>(
    mut tx: T,
    mut rx: R,
    flow_id: FlowId,
    group: String,
    held: Weak<timeline::ReplayTimeline>,
    mut events: tokio::sync::broadcast::Receiver<ReplayEvent>,
    params: WsParams,
) where
    T: futures::Sink<Message> + Unpin,
    R: futures::Stream<Item = Result<Message, E>> + Unpin,
{
    let mut tick = tokio::time::interval(params.state_every());
    // A slow client gets the latest state, not a burst of the ticks it
    // missed (which would crowd out events and push it into `lagged`).
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let diagnostics = params.diagnostics();
    loop {
        tokio::select! {
            ev = events.recv() => {
                let msg = match ev {
                    Ok(ev) => ReplayWsMessage::Event(ev),
                    // The socket fell behind: say so, the client reads state.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        ReplayWsMessage::Lagged { missed: n }
                    }
                    // The timeline was dropped (its flow stopped): close
                    // the socket the same way as when a new run replaced it.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let _ = tx.send(group_ended()).await;
                        break;
                    }
                };
                if tx.send(text(&msg)).await.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                let Some(tl) = current(flow_id, &group, &held) else {
                    let _ = tx.send(group_ended()).await;
                    break;
                };
                let state = text(&ReplayWsMessage::State(tl.state()));
                let diag = diagnostics.then(|| text(&ReplayWsMessage::Diagnostics(tl.diagnostics())));
                drop(tl);
                if tx.send(state).await.is_err() {
                    break;
                }
                if let Some(d) = diag {
                    if tx.send(d).await.is_err() {
                        break;
                    }
                }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(t) = msg else { continue };
                // The next tick closes the socket if the group is gone.
                let Some(tl) = current(flow_id, &group, &held) else { continue };
                let reply = ws_reply(&tl, &t);
                drop(tl);
                if tx.send(text(&reply)).await.is_err() {
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

    /// `source` and `cue {source}` over REST: refused as 400 (which angle,
    /// a source that does not open) or 409 (no common time) with the code
    /// in `error`, applied as 200 with the motion unchanged.
    #[tokio::test]
    async fn source_switch_over_rest() {
        use timeline::switch_tests::angle;
        use timeline::TimeDomain;
        let post = |flow: FlowId, body: serde_json::Value| async move {
            replay_command(Path((flow, "g".to_string())), JsonBody(body)).await
        };
        let code = |r: Result<Json<ReplayCommandResult>, ApiError>| match r {
            Err((status, Json(e))) => (status, e.error),
            Ok(_) => (StatusCode::OK, String::new()),
        };

        let flow = uuid::Uuid::new_v4();
        let tl = timeline::get_or_create(flow, "g");
        let (_a, _ra) = angle(&tl, "a", TimeDomain::Tai);
        let (_b, rb) = angle(&tl, "b", TimeDomain::Tai);
        let src = serde_json::json!({"cmd": "source", "source": "cam2"});
        assert_eq!(
            code(post(flow, src).await),
            (StatusCode::BAD_REQUEST, "invalid".to_string())
        );
        let bad = serde_json::json!({"cmd": "source", "source": "bad", "block": "b"});
        assert_eq!(code(post(flow, bad).await).0, StatusCode::BAD_REQUEST);
        let ok = serde_json::json!({"cmd": "source", "source": "cam2", "block": "b",
            "client_ref": "r1"});
        let Json(res) = post(flow, ok).await.unwrap();
        assert_eq!(res.client_ref.as_deref(), Some("r1"));
        assert_eq!(res.motion.speed, 0.0);
        assert_eq!(
            rb.begun.lock().unwrap().clone(),
            vec![("cam2".to_string(), Some("r1".to_string()))]
        );

        let loose = uuid::Uuid::new_v4();
        let tl2 = timeline::get_or_create(loose, "g");
        let (_c, _rc) = angle(&tl2, "c", TimeDomain::Unanchored);
        let cue = serde_json::json!({"cmd": "cue", "in": {"rel_ms": 0}, "out": {"rel_ms": 1000},
            "source": "cam2"});
        assert_eq!(
            code(post(loose, cue).await),
            (StatusCode::CONFLICT, "conflict".to_string())
        );
        drop((tl, tl2));
    }

    /// `cue_next` and `clear_queue` over REST: the feature is announced, a
    /// clip is queued (200, the motion unchanged, the state counts it), a
    /// backward speed is 400, and the queue is emptied.
    #[tokio::test]
    async fn cue_next_over_rest() {
        use timeline::switch_tests::angle;
        use timeline::TimeDomain;
        assert!(replay_index()
            .await
            .0
            .features
            .contains(&"cue_next".to_string()));
        let flow = uuid::Uuid::new_v4();
        let tl = timeline::get_or_create(flow, "g");
        let (_a, _ra) = angle(&tl, "a", TimeDomain::Tai);
        let post = |body: serde_json::Value| async move {
            replay_command(Path((flow, "g".to_string())), JsonBody(body)).await
        };
        let next = serde_json::json!({"cmd": "cue_next", "in": {"rel_ms": 1000},
            "out": {"rel_ms": 2000}, "speed": 0.5, "client_ref": "q1"});
        let Json(res) = post(next).await.unwrap();
        assert_eq!(res.client_ref.as_deref(), Some("q1"));
        assert_eq!(res.motion.speed, 0.0);
        assert_eq!(tl.state().queue.waiting, 1);
        let back = serde_json::json!({"cmd": "cue_next", "in": {"rel_ms": 1000},
            "out": {"rel_ms": 2000}, "speed": -1.0});
        match post(back).await {
            Err((status, Json(e))) => {
                assert_eq!(
                    (status, e.error.as_str()),
                    (StatusCode::BAD_REQUEST, "invalid")
                )
            }
            Ok(_) => panic!("a backward clip was queued"),
        }
        let Json(cleared) = post(serde_json::json!({"cmd": "clear_queue"}))
            .await
            .unwrap();
        assert_eq!(cleared.motion.speed, 0.0);
        assert_eq!(tl.state().queue.waiting, 0);
        drop(tl);
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
    fn ws_params_clamp_the_state_rate() {
        let p = |state_ms, diagnostics: Option<&str>| WsParams {
            state_ms,
            diagnostics: diagnostics.map(str::to_string),
        };
        assert_eq!(p(None, None).state_every(), Duration::from_millis(50));
        assert_eq!(p(Some(10), None).state_every(), Duration::from_millis(50));
        assert_eq!(p(Some(200), None).state_every(), Duration::from_millis(200));
        assert_eq!(
            p(Some(60_000), None).state_every(),
            Duration::from_millis(1000)
        );
        assert!(p(None, Some("1")).diagnostics());
        assert!(!p(None, Some("0")).diagnostics());
        assert!(!p(None, None).diagnostics());
    }

    /// A socket's reply is a typed message: `ack` with the motion, `seq` and
    /// `client_ref`, or `error` with its code, `seq` and `client_ref`.
    #[test]
    fn ws_replies_echo_seq_and_client_ref() {
        let flow = uuid::Uuid::new_v4();
        let tl = timeline::get_or_create(flow, "g");
        // No media: a conflict.
        let r = ws_reply(&tl, r#"{"cmd":"pause","seq":4,"client_ref":"a"}"#);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["seq"], 4);
        assert_eq!(v["code"], "conflict");
        assert_eq!(v["client_ref"], "a");
        let r = ws_reply(&tl, "not json");
        assert!(matches!(
            r,
            ReplayWsMessage::Error(ReplayWsError {
                code: ReplayErrorCode::Invalid,
                ..
            })
        ));
        assert_eq!(
            command_error(CommandError::OutOfRange("x".into())).0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            command_error(CommandError::OutOfRange("x".into())).1.error,
            "out_of_range"
        );
    }

    /// The socket loop: an `ack` goes out before the event its command
    /// caused; a socket that fell behind gets `lagged` with the number
    /// missed, and every jump in `event_seq` equals what was reported missed.
    #[tokio::test]
    async fn the_socket_acks_before_events_and_reports_lag() {
        use futures::channel::mpsc;
        use std::sync::atomic::Ordering;
        let flow = uuid::Uuid::new_v4();
        let tl = timeline::get_or_create(flow, "g");
        let p = Arc::new(timeline::PlayerStats::new("cam0"));
        let base: i64 = 1_800_000_000_000_000_000;
        p.start_tai.store(base, Ordering::Relaxed);
        p.end_tai.store(base + 60_000_000_000, Ordering::Relaxed);
        p.init_grid(50, 1, base);
        tl.register_player(&p);
        let events = tl.subscribe();
        // More events than the channel holds (64) before the socket reads.
        let n = 70u64;
        for _ in 0..n {
            tl.apply(timeline::Request {
                command: timeline::Command::Live { margin_ms: None },
                client_ref: None,
            })
            .unwrap();
        }
        let (to_server, from_client) =
            mpsc::unbounded::<Result<Message, std::convert::Infallible>>();
        let (to_client, mut from_server) = mpsc::unbounded::<Message>();
        let params = WsParams {
            diagnostics: None,
            state_ms: Some(1000),
        };
        let task = tokio::spawn(run_socket(
            to_client,
            from_client,
            flow,
            "g".to_string(),
            Arc::downgrade(&tl),
            events,
            params,
        ));
        to_server
            .unbounded_send(Ok(Message::Text(
                r#"{"cmd":"live","seq":9,"client_ref":"go"}"#.into(),
            )))
            .unwrap();
        // Read until the command's event has come.
        let mut got: Vec<serde_json::Value> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !got
            .iter()
            .any(|v| v["type"] == "event" && v["client_ref"] == "go")
        {
            let m = tokio::time::timeout_at(deadline, from_server.next())
                .await
                .expect("the socket stalled")
                .expect("the socket closed");
            if let Message::Text(t) = m {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] != "state" {
                    got.push(v);
                }
            }
        }
        drop(to_server);
        task.await.unwrap();
        // No silent gap: every jump in `event_seq` comes after `lagged`
        // messages whose `missed` add up to the jump. There may be more than
        // one: the command's own event can push another unread one out of
        // the channel (it keeps the last 64) after the first `lagged`.
        let mut last = 0u64;
        let mut pending = 0u64;
        let mut missed_total = 0u64;
        let mut received = 0u64;
        for v in &got {
            if v["type"] == "lagged" {
                let m = v["missed"].as_u64().unwrap();
                assert!(m > 0, "{:?}", got);
                pending += m;
                missed_total += m;
            } else if v["type"] == "event" {
                let seq = v["event_seq"].as_u64().unwrap();
                assert_eq!(seq, last + 1 + pending, "{:?}", got);
                last = seq;
                pending = 0;
                received += 1;
            }
        }
        // The socket fell behind, and every event is either received or
        // counted as missed.
        assert!(missed_total >= n - 64, "missed {}", missed_total);
        assert_eq!(missed_total + received, n + 1);
        // The ack comes before the event its command caused.
        let ack = got
            .iter()
            .position(|v| v["type"] == "ack" && v["seq"] == 9)
            .expect("no ack");
        let ev = got
            .iter()
            .position(|v| v["type"] == "event" && v["client_ref"] == "go")
            .unwrap();
        assert!(ack < ev, "{:?}", got);
        assert_eq!(got[ev]["event_seq"], n + 1);
        tl.unregister_player(&p);
    }

    #[test]
    fn another_group_is_not_the_held_one() {
        let flow = uuid::Uuid::new_v4();
        let a = timeline::get_or_create(flow, "a");
        let _b = timeline::get_or_create(flow, "b");
        assert!(current(flow, "b", &Arc::downgrade(&a)).is_none());
    }
}
