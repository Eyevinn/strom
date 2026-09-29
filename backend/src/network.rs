//! Network interface discovery.

use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use std::net::Ipv4Addr;
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

/// Get the IPv4 address for a specific network interface by name.
/// Returns None if the interface doesn't exist or has no IPv4 address.
pub fn get_interface_ipv4(interface_name: &str) -> Option<Ipv4Addr> {
    let interfaces = NetworkInterface::show().ok()?;

    for iface in interfaces {
        if iface.name == interface_name {
            for addr in iface.addr {
                if let Addr::V4(v4) = addr {
                    let ip = v4.ip;
                    // Skip loopback and link-local addresses
                    if !ip.is_loopback() && !ip.is_link_local() {
                        return Some(ip);
                    }
                }
            }
        }
    }

    None
}

/// Get the source IPv4 address that the kernel would use to reach the given destination.
///
/// This uses the UDP connect() + getsockname() trick: by "connecting" a UDP socket
/// to the destination (no actual packets are sent), we can ask the kernel which
/// local address it would use based on the routing table.
///
/// This is the correct way to determine the source IP for multicast - the kernel
/// will select the appropriate interface based on multicast routes or the default gateway.
pub fn get_source_ipv4_for_destination(dest: &str) -> Option<Ipv4Addr> {
    use std::net::UdpSocket;

    // Create an unbound UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;

    // "Connect" to the destination - this doesn't send any packets for UDP,
    // but it does cause the kernel to select a source address based on routing
    let dest_with_port = if dest.contains(':') {
        dest.to_string()
    } else {
        format!("{}:9", dest) // Use discard port, doesn't matter for UDP connect
    };
    socket.connect(&dest_with_port).ok()?;

    // Get the local address the kernel selected
    let local_addr = socket.local_addr().ok()?;

    match local_addr.ip() {
        std::net::IpAddr::V4(ipv4) => Some(ipv4),
        std::net::IpAddr::V6(_) => None,
    }
}

/// Get the default local IPv4 address by asking the kernel which address it would
/// use to reach a public IP (8.8.8.8). This is more reliable than iterating
/// interfaces, as it respects the routing table.
///
/// Falls back to interface iteration if the routing query fails.
pub fn get_default_ipv4() -> Option<Ipv4Addr> {
    // First, try to get the source IP for a well-known destination
    // This respects the routing table and default gateway
    if let Some(ip) = get_source_ipv4_for_destination("8.8.8.8") {
        return Some(ip);
    }

    // Fallback: iterate interfaces (old behavior)
    let interfaces = NetworkInterface::show().ok()?;

    for iface in interfaces {
        // Skip loopback interfaces
        if iface.name.starts_with("lo") {
            continue;
        }

        for addr in iface.addr {
            if let Addr::V4(v4) = addr {
                let ip = v4.ip;
                // Skip loopback and link-local addresses
                if !ip.is_loopback() && !ip.is_link_local() {
                    return Some(ip);
                }
            }
        }
    }

    None
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

    #[test]
    fn test_get_source_ipv4_for_destination() {
        // The loopback route exists on every host, with or without a network,
        // so the kernel's answer is fixed. Both the bare-address and the
        // address:port forms must work.
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        assert_eq!(get_source_ipv4_for_destination("127.0.0.1"), Some(loopback));
        assert_eq!(
            get_source_ipv4_for_destination("127.0.0.1:5004"),
            Some(loopback)
        );
    }

    #[test]
    fn test_get_interface_ipv4_skips_loopback_and_unknown() {
        assert_eq!(get_interface_ipv4("no-such-interface-xyz"), None);

        let response = discover_interfaces();
        let lo = response
            .interfaces
            .iter()
            .find(|i| i.is_loopback)
            .expect("a loopback interface");
        assert_eq!(
            get_interface_ipv4(&lo.name),
            None,
            "loopback address must not be returned for {}",
            lo.name
        );
    }
}
