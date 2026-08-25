//! API handlers.

pub mod blocks;
pub mod discovery;
pub mod elements;
pub mod flows;
pub mod gst_launch;
pub mod logging;
pub mod mcp;
pub mod media;
pub mod mediaplayer;
pub mod network;
pub mod osc;
pub mod probes;
pub mod sdp_transform;
pub mod system_clock;
pub mod version;
pub mod vision_mixer_page;
pub mod websocket;
pub mod whep_player;
pub mod whip_ingest;

use axum::{
    extract::OriginalUri,
    http::{Method, StatusCode},
    Json,
};
use strom_types::api::ErrorResponse;

/// Fallback for unmatched `/api/*` paths.
///
/// Without this the top-level SPA fallback answers with `200 OK` and the frontend
/// HTML document, which makes a missing route look like a successful call to any
/// client that only checks the status code.
pub async fn not_found(
    method: Method,
    OriginalUri(uri): OriginalUri,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::with_details(
            "API endpoint not found",
            format!("No API route matches {} {}", method, uri.path()),
        )),
    )
}
