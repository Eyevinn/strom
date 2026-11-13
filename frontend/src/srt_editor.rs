//! The connection editor of the SRT blocks.
//!
//! The four SRT blocks keep their whole connection in one `srt_uri` property.
//! This module shows it two ways at once: the `srt_uri` text itself, and
//! fields read out of it (connection mode, host, port, stream id,
//! passphrase, key length). Both edit the same property: an edited field
//! rebuilds `srt_uri`, edited text is parsed into the fields again on the
//! next frame. Nothing else is stored. The parsing and writing live in
//! `strom_types::srt_uri`.
//!
//! The properties panel calls [`show_connection`] in place of the `srt_uri`
//! row, and [`show_connect_actions`] in the action row at the top, where
//! the dial-in address and VLC buttons live apart from the parameters.

use egui::{Color32, RichText, Ui};
use strom_types::block::ExposedProperty;
use strom_types::capabilities::RelayEndpoint;
use strom_types::relay::DEFAULT_RELAY_NAME;
use strom_types::srt_relay::{
    mask_srt_uri, new_srt_passphrase, new_srt_relay_id, parse_srt_relay_uri, srt_relay_dial_in_uri,
    srt_relay_player_uri, SrtRelayRole, SRT_RELAY_MIN_SAFE_ID_LEN,
};
use strom_types::srt_uri::{
    is_short_relay_stream_id, is_valid_relay_stream_id, passphrase_problem, validate_srt_uri,
    SrtConnectionMode, SrtUriError, SrtUriFields, SRT_PBKEYLEN_VALUES,
};
use strom_types::{BlockDefinition, BlockInstance, FlowId, PropertyValue};

use crate::capabilities::CapabilitiesState;
use crate::qr::QrCache;
use crate::srt_relay::{relay_public, relays_unknown_note, reported_relays, TakenRelayIds};

const WARNING: Color32 = Color32::from_rgb(255, 180, 50);

/// The colour of a property label whose value differs from the default.
const MODIFIED: Color32 = Color32::from_rgb(150, 100, 255);

/// Width of the label column, shared by the grids above and below the URI
/// text so their fields line up.
const LABEL_WIDTH: f32 = 80.0;

/// What a hidden passphrase looks like in the URI text.
const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// What the editor needs to know besides the block.
pub struct SrtEditorContext<'a> {
    pub flow_id: Option<FlowId>,
    pub definition_id: &'a str,
    pub role: SrtRelayRole,
    /// The `srt_uri` default of the block definition. A value equal to it is
    /// not stored, as for every other property.
    pub default_uri: Option<&'a str>,
    /// The `srt_uri` description, shown on hover over the URI label.
    pub description: &'a str,
    /// `GET /api/capabilities`, with the SRT relays once loaded.
    pub capabilities: &'a CapabilitiesState,
    /// Relay streams other SRT blocks (in any flow) use.
    pub taken_relay_ids: &'a TakenRelayIds,
}

impl<'a> SrtEditorContext<'a> {
    /// The context for an SRT block whose `srt_uri` property is `uri_prop`.
    pub fn new(
        definition: &'a BlockDefinition,
        uri_prop: &'a ExposedProperty,
        flow_id: Option<FlowId>,
        role: SrtRelayRole,
        capabilities: &'a CapabilitiesState,
        taken_relay_ids: &'a TakenRelayIds,
    ) -> Self {
        SrtEditorContext {
            flow_id,
            definition_id: &definition.id,
            role,
            default_uri: match &uri_prop.default_value {
                Some(PropertyValue::String(s)) => Some(s.as_str()),
                _ => None,
            },
            description: &uri_prop.description,
            capabilities,
            taken_relay_ids,
        }
    }
}

/// What the editor did.
#[derive(Default)]
pub struct SrtEditorOutcome {
    /// The user asked to fetch the server's capabilities (and relays) again.
    pub refresh_requested: bool,
    /// Open this URL in VLC (an XSPF playlist).
    pub vlc_requested: Option<String>,
    /// Save an XSPF playlist for this URL (native only).
    #[cfg(not(target_arch = "wasm32"))]
    pub vlc_download_requested: Option<String>,
}

