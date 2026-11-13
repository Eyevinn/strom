//! What this Strom can do on the network it runs on (#1257).
//!
//! Whether a locally bound port is reachable from outside is stated by the
//! operator, not detected. Each protocol's mode availability is derived from
//! it in [`ReachabilitySettings::inbound_reachable_for`], the one place a
//! per-protocol override would go.

use std::collections::BTreeSet;

use strom_types::capabilities::{
    Capabilities, NetworkCapabilities, ProtocolCapabilities, RelayEndpoint, SrtCapabilities,
    SrtModes, WebRtcCapabilities,
};
use strom_types::ports::PortSpan;

/// A protocol where a remote party may connect to a port Strom binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundProtocol {
    Srt,
}

/// What the operator stated about inbound reachability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReachabilitySettings {
    /// `[network] inbound_reachable` / `STROM_INBOUND_REACHABLE`.
    pub inbound_reachable: bool,
}

impl Default for ReachabilitySettings {
    fn default() -> Self {
        Self {
            inbound_reachable: true,
        }
    }
}

impl ReachabilitySettings {
    /// Whether a remote party can reach a port Strom binds for `protocol`.
    ///
    /// Today this is the global setting for every protocol.
    pub fn inbound_reachable_for(&self, protocol: InboundProtocol) -> bool {
        match protocol {
            InboundProtocol::Srt => self.inbound_reachable,
        }
    }

    /// Which SRT modes can work. Caller always can; listener and rendezvous
    /// need the peer to reach a locally bound port.
    pub fn srt_modes(&self) -> SrtModes {
        let inbound = self.inbound_reachable_for(InboundProtocol::Srt);
        SrtModes {
            listener: inbound,
            caller: true,
            rendezvous: inbound,
        }
    }
}

/// The configured ports as runs, `None` when none are configured.
fn port_spans(ports: &BTreeSet<u16>) -> Option<Vec<PortSpan>> {
    if ports.is_empty() {
        return None;
    }
    Some(strom_types::ports::spans(ports.iter().copied()))
}

/// Build the capabilities report. Takes only what it may expose: never pass
/// ICE URLs, credentials or internal relay addresses through here, only
/// whether ICE servers are configured and each relay's name and public
/// address (`crate::relay::Relays::endpoints`).
pub fn build(
    reachability: ReachabilitySettings,
    ports: &BTreeSet<u16>,
    srt_relays: Vec<RelayEndpoint>,
    has_ice_servers: bool,
    ice_transport_policy: &str,
) -> Capabilities {
    Capabilities {
        network: NetworkCapabilities {
            inbound_reachable: reachability.inbound_reachable,
            ports: port_spans(ports),
        },
        protocols: ProtocolCapabilities {
            srt: SrtCapabilities {
                modes: reachability.srt_modes(),
                relays: srt_relays,
            },
            webrtc: WebRtcCapabilities {
                ice_servers: has_ice_servers,
                ice_transport_policy: ice_transport_policy.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_by_default_allows_every_srt_mode() {
        let caps = build(
            ReachabilitySettings::default(),
            &BTreeSet::new(),
            Vec::new(),
            false,
            "all",
        );
        assert!(caps.network.inbound_reachable);
        assert_eq!(caps.network.ports, None);
        assert_eq!(
            caps.protocols.srt.modes,
            SrtModes {
                listener: true,
                caller: true,
                rendezvous: true
            }
        );
        assert!(caps.protocols.srt.relays.is_empty());
    }

    #[test]
    fn unreachable_allows_only_caller() {
        let caps = build(
            ReachabilitySettings {
                inbound_reachable: false,
            },
            &BTreeSet::new(),
            Vec::new(),
            true,
            "relay",
        );
        assert!(!caps.network.inbound_reachable);
        assert_eq!(
            caps.protocols.srt.modes,
            SrtModes {
                listener: false,
                caller: true,
                rendezvous: false
            }
        );
        assert!(caps.protocols.webrtc.ice_servers);
        assert_eq!(caps.protocols.webrtc.ice_transport_policy, "relay");
    }

    #[test]
    fn ports_are_reported_as_ranges() {
        let ports: BTreeSet<u16> = (47100..=47199).chain([47250]).collect();
        let caps = build(
            ReachabilitySettings::default(),
            &ports,
            Vec::new(),
            false,
            "all",
        );
        assert_eq!(
            caps.network.ports,
            Some(vec![
                PortSpan {
                    first: 47100,
                    last: 47199
                },
                PortSpan {
                    first: 47250,
                    last: 47250
                },
            ])
        );
    }
}
