//! SRT relays in the GUI: the relays the server reports in
//! `GET /api/capabilities` (`protocols.srt.relays`), shared by the Links page
//! and the block properties panel (see `crate::srt_editor`), and the relay
//! address of copied SRT blocks.

use std::collections::HashSet;

use strom_types::capabilities::{Capabilities, RelayEndpoint};
use strom_types::relay::{find_relay, RelayLookupError, DEFAULT_RELAY_NAME};
use strom_types::srt_relay::{
    parse_srt_relay_uri, renew_srt_relay_uri, srt_relay_role, SrtRelayRole, SRT,
};
use strom_types::{BlockInstance, PropertyValue};

/// Relay streams in use, as `(relay name, stream id)`: an id names one
/// stream on one relay.
pub type TakenRelayIds = HashSet<(String, String)>;

/// What the outside party does with the dial-in URL of a block with this
/// role: it sends to an input block (Strom requests) and pulls from an output
/// block (Strom publishes).
pub fn dial_in_action(role: SrtRelayRole) -> &'static str {
    match role {
        SrtRelayRole::Request => "Send to",
        SrtRelayRole::Publish => "Pull from",
    }
}

/// The relay name and stream id of an SRT block whose `srt_uri` is a valid
/// `srt+relay://` URI.
pub fn block_relay_key(block: &BlockInstance) -> Option<(String, String)> {
    srt_relay_role(&block.block_definition_id)?;
    match block.properties.get("srt_uri") {
        Some(PropertyValue::String(uri)) => parse_srt_relay_uri(uri)
            .ok()
            .flatten()
            .map(|p| (p.relay_name().to_string(), p.id)),
        _ => None,
    }
}

/// Give a copied SRT block a fresh relay address (new id and passphrase, the
/// relay name and other parameters kept). A relay id names a stream on the
/// relay, so a copy that kept the id would pair with, or be refused in
/// favour of, the original. Returns whether the address changed.
pub fn renew_relay_address(block: &mut BlockInstance) -> bool {
    if srt_relay_role(&block.block_definition_id).is_none() {
        return false;
    }
    let Some(PropertyValue::String(uri)) = block.properties.get("srt_uri") else {
        return false;
    };
    let Some(renewed) = renew_srt_relay_uri(uri) else {
        return false;
    };
    block
        .properties
        .insert("srt_uri".to_string(), PropertyValue::String(renewed));
    true
}

/// The SRT relays the server reports, `default` first; `None` while the
/// capabilities are not loaded.
pub fn reported_relays(caps: Option<&Capabilities>) -> Option<&[RelayEndpoint]> {
    caps.map(|c| c.protocols.srt.relays.as_slice())
}

/// The public address of the SRT relay called `name`, or a note saying why
/// it is not known.
pub fn relay_public(caps: Option<&Capabilities>, name: &str) -> Result<String, String> {
    let Some(relays) = reported_relays(caps) else {
        return Err("The server's SRT relays are not known yet.".to_string());
    };
    match find_relay(SRT, relays, name) {
        Ok(relay) => Ok(relay.public.clone()),
        Err(RelayLookupError::NotConfigured { protocol }) => Err(format!(
            "No SRT relay is configured on this server ({}).",
            protocol.public_env_var()
        )),
        Err(RelayLookupError::Unknown { configured, .. }) => Err(format!(
            "This server has no SRT relay named '{name}' (configured: {}).",
            configured.join(", ")
        )),
    }
}

