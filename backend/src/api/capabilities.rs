//! Capabilities API: what this Strom can do on the network it runs on.

use axum::{extract::State, Json};
use strom_types::capabilities::Capabilities;

use crate::state::AppState;

/// Get what this Strom can do on the network it runs on
///
/// Reports whether locally bound ports are reachable from outside, the ports
/// meant for remote parties to connect to, and per protocol which connection
/// modes can work. Clients use it to offer only what works, for example SRT
/// caller instead of listener on a Strom behind NAT. It never exposes ICE/TURN
/// URLs, credentials or internal addresses.
#[utoipa::path(
    get,
    path = "/api/capabilities",
    tag = "System",
    responses(
        (status = 200, description = "Capabilities of this Strom", body = Capabilities)
    )
)]
pub async fn get_capabilities(State(state): State<AppState>) -> Json<Capabilities> {
    Json(state.capabilities().await)
}
