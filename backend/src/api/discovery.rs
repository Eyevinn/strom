//! API endpoints for AES67 stream discovery.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::discovery::DiscoveredStreamResponse;
use crate::state::AppState;

/// List all discovered AES67 streams.
#[utoipa::path(
    get,
    path = "/api/discovery/streams",
    responses(
        (status = 200, description = "List of discovered streams", body = Vec<DiscoveredStreamResponse>),
    ),
    tag = "discovery"
)]
pub async fn list_streams(State(state): State<AppState>) -> impl IntoResponse {
    let streams = state.discovery().get_streams().await;
    let responses: Vec<DiscoveredStreamResponse> =
        streams.iter().map(|s| s.to_api_response()).collect();
    Json(responses)
}

/// Get a specific discovered stream by ID.
#[utoipa::path(
    get,
    path = "/api/discovery/streams/{id}",
    params(
        ("id" = String, Path, description = "Stream ID")
    ),
    responses(
        (status = 200, description = "Stream details", body = DiscoveredStreamResponse),
        (status = 404, description = "Stream not found"),
    ),
    tag = "discovery"
)]
pub async fn get_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.discovery().get_stream(&id).await {
        Some(stream) => Ok(Json(stream.to_api_response())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Get the raw SDP for a discovered stream.
#[utoipa::path(
    get,
    path = "/api/discovery/streams/{id}/sdp",
    params(
        ("id" = String, Path, description = "Stream ID")
    ),
    responses(
        (status = 200, description = "SDP content", body = String, content_type = "application/sdp"),
        (status = 404, description = "Stream not found"),
    ),
    tag = "discovery"
)]
pub async fn get_stream_sdp(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.discovery().get_stream_sdp(&id).await {
        Some(sdp) => Ok(([(axum::http::header::CONTENT_TYPE, "application/sdp")], sdp)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Response for announced streams list.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnnouncedStreamResponse {
    pub flow_id: String,
    pub block_id: String,
    pub origin_ip: String,
    pub sdp: String,
}

/// List streams being announced by this Strom instance.
#[utoipa::path(
    get,
    path = "/api/discovery/announced",
    responses(
        (status = 200, description = "List of announced streams", body = Vec<AnnouncedStreamResponse>),
    ),
    tag = "discovery"
)]
pub async fn list_announced(State(state): State<AppState>) -> impl IntoResponse {
    let streams = state.discovery().get_announced_streams().await;
    let responses: Vec<AnnouncedStreamResponse> = streams
        .iter()
        .map(|s| AnnouncedStreamResponse {
            flow_id: s.flow_id.to_string(),
            block_id: s.block_id.clone(),
            origin_ip: s.origin_ip.to_string(),
            sdp: s.sdp.clone(),
        })
        .collect();
    Json(responses)
}
