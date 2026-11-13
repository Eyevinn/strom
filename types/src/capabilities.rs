//! What this Strom can do on the network it runs on (`GET /api/capabilities`).
//!
//! Whether a port Strom binds can be reached from outside depends on how it is
//! deployed (a VM or host networking: yes; Kubernetes or a NAT'd container:
//! no), and cannot be detected reliably at runtime. The operator states it
//! (`[network] inbound_reachable`), and Strom reports per protocol which
//! connection modes work, so clients offer only those (#1257).
//!
//! The shape is meant to grow: a new protocol adds a key under `protocols`
//! with the same sub-shape as SRT (`modes`, `relays`). Nothing here exposes an
//! address, an ICE/TURN URL or a credential.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

use crate::api::SrtMode;
use crate::ports::PortSpan;

/// Everything `GET /api/capabilities` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Capabilities {
    /// The host and network Strom runs on.
    pub network: NetworkCapabilities,
    /// Per-protocol capabilities.
    pub protocols: ProtocolCapabilities,
}

/// The host and network Strom runs on, independent of any protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NetworkCapabilities {
    /// Whether a port Strom binds locally can be reached from outside.
    pub inbound_reachable: bool,
    /// The ports meant for remote parties to connect to (`STROM_PORTS`), as
    /// ascending runs (`{"first": 47100, "last": 47199}`; a single port has
    /// `first == last`). `null` when none are configured: any port. Always
    /// present.
    #[cfg_attr(feature = "openapi", schema(required = true))]
    pub ports: Option<Vec<PortSpan>>,
}

impl NetworkCapabilities {
    /// Whether `port` is one of the configured ports. Always true when no
    /// ports are configured.
    pub fn port_allowed(&self, port: u16) -> bool {
        match &self.ports {
            None => true,
            Some(spans) => spans.iter().any(|span| span.contains(port)),
        }
    }
}

/// Capabilities per protocol. A protocol added later gets its own key here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ProtocolCapabilities {
    /// SRT.
    pub srt: SrtCapabilities,
    /// WebRTC (WHIP/WHEP). ICE and TURN handle reachability, so there are no
    /// modes to report, only whether ICE servers are configured.
    pub webrtc: WebRtcCapabilities,
}

/// What SRT can do on this Strom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtCapabilities {
    /// Which connection modes can work.
    pub modes: SrtModes,
    /// Relays a flow can use instead of listening itself.
    pub relays: Vec<RelayEndpoint>,
}

/// SRT connection modes and whether each can work on this Strom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtModes {
    /// Strom binds a port and waits for the peer. Needs inbound reachability.
    pub listener: bool,
    /// Strom connects out to the peer. Always possible.
    pub caller: bool,
    /// Both sides connect to each other; the peer must reach a port Strom
    /// binds. Needs inbound reachability.
    pub rendezvous: bool,
}

impl SrtModes {
    /// Whether `mode` can work.
    pub fn allows(&self, mode: SrtMode) -> bool {
        match mode {
            SrtMode::Listener => self.listener,
            SrtMode::Caller => self.caller,
            SrtMode::Rendezvous => self.rendezvous,
        }
    }
}

/// A relay a flow can publish to or pull from instead of binding a port
/// itself. Not tied to one protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RelayEndpoint {
    /// The relay's configured name.
    pub name: String,
    /// The address remote parties use to reach the relay.
    pub public: String,
}

/// What WebRTC is configured with, without URLs or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WebRtcCapabilities {
    /// Whether any ICE (STUN/TURN) servers are configured.
    pub ice_servers: bool,
    /// The ICE transport policy (`"all"` or `"relay"`).
    pub ice_transport_policy: String,
}

/// The local side of an `srt://` URI: the mode it selects and the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrtUriEndpoint {
    /// The mode the URI selects.
    pub mode: SrtMode,
    /// The port in the URI, if any.
    pub port: Option<u16>,
}