impl SrtEditorOutcome {
    /// Hand the editor's requests on to the app.
    pub fn apply_to(self, result: &mut crate::properties::BlockInspectorResult) {
        // Fixed network caching for VLC, not tied to the SRT latency.
        const VLC_NETWORK_CACHING_MS: i32 = 1000;
        result.capabilities_refresh_requested |= self.refresh_requested;
        if let Some(uri) = self.vlc_requested {
            result.vlc_playlist_requested = Some((uri, VLC_NETWORK_CACHING_MS));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(uri) = self.vlc_download_requested {
            result.vlc_playlist_download_only = Some((uri, VLC_NETWORK_CACHING_MS));
        }
    }
}

/// Per-block UI state, kept in egui memory.
#[derive(Clone, Default)]
struct EditorState {
    show_passphrase: bool,
    show_qr: bool,
    /// Host and port from before a switch to the relay, restored on the way
    /// back: the relay URI has no room for them.
    remembered_host: String,
    remembered_port: String,
    /// The fields of the last URI that could be read, shown (disabled) while
    /// the text cannot be.
    last_readable: Option<SrtUriFields>,
}

fn state_id(ctx: &SrtEditorContext<'_>, block: &BlockInstance) -> egui::Id {
    egui::Id::new(("srt_editor", ctx.flow_id, block.id.as_str()))
}

fn current_uri(block: &BlockInstance, ctx: &SrtEditorContext<'_>) -> String {
    match block.properties.get("srt_uri") {
        Some(PropertyValue::String(s)) => s.clone(),
        _ => ctx.default_uri.unwrap_or_default().to_string(),
    }
}

/// Store `uri` as the block's `srt_uri`, or drop the property when it is
/// the default.
fn store_uri(block: &mut BlockInstance, ctx: &SrtEditorContext<'_>, uri: String) {
    if ctx.default_uri == Some(uri.as_str()) {
        block.properties.remove("srt_uri");
    } else {
        block
            .properties
            .insert("srt_uri".to_string(), PropertyValue::String(uri));
    }
}

/// Keep only what a host may hold: no URI delimiters, no whitespace.
pub fn clean_host(host: &str) -> String {
    host.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '[' | ']' | '/' | '@' | '?' | '#' | '&'))
        .collect()
}

/// Keep only the digits of a port.
pub fn clean_port(port: &str) -> String {
    port.chars().filter(char::is_ascii_digit).collect()
}

/// Keep only the characters a relay stream id may hold.
pub fn clean_relay_stream_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// Why the relay cannot be picked, or `None` when it can.
fn relay_unavailable_reason(caps: &CapabilitiesState) -> Option<String> {
    match reported_relays(caps) {
        Some(relays) if !relays.is_empty() => None,
        Some(_) => relay_public(caps, DEFAULT_RELAY_NAME).err(),
        None => relays_unknown_note(caps),
    }
}

/// The relay names to offer in the relay selector, or `None` when it is
/// hidden: only the `default` relay is configured and the URI uses it. A URI
/// naming a relay the server does not report keeps the selector, so the
/// user can pick a configured one.
pub fn relay_choices(relays: &[RelayEndpoint], current: &str) -> Option<Vec<String>> {
    let names: Vec<String> = relays.iter().map(|r| r.name.clone()).collect();
    let only_default = names.len() == 1 && names[0] == DEFAULT_RELAY_NAME;
    let current_reported = names.iter().any(|n| n == current);
    if names.is_empty() || (only_default && current_reported) {
        None
    } else {
        Some(names)
    }
}

/// The relay a block switching onto the relay gets: the one its URI names,
/// else `default`, else the first one the server reports. Empty means
/// `default` (no `@<relay>` written).
pub fn relay_for_switch(relays: &[RelayEndpoint], current: &str) -> String {
    let has = |name: &str| relays.iter().any(|r| r.name == name);
    if !current.is_empty() && has(current) {
        return current.to_string();
    }
    match relays.first() {
        Some(first) if !has(DEFAULT_RELAY_NAME) => first.name.clone(),
        _ => String::new(),
    }
}

/// The name of the relay an `SrtUriFields::relay` value means.
fn relay_name(relay: &str) -> &str {
    if relay.is_empty() {
        DEFAULT_RELAY_NAME
    } else {
        relay
    }
}

/// One short note line in the value column of the grid.
fn note_row(ui: &mut Ui, color: Color32, text: impl Into<String>) {
    ui.label("");
    ui.add(egui::Label::new(RichText::new(text.into()).color(color).small()).wrap());
    ui.end_row();
}

