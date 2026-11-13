//! Warnings in the SRT blocks' properties when the selected mode cannot work
//! on this Strom's network (#1257). They warn, never block: the operator may
//! know better than the configuration.

use egui::{Color32, Ui};
use strom_types::api::SrtMode;
use strom_types::capabilities::{parse_srt_uri, Capabilities};

/// What is wrong with `srt_uri` on a Strom with these capabilities. Empty when
/// nothing is, or when the URI is not one we can read.
pub fn warnings(srt_uri: &str, caps: &Capabilities) -> Vec<String> {
    let Some(endpoint) = parse_srt_uri(srt_uri) else {
        return Vec::new();
    };
    if endpoint.mode == SrtMode::Caller {
        return Vec::new();
    }

    let mut out = Vec::new();
    if !caps.protocols.srt.modes.allows(endpoint.mode) {
        let problem = match endpoint.mode {
            SrtMode::Rendezvous => "the peer cannot reach it in rendezvous mode",
            _ => "a listener cannot be reached",
        };
        out.push(format!(
            "This Strom is not reachable from outside: {problem}. {}",
            suggestion(caps)
        ));
    }
    if let (Some(port), Some(ranges)) = (endpoint.port, caps.network.ports.as_ref()) {
        if !caps.network.port_allowed(port) {
            out.push(format!(
                "Port {port} is outside the ports this Strom is configured for ({}): \
                 it may not be reachable from outside.",
                ranges.join(", ")
            ));
        }
    }
    out
}

/// What to use instead of a mode that cannot work: the relay when the server
/// reports one (by name unless it is `default`), else caller mode.
fn suggestion(caps: &Capabilities) -> String {
    // The server reports `default` first.
    match caps.protocols.srt.relays.first() {
        Some(relay) => format!(
            "Use caller mode, or Relay mode to go through {}.",
            crate::srt_relay::relay_label(&relay.name)
        ),
        None => "Use caller mode, or ask the operator for an SRT relay.".to_string(),
    }
}

/// Render the warnings, in the orange used for other warnings in the UI.
pub fn show(ui: &mut Ui, warnings: &[String]) {
    let color = Color32::from_rgb(255, 165, 0);
    for warning in warnings {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(color, egui_phosphor::regular::WARNING);
            ui.colored_label(color, warning);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::capabilities::{
        NetworkCapabilities, ProtocolCapabilities, SrtCapabilities, SrtModes, WebRtcCapabilities,
    };

    fn caps(reachable: bool, ports: Option<Vec<&str>>) -> Capabilities {
        Capabilities {
            network: NetworkCapabilities {
                inbound_reachable: reachable,
                ports: ports.map(|p| p.into_iter().map(String::from).collect()),
            },
            protocols: ProtocolCapabilities {
                srt: SrtCapabilities {
                    modes: SrtModes {
                        listener: reachable,
                        caller: true,
                        rendezvous: reachable,
                    },
                    relays: Vec::new(),
                },
                webrtc: WebRtcCapabilities {
                    ice_servers: false,
                    ice_transport_policy: "all".to_string(),
                },
            },
        }
    }

    #[test]
    fn reachable_strom_without_ports_never_warns() {
        let caps = caps(true, None);
        assert!(warnings("srt://:5000?mode=listener", &caps).is_empty());
        assert!(warnings("srt://:5000", &caps).is_empty());
        assert!(warnings("srt://192.0.2.1:5000?mode=rendezvous", &caps).is_empty());
    }

    #[test]
    fn unreachable_strom_warns_on_listener_and_rendezvous() {
        let caps = caps(false, None);
        let w = warnings("srt://:5000?mode=listener", &caps);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("a listener cannot be reached"));
        // No host and no mode is a listener.
        assert_eq!(warnings("srt://:5000", &caps).len(), 1);
        let w = warnings("srt://192.0.2.1:5000?mode=rendezvous", &caps);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("rendezvous"));
    }

    #[test]
    fn caller_never_warns() {
        let caps = caps(false, Some(vec!["47100-47199"]));
        assert!(warnings("srt://192.0.2.1:5000", &caps).is_empty());
        assert!(warnings("srt://192.0.2.1:5000?mode=caller", &caps).is_empty());
    }

    #[test]
    fn listener_port_outside_configured_ports_warns() {
        let caps = caps(true, Some(vec!["47100-47199"]));
        let w = warnings("srt://:5000?mode=listener", &caps);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("Port 5000"));
        assert!(w[0].contains("47100-47199"));
        assert!(warnings("srt://:47150?mode=listener", &caps).is_empty());
    }

    #[test]
    fn unreadable_uri_does_not_warn() {
        let caps = caps(false, None);
        assert!(warnings("", &caps).is_empty());
        assert!(warnings("srt+relay://edge/stream", &caps).is_empty());
    }

    fn with_relays(mut caps: Capabilities, names: &[&str]) -> Capabilities {
        caps.protocols.srt.relays = names
            .iter()
            .map(|n| strom_types::capabilities::RelayEndpoint {
                name: n.to_string(),
                public: format!("{n}.example.com:9000"),
            })
            .collect();
        caps
    }

    #[test]
    fn an_unreachable_strom_suggests_the_relay() {
        let w = warnings("srt://:5000?mode=listener", &caps(false, None));
        assert!(
            w[0].contains("ask the operator for an SRT relay"),
            "{}",
            w[0]
        );

        let caps_default = with_relays(caps(false, None), &["default", "eu-north"]);
        let w = warnings("srt://:5000?mode=listener", &caps_default);
        assert!(
            w[0].ends_with("Relay mode to go through the SRT relay."),
            "{}",
            w[0]
        );

        let caps_named = with_relays(caps(false, None), &["eu-north"]);
        let w = warnings("srt://192.0.2.1:5000?mode=rendezvous", &caps_named);
        assert!(w[0].contains("rendezvous"), "{}", w[0]);
        assert!(w[0].contains("SRT relay 'eu-north'"), "{}", w[0]);
    }
}
