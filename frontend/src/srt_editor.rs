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

use std::collections::HashSet;

use egui::{Color32, RichText, Ui};
use strom_types::block::ExposedProperty;
use strom_types::srt_relay::{
    mask_srt_uri, new_srt_passphrase, new_srt_relay_id, parse_srt_relay_uri, srt_relay_dial_in_uri,
    srt_relay_player_uri, SrtRelayRole, SRT_RELAY_MIN_SAFE_ID_LEN,
};
use strom_types::srt_uri::{
    is_short_relay_stream_id, is_valid_relay_stream_id, passphrase_problem, SrtConnectionMode,
    SrtUriFields, SRT_PBKEYLEN_VALUES,
};
use strom_types::{BlockDefinition, BlockInstance, FlowId, PropertyValue};

use crate::qr::QrCache;
use crate::srt_relay::SrtRelayFetch;

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
    pub relay: &'a SrtRelayFetch,
    /// Relay stream ids other SRT blocks (in any flow) use.
    pub taken_relay_ids: &'a HashSet<String>,
}

impl<'a> SrtEditorContext<'a> {
    /// The context for an SRT block whose `srt_uri` property is `uri_prop`.
    pub fn new(
        definition: &'a BlockDefinition,
        uri_prop: &'a ExposedProperty,
        flow_id: Option<FlowId>,
        role: SrtRelayRole,
        relay: &'a SrtRelayFetch,
        taken_relay_ids: &'a HashSet<String>,
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
            relay,
            taken_relay_ids,
        }
    }
}

/// What the editor did.
#[derive(Default)]
pub struct SrtEditorOutcome {
    /// The user asked to fetch the relay status again.
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
        result.srt_relay_refresh_requested |= self.refresh_requested;
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
fn relay_unavailable_reason(relay: &SrtRelayFetch) -> Option<String> {
    match relay {
        SrtRelayFetch::Loaded(info) if info.configured => None,
        SrtRelayFetch::Loaded(_) => {
            Some("No SRT relay is configured on this server (STROM_SRT_RELAY).".to_string())
        }
        SrtRelayFetch::Failed(e) => Some(format!("Could not ask for the SRT relay status: {e}")),
        SrtRelayFetch::NotRequested | SrtRelayFetch::Loading => {
            Some("Asking for the SRT relay status...".to_string())
        }
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
    relay: &SrtRelayFetch,
    outcome: &mut SrtEditorOutcome,
) -> Option<SrtConnectionMode> {
    let mut picked = None;
    ui.horizontal(|ui| {
        let relay_configured = relay_unavailable_reason(relay).is_none();
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
        if matches!(relay, SrtRelayFetch::Failed(_))
            && ui
                .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                .on_hover_text("Ask the server for the SRT relay status again")
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
                    mode_buttons(ui, readable.then_some(fields.mode), ctx.relay, &mut outcome)
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
                if leaving_relay {
                    fields.port = state.remembered_port.clone();
                    if mode == SrtConnectionMode::Caller {
                        fields.host = state.remembered_host.clone();
                    }
                }
            }
            let relay_known = matches!(
                ctx.relay,
                SrtRelayFetch::Loaded(_) | SrtRelayFetch::Failed(_)
            );
            if readable && relay_known && fields.mode == SrtConnectionMode::Relay {
                if let Some(reason) = relay_unavailable_reason(ctx.relay) {
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
    // Shown with the header closed too: the fields are disabled until fixed.
    if let Err(e) = &parsed {
        ui.add(
            egui::Label::new(
                RichText::new(format!("Can't read this URI: {e}."))
                    .color(WARNING)
                    .small(),
            )
            .wrap(),
        );
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
                if relay
                    && ui
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
                if ctx.taken_relay_ids.contains(id) {
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

/// The URL the other side dials for a block whose `srt_uri` is `uri`: the
/// listener at this server, or the relay with the stream ID. It includes the
/// passphrase (and key length), in clear: the operator configures the block
/// and pastes this into the other device, and can see the passphrase here
/// anyway. `Ok(None)` for a caller (Strom dials out, nobody dials in) and
/// for a URI that cannot be read; `Err(note)` when the relay address is not
/// known.
pub fn connect_url(uri: &str, relay: &SrtRelayFetch) -> Result<Option<String>, String> {
    let Ok(fields) = SrtUriFields::parse(uri) else {
        return Ok(None);
    };
    match fields.mode {
        SrtConnectionMode::Caller => Ok(None),
        SrtConnectionMode::Listener => Ok(Some(crate::app::transform_srt_uri_for_vlc(uri.trim()))),
        SrtConnectionMode::Relay if !is_valid_relay_stream_id(&fields.stream_id) => Ok(None),
        SrtConnectionMode::Relay => {
            let public = relay.public_or_note()?;
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
    !matches!(connect_url(&current_uri(block, ctx), ctx.relay), Ok(None))
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
    let url = match connect_url(&uri, ctx.relay) {
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
            .on_hover_text(format!("{}:\n{url}", connect_heading(ctx.role)))
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
        let relay_public = ctx.relay.public_or_note().ok();
        if let Some(vlc_uri) =
            crate::app::vlc_player_uri(ctx.definition_id, &uri, relay_public.as_deref())
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
    use strom_types::srt_relay::SrtRelayInfo;

    fn relay(public: Option<&str>) -> SrtRelayFetch {
        SrtRelayFetch::Loaded(SrtRelayInfo {
            configured: public.is_some(),
            relay: public.map(str::to_string),
            public: public.map(str::to_string),
            error: None,
        })
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
        assert!(note.contains("STROM_SRT_RELAY"), "{note}");
    }

    #[test]
    fn a_listener_connect_url_carries_the_passphrase() {
        let url = connect_url(
            "srt://:5000?mode=listener&passphrase=abcdefghijkl&latency=200",
            &relay(None),
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