/// The connection mode buttons. Returns the mode picked, if one was.
fn mode_buttons(
    ui: &mut Ui,
    current: Option<SrtConnectionMode>,
    caps: &CapabilitiesState,
    outcome: &mut SrtEditorOutcome,
) -> Option<SrtConnectionMode> {
    let mut picked = None;
    ui.horizontal(|ui| {
        let relay_configured = relay_unavailable_reason(caps).is_none();
        for mode in SrtConnectionMode::ALL {
            let selected = current == Some(mode);
            // Relay is offered only with a relay configured, but a block
            // already on the relay keeps showing it.
            if mode == SrtConnectionMode::Relay && !relay_configured && !selected {
                continue;
            }
            let response = ui.add(egui::Button::selectable(selected, mode.label()));
            let hover = match mode {
                SrtConnectionMode::Listener => "Strom waits on a port; the other side dials in",
                SrtConnectionMode::Caller => "Strom dials the other side",
                SrtConnectionMode::Relay => {
                    "Strom dials the SRT relay; the other side dials the relay with the stream ID"
                }
            };
            if response.on_hover_text(hover).clicked() && !selected {
                picked = Some(mode);
            }
        }
        if reported_relays(caps).is_none()
            && ui
                .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                .on_hover_text("Ask the server for its SRT relays again")
                .clicked()
        {
            outcome.refresh_requested = true;
        }
    });
    picked
}

