//! Network interface discovery.

use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use strom_types::{
    Ipv4AddressInfo, Ipv6AddressInfo, NetworkInterfaceInfo, NetworkInterfacesResponse,
};
use tracing::warn;

/// Discover all network interfaces on the system.
pub fn discover_interfaces() -> NetworkInterfacesResponse {
    let interfaces = match NetworkInterface::show() {
        Ok(interfaces) => interfaces,
        Err(e) => {
            warn!("Failed to discover network interfaces: {}", e);
            return NetworkInterfacesResponse {
                interfaces: Vec::new(),
            };
        }
    };

    let mut result = Vec::new();

    for iface in interfaces {
        let mut ipv4_addresses = Vec::new();
        let mut ipv6_addresses = Vec::new();

        for addr in &iface.addr {
            match addr {
                Addr::V4(v4) => {
                    ipv4_addresses.push(Ipv4AddressInfo {
                        address: v4.ip.to_string(),
                        netmask: v4.netmask.map(|n| n.to_string()),
                        broadcast: v4.broadcast.map(|b| b.to_string()),
                    });
                }
                Addr::V6(v6) => {
                    ipv6_addresses.push(Ipv6AddressInfo {
                        address: v6.ip.to_string(),
                        netmask: v6.netmask.map(|n| n.to_string()),
                    });
                }
            }
        }

        // Check if this is a loopback interface
        let is_loopback = iface.name == "lo"
            || iface.name.starts_with("lo")
            || ipv4_addresses.iter().any(|a| a.address == "127.0.0.1")
            || ipv6_addresses.iter().any(|a| a.address == "::1");

        // Consider interface "up" if it has any addresses
        let is_up = !ipv4_addresses.is_empty() || !ipv6_addresses.is_empty();

        result.push(NetworkInterfaceInfo {
            name: iface.name,
            index: iface.index,
            mac_address: iface.mac_addr,
            ipv4_addresses,
            ipv6_addresses,
            is_loopback,
            is_up,
        });
    }

    // Sort by index for consistent ordering
    result.sort_by_key(|i| i.index);

    NetworkInterfacesResponse { interfaces: result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_interfaces() {
        let response = discover_interfaces();
        // Should at least find loopback
        assert!(
            !response.interfaces.is_empty(),
            "Should discover at least one interface"
        );

        // Should have a loopback interface on most systems
        let has_loopback = response.interfaces.iter().any(|i| i.is_loopback);
        assert!(has_loopback, "Should have a loopback interface");
    }
}
