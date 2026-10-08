//! Stinger transitions on a vision mixer: the library (the stinger source's
//! playlist and per-clip settings), cueing, taking, and the example clips.
//!
//! A take is also available through the transition endpoint with
//! `transition_type` "stinger".

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::stinger::{
    StingerAddClipRequest, StingerClip, StingerClipSettings, StingerCueRequest,
    StingerExamplesResponse, StingerFileGuard, StingerState, StingerTakeRequest,
    StingerTakeResponse,
};
use strom_types::FlowId;
use tracing::info;

use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

/// 409 when the library changed under the request, 400 otherwise.
pub(crate) fn error_response(what: &str, e: crate::gst::pipeline::PipelineError) -> ApiError {
    let status = match e {
        crate::gst::pipeline::PipelineError::Conflict(_) => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    (
        status,
        Json(ErrorResponse::with_details(what, e.to_string())),
    )
}

fn bad_request(what: &str) -> impl Fn(crate::gst::pipeline::PipelineError) -> ApiError + '_ {
    move |e| error_response(what, e)
}

/// Stinger state of a vision mixer: its source, clips (with analysis and how
/// each plays here), the cued clip, and the last take's measurements.
#[utoipa::path(
    get,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    responses(
        (status = 200, description = "Stinger state", body = StingerState),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn get_stinger_state(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<StingerState>, ApiError> {
    state
        .stinger_state(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(bad_request("Failed to read stinger state"))
}

/// Cue a stinger clip: load it and park it on its first frame, so a take
/// starts it without delay.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/cue",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = StingerCueRequest,
    responses(
        (status = 200, description = "Clip cued; the stinger state after the cue", body = StingerState),
        (status = 400, description = "The clip could not be cued", body = ErrorResponse),
        (status = 409, description = "The library changed: `file` is no longer at `index`", body = ErrorResponse),
    )
)]
pub async fn cue_stinger(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<StingerCueRequest>,
) -> Result<Json<StingerState>, ApiError> {
    info!(
        "Cueing stinger clip {} on vision mixer {} in flow {}",
        req.index, block_id, flow_id
    );
    state
        .stinger_cue(&flow_id, &block_id, req.index, req.file.as_deref())
        .await
        .map_err(bad_request("Failed to cue stinger"))?;
    state
        .stinger_state(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(bad_request("Failed to read stinger state"))
}

/// Reload the stinger library from disk: analyse clips whose files changed,
/// load and park the cued clip again when its file changed since it was
/// loaded, and report clips whose files are gone (`missing`). Analyses run in
/// the background; poll the stinger state for them. Refused while a stinger
/// is on air.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/reload",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    responses(
        (status = 200, description = "Library reloaded; the stinger state after the reload", body = StingerState),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 409, description = "A stinger is on air", body = ErrorResponse),
    )
)]
pub async fn reload_stinger(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<StingerState>, ApiError> {
    info!(
        "Reloading stinger library on vision mixer {} in flow {}",
        block_id, flow_id
    );
    state
        .stinger_reload(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(bad_request("Failed to reload stinger library"))
}

/// Take a stinger from PGM to PVW.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/take",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = StingerTakeRequest,
    responses(
        (status = 200, description = "Take started", body = StingerTakeResponse),
        (status = 400, description = "The take could not start", body = ErrorResponse),
        (status = 409, description = "The library changed: `file` is not the clip that would play", body = ErrorResponse),
    )
)]
pub async fn take_stinger(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<StingerTakeRequest>,
) -> Result<Json<StingerTakeResponse>, ApiError> {
    info!(
        "Stinger take on vision mixer {} in flow {} (clip {:?})",
        block_id, flow_id, req.index
    );
    state
        .stinger_take(&flow_id, &block_id, req.index, req.file.as_deref())
        .await
        .map(Json)
        .map_err(bad_request("Failed to take stinger"))
}

/// Store a stinger clip's settings (layout, cut point, mix, premultiplied
/// alpha, matte inversion) on the stinger source block.
#[utoipa::path(
    put,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/clips/{index}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID"),
        ("index" = usize, Path, description = "Playlist index on the stinger source"),
        ("file" = Option<String>, Query, description = "The file expected at `index`; 409 when the library has changed")
    ),
    request_body = StingerClipSettings,
    responses(
        (status = 200, description = "Settings stored; the clip as it now plays", body = StingerClip),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 409, description = "The library changed: `file` is no longer at `index`", body = ErrorResponse),
    )
)]
pub async fn set_stinger_clip_settings(
    State(state): State<AppState>,
    Path((flow_id, block_id, index)): Path<(FlowId, String, usize)>,
    Query(guard): Query<StingerFileGuard>,
    Json(settings): Json<StingerClipSettings>,
) -> Result<Json<StingerClip>, ApiError> {
    state
        .stinger_set_clip_settings(&flow_id, &block_id, index, guard.file.as_deref(), settings)
        .await
        .map(Json)
        .map_err(bad_request("Failed to store stinger clip settings"))
}

/// Render Strom's example stingers (classic, track matte, mask only) into the
/// media directory and add them to the stinger source's playlist.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/examples",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    responses(
        (status = 200, description = "Examples written", body = StingerExamplesResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn write_stinger_examples(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
) -> Result<Json<StingerExamplesResponse>, ApiError> {
    state
        .stinger_write_examples(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(bad_request("Failed to write example stingers"))
}

/// Add a clip to the stinger library (the stinger source's playlist). Adding
/// a file already in the library returns it unchanged. Upload the file first
/// with the media API when it is not on the server yet.
#[utoipa::path(
    post,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/clips",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID")
    ),
    request_body = StingerAddClipRequest,
    responses(
        (status = 200, description = "The clip, as it plays here", body = StingerClip),
        (status = 400, description = "Invalid request, or no such file", body = ErrorResponse),
    )
)]
pub async fn add_stinger_clip(
    State(state): State<AppState>,
    Path((flow_id, block_id)): Path<(FlowId, String)>,
    Json(req): Json<StingerAddClipRequest>,
) -> Result<Json<StingerClip>, ApiError> {
    info!(
        "Adding stinger clip {} on vision mixer {} in flow {}",
        req.file, block_id, flow_id
    );
    state
        .stinger_add_clip(&flow_id, &block_id, &req.file, req.settings)
        .await
        .map(Json)
        .map_err(bad_request("Failed to add stinger clip"))
}

/// Remove a clip from the stinger library, with its settings. The file stays
/// in the media directory. Refused while a stinger is on air.
#[utoipa::path(
    delete,
    path = "/api/flows/{flow_id}/blocks/{block_id}/stinger/clips/{index}",
    tag = "flows",
    params(
        ("flow_id" = String, Path, description = "Flow ID (UUID)"),
        ("block_id" = String, Path, description = "Vision mixer block instance ID"),
        ("index" = usize, Path, description = "Playlist index on the stinger source"),
        ("file" = Option<String>, Query, description = "The file expected at `index`; 409 when the library has changed")
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 409, description = "The library changed: `file` is no longer at `index`", body = ErrorResponse),
    )
)]
pub async fn remove_stinger_clip(
    State(state): State<AppState>,
    Path((flow_id, block_id, index)): Path<(FlowId, String, usize)>,
    Query(guard): Query<StingerFileGuard>,
) -> Result<StatusCode, ApiError> {
    info!(
        "Removing stinger clip {} on vision mixer {} in flow {}",
        index, block_id, flow_id
    );
    state
        .stinger_remove_clip(&flow_id, &block_id, index, guard.file.as_deref())
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(bad_request("Failed to remove stinger clip"))
}