/// The connection group: mode, the `srt_uri` text, host, port, stream id,
/// passphrase and key length. Shown where the `srt_uri` row would be.
pub fn show_connection(
    ui: &mut Ui,
    block: &mut BlockInstance,
    ctx: &SrtEditorContext<'_>,
) -> SrtEditorOutcome {
    let mut outcome = SrtEditorOutcome::default();
    let sid = state_id(ctx, block);
    let mut state: EditorState = ui.data(|d| d.get_temp(sid)).unwrap_or_default();
    let uri = current_uri(block, ctx);
    let custom = block.properties.contains_key("srt_uri");

    let parsed = SrtUriFields::parse(&uri);
    let readable = parsed.is_ok();
    let mut fields = match &parsed {
        Ok(fields) => {
            state.last_readable = Some(fields.clone());
            fields.clone()
        }
        Err(_) => state.last_readable.clone().unwrap_or_default(),
    };
    let before = fields.clone();
    let mut text_edited = false;
    let mut new_stream_id = false;

    egui::Grid::new(("srt_editor_mode", ctx.flow_id, block.id.as_str()))
        .num_columns(2)
        .min_col_width(LABEL_WIDTH)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            // Connection mode.
            if custom {
                ui.colored_label(MODIFIED, "• Type:")
                    .on_hover_text("Changed from the default");
            } else {
                ui.label("Connection Type");
            }
            let picked = ui
                .add_enabled_ui(readable, |ui| {
                    mode_buttons(
                        ui,
                        readable.then_some(fields.mode),
                        ctx.capabilities,
                        &mut outcome,
                    )
                })
                .inner;
            ui.end_row();
            if let Some(mode) = picked {
                if fields.mode != SrtConnectionMode::Relay {
                    state.remembered_host = fields.host.clone();
                    state.remembered_port = fields.port.clone();
                }
                let leaving_relay = fields.mode == SrtConnectionMode::Relay;
                fields.switch_mode(mode, new_srt_relay_id);
                if mode == SrtConnectionMode::Relay {
                    if let Some(relays) = reported_relays(ctx.capabilities) {
                        fields.relay = relay_for_switch(relays, &fields.relay);
                    }
                }
                if leaving_relay {
                    fields.port = state.remembered_port.clone();
                    if mode == SrtConnectionMode::Caller {
                        fields.host = state.remembered_host.clone();
                    }
                }
            }
            let relays = reported_relays(ctx.capabilities);
            if readable && fields.mode == SrtConnectionMode::Relay {
                if let Some(relays) = relays {
                    let current = relay_name(&fields.relay).to_string();
                    if let Some(names) = relay_choices(relays, &current) {
                        ui.label("Relay").on_hover_text(
                            "The SRT relay Strom dials; the other side dials its public address",
                        );
                        egui::ComboBox::from_id_salt((
                            "srt_editor_relay",
                            ctx.flow_id,
                            block.id.as_str(),
                        ))
                        .selected_text(&current)
                        .show_ui(ui, |ui| {
                            for name in &names {
                                let public = relays
                                    .iter()
                                    .find(|r| &r.name == name)
                                    .map(|r| r.public.as_str())
                                    .unwrap_or_default();
                                if ui
                                    .selectable_label(current == *name, name)
                                    .on_hover_text(public)
                                    .clicked()
                                {
                                    fields.relay = if name == DEFAULT_RELAY_NAME {
                                        String::new()
                                    } else {
                                        name.clone()
                                    };
                                }
                            }
                        });
                        ui.end_row();
                    }
                    // The URI names a relay the server does not report: the
                    // block will not start.
                    if !relays.is_empty() {
                        if let Err(note) = relay_public(ctx.capabilities, &current) {
                            note_row(ui, WARNING, format!("\u{26a0} {note}"));
                        }
                    }
                }
                if let Some(reason) = relay_unavailable_reason(ctx.capabilities) {
                    note_row(ui, WARNING, reason);
                }
            }
        });

    // The srt_uri text, full width when opened: always editable, also when
    // the fields are not.
    let header = egui::CollapsingHeader::new("SRT URI")
        .id_salt(("srt_editor_uri", ctx.flow_id, block.id.as_str()))
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if !ctx.description.is_empty() {
                    ui.add(egui::Label::new(RichText::new(ctx.description).small().weak()).wrap());
                }
                if custom
                    && ui
                        .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                        .on_hover_text("Reset to default")
                        .clicked()
                {
                    block.properties.remove("srt_uri");
                    text_edited = true;
                }
            });
            let mut text = uri.clone();
            if show_srt_uri_text(ui, &mut text, ctx.flow_id, &block.id) {
                store_uri(block, ctx, text);
                text_edited = true;
            }
        });
    header
        .header_response
        .on_hover_text("The block's srt_uri property: the fields read and write it");
    // Shown with the header closed too: what GStreamer would read otherwise
    // than written (the blocks refuse to build on it), else why the fields
    // are disabled, then what has no effect.
    let checked = validate_srt_uri(&uri);
    let problem = match (&checked, &parsed) {
        // The passphrase field shows its own length problem.
        (Err(SrtUriError::PassphraseLength(_)), Ok(_)) | (Ok(_), Ok(_)) => None,
        (Err(e), _) => Some((ui.visuals().error_fg_color, e.to_string())),
        (Ok(_), Err(e)) => Some((WARNING, format!("Can't read this URI: {e}"))),
    };
    let warnings = checked.iter().flatten().map(|w| (WARNING, w.to_string()));
    for (color, text) in problem.into_iter().chain(warnings) {
        ui.add(egui::Label::new(RichText::new(format!("{text}.")).color(color).small()).wrap());
    }
    ui.add_space(4.0);

    egui::Grid::new(("srt_editor_fields", ctx.flow_id, block.id.as_str()))
        .num_columns(2)
        .min_col_width(LABEL_WIDTH)
        .spacing([8.0, 4.0])
        .show(ui, |ui| {
            // The fields follow the text; while it cannot be read they
            // show the last readable value, disabled.
            if !readable {
                ui.disable();
            }

            // Host and port.
            if fields.mode == SrtConnectionMode::Caller {
                ui.label("Host");
                let mut host = fields.host.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut host).hint_text("host or IP address"))
                    .changed()
                {
                    fields.host = clean_host(&host);
                }
                ui.end_row();
                if readable && fields.host.is_empty() {
                    note_row(ui, ui.visuals().error_fg_color, "Enter the address to dial.");
                }
            }
            if fields.mode != SrtConnectionMode::Relay {
                ui.label("Port");
                let mut port = fields.port.clone();
                if ui
                    .add(egui::TextEdit::singleline(&mut port).desired_width(80.0))
                    .changed()
                {
                    fields.port = clean_port(&port);
                }
                ui.end_row();
                let port_ok = fields.port.parse::<u16>().is_ok_and(|p| p > 0);
                if readable && !port_ok {
                    note_row(
                        ui,
                        ui.visuals().error_fg_color,
                        "Enter a port from 1 to 65535.",
                    );
                }
            }

            // Stream ID.
            let relay = fields.mode == SrtConnectionMode::Relay;
            ui.label("Stream ID");
            ui.horizontal(|ui| {
                let mut id = fields.stream_id.clone();
                let hint = if relay { "required" } else { "optional" };
                if ui
                    .add(egui::TextEdit::singleline(&mut id).hint_text(hint))
                    .changed()
                {
                    fields.stream_id = if relay {
                        clean_relay_stream_id(&id)
                    } else {
                        id
                    };
                }
                if ui
                    .add_enabled(
                        !fields.stream_id.is_empty(),
                        egui::Button::new(egui_phosphor::regular::COPY).small(),
                    )
                    .on_hover_text("Copy stream ID")
                    .clicked()
                {
                    crate::clipboard::copy_text_with_ctx(ui.ctx(), &fields.stream_id);
                }
                if ui
                    .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                    .on_hover_text("Generate a new random stream ID")
                    .clicked()
                {
                    new_stream_id = true;
                }
                if ui
                    .add_enabled(
                        !fields.stream_id.is_empty(),
                        egui::Button::new(egui_phosphor::regular::X).small(),
                    )
                    .on_hover_text("Clear stream ID")
                    .clicked()
                {
                    fields.stream_id.clear();
                }
            });
            ui.end_row();
            if relay && readable {
                let id = &fields.stream_id;
                if id.is_empty() {
                    note_row(
                        ui,
                        ui.visuals().error_fg_color,
                        "The relay needs a stream ID: press New.",
                    );
                } else if !is_valid_relay_stream_id(id) {
                    note_row(
                        ui,
                        ui.visuals().error_fg_color,
                        "Use at most 64 characters from A-Z, a-z, 0-9, '_' and '-'.",
                    );
                } else if is_short_relay_stream_id(id) {
                    note_row(
                        ui,
                        WARNING,
                        format!(
                            "\u{26a0} Short stream IDs are easy to guess: use {SRT_RELAY_MIN_SAFE_ID_LEN}+ characters."
                        ),
                    );
                }
                let key = (relay_name(&fields.relay).to_string(), id.clone());
                if ctx.taken_relay_ids.contains(&key) {
                    note_row(
                        ui,
                        WARNING,
                        "\u{26a0} Another SRT block uses this stream ID: only one of them connects.",
                    );
                }
            }

            // Passphrase.
            ui.label("Passphrase");
            ui.horizontal(|ui| {
                let mut passphrase = fields.passphrase.clone();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut passphrase)
                            .password(!state.show_passphrase)
                            .hint_text("none (unencrypted)"),
                    )
                    .changed()
                {
                    fields.passphrase = passphrase;
                }
                if ui
                    .add_enabled(
                        !fields.passphrase.is_empty(),
                        egui::Button::new(egui_phosphor::regular::COPY).small(),
                    )
                    .on_hover_text("Copy passphrase")
                    .clicked()
                {
                    crate::clipboard::copy_text_with_ctx(ui.ctx(), &fields.passphrase);
                }
                if ui
                    .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                    .on_hover_text("Generate a new random 32-character passphrase")
                    .clicked()
                {
                    fields.passphrase = new_srt_passphrase();
                }
                let (icon, hover) = if state.show_passphrase {
                    (egui_phosphor::regular::EYE_SLASH, "Hide passphrase")
                } else {
                    (egui_phosphor::regular::EYE, "Show passphrase")
                };
                if ui
                    .add_enabled(
                        !fields.passphrase.is_empty(),
                        egui::Button::new(icon).small(),
                    )
                    .on_hover_text(hover)
                    .clicked()
                {
                    state.show_passphrase = !state.show_passphrase;
                }
                if ui
                    .add_enabled(
                        !fields.passphrase.is_empty(),
                        egui::Button::new(egui_phosphor::regular::X).small(),
                    )
                    .on_hover_text("Clear passphrase (no encryption)")
                    .clicked()
                {
                    fields.passphrase.clear();
                }
            });
            ui.end_row();
            if readable {
                // Empty is fine: no encryption.
                if let Some(problem) = passphrase_problem(&fields.passphrase) {
                    note_row(ui, ui.visuals().error_fg_color, problem);
                }
            }

            // Key length, only where it matters: with a passphrase, or when
            // the URI sets one.
            if !fields.passphrase.is_empty() || fields.pbkeylen.is_some() {
                ui.label("Key length");
                let shown = match fields.pbkeylen.as_deref() {
                    None => "Default".to_string(),
                    Some(v) => format!("{v} bytes"),
                };
                egui::ComboBox::from_id_salt((
                    "srt_editor_pbkeylen",
                    ctx.flow_id,
                    block.id.as_str(),
                ))
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut fields.pbkeylen, None, "Default");
                    for v in SRT_PBKEYLEN_VALUES {
                        let bits = v.parse::<u32>().unwrap_or(0) * 8;
                        ui.selectable_value(
                            &mut fields.pbkeylen,
                            Some(v.to_string()),
                            format!("{v} bytes (AES-{bits})"),
                        );
                    }
                })
                .response
                .on_hover_text("AES key length (pbkeylen). Used only with a passphrase.");
                ui.end_row();
            }
        });

    if new_stream_id {
        fields.stream_id = new_srt_relay_id();
    }
    // Rebuild the text only from an edited field, and never over text the
    // user typed this frame.
    if readable && !text_edited && fields != before {
        store_uri(block, ctx, fields.to_uri());
    }

    ui.data_mut(|d| d.insert_temp(sid, state));
    ui.add_space(8.0);
    outcome
}

