//! Network interface types for the Strom GStreamer flow engine.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Information about a network interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NetworkInterfaceInfo {
    /// Interface name (e.g., "eth0", "enp0s3")
    pub name: String,

    /// Interface index
    pub index: u32,

    /// MAC address (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,

    /// IPv4 addresses assigned to this interface
    pub ipv4_addresses: Vec<Ipv4AddressInfo>,

    /// IPv6 addresses assigned to this interface
    pub ipv6_addresses: Vec<Ipv6AddressInfo>,

    /// Whether this interface is a loopback interface
    pub is_loopback: bool,

    /// Whether this interface appears to be up (has addresses)
    pub is_up: bool,
}

/// IPv4 address information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Ipv4AddressInfo {
    /// The IPv4 address
    pub address: String,

    /// Network mask (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,

    /// Broadcast address (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broadcast: Option<String>,
}

/// IPv6 address information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Ipv6AddressInfo {
    /// The IPv6 address
    pub address: String,

    /// Network mask (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netmask: Option<String>,
}

/// Response containing list of network interfaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NetworkInterfacesResponse {
    /// List of network interfaces
    pub interfaces: Vec<NetworkInterfaceInfo>,
}
