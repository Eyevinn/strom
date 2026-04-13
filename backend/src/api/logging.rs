//! Runtime log level API endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use strom_types::api::{ErrorResponse, LogLevelResponse, SetLogLevelRequest};
use tracing::info;

use crate::state::AppState;

/// Get the current log level filter
#[utoipa::path(
    get,
    path = "/api/log-level",
    tag = "System",
    responses(
        (status = 200, description = "Current log level", body = LogLevelResponse)
    )
)]
pub async fn get_log_level(State(state): State<AppState>) -> Json<LogLevelResponse> {
    Json(LogLevelResponse {
        current: state.current_log_filter(),
        default: state.default_log_filter(),
    })
}

/// Set the log level filter at runtime
#[utoipa::path(
    put,
    path = "/api/log-level",
    tag = "System",
    request_body = SetLogLevelRequest,
    responses(
        (status = 200, description = "Log level updated", body = LogLevelResponse),
        (status = 400, description = "Invalid filter string", body = ErrorResponse)
    )
)]
pub async fn set_log_level(
    State(state): State<AppState>,
    Json(req): Json<SetLogLevelRequest>,
) -> Result<Json<LogLevelResponse>, (StatusCode, Json<ErrorResponse>)> {
    let filter = req.filter.trim().to_string();
    if filter.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("Filter string must not be empty")),
        ));
    }

    state.reload_log_filter(&filter).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::with_details("Invalid log filter", e)),
        )
    })?;

    info!("Log filter changed to: {}", filter);

    Ok(Json(LogLevelResponse {
        current: state.current_log_filter(),
        default: state.default_log_filter(),
    }))
}