/// Who dials the address of a block with this role.
pub fn connect_heading(role: SrtRelayRole) -> &'static str {
    match role {
        SrtRelayRole::Request => "Senders connect to",
        SrtRelayRole::Publish => "Viewers connect to",
    }
}

/// The hover text of "Copy address": who connects, and the address with the
/// passphrase masked. The copied text keeps it (see [`connect_url`]), so the
/// hover says so, but the secret is not shown on screen unasked.
fn copy_address_hover(role: SrtRelayRole, url: &str) -> String {
    let masked = mask_srt_uri(url, MASK);
    let note = if masked == url {
        ""
    } else {
        "\n(copies with the passphrase)"
    };
    format!("{}:\n{masked}{note}", connect_heading(role))
}

/// The URL the other side dials for a block whose `srt_uri` is `uri`: the
/// listener at this server, or the public address of the relay the URI names
/// with the stream ID. It includes the
/// passphrase (and key length), in clear: the operator configures the block
/// and pastes this into the other device, and can see the passphrase here
/// anyway. `Ok(None)` for a caller (Strom dials out, nobody dials in) and
/// for a URI that cannot be read; `Err(note)` when the relay address is not
/// known (capabilities not loaded, or no such relay on the server).
pub fn connect_url(uri: &str, caps: &CapabilitiesState) -> Result<Option<String>, String> {
    let Ok(fields) = SrtUriFields::parse(uri) else {
        return Ok(None);
    };
    match fields.mode {
        SrtConnectionMode::Caller => Ok(None),
        SrtConnectionMode::Listener => Ok(Some(crate::app::transform_srt_uri_for_vlc(uri.trim()))),
        SrtConnectionMode::Relay if !is_valid_relay_stream_id(&fields.stream_id) => Ok(None),
        SrtConnectionMode::Relay => {
            let public = relay_public(caps, relay_name(&fields.relay))?;
            Ok(Some(match parse_srt_relay_uri(uri.trim()) {
                Ok(Some(parsed)) => srt_relay_player_uri(&public, &parsed),
                _ => srt_relay_dial_in_uri(&public, &fields.stream_id),
            }))
        }
    }
}

