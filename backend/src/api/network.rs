//! Network interface API endpoints.

use axum::Json;
use strom_types::NetworkInterfacesResponse;

use crate::network::discover_interfaces;

/// List all network interfaces on the system.
///
/// Returns information about all network interfaces including name, MAC address,
/// and IP addresses. Useful for selecting which interface to use for multicast
/// operations (e.g., AES67 streams).
#[utoipa::path(
    get,
    path = "/api/network/interfaces",
    tag = "Network",
    responses(
        (status = 200, description = "List of network interfaces", body = NetworkInterfacesResponse)
    )
)]
pub async fn list_interfaces() -> Json<NetworkInterfacesResponse> {
    Json(discover_interfaces())
}
