//! SRT relay in the GUI: the relay status from `GET /api/srt-relay`, shared by
//! the Links page and the block properties panel (see `crate::srt_editor`),
//! and the relay address of copied SRT blocks.

use std::sync::{Arc, Mutex};

use egui::Context;
use strom_types::srt_relay::{
    parse_srt_relay_uri, renew_srt_relay_uri, srt_relay_role, SrtRelayInfo, SrtRelayRole,
};
use strom_types::{BlockInstance, PropertyValue};

use crate::api::ApiClient;

/// What the outside party does with the dial-in URL of a block with this
/// role: it sends to an input block (Strom requests) and pulls from an output
/// block (Strom publishes).
pub fn dial_in_action(role: SrtRelayRole) -> &'static str {
    match role {
        SrtRelayRole::Request => "Send to",
        SrtRelayRole::Publish => "Pull from",
    }
}

/// The relay stream id of an SRT block whose `srt_uri` is a valid
/// `srt+relay://` URI.
pub fn block_relay_id(block: &BlockInstance) -> Option<String> {
    srt_relay_role(&block.block_definition_id)?;
    match block.properties.get("srt_uri") {
        Some(PropertyValue::String(uri)) => parse_srt_relay_uri(uri).ok().flatten().map(|p| p.id),
        _ => None,
    }
}

/// Give a copied SRT block a fresh relay address (new id and passphrase, the
/// other parameters kept). Relay ids name a stream across the whole relay,
/// so a copy that kept the id would pair with, or be refused in favour of,
/// the original. Returns whether the address changed.
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

/// `GET /api/srt-relay`, filled in by a background request.
#[derive(Default, Clone)]
pub enum SrtRelayFetch {
    #[default]
    NotRequested,
    Loading,
    Loaded(SrtRelayInfo),
    Failed(String),
}

impl SrtRelayFetch {
    /// Whether the server has a relay configured. `None` while not known.
    pub fn configured(&self) -> Option<bool> {
        match self {
            SrtRelayFetch::Loaded(info) => Some(info.configured),
            _ => None,
        }
    }

    /// The relay's public address, or a note saying why it is not known.
    pub fn public_or_note(&self) -> Result<String, String> {
        match self {
            SrtRelayFetch::Loaded(SrtRelayInfo {
                configured: true,
                public: Some(public),
                ..
            }) => Ok(public.clone()),
            SrtRelayFetch::Loaded(_) => {
                Err("No SRT relay is configured on this server (STROM_SRT_RELAY).".to_string())
            }
            SrtRelayFetch::Failed(e) => Err(format!("Could not ask for the SRT relay status: {e}")),
            SrtRelayFetch::NotRequested | SrtRelayFetch::Loading => {
                Err("Asking for the SRT relay status...".to_string())
            }
        }
    }
}

/// The relay status, fetched once on demand and again on refresh.
#[derive(Default)]
pub struct SrtRelayStatus {
    state: Arc<Mutex<SrtRelayFetch>>,
}

impl SrtRelayStatus {
    /// The current state.
    pub fn get(&self) -> SrtRelayFetch {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Ask the backend unless it has been asked already.
    pub fn ensure_requested(&self, api: &ApiClient, ctx: &Context) {
        if matches!(self.get(), SrtRelayFetch::NotRequested) {
            self.refresh(api, ctx);
        }
    }

    /// Ask the backend for the relay status in the background.
    pub fn refresh(&self, api: &ApiClient, ctx: &Context) {
        if let Ok(mut state) = self.state.lock() {
            *state = SrtRelayFetch::Loading;
        }
        let api = api.clone();
        let ctx = ctx.clone();
        let slot = Arc::clone(&self.state);
        crate::app::spawn_task(async move {
            let result = match api.get_srt_relay().await {
                Ok(info) => SrtRelayFetch::Loaded(info),
                Err(e) => SrtRelayFetch::Failed(e.to_string()),
            };
            if let Ok(mut state) = slot.lock() {
                *state = result;
            }
            ctx.request_repaint();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_copied_relay_block_gets_a_fresh_address() {
        let old = "srt+relay://cam1cam1cam1cam1?passphrase=oldoldoldold&latency=200";
        let mut copy = block("builtin.mpegtssrt_output", old);
        assert!(renew_relay_address(&mut copy));
        let parsed = parse_srt_relay_uri(srt_uri(&copy)).unwrap().unwrap();
        assert_ne!(parsed.id, "cam1cam1cam1cam1");
        assert_ne!(parsed.passphrase().as_deref(), Some("oldoldoldold"));
        assert_eq!(parsed.param("latency").as_deref(), Some("200"));
        assert_eq!(block_relay_id(&copy), Some(parsed.id));
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
            block_relay_id(&block("builtin.whep_output", "srt+relay://cam1")),
            None
        );
    }

    #[test]
    fn public_or_note_explains_a_missing_address() {
        let info = |configured, public: Option<&str>| {
            SrtRelayFetch::Loaded(SrtRelayInfo {
                configured,
                relay: configured.then(|| "relay:9000".to_string()),
                public: public.map(str::to_string),
                error: None,
            })
        };
        assert_eq!(
            info(true, Some("srt.example.com:9000")).public_or_note(),
            Ok("srt.example.com:9000".to_string())
        );
        assert!(info(false, None)
            .public_or_note()
            .unwrap_err()
            .contains("STROM_SRT_RELAY"));
        assert!(SrtRelayFetch::Loading.public_or_note().is_err());
        assert_eq!(SrtRelayFetch::Loading.configured(), None);
    }
}
