//! OSC (Eyevinn Open Source Cloud) authentication API endpoints.
//!
//! Lets a control plane (e.g. Open Live) push the OSC Personal Access Token to a
//! running Strom instance instead of baking it into an env var. The PAT is held
//! in memory only and used to mint per-service Service Access Tokens — see
//! [`crate::osc`]. These routes sit behind the standard auth middleware.

use axum::http::StatusCode;
use axum::Json;
use strom_types::api::{ErrorResponse, OscAuthStatusResponse, SetOscPatRequest};
use tracing::info;

/// Get the OSC PAT configuration status
///
/// Reports whether a Personal Access Token is currently configured (via the
/// `STROM_OSC_PAT` / `OSC_ACCESS_TOKEN` env var or a previous API call). The
/// token value itself is never returned.
#[utoipa::path(
    get,
    path = "/api/osc/pat",
    tag = "System",
    responses(
        (status = 200, description = "OSC PAT status", body = OscAuthStatusResponse)
    )
)]
pub async fn get_osc_pat_status() -> Json<OscAuthStatusResponse> {
    Json(OscAuthStatusResponse {
        configured: crate::osc::sat_provider().has_pat(),
    })
}

/// Set the OSC Personal Access Token at runtime
///
/// Stores the PAT in memory (not persisted) and clears any cached Service Access
/// Tokens so the next request mints from the new PAT.
#[utoipa::path(
    put,
    path = "/api/osc/pat",
    tag = "System",
    request_body = SetOscPatRequest,
    responses(
        (status = 200, description = "PAT updated", body = OscAuthStatusResponse),
        (status = 400, description = "Empty token", body = ErrorResponse)
    )
)]
pub async fn set_osc_pat(
    Json(req): Json<SetOscPatRequest>,
) -> Result<Json<OscAuthStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let pat = req.pat.trim().to_string();
    if pat.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("PAT must not be empty")),
        ));
    }

    crate::osc::sat_provider().set_pat(pat).await;
    info!("OSC PAT set via API");

    Ok(Json(OscAuthStatusResponse { configured: true }))
}

/// Clear the OSC Personal Access Token
///
/// Forgets the in-memory PAT and all cached Service Access Tokens.
#[utoipa::path(
    delete,
    path = "/api/osc/pat",
    tag = "System",
    responses(
        (status = 200, description = "PAT cleared", body = OscAuthStatusResponse)
    )
)]
pub async fn clear_osc_pat() -> Json<OscAuthStatusResponse> {
    crate::osc::sat_provider().clear_pat().await;
    info!("OSC PAT cleared via API");
    Json(OscAuthStatusResponse { configured: false })
}
