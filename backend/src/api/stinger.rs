//! Stinger transitions on a vision mixer: the library (the stinger source's
//! playlist and per-clip settings), cueing, taking, and the example clips.
//!
//! A take is also available through the transition endpoint with
//! `transition_type` "stinger".

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use strom_types::api::ErrorResponse;
use strom_types::stinger::{
    StingerClip, StingerClipSettings, StingerCueRequest, StingerExamplesResponse, StingerState,
    StingerTakeRequest, StingerTakeResponse,
};
use strom_types::FlowId;
use tracing::info;

use crate::state::AppState;

type ApiError = (StatusCode, Json<ErrorResponse>);

fn bad_request(what: &str) -> impl Fn(crate::gst::pipeline::PipelineError) -> ApiError + '_ {
    move |e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details(what, e.to_string())),
        )
    }
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
        .stinger_cue(&flow_id, &block_id, req.index)
        .await
        .map_err(bad_request("Failed to cue stinger"))?;
    state
        .stinger_state(&flow_id, &block_id)
        .await
        .map(Json)
        .map_err(bad_request("Failed to read stinger state"))
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
        .stinger_take(&flow_id, &block_id, req.index)
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
        ("index" = usize, Path, description = "Playlist index on the stinger source")
    ),
    request_body = StingerClipSettings,
    responses(
        (status = 200, description = "Settings stored; the clip as it now plays", body = StingerClip),
        (status = 400, description = "Invalid request", body = ErrorResponse),
    )
)]
pub async fn set_stinger_clip_settings(
    State(state): State<AppState>,
    Path((flow_id, block_id, index)): Path<(FlowId, String, usize)>,
    Json(settings): Json<StingerClipSettings>,
) -> Result<Json<StingerClip>, ApiError> {
    state
        .stinger_set_clip_settings(&flow_id, &block_id, index, settings)
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
