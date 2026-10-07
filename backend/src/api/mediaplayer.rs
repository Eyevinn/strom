//! Media player API handlers.

use crate::json_rejection::{JsonBody, ValidatedJson};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
pub use strom_types::mediaplayer::{
    GotoRequest, PlayerAction, PlayerControlRequest, PlayerStateResponse, SeekRequest,
    SetPlaylistRequest,
};
use strom_types::{api::ErrorResponse, element::PropertyValue, FlowId};
use tracing::info;

use crate::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use crate::state::AppState;

/// Run a player control call off the async runtime. It waits for the
/// player's other control calls and, after a file switch, for the source to
/// set up the new stream, none of which may stall a runtime worker.
async fn control<F>(call: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|e| Err(format!("control call failed: {}", e)))
}

/// Get the current state of a media player block.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/player/state",
    tag = "media_player",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    responses(
        (status = 200, description = "Player state", body = PlayerStateResponse),
        (status = 404, description = "Player not found", body = ErrorResponse)
    )
)]
pub async fn get_player_state(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<PlayerStateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let key = MediaPlayerKey { flow_id, block_id };

    let player = MEDIA_PLAYER_REGISTRY.get(&key).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Media player not found")),
    ))?;

    let playlist = player.playlist_files();
    let current_index = player.current_index();

    Ok(Json(PlayerStateResponse {
        state: player.state(),
        position_ns: player.position().unwrap_or(0),
        duration_ns: player.duration().unwrap_or(0),
        current_file_index: current_index,
        total_files: playlist.len(),
        current_file: player.current_file(),
        playlist,
        loop_playlist: player
            .loop_playlist
            .load(std::sync::atomic::Ordering::SeqCst),
    }))
}

/// Set the playlist for a media player block.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/player/playlist",
    tag = "media_player",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = SetPlaylistRequest,
    responses(
        (status = 200, description = "Playlist set"),
        (status = 404, description = "Flow or block not found", body = ErrorResponse)
    )
)]
pub async fn set_playlist(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    ValidatedJson(req): ValidatedJson<SetPlaylistRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    info!(
        "Setting playlist for player {}: {} files",
        block_id,
        req.files.len()
    );

    // Always store playlist as a block property so it persists
    let mut flow = state.get_flow(&flow_id).await.ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Flow not found")),
    ))?;

    // Find the block and update its playlist property
    let block = flow.blocks.iter_mut().find(|b| b.id == block_id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Block not found")),
    ))?;

    // Store playlist as JSON string in properties
    let playlist_json = serde_json::to_string(&req.files).unwrap_or_else(|_| "[]".to_string());
    block
        .properties
        .insert("playlist".to_string(), PropertyValue::String(playlist_json));

    // Save the updated flow
    if let Err(e) = state.upsert_flow(flow).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse::with_details(
                "Failed to save flow",
                e.to_string(),
            )),
        ));
    }

    // If flow is running, also update the runtime player
    let key = MediaPlayerKey {
        flow_id,
        block_id: block_id.clone(),
    };

    if let Some(player) = MEDIA_PLAYER_REGISTRY.get(&key) {
        let was_stopped = player.state() == strom_types::mediaplayer::PlayerState::Stopped;
        player.set_playlist(req.files);

        // Only auto-start from the beginning if the player was stopped
        if was_stopped && player.playlist_len() > 0 {
            let _ = control(move || player.goto(0)).await;
        }
    }

    Ok(StatusCode::OK)
}

/// Control the media player (play, pause, stop, next, previous).
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/player/control",
    tag = "media_player",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = PlayerControlRequest,
    responses(
        (status = 200, description = "Action performed"),
        (status = 400, description = "Action failed", body = ErrorResponse),
        (status = 404, description = "Player not found", body = ErrorResponse)
    )
)]
pub async fn control_player(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    JsonBody(req): JsonBody<PlayerControlRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let key = MediaPlayerKey {
        flow_id,
        block_id: block_id.clone(),
    };

    let player = MEDIA_PLAYER_REGISTRY.get(&key).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Media player not found")),
    ))?;

    info!("Player {} control: {:?}", block_id, req.action);

    let action = req.action;
    let result = control(move || match action {
        PlayerAction::Play => player.play(),
        PlayerAction::Pause => player.pause(),
        PlayerAction::Stop => player.stop(),
        PlayerAction::Next => player.next(),
        PlayerAction::Previous => player.previous(),
    })
    .await;

    result.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details("Action failed", e)),
        )
    })?;

    Ok(StatusCode::OK)
}

/// Seek to a position in the current file.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/player/seek",
    tag = "media_player",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = SeekRequest,
    responses(
        (status = 200, description = "Seek performed"),
        (status = 400, description = "Seek failed", body = ErrorResponse),
        (status = 404, description = "Player not found", body = ErrorResponse)
    )
)]
pub async fn seek_player(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    JsonBody(req): JsonBody<SeekRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let key = MediaPlayerKey {
        flow_id,
        block_id: block_id.clone(),
    };

    let player = MEDIA_PLAYER_REGISTRY.get(&key).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Media player not found")),
    ))?;

    // Validate seek position against duration (if known)
    if let Some(duration) = player.duration() {
        if duration > 0 && req.position_ns > duration {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details(
                    "Seek position out of range",
                    format!(
                        "Position {} ns exceeds duration {} ns",
                        req.position_ns, duration
                    ),
                )),
            ));
        }
    }

    info!("Player {} seek to {} ns", block_id, req.position_ns);
    let position_ns = req.position_ns;
    control(move || player.seek(position_ns))
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::with_details("Seek failed", e)),
            )
        })?;

    Ok(StatusCode::OK)
}

/// Go to a specific file in the playlist.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/player/goto",
    tag = "media_player",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Block ID")
    ),
    request_body = GotoRequest,
    responses(
        (status = 200, description = "Goto performed"),
        (status = 400, description = "Goto failed", body = ErrorResponse),
        (status = 404, description = "Player not found", body = ErrorResponse)
    )
)]
pub async fn goto_file(
    State(_state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    JsonBody(req): JsonBody<GotoRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let key = MediaPlayerKey {
        flow_id,
        block_id: block_id.clone(),
    };

    let player = MEDIA_PLAYER_REGISTRY.get(&key).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Media player not found")),
    ))?;

    info!("Player {} goto file index {}", block_id, req.index);

    let index = req.index;
    control(move || player.goto(index)).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details("Goto failed", e)),
        )
    })?;

    Ok(StatusCode::OK)
}
