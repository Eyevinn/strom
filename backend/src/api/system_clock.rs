//! System clock synchronization API endpoint.

use axum::http::StatusCode;
use axum::Json;
use strom_types::api::SystemClockInfo;

/// Get system clock synchronization state
///
/// Returns the current kernel time-discipline state as reported by
/// `ntp_adjtime(2)`. This includes the TAI-UTC offset, PLL status,
/// current offset being applied, and frequency adjustment — the same
/// information used by chrony, ntpd and systemd-timesyncd.
///
/// Useful for diagnosing flows that use `Realtime` or `Tai` pipeline
/// clocks, where the accuracy depends on how the OS clock is being
/// synchronized.
#[utoipa::path(
    get,
    path = "/api/system/clock",
    tag = "System",
    responses(
        (status = 200, description = "System clock state", body = SystemClockInfo),
        (status = 500, description = "Failed to read system clock state")
    )
)]
pub async fn get_system_clock() -> Result<Json<SystemClockInfo>, StatusCode> {
    use crate::system_clock::SystemClockError;
    crate::system_clock::read_system_clock_info()
        .map(Json)
        .map_err(|e| match e {
            SystemClockError::Unsupported => {
                tracing::debug!("System clock info unsupported on this platform");
                StatusCode::NOT_IMPLEMENTED
            }
            SystemClockError::Io(err) => {
                tracing::error!("Failed to read system clock info: {}", err);
                StatusCode::INTERNAL_SERVER_ERROR
            }
        })
}
