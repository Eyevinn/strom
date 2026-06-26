//! OSC (Eyevinn Open Source Cloud) authentication API endpoints.
//!
//! Lets a control plane (e.g. Open Live) push OSC Personal Access Tokens to a
//! running Strom instance instead of baking them into an env var. PATs are held
//! in memory only and used to mint per-service Service Access Tokens — see
//! [`crate::osc`]. These routes sit behind the standard auth middleware.
//!
//! The write API is **per credential key** only (`PUT/DELETE /api/osc/pat/{key}`,
//! key = flow id), so a control plane can always push per-flow without knowing
//! whether the instance is shared. The instance-wide default PAT is bootstrap-only
//! (the `STROM_OSC_PAT` env var) and acts as a fallback for the single-tenant case;
//! `GET /api/osc/pat` reports status.

use axum::extract::Path;
use axum::http::StatusCode;
use axum::Json;
use strom_types::api::{ErrorResponse, OscAuthStatusResponse, SetOscPatRequest};
use tracing::info;

fn status() -> OscAuthStatusResponse {
    let p = crate::osc::sat_provider();
    OscAuthStatusResponse {
        configured: p.has_default_pat(),
        keys: p.configured_keys(),
    }
}

/// Get the OSC PAT configuration status
///
/// Reports whether the default PAT is configured and which per-flow credential
/// keys have a PAT registered. Token values are never returned.
#[utoipa::path(
    get,
    path = "/api/osc/pat",
    tag = "System",
    responses(
        (status = 200, description = "OSC PAT status", body = OscAuthStatusResponse)
    )
)]
pub async fn get_osc_pat_status() -> Json<OscAuthStatusResponse> {
    Json(status())
}

/// Set a per-flow OSC Personal Access Token
///
/// Registers a PAT for one credential key (the flow id). Flows resolve their own
/// PAT first, falling back to the default — isolating OSC tenants on a shared
/// instance. Held in memory only.
#[utoipa::path(
    put,
    path = "/api/osc/pat/{key}",
    tag = "System",
    params(("key" = String, Path, description = "Credential key (flow id) the PAT is scoped to")),
    request_body = SetOscPatRequest,
    responses(
        (status = 200, description = "PAT updated", body = OscAuthStatusResponse),
        (status = 400, description = "Empty token", body = ErrorResponse)
    )
)]
pub async fn set_osc_pat_keyed(
    Path(key): Path<String>,
    Json(req): Json<SetOscPatRequest>,
) -> Result<Json<OscAuthStatusResponse>, (StatusCode, Json<ErrorResponse>)> {
    let pat = req.pat.trim().to_string();
    if pat.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("PAT must not be empty")),
        ));
    }
    crate::osc::sat_provider().set_pat(key.clone(), pat).await;
    info!("OSC PAT set via API for credential {}", key);
    Ok(Json(status()))
}

/// Clear a per-flow OSC Personal Access Token
#[utoipa::path(
    delete,
    path = "/api/osc/pat/{key}",
    tag = "System",
    params(("key" = String, Path, description = "Credential key (flow id) to clear")),
    responses(
        (status = 200, description = "PAT cleared", body = OscAuthStatusResponse)
    )
)]
pub async fn clear_osc_pat_keyed(Path(key): Path<String>) -> Json<OscAuthStatusResponse> {
    crate::osc::sat_provider().clear_pat(&key).await;
    info!("OSC PAT cleared via API for credential {}", key);
    Json(status())
}