/// How a relay is named in the GUI: by name, except the default relay.
pub fn relay_label(name: &str) -> String {
    if name == DEFAULT_RELAY_NAME {
        "the SRT relay".to_string()
    } else {
        format!("SRT relay '{name}'")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use strom_types::capabilities::{
        NetworkCapabilities, ProtocolCapabilities, SrtCapabilities, SrtModes, WebRtcCapabilities,
    };

    fn block(definition: &str, srt_uri: &str) -> BlockInstance {
        BlockInstance {
            id: "b1".to_string(),
            block_definition_id: definition.to_string(),
            name: None,
            properties: [(
                "srt_uri".to_string(),
                PropertyValue::String(srt_uri.to_string()),
            )]
            .into(),
            position: strom_types::block::Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }
    }

    fn srt_uri(block: &BlockInstance) -> &str {
        match block.properties.get("srt_uri") {
            Some(PropertyValue::String(s)) => s,
            _ => panic!("no srt_uri"),
        }
    }

    /// Capabilities that report these SRT relays as `(name, public)`.
    pub(crate) fn caps_with_relays(relays: &[(&str, &str)]) -> Capabilities {
        Capabilities {
            network: NetworkCapabilities {
                inbound_reachable: false,
                ports: None,
            },
            protocols: ProtocolCapabilities {
                srt: SrtCapabilities {
                    modes: SrtModes {
                        listener: false,
                        caller: true,
                        rendezvous: false,
                    },
                    relays: relays
                        .iter()
                        .map(|(name, public)| RelayEndpoint {
                            name: name.to_string(),
                            public: public.to_string(),
                        })
                        .collect(),
                },
                webrtc: WebRtcCapabilities {
                    ice_servers: false,
                    ice_transport_policy: "all".to_string(),
                },
            },
        }
    }

    #[test]
    fn a_copied_relay_block_gets_a_fresh_address() {
        let old = "srt+relay://cam1cam1cam1cam1?passphrase=oldoldoldold&pbkeylen=32";
        let mut copy = block("builtin.mpegtssrt_output", old);
        assert!(renew_relay_address(&mut copy));
        let parsed = parse_srt_relay_uri(srt_uri(&copy)).unwrap().unwrap();
        assert_ne!(parsed.id, "cam1cam1cam1cam1");
        assert_ne!(parsed.passphrase().as_deref(), Some("oldoldoldold"));
        assert_eq!(parsed.param("pbkeylen").as_deref(), Some("32"));
        assert_eq!(
            block_relay_key(&copy),
            Some(("default".to_string(), parsed.id))
        );
    }

    #[test]
    fn a_copied_relay_block_keeps_its_relay() {
        let old = "srt+relay://cam1cam1cam1cam1@eu-north?passphrase=oldoldoldold";
        let mut copy = block("builtin.mpegtssrt_input", old);
        assert!(renew_relay_address(&mut copy));
        let parsed = parse_srt_relay_uri(srt_uri(&copy)).unwrap().unwrap();
        assert_eq!(parsed.relay.as_deref(), Some("eu-north"));
        assert_ne!(parsed.id, "cam1cam1cam1cam1");
        assert_eq!(
            block_relay_key(&copy),
            Some(("eu-north".to_string(), parsed.id))
        );
    }

    #[test]
    fn other_blocks_keep_their_uri_on_copy() {
        for (definition, uri) in [
            ("builtin.mpegtssrt_output", "srt://:5000?mode=listener"),
            ("builtin.mpegtssrt_input", "srt+relay://bad id"),
            ("builtin.whep_output", "srt+relay://cam1"),
        ] {
            let mut copy = block(definition, uri);
            assert!(!renew_relay_address(&mut copy), "{definition} {uri}");
            assert_eq!(srt_uri(&copy), uri);
        }
        assert_eq!(
            block_relay_key(&block("builtin.whep_output", "srt+relay://cam1")),
            None
        );
    }

    #[test]
    fn relay_public_explains_a_missing_address() {
        let caps = caps_with_relays(&[
            ("default", "srt.example.com:9000"),
            ("eu-north", "srt-eu.example.com:9000"),
        ]);
        assert_eq!(
            relay_public(Some(&caps), "default"),
            Ok("srt.example.com:9000".to_string())
        );
        assert_eq!(
            relay_public(Some(&caps), "eu-north"),
            Ok("srt-eu.example.com:9000".to_string())
        );
        let note = relay_public(Some(&caps), "us").unwrap_err();
        assert!(note.contains("'us'"), "{note}");
        assert!(note.contains("default, eu-north"), "{note}");

        let none = caps_with_relays(&[]);
        let note = relay_public(Some(&none), "default").unwrap_err();
        assert!(note.contains("STROM_SRT_RELAY_PUBLIC"), "{note}");
        assert!(relay_public(None, "default").is_err());
    }

    #[test]
    fn the_default_relay_is_not_named() {
        assert_eq!(relay_label("default"), "the SRT relay");
        assert_eq!(relay_label("eu-north"), "SRT relay 'eu-north'");
    }
}
