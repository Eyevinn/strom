//! Phone cameras (TallyCam): the device and controller WebSockets, the camera
//! list, invites, and the operator page.
//!
//! Phones connect to `/api/cameras/device` with the TallyCam control protocol.
//! That route is public: a phone has no Strom session. It is guarded by the
//! invite token instead (a new phone needs one from `POST
//! /api/cameras/invite`; a known phone reconnects without). Operator UIs use
//! `/api/cameras/control`, behind Strom's authentication.
//!
//! A phone proves who it is with its device key: the hello carries the
//! public key (the device id is derived from it), Strom sends a challenge and
//! the phone signs it. An invite binds the key; afterwards the key alone
//! admits the phone, until it is forgotten (`DELETE /api/cameras/{id}`).

use std::net::SocketAddr;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use strom_types::api::ErrorResponse;
use strom_types::cameras::{CameraInviteResponse, CameraListResponse};
use tallycam_hub::server::{
    controller_session, device_session_for, limit_upgrade, operator_page, request_host_of,
    same_origin_of, DeviceConnection, DeviceQuery, CLAPPER_HTML,
};
use tracing::warn;

use crate::cameras::CONTROL_PATH;
use crate::state::AppState;

/// Phone connection (TallyCam control protocol, device side).
#[utoipa::path(
    get,
    path = "/api/cameras/device",
    tag = "cameras",
    description = "WebSocket for phones running TallyCam. Public: a phone proves its device key \
                   (the hello carries the public key, Strom answers with a `challenge`, the phone \
                   signs it in `auth`). A new phone also needs the one-time invite token from \
                   `POST /api/cameras/invite` (the `token` query parameter of the control URL in \
                   the invite), which binds its key; a known phone is admitted by its key alone. \
                   Apps without device keys are refused. A refused phone gets a `message` with the \
                   reason and is disconnected.",
    params(
        ("token" = Option<String>, Query, description = "One-time invite token")
    ),
    responses(
        (status = 101, description = "WebSocket connection upgraded")
    )
)]
pub async fn device_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<DeviceQuery>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let hub = state.cameras().hub();
    let connection = DeviceConnection {
        addr,
        token: query.token,
        // X-Forwarded-Host, else Host, else the HTTP/2 authority: the host
        // the phone's device-key proof is bound to (LCP protocol 4).
        host: request_host_of(&headers, &uri),
    };
    limit_upgrade(ws).on_upgrade(move |socket| device_session_for(socket, hub, connection))
}

/// Operator UI connection (TallyCam control protocol, controller side).
#[utoipa::path(
    get,
    path = "/api/cameras/control",
    tag = "cameras",
    description = "WebSocket for operator UIs: a snapshot of every camera, then their events \
                   (state, status, previews, results, logs, tally); requests set controls, run \
                   commands and show messages on the phones. Previews run while at least one \
                   controller is connected and watches the camera. A browser upgrade from \
                   another origin (another site, or another port of this host) is refused.",
    responses(
        (status = 101, description = "WebSocket connection upgraded"),
        (status = 401, description = "Authentication required"),
        (status = 403, description = "Cross-origin upgrade refused")
    )
)]
pub async fn control_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    // Browsers make no CORS check for WebSockets, and the session cookie
    // (SameSite=Strict) still goes to another port of the same host: a page
    // there must not drive the cameras through the operator's browser.
    if !same_origin_of(&headers, &uri) {
        warn!(
            "Camera controller from {} refused: cross-origin upgrade",
            addr
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    let hub = state.cameras().hub();
    limit_upgrade(ws).on_upgrade(move |socket| controller_session(socket, hub, addr))
}

/// List the known cameras.
#[utoipa::path(
    get,
    path = "/api/cameras",
    tag = "cameras",
    description = "Every camera Strom knows (connected now, or admitted before), with its tally, \
                   the SRT input blocks it is bound to, and its latest battery and stream state.",
    responses(
        (status = 200, description = "Known cameras", body = CameraListResponse)
    )
)]
pub async fn list_cameras(State(state): State<AppState>) -> Json<CameraListResponse> {
    let flows = state.get_flows().await;
    let mut running = std::collections::HashSet::new();
    for flow in &flows {
        if state.get_flow_state(&flow.id).await.is_some() {
            running.insert(flow.id);
        }
    }
    let cameras = state.cameras().list(&flows, &|id| running.contains(id));
    Json(CameraListResponse { cameras })
}

/// Forget a camera.
#[utoipa::path(
    delete,
    path = "/api/cameras/{id}",
    tag = "cameras",
    description = "Forget (revoke) a camera: it is disconnected, removed from the list and \
                   from `cameras.json`, and its device key no longer admits it. To come back it \
                   needs a new invite. SRT input blocks bound to it keep their `camera` property.",
    params(
        ("id" = String, Path, description = "Camera (device) id")
    ),
    responses(
        (status = 204, description = "Forgotten"),
        (status = 404, description = "No such camera", body = ErrorResponse)
    )
)]
pub async fn forget_camera(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    if state.cameras().hub().forget(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(format!("no camera '{id}'"))),
        ))
    }
}

/// Create a one-time invite for a new phone.
#[utoipa::path(
    post,
    path = "/api/cameras/invite",
    tag = "cameras",
    description = "A provisioning link for a new phone and its QR code. The link carries the \
                   device WebSocket URL with a one-time token; it admits one phone and expires. \
                   The address in it is `cameras.public_host` (or `cameras.device_url`) when \
                   configured, else the host this request was made to, or this machine's first \
                   LAN address when that is loopback.",
    responses(
        (status = 200, description = "Invite", body = CameraInviteResponse),
        (status = 503, description = "No address a phone could reach", body = ErrorResponse)
    )
)]
pub async fn create_invite(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Json<CameraInviteResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .cameras()
        .invite(request_host_of(&headers, &uri).as_deref())
        .map(Json)
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, Json(ErrorResponse::new(e))))
}

/// The TallyCam operator page, against Strom's camera endpoints. The page is
/// public; what it loads (the control WebSocket, invites) is not.
/// GET /cameras
pub async fn cameras_page() -> impl IntoResponse {
    Html(operator_page(&serde_json::json!({
        "controlPath": CONTROL_PATH,
        "inviteUrl": "/api/cameras/invite",
        "devicesUrl": "/api/cameras",
        "clapperUrl": "/cameras/clapper",
        "title": "Strom Cameras",
    })))
}

/// The clapper page: flash and beep at whole seconds, to film for A/V sync.
/// GET /cameras/clapper
pub async fn clapper_page() -> impl IntoResponse {
    Html(CLAPPER_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;

    /// A browser on HTTP/2 sends no `Host`; the invite and the origin check
    /// use the URI's authority (the hub's helpers, as the routes call them).
    #[test]
    fn an_http2_request_gets_its_host_from_the_uri() {
        let uri: Uri = "https://strom.example.com:8082/api/cameras/invite"
            .parse()
            .unwrap();
        assert_eq!(
            request_host_of(&HeaderMap::new(), &uri).as_deref(),
            Some("strom.example.com:8082")
        );
        let mut browser = HeaderMap::new();
        browser.insert(
            header::ORIGIN,
            "https://strom.example.com:8082".parse().unwrap(),
        );
        assert!(same_origin_of(&browser, &uri));
    }
}