/// Read the mode and port from an `srt://` URI, the way the GStreamer SRT
/// elements do: an explicit `mode=` wins, otherwise a URI without a host is a
/// listener and one with a host is a caller. `None` for other schemes or an
/// unknown mode.
pub fn parse_srt_uri(uri: &str) -> Option<SrtUriEndpoint> {
    let rest = uri.trim().strip_prefix("srt://")?;
    let (authority, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let authority = authority.split('/').next().unwrap_or("");

    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']').unwrap_or((v6, ""));
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    let port = port.and_then(|p| p.parse::<u16>().ok());

    let explicit_mode = query.and_then(|q| {
        q.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "mode").then(|| value.to_ascii_lowercase())
        })
    });
    let mode = match explicit_mode.as_deref() {
        Some(nick) => SrtMode::from_nick(nick)?,
        None if host.is_empty() => SrtMode::Listener,
        None => SrtMode::Caller,
    };
    Some(SrtUriEndpoint { mode, port })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Capabilities {
        Capabilities {
            network: NetworkCapabilities {
                inbound_reachable: false,
                ports: Some(vec![PortSpan {
                    first: 47100,
                    last: 47199,
                }]),
            },
            protocols: ProtocolCapabilities {
                srt: SrtCapabilities {
                    modes: SrtModes {
                        listener: false,
                        caller: true,
                        rendezvous: false,
                    },
                    relays: Vec::new(),
                },
                webrtc: WebRtcCapabilities {
                    ice_servers: true,
                    ice_transport_policy: "all".to_string(),
                },
            },
        }
    }

    #[test]
    fn serializes_to_the_documented_shape() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "network": {
                    "inbound_reachable": false,
                    "ports": [{ "first": 47100, "last": 47199 }]
                },
                "protocols": {
                    "srt": {
                        "modes": { "listener": false, "caller": true, "rendezvous": false },
                        "relays": []
                    },
                    "webrtc": { "ice_servers": true, "ice_transport_policy": "all" }
                }
            })
        );
        let back: Capabilities = serde_json::from_value(json).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn unset_ports_serialize_as_null() {
        let mut caps = sample();
        caps.network.ports = None;
        let json = serde_json::to_value(caps).unwrap();
        assert!(json["network"]["ports"].is_null());
        assert!(json["network"].as_object().unwrap().contains_key("ports"));
    }

    #[test]
    fn relay_endpoint_shape() {
        let relay = RelayEndpoint {
            name: "edge".to_string(),
            public: "srt://relay.example.com:9000".to_string(),
        };
        assert_eq!(
            serde_json::to_value(relay).unwrap(),
            serde_json::json!({ "name": "edge", "public": "srt://relay.example.com:9000" })
        );
    }

    #[test]
    fn port_allowed_checks_the_configured_ranges() {
        let mut net = sample().network;
        assert!(net.port_allowed(47100));
        assert!(net.port_allowed(47199));
        assert!(!net.port_allowed(5000));
        net.ports = Some(vec![
            PortSpan {
                first: 47100,
                last: 47101,
            },
            PortSpan {
                first: 47250,
                last: 47250,
            },
        ]);
        assert!(net.port_allowed(47101));
        assert!(!net.port_allowed(47102));
        assert!(net.port_allowed(47250));
        assert!(!net.port_allowed(47249));
        assert!(!net.port_allowed(47251));
        net.ports = Some(Vec::new());
        assert!(!net.port_allowed(47250));
        net.ports = None;
        assert!(net.port_allowed(5000));
    }

    #[test]
    fn srt_modes_allows() {
        let modes = sample().protocols.srt.modes;
        assert!(modes.allows(SrtMode::Caller));
        assert!(!modes.allows(SrtMode::Listener));
        assert!(!modes.allows(SrtMode::Rendezvous));
    }

    #[test]
    fn parses_srt_uri_modes_and_ports() {
        let p = |uri| parse_srt_uri(uri);
        assert_eq!(
            p("srt://:5000"),
            Some(SrtUriEndpoint {
                mode: SrtMode::Listener,
                port: Some(5000)
            })
        );
        assert_eq!(
            p("srt://192.0.2.1:5000"),
            Some(SrtUriEndpoint {
                mode: SrtMode::Caller,
                port: Some(5000)
            })
        );
        assert_eq!(
            p("srt://:5000?mode=listener&latency=200"),
            Some(SrtUriEndpoint {
                mode: SrtMode::Listener,
                port: Some(5000)
            })
        );
        assert_eq!(
            p("srt://192.0.2.1:6000?latency=200&mode=rendezvous"),
            Some(SrtUriEndpoint {
                mode: SrtMode::Rendezvous,
                port: Some(6000)
            })
        );
        assert_eq!(
            p("srt://0.0.0.0:5000?mode=listener").map(|e| e.mode),
            Some(SrtMode::Listener)
        );
        assert_eq!(
            p("srt://example.com:5000?mode=caller").map(|e| e.mode),
            Some(SrtMode::Caller)
        );
        assert_eq!(
            p("srt://[::]:7000"),
            Some(SrtUriEndpoint {
                mode: SrtMode::Caller,
                port: Some(7000)
            })
        );
        assert_eq!(p("udp://:5000"), None);
        assert_eq!(p("srt://:5000?mode=bogus"), None);
    }
}