/// Whether the block has dial-in actions: a listener, or a relay block
/// (with a note when the relay address is not known).
pub fn has_connect_actions(block: &BlockInstance, ctx: &SrtEditorContext<'_>) -> bool {
    !matches!(
        connect_url(&current_uri(block, ctx), ctx.capabilities),
        Ok(None)
    )
}

/// The dial-in actions of an SRT block, for the action row at the top of
/// its properties: copy the address the other side dials, show it as a QR
/// code, and open it in VLC (MPEG-TS outputs). Listeners and relay blocks
/// only; a caller shows nothing (see [`has_connect_actions`]).
pub fn show_connect_actions(
    ui: &mut Ui,
    block: &BlockInstance,
    ctx: &SrtEditorContext<'_>,
    qr_cache: &mut QrCache,
) -> SrtEditorOutcome {
    let mut outcome = SrtEditorOutcome::default();
    let uri = current_uri(block, ctx);
    let url = match connect_url(&uri, ctx.capabilities) {
        Ok(Some(url)) => url,
        Ok(None) => return outcome,
        Err(note) => {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Label::new(RichText::new(note).weak().small()).wrap());
                if ui
                    .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                    .on_hover_text("Ask the server again")
                    .clicked()
                {
                    outcome.refresh_requested = true;
                }
            });
            return outcome;
        }
    };
    let sid = state_id(ctx, block);
    let mut state: EditorState = ui.data(|d| d.get_temp(sid)).unwrap_or_default();

    ui.horizontal(|ui| {
        if ui
            .button(format!("{} Copy address", egui_phosphor::regular::COPY))
            .on_hover_text(copy_address_hover(ctx.role, &url))
            .clicked()
        {
            crate::clipboard::copy_text_with_ctx(ui.ctx(), &url);
        }
        if ui
            .selectable_label(state.show_qr, egui_phosphor::regular::QR_CODE)
            .on_hover_text("Toggle a QR code of the address")
            .clicked()
        {
            state.show_qr = !state.show_qr;
        }
        let public = SrtUriFields::parse(&uri)
            .ok()
            .and_then(|f| relay_public(ctx.capabilities, relay_name(&f.relay)).ok());
        if let Some(vlc_uri) =
            crate::app::vlc_player_uri(ctx.definition_id, &uri, public.as_deref())
        {
            if ui
                .button(format!("{} Open in VLC", egui_phosphor::regular::PLAY))
                .on_hover_text("Download XSPF playlist and open in VLC")
                .clicked()
            {
                outcome.vlc_requested = Some(vlc_uri.clone());
            }
            #[cfg(not(target_arch = "wasm32"))]
            if ui
                .button(format!(
                    "{} Download",
                    egui_phosphor::regular::DOWNLOAD_SIMPLE
                ))
                .on_hover_text("Download XSPF playlist file")
                .clicked()
            {
                outcome.vlc_download_requested = Some(vlc_uri);
            }
        }
    });
    if state.show_qr {
        ui.add_space(4.0);
        if let Some(texture) = qr_cache.get_or_create(ui.ctx(), &url) {
            ui.image(egui::load::SizedTexture::new(
                texture.id(),
                egui::vec2(200.0, 200.0),
            ));
        }
        ui.label(RichText::new(&url).monospace().small());
    }

    ui.data_mut(|d| d.insert_temp(sid, state));
    outcome
}

