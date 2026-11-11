//! SRT relay in the GUI: the relay status from `GET /api/srt-relay`, shared by
//! the Links page and the block properties panel, and the `srt_uri` helper
//! the properties panel shows for the SRT blocks.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use egui::{Color32, Context, Ui};
use strom_types::srt_relay::{
    is_srt_relay_uri, mask_srt_uri, new_srt_relay_uri, parse_srt_relay_uri, renew_srt_relay_uri,
    srt_relay_dial_in_uri, srt_relay_role, srt_uri_query, SrtRelayInfo, SrtRelayRole,
    SRT_RELAY_MIN_SAFE_ID_LEN,
};
use strom_types::{BlockInstance, FlowId, PropertyValue};

use crate::api::ApiClient;
use crate::qr::QrCache;

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
            SrtRelayFetch::Loaded(info) if !info.configured => {
                Err("No SRT relay is configured on this server (STROM_SRT_RELAY).".to_string())
            }
            SrtRelayFetch::Loaded(info) => match &info.public {
                Some(public) => Ok(public.clone()),
                None => Err(format!(
                    "The relay's public address is not known{}.",
                    info.error
                        .as_ref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                )),
            },
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

/// What the `srt_uri` helper did.
#[derive(Default)]
pub struct SrtUriHelperOutcome {
    /// `srt_uri` was replaced.
    pub changed: bool,
    /// The user asked to fetch the relay status again.
    pub refresh_requested: bool,
}

/// Per-block UI state of the helper, kept in egui memory.
#[derive(Clone, Default)]
struct HelperUiState {
    show_passphrase: bool,
    show_qr: bool,
    confirm_new_address: bool,
}

const WARNING: Color32 = Color32::from_rgb(255, 180, 50);

/// What a hidden passphrase looks like.
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// The `srt_uri` text field of an SRT block. While it does not have the
/// keyboard focus it shows the URI with the passphrase masked, as the helper
/// below it does; clicking into it shows and edits the real value. Returns
/// whether the value changed. `block_id` keeps the field's focus per block.
pub fn show_srt_uri_editor(ui: &mut Ui, value: &mut String, block_id: &str) -> bool {
    let id = egui::Id::new(("srt_uri_editor", block_id));
    if ui.memory(|m| m.has_focus(id)) {
        return ui.add(egui::TextEdit::singleline(value).id(id)).changed();
    }
    let mut shown = mask_srt_uri(value, MASK);
    let masked = shown != *value;
    let response = ui.add(egui::TextEdit::singleline(&mut shown).id(id));
    if masked {
        response.on_hover_text("Click to show and edit the passphrase");
        // An edit to the masked copy would save the mask as the passphrase.
        false
    } else if response.changed() {
        *value = shown;
        true
    } else {
        false
    }
}

