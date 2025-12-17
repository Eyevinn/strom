//! Media player API handlers.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use strom_types::{api::ErrorResponse, element::PropertyValue, FlowId};
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::blocks::builtin::mediaplayer::{MediaPlayerKey, MEDIA_PLAYER_REGISTRY};
use crate::state::AppState;

/// Player control action.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PlayerAction {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

/// Request to control the media player.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PlayerControlRequest {
    pub action: PlayerAction,
}

/// Request to set the playlist.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetPlaylistRequest {
    /// List of file URIs (e.g., "file:///path/to/video.mp4")
    pub files: Vec<String>,
}

/// Request to seek to a position.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SeekRequest {
    /// Position in nanoseconds
    pub position_ns: u64,
}

/// Request to go to a specific file.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GotoRequest {
    /// File index (0-based)
    pub index: usize,
}

/// Response with the current player state.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlayerStateResponse {
    /// Current playback state: "playing", "paused", "stopped"
    pub state: String,
    /// Current position in nanoseconds
    pub position_ns: u64,
    /// Total duration in nanoseconds
    pub duration_ns: u64,
    /// Current file index (0-based)
    pub current_file_index: usize,
    /// Total number of files in playlist
    pub total_files: usize,
    /// Current file path/URI
    pub current_file: Option<String>,
    /// Full playlist
    pub playlist: Vec<String>,
    /// Whether playlist loops
    pub loop_playlist: bool,
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

    let playlist = player
        .playlist
        .read()
        .map(|p| p.clone())
        .unwrap_or_default();

    Ok(Json(PlayerStateResponse {
        state: player.state_string(),
        position_ns: player.position().unwrap_or(0),
        duration_ns: player.duration().unwrap_or(0),
        current_file_index: player
            .current_index
            .load(std::sync::atomic::Ordering::SeqCst),
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
    Json(req): Json<SetPlaylistRequest>,
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
        player.set_playlist(req.files);

        // Load the first file if playlist is not empty
        if player.playlist_len() > 0 {
            let _ = player.goto(0);
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
    Json(req): Json<PlayerControlRequest>,
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

    let result = match req.action {
        PlayerAction::Play => player.play(),
        PlayerAction::Pause => player.pause(),
        PlayerAction::Stop => player.pause(), // TODO: Implement proper stop (pause + seek to beginning)
        PlayerAction::Next => player.next(),
        PlayerAction::Previous => player.previous(),
    };

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
    Json(req): Json<SeekRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let key = MediaPlayerKey {
        flow_id,
        block_id: block_id.clone(),
    };

    let player = MEDIA_PLAYER_REGISTRY.get(&key).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("Media player not found")),
    ))?;

    info!("Player {} seek to {} ns", block_id, req.position_ns);
    warn!(
        "Seek may not work correctly with live streaming outputs (sync=true). \
         This is a known limitation. See docs/MEDIAPLAYER_TEST_HARNESS.md for details."
    );

    // Seek is now scheduled on GLib main loop, so this returns immediately
    player.seek(req.position_ns).map_err(|e| {
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
    Json(req): Json<GotoRequest>,
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

    player.goto(req.index).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details("Goto failed", e)),
        )
    })?;

    Ok(StatusCode::OK)
}