/// The text as one URI: whitespace and line breaks (from Enter or a paste)
/// removed.
pub fn single_line_uri(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The `srt_uri` text box: one logical line, soft-wrapped to the panel width
/// so a long URI shows whole. While it has the keyboard focus it shows and
/// edits the real value; otherwise it shows the URI with the passphrase
/// masked, so the secret is not on screen unasked. Enter ends the edit.
/// Returns whether the value changed.
fn show_srt_uri_text(
    ui: &mut Ui,
    value: &mut String,
    flow_id: Option<FlowId>,
    block_id: &str,
) -> bool {
    let id = egui::Id::new(("srt_uri_text", flow_id, block_id));
    fn edit(text: &mut String, id: egui::Id) -> egui::TextEdit<'_> {
        egui::TextEdit::multiline(text)
            .id(id)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace)
    }
    if ui.memory(|m| m.has_focus(id)) {
        let mut text = value.clone();
        let response = ui.add(edit(&mut text, id));
        if text.contains(['\n', '\r']) && !value.contains(['\n', '\r']) {
            // Enter: done.
            response.surrender_focus();
        }
        let text = single_line_uri(&text);
        if response.changed() && text != *value {
            *value = text;
            return true;
        }
        return false;
    }
    let mut shown = mask_srt_uri(value, MASK);
    let masked = shown != *value;
    let response = ui.add(edit(&mut shown, id));
    if masked {
        response.on_hover_text("Click to show and edit the passphrase");
        // An edit to the masked copy would save the mask as the passphrase.
        false
    } else if response.changed() {
        let text = single_line_uri(&shown);
        let changed = text != *value;
        *value = text;
        changed
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srt_relay::tests::caps_with_relays;

    fn relay(public: Option<&str>) -> CapabilitiesState {
        match public {
            Some(public) => caps_with_relays(&[("default", public)]),
            None => caps_with_relays(&[]),
        }
    }

    fn endpoints(names: &[&str]) -> Vec<RelayEndpoint> {
        names
            .iter()
            .map(|n| RelayEndpoint {
                name: n.to_string(),
                public: format!("{n}.example.com:9000"),
            })
            .collect()
    }

    #[test]
    fn the_relay_selector_shows_only_with_a_choice() {
        // Only `default`: implicit.
        assert_eq!(relay_choices(&endpoints(&["default"]), "default"), None);
        assert_eq!(relay_choices(&endpoints(&[]), "default"), None);
        // Several relays: all of them, `default` first as the server sends them.
        assert_eq!(
            relay_choices(&endpoints(&["default", "eu-north"]), "default"),
            Some(vec!["default".to_string(), "eu-north".to_string()])
        );
        // A URI naming a relay the server does not report keeps the selector.
        assert_eq!(
            relay_choices(&endpoints(&["default"]), "us"),
            Some(vec!["default".to_string()])
        );
        // One relay that is not `default`: a URI without `@relay` cannot use it.
        assert_eq!(
            relay_choices(&endpoints(&["eu-north"]), "default"),
            Some(vec!["eu-north".to_string()])
        );
    }

    #[test]
    fn switching_onto_the_relay_picks_a_configured_relay() {
        let both = endpoints(&["default", "eu-north"]);
        assert_eq!(relay_for_switch(&both, ""), "");
        assert_eq!(relay_for_switch(&both, "eu-north"), "eu-north");
        assert_eq!(relay_for_switch(&both, "us"), "");
        let eu_only = endpoints(&["eu-north"]);
        assert_eq!(relay_for_switch(&eu_only, ""), "eu-north");
        assert_eq!(relay_for_switch(&[], "us"), "");
    }

    #[test]
    fn the_dial_in_address_uses_the_named_relay() {
        let caps = caps_with_relays(&[
            ("default", "srt.example.com:9000"),
            ("eu-north", "srt-eu.example.com:9000"),
        ]);
        assert_eq!(
            connect_url("srt+relay://cam1cam1cam1cam1@eu-north", &caps),
            Ok(Some(
                "srt://srt-eu.example.com:9000?streamid=cam1cam1cam1cam1".to_string()
            ))
        );
        assert_eq!(
            connect_url("srt+relay://cam1cam1cam1cam1", &caps),
            Ok(Some(
                "srt://srt.example.com:9000?streamid=cam1cam1cam1cam1".to_string()
            ))
        );
        let note = connect_url("srt+relay://cam1cam1cam1cam1@us", &caps).unwrap_err();
        assert!(note.contains("'us'"), "{note}");
        assert!(connect_url("srt+relay://cam1cam1cam1cam1", &CapabilitiesState::Loading).is_err());
    }

    #[test]
    fn copy_address_hover_masks_the_passphrase() {
        let hover = copy_address_hover(
            SrtRelayRole::Publish,
            "srt://relay.example.com:9000?streamid=abc&passphrase=topsecret123",
        );
        assert!(!hover.contains("topsecret123"), "{hover}");
        assert!(hover.contains("streamid=abc"), "{hover}");
        assert!(hover.contains("copies with the passphrase"), "{hover}");

        let hover = copy_address_hover(SrtRelayRole::Request, "srt://host:5000?mode=caller");
        assert_eq!(hover, "Senders connect to:\nsrt://host:5000?mode=caller");
    }

    #[test]
    fn inputs_are_cleaned() {
        assert_eq!(clean_host(" [2001:db8::1]/x "), "2001:db8::1x");
        assert_eq!(clean_port("50a00 "), "5000");
        assert_eq!(clean_relay_stream_id("Ab 3?k#Q_-"), "Ab3kQ_-");
    }

    #[test]
    fn a_caller_has_no_connect_url() {
        let r = relay(Some("relay.example.com:9000"));
        assert_eq!(
            connect_url("srt://192.0.2.1:4000?mode=caller", &r),
            Ok(None)
        );
        assert_eq!(connect_url("not a uri", &r), Ok(None));
        assert_eq!(connect_url("srt+relay://bad id", &r), Ok(None));
    }

    #[test]
    fn a_relay_connect_url_carries_the_passphrase() {
        let uri = "srt+relay://cam1cam1cam1cam1?passphrase=abcdefghijkl&pbkeylen=32";
        assert_eq!(
            connect_url(uri, &relay(Some("relay.example.com:9000"))),
            Ok(Some(
                "srt://relay.example.com:9000?streamid=cam1cam1cam1cam1&passphrase=abcdefghijkl&pbkeylen=32"
                    .to_string()
            ))
        );
        let note = connect_url(uri, &relay(None)).unwrap_err();
        assert!(note.contains("STROM_SRT_RELAY_PUBLIC"), "{note}");
    }

    #[test]
    fn a_listener_connect_url_carries_the_passphrase() {
        let url = connect_url(
            "srt://:5000?mode=listener&passphrase=abcdefghijkl&latency=200",
            &CapabilitiesState::Loading,
        )
        .unwrap()
        .unwrap();
        assert!(url.contains("passphrase=abcdefghijkl"), "{url}");
        assert!(
            url.ends_with(":5000?passphrase=abcdefghijkl&latency=200&mode=caller"),
            "{url}"
        );
    }

    #[test]
    fn the_uri_text_is_one_line() {
        assert_eq!(
            single_line_uri(" srt://:5000?mode=listener\n&latency=200\r\n"),
            "srt://:5000?mode=listener&latency=200"
        );
    }

    #[test]
    fn the_heading_names_who_dials() {
        assert_eq!(connect_heading(SrtRelayRole::Request), "Senders connect to");
        assert_eq!(connect_heading(SrtRelayRole::Publish), "Viewers connect to");
    }
}
