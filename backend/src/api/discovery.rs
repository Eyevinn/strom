//! API endpoints for AES67 stream discovery and device discovery.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::discovery::{DeviceCategory, DeviceResponse, DiscoveredStreamResponse};
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

// --- Device Discovery Endpoints ---

/// Device discovery status response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeviceDiscoveryStatus {
    /// Whether device discovery is running.
    pub running: bool,
    /// Whether NDI device provider is available.
    pub ndi_available: bool,
    /// Total number of discovered devices.
    pub device_count: usize,
    /// Number of devices by category.
    pub by_category: DeviceCountByCategory,
}

/// Device counts by category.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DeviceCountByCategory {
    pub audio_source: usize,
    pub audio_sink: usize,
    pub video_source: usize,
    pub network_source: usize,
    pub other: usize,
}

/// Get device discovery status.
#[utoipa::path(
    get,
    path = "/api/discovery/devices/status",
    responses(
        (status = 200, description = "Device discovery status", body = DeviceDiscoveryStatus),
    ),
    tag = "discovery"
)]
pub async fn device_status(State(state): State<AppState>) -> impl IntoResponse {
    let devices = state.discovery().get_devices().await;
    let ndi_available = state.discovery().is_ndi_available().await;

    let mut by_category = DeviceCountByCategory {
        audio_source: 0,
        audio_sink: 0,
        video_source: 0,
        network_source: 0,
        other: 0,
    };

    for device in &devices {
        match device.category {
            DeviceCategory::AudioSource => by_category.audio_source += 1,
            DeviceCategory::AudioSink => by_category.audio_sink += 1,
            DeviceCategory::VideoSource => by_category.video_source += 1,
            DeviceCategory::NetworkSource => by_category.network_source += 1,
            DeviceCategory::Other => by_category.other += 1,
        }
    }

    Json(DeviceDiscoveryStatus {
        running: !devices.is_empty() || ndi_available,
        ndi_available,
        device_count: devices.len(),
        by_category,
    })
}

/// Query parameters for device listing.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceListQuery {
    /// Filter by category (audio_source, audio_sink, video_source, network_source).
    pub category: Option<String>,
}

/// List all discovered devices.
#[utoipa::path(
    get,
    path = "/api/discovery/devices",
    params(
        ("category" = Option<String>, Query, description = "Filter by category")
    ),
    responses(
        (status = 200, description = "List of discovered devices", body = Vec<DeviceResponse>),
    ),
    tag = "discovery"
)]
pub async fn list_devices(
    State(state): State<AppState>,
    Query(query): Query<DeviceListQuery>,
) -> impl IntoResponse {
    let devices = if let Some(category_str) = query.category {
        let category = match category_str.as_str() {
            "audio_source" | "audiosource" => Some(DeviceCategory::AudioSource),
            "audio_sink" | "audiosink" => Some(DeviceCategory::AudioSink),
            "video_source" | "videosource" => Some(DeviceCategory::VideoSource),
            "network_source" | "networksource" | "ndi" => Some(DeviceCategory::NetworkSource),
            _ => None,
        };

        if let Some(cat) = category {
            state.discovery().get_devices_by_category(cat).await
        } else {
            state.discovery().get_devices().await
        }
    } else {
        state.discovery().get_devices().await
    };

    let responses: Vec<DeviceResponse> = devices.iter().map(|d| d.to_api_response()).collect();
    Json(responses)
}

/// Get a specific device by ID.
#[utoipa::path(
    get,
    path = "/api/discovery/devices/{id}",
    params(
        ("id" = String, Path, description = "Device ID")
    ),
    responses(
        (status = 200, description = "Device details", body = DeviceResponse),
        (status = 404, description = "Device not found"),
    ),
    tag = "discovery"
)]
pub async fn get_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.discovery().get_device(&id).await {
        Some(device) => Ok(Json(device.to_api_response())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Refresh discovered devices (trigger re-scan).
#[utoipa::path(
    post,
    path = "/api/discovery/devices/refresh",
    responses(
        (status = 200, description = "Devices refreshed"),
    ),
    tag = "discovery"
)]
pub async fn refresh_devices(State(state): State<AppState>) -> impl IntoResponse {
    state.discovery().refresh_devices().await;
    StatusCode::OK
}

// --- NDI Discovery Endpoints (backward compatibility) ---

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
        (status = 200, description = "List of discovered NDI sources", body = Vec<DeviceResponse>),
    ),
    tag = "discovery"
)]
pub async fn list_ndi_sources(State(state): State<AppState>) -> impl IntoResponse {
    let sources = state.discovery().get_ndi_sources().await;
    let responses: Vec<DeviceResponse> = sources.iter().map(|s| s.to_api_response()).collect();
    Json(responses)
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
    state.discovery().refresh_devices().await;
    StatusCode::OK
}
