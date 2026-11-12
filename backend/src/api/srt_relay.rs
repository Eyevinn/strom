//! SRT relay status API.

use axum::{extract::State, Json};
use strom_types::srt_relay::SrtRelayInfo;

use crate::state::AppState;

/// Report the SRT relay that `srt+relay://<id>` URIs dial out to.
#[utoipa::path(
    get,
    path = "/api/srt-relay",
    tag = "SRT Relay",
    description = "Whether an SRT relay is configured, the SRT address Strom dials \
                   (`STROM_SRT_RELAY_INTERNAL` if set, else `STROM_SRT_RELAY`), and the public \
                   `host:port` the outside world dials with `streamid=<id>` (`STROM_SRT_RELAY`). \
                   Read from the configuration only; the relay is not contacted. A server \
                   without a relay answers `configured: false`.",
    responses(
        (status = 200, description = "SRT relay status, configured or not", body = SrtRelayInfo)
    )
)]
pub async fn get_srt_relay(State(state): State<AppState>) -> Json<SrtRelayInfo> {
    Json(state.srt_relay_info())
}