/// The relay helper shown under the `srt_uri` property of an SRT block:
/// a button that sets a new `srt+relay://` address, and for a relay URI the
/// address and passphrase the outside party needs.
///
/// `taken_relay_ids` are the relay ids other SRT blocks (in any flow) use.
pub fn show_srt_uri_helper(
    ui: &mut Ui,
    block: &mut BlockInstance,
    flow_id: Option<FlowId>,
    role: SrtRelayRole,
    relay: &SrtRelayFetch,
    taken_relay_ids: &HashSet<String>,
    qr_cache: &mut QrCache,
) -> SrtUriHelperOutcome {
    let mut outcome = SrtUriHelperOutcome::default();
    let srt_uri = match block.properties.get("srt_uri") {
        Some(PropertyValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    let state_id = egui::Id::new(("srt_relay_helper", flow_id, block.id.as_str()));
    let mut state: HelperUiState = ui.data(|d| d.get_temp(state_id)).unwrap_or_default();

    ui.indent("srt_relay_helper", |ui| {
        if !is_srt_relay_uri(&srt_uri) {
            show_publish_button(ui, block, relay, &mut outcome);
            return;
        }
        let parsed = match parse_srt_relay_uri(&srt_uri) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return,
            Err(e) => {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(e.to_string()).color(ui.visuals().error_fg_color),
                    )
                    .wrap(),
                );
                return;
            }
        };
        let passphrase = parsed.passphrase();

        // Where the outside party dials.
        match relay.public_or_note() {
            Ok(public) => {
                let url = srt_relay_dial_in_uri(&public, &parsed.id);
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{}:", dial_in_action(role)));
                    ui.label(egui::RichText::new(&url).monospace());
                    if ui
                        .small_button(egui_phosphor::regular::COPY)
                        .on_hover_text("Copy URL")
                        .clicked()
                    {
                        crate::clipboard::copy_text_with_ctx(ui.ctx(), &url);
                    }
                    if ui
                        .selectable_label(state.show_qr, egui_phosphor::regular::QR_CODE)
                        .on_hover_text("Show a QR code")
                        .clicked()
                    {
                        state.show_qr = !state.show_qr;
                    }
                });
                if state.show_qr {
                    if let Some(texture) = qr_cache.get_or_create(ui.ctx(), &url) {
                        ui.image(egui::load::SizedTexture::new(
                            texture.id(),
                            egui::vec2(160.0, 160.0),
                        ));
                    }
                }
            }
            Err(note) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "{} the relay with",
                        dial_in_action(role).to_lowercase()
                    ));
                    let streamid = format!("streamid={}", parsed.id);
                    ui.label(egui::RichText::new(&streamid).monospace());
                    if ui
                        .small_button(egui_phosphor::regular::COPY)
                        .on_hover_text("Copy stream id")
                        .clicked()
                    {
                        crate::clipboard::copy_text_with_ctx(ui.ctx(), &parsed.id);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(note).weak().small());
                    if ui
                        .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                        .on_hover_text("Ask the server again")
                        .clicked()
                    {
                        outcome.refresh_requested = true;
                    }
                });
            }
        }

        // The passphrase, hidden until asked for.
        if let Some(passphrase) = &passphrase {
            ui.horizontal_wrapped(|ui| {
                ui.label("and the passphrase");
                let shown = if state.show_passphrase {
                    passphrase.clone()
                } else {
                    "\u{2022}".repeat(8)
                };
                ui.label(egui::RichText::new(shown).monospace());
                let (icon, hover) = if state.show_passphrase {
                    (egui_phosphor::regular::EYE_SLASH, "Hide passphrase")
                } else {
                    (egui_phosphor::regular::EYE, "Show passphrase")
                };
                if ui.small_button(icon).on_hover_text(hover).clicked() {
                    state.show_passphrase = !state.show_passphrase;
                }
                if ui
                    .small_button(egui_phosphor::regular::COPY)
                    .on_hover_text("Copy passphrase")
                    .clicked()
                {
                    crate::clipboard::copy_text_with_ctx(ui.ctx(), passphrase);
                }
            });
        }

        // Weak settings.
        if taken_relay_ids.contains(&parsed.id) {
            ui.colored_label(
                WARNING,
                "\u{26a0} Another SRT block uses this relay id: only one of them will connect. \
                 Make a new address for this one.",
            );
        }
        if passphrase.is_none() {
            ui.colored_label(
                WARNING,
                "\u{26a0} No passphrase: anyone who knows the id can connect.",
            );
        }
        if parsed.id.len() < SRT_RELAY_MIN_SAFE_ID_LEN {
            ui.colored_label(
                WARNING,
                format!(
                    "\u{26a0} The id is short and easy to guess (use {}+ characters).",
                    SRT_RELAY_MIN_SAFE_ID_LEN
                ),
            );
        }

        // A new address, only after a confirmation: the old one stops working.
        if state.confirm_new_address {
            ui.colored_label(
                WARNING,
                "New id and passphrase? The current address stops working.",
            );
            ui.horizontal(|ui| {
                if ui.button("Replace").clicked() {
                    block.properties.insert(
                        "srt_uri".to_string(),
                        PropertyValue::String(new_srt_relay_uri(&parsed.query)),
                    );
                    state = HelperUiState::default();
                    outcome.changed = true;
                }
                if ui.button("Cancel").clicked() {
                    state.confirm_new_address = false;
                }
            });
        } else if ui
            .small_button(format!(
                "{} New address",
                egui_phosphor::regular::ARROWS_CLOCKWISE
            ))
            .on_hover_text("Make a new random id and passphrase")
            .clicked()
        {
            state.confirm_new_address = true;
        }
    });

    ui.data_mut(|d| d.insert_temp(state_id, state));
    ui.add_space(8.0);
    outcome
}

/// "Publish via SRT relay": replace a plain `srt_uri` with a new relay URI.
fn show_publish_button(
    ui: &mut Ui,
    block: &mut BlockInstance,
    relay: &SrtRelayFetch,
    outcome: &mut SrtUriHelperOutcome,
) {
    let configured = relay.configured() == Some(true);
    let disabled_hint = match relay {
        SrtRelayFetch::Loaded(_) => {
            "No SRT relay is configured on this server (STROM_SRT_RELAY).".to_string()
        }
        SrtRelayFetch::Failed(e) => format!("Could not ask for the SRT relay status: {e}"),
        _ => "Asking for the SRT relay status...".to_string(),
    };
    ui.horizontal(|ui| {
        let button = ui
            .add_enabled(
                configured,
                egui::Button::new(format!(
                    "{} Publish via SRT relay",
                    egui_phosphor::regular::BROADCAST
                )),
            )
            .on_hover_text(
                "Set a random srt+relay:// address with a passphrase. \
                 It is saved with the flow and stays the same across restarts.",
            )
            .on_disabled_hover_text(&disabled_hint);
        if button.clicked() {
            // Keep settings such as latency and pbkeylen; the relay path sets
            // mode and streamid itself.
            let current = match block.properties.get("srt_uri") {
                Some(PropertyValue::String(uri)) => uri.clone(),
                _ => String::new(),
            };
            block.properties.insert(
                "srt_uri".to_string(),
                PropertyValue::String(new_srt_relay_uri(srt_uri_query(&current))),
            );
            outcome.changed = true;
        }
        if matches!(relay, SrtRelayFetch::Failed(_))
            && ui
                .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                .on_hover_text("Ask the server again")
                .clicked()
        {
            outcome.refresh_requested = true;
        }
    });
    ui.add_space(8.0);
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
        let info = |configured, public: Option<&str>, error: Option<&str>| {
            SrtRelayFetch::Loaded(SrtRelayInfo {
                configured,
                relay: configured.then(|| "relay:9000".to_string()),
                public: public.map(str::to_string),
                error: error.map(str::to_string),
            })
        };
        assert_eq!(
            info(true, Some("srt.example.com:9000"), None).public_or_note(),
            Ok("srt.example.com:9000".to_string())
        );
        assert!(info(false, None, None)
            .public_or_note()
            .unwrap_err()
            .contains("STROM_SRT_RELAY"));
        assert!(info(true, None, Some("timeout"))
            .public_or_note()
            .unwrap_err()
            .contains("timeout"));
        assert!(SrtRelayFetch::Loading.public_or_note().is_err());
        assert_eq!(SrtRelayFetch::Loading.configured(), None);
    }
}
