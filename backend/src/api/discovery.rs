//! API endpoints for AES67 stream discovery and NDI source discovery.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::discovery::{DiscoveredStreamResponse, NdiSourceResponse};
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
    /// Network interface the stream is announced on (None = all interfaces).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_interface: Option<String>,
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
            announce_interface: s.announce_interface.clone(),
        })
        .collect();
    Json(responses)
}

// --- NDI Discovery Endpoints ---

/// NDI discovery status response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NdiDiscoveryStatus {
    /// Whether NDI discovery is available (plugin installed).
    pub available: bool,
    /// Number of discovered NDI sources.
    pub source_count: usize,
}

/// Get NDI discovery status.
#[utoipa::path(
    get,
    path = "/api/discovery/ndi/status",
    responses(
        (status = 200, description = "NDI discovery status", body = NdiDiscoveryStatus),
    ),
    tag = "discovery"
)]
pub async fn ndi_status(State(state): State<AppState>) -> impl IntoResponse {
    let available = state.discovery().is_ndi_available().await;
    let sources = state.discovery().get_ndi_sources().await;
    Json(NdiDiscoveryStatus {
        available,
        source_count: sources.len(),
    })
}

/// List all discovered NDI sources.
#[utoipa::path(
    get,
    path = "/api/discovery/ndi/sources",
    responses(
        (status = 200, description = "List of discovered NDI sources", body = Vec<NdiSourceResponse>),
    ),
    tag = "discovery"
)]
pub async fn list_ndi_sources(State(state): State<AppState>) -> impl IntoResponse {
    let sources = state.discovery().get_ndi_sources().await;
    let responses: Vec<NdiSourceResponse> = sources.iter().map(|s| s.to_api_response()).collect();
    Json(responses)
}

/// Get a specific NDI source by ID.
#[utoipa::path(
    get,
    path = "/api/discovery/ndi/sources/{id}",
    params(
        ("id" = String, Path, description = "NDI Source ID")
    ),
    responses(
        (status = 200, description = "NDI source details", body = NdiSourceResponse),
        (status = 404, description = "NDI source not found"),
    ),
    tag = "discovery"
)]
pub async fn get_ndi_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.discovery().get_ndi_source(&id).await {
        Some(source) => Ok(Json(source.to_api_response())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Refresh NDI sources (trigger re-scan).
#[utoipa::path(
    post,
    path = "/api/discovery/ndi/refresh",
    responses(
        (status = 200, description = "NDI sources refreshed"),
    ),
    tag = "discovery"
)]
pub async fn refresh_ndi_sources(State(state): State<AppState>) -> impl IntoResponse {
    state.discovery().refresh_ndi_sources().await;
    StatusCode::OK
}
