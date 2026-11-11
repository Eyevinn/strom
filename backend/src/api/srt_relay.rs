//! SRT relay status API.

use axum::{extract::State, Json};
use strom_types::srt_relay::SrtRelayInfo;

use crate::state::AppState;

/// Report the SRT relay that `srt+relay://<id>` URIs dial out to.
#[utoipa::path(
    get,
    path = "/api/srt-relay",
    tag = "SRT Relay",
    description = "Whether an SRT relay is configured, its SRT address as Strom reaches it, \
                   and the public `host:port` the outside world dials (with `streamid=<id>`). \
                   `public` comes from `STROM_SRT_RELAY_PUBLIC` or is asked from the relay \
                   and cached; when it is not known, `error` says why. A server without a \
                   relay answers `configured: false`.",
    responses(
        (status = 200, description = "SRT relay status, configured or not", body = SrtRelayInfo)
    )
)]
pub async fn get_srt_relay(State(state): State<AppState>) -> Json<SrtRelayInfo> {
    Json(state.srt_relay_info().await)
}
