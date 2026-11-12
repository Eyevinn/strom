//! Links page for quick access to WHEP players, SRT streams, and API endpoints.

use egui::{Context, Ui};
use std::collections::{HashMap, HashSet};
use strom_types::srt_relay::{
    parse_srt_relay_uri, srt_relay_dial_in_uri, srt_relay_role, strip_srt_uri_secrets, SrtRelayRole,
};
use strom_types::{Flow, PropertyValue};

use crate::api::ApiClient;
use crate::app::{
    download_file, escape_xml, generate_vlc_playlist, vlc_can_play_block, vlc_player_uri,
};
use crate::qr::QrCache;
use crate::srt_relay::{dial_in_action, SrtRelayStatus};

/// An SRT stream VLC can play: a listener on this server or an output on the
/// SRT relay.
struct VlcStreamInfo {
    /// Track name: the flow name, plus the block for a relay output.
    name: String,
    /// The URL VLC opens (see `crate::app::vlc_player_uri`).
    vlc_uri: String,
    /// `vlc_uri` carries a passphrase.
    has_passphrase: bool,
}

/// The SRT streams VLC can play.
struct VlcStreams {
    streams: Vec<VlcStreamInfo>,
    /// Relay outputs left out because the relay's public address is not known.
    relay_outputs_without_public: usize,
}

/// A block whose `srt_uri` is `srt+relay://<id>`.
struct SrtRelayBlockInfo {
    flow_name: String,
    block_label: String,
    role: SrtRelayRole,
    /// The stream id, or why the URI is not valid.
    id: Result<String, String>,
    has_passphrase: bool,
    /// Another SRT block, in this flow or another, uses the same relay id.
    duplicate_id: bool,
}

/// Tab selection for Links page.
#[derive(Default, Clone, Copy, PartialEq)]
enum LinksTab {
    #[default]
    Whep,
    Srt,
    Api,
}

/// Links page state.
pub struct LinksPage {
    selected_tab: LinksTab,
    /// URLs for which the QR code is currently shown.
    qr_visible: HashSet<String>,
    /// Cached QR code textures.
    qr_cache: QrCache,
}

impl LinksPage {
    pub fn new() -> Self {
        Self {
            selected_tab: LinksTab::default(),
            qr_visible: HashSet::new(),
            qr_cache: QrCache::new(),
        }
    }

    /// Extract the SRT blocks that use the relay (`srt+relay://`) from flows,
    /// sorted by flow and block.
    fn get_srt_relay_blocks(flows: &[Flow]) -> Vec<SrtRelayBlockInfo> {
        let mut found = Vec::new();
        for flow in flows {
            for block in &flow.blocks {
                let Some(role) = srt_relay_role(&block.block_definition_id) else {
                    continue;
                };
                let Some(PropertyValue::String(srt_uri)) = block.properties.get("srt_uri") else {
                    continue;
                };
                let (id, has_passphrase) = match parse_srt_relay_uri(srt_uri) {
                    Ok(None) => continue,
                    Ok(Some(parsed)) => {
                        let has_passphrase = parsed.passphrase().is_some();
                        (Ok(parsed.id), has_passphrase)
                    }
                    Err(e) => (Err(e.to_string()), false),
                };
                found.push(SrtRelayBlockInfo {
                    flow_name: flow.name.clone(),
                    block_label: block.name.clone().unwrap_or_else(|| block.id.clone()),
                    role,
                    id,
                    has_passphrase,
                    duplicate_id: false,
                });
            }
        }
        // A relay id names one stream on the whole relay, across flows.
        let mut uses: HashMap<String, usize> = HashMap::new();
        for id in found.iter().filter_map(|b| b.id.as_ref().ok()) {
            *uses.entry(id.clone()).or_default() += 1;
        }
        for block in &mut found {
            block.duplicate_id = block.id.as_ref().is_ok_and(|id| uses[id] > 1);
        }
        found.sort_by(|a, b| (&a.flow_name, &a.block_label).cmp(&(&b.flow_name, &b.block_label)));
        found
    }

    /// Extract the SRT streams VLC can play from flows. `relay_public` is the
    /// relay's public address, if known.
    fn get_vlc_streams(flows: &[Flow], relay_public: Option<&str>) -> VlcStreams {
        let mut found = VlcStreams {
            streams: Vec::new(),
            relay_outputs_without_public: 0,
        };

        for flow in flows {
            for block in &flow.blocks {
                let Some(PropertyValue::String(srt_uri)) = block.properties.get("srt_uri") else {
                    continue;
                };
                let relay = parse_srt_relay_uri(srt_uri).ok().flatten();
                match vlc_player_uri(&block.block_definition_id, srt_uri, relay_public) {
                    Some(vlc_uri) => {
                        let name = if relay.is_some() {
                            let label = block.name.as_deref().unwrap_or(&block.id);
                            format!("{} / {}", flow.name, label)
                        } else {
                            flow.name.clone()
                        };
                        // From the URL itself, so a listener's passphrase
                        // counts too, not only a relay's.
                        let has_passphrase = strip_srt_uri_secrets(&vlc_uri) != vlc_uri;
                        found.streams.push(VlcStreamInfo {
                            name,
                            has_passphrase,
                            vlc_uri,
                        });
                    }
                    None if relay.is_some()
                        && relay_public.is_none()
                        && vlc_can_play_block(&block.block_definition_id) =>
                    {
                        found.relay_outputs_without_public += 1;
                    }
                    None => {}
                }
            }
        }

        found.streams.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    /// Generate a combined VLC playlist for all SRT streams VLC can play.
    fn generate_combined_playlist(streams: &[VlcStreamInfo]) -> String {
        let mut tracks = String::new();

        for stream in streams {
            let escaped_uri = escape_xml(&stream.vlc_uri);
            // The title is shown in VLC's playlist: no passphrase in it.
            let escaped_title = escape_xml(&format!(
                "{} ({})",
                stream.name,
                strip_srt_uri_secrets(&stream.vlc_uri)
            ));

            tracks.push_str(&format!(
                r#"    <track>
      <location>{}</location>
      <title>{}</title>
      <extension application="http://www.videolan.org/vlc/playlist/0">
        <vlc:option>network-caching=1000</vlc:option>
      </extension>
    </track>
"#,
                escaped_uri, escaped_title
            ));
        }

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<playlist xmlns="http://xspf.org/ns/0/" xmlns:vlc="http://www.videolan.org/vlc/playlist/ns/0/" version="1">
  <title>Strom SRT Streams</title>
  <trackList>
{}  </trackList>
</playlist>
"#,
            tracks
        )
    }

    /// Render the links page.
    pub fn render(
        &mut self,
        ui: &mut Ui,
        api: &ApiClient,
        ctx: &Context,
        flows: &[Flow],
        server_hostname: Option<&str>,
        srt_relay: &SrtRelayStatus,
    ) {
        let server_base = api.base_url().trim_end_matches("/api");

        ui.add_space(8.0);

        // Tab bar
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.selected_tab, LinksTab::Whep, "WHIP/WHEP");
            ui.selectable_value(&mut self.selected_tab, LinksTab::Srt, "MPEG-TS/SRT");
            ui.selectable_value(&mut self.selected_tab, LinksTab::Api, "API");
        });

        ui.separator();

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(16.0);

                match self.selected_tab {
                    LinksTab::Whep => Self::render_whep_tab(
                        ui,
                        ctx,
                        server_base,
                        &mut self.qr_visible,
                        &mut self.qr_cache,
                        server_hostname,
                    ),
                    LinksTab::Srt => {
                        let relay_blocks = Self::get_srt_relay_blocks(flows);
                        if !relay_blocks.is_empty() {
                            srt_relay.ensure_requested(api, ctx);
                        }
                        self.render_srt_tab(ui, ctx, flows, &relay_blocks, srt_relay);
                        self.render_srt_relay_section(
                            ui,
                            ctx,
                            api,
                            &relay_blocks,
                            server_hostname,
                            srt_relay,
                        );
                    }
                    LinksTab::Api => self.render_api_tab(ui, ctx, server_base),
                }
            });
    }

    fn render_whep_tab(
        ui: &mut Ui,
        ctx: &Context,
        server_base: &str,
        qr_visible: &mut HashSet<String>,
        qr_cache: &mut QrCache,
        server_hostname: Option<&str>,
    ) {
        ui.heading("WHIP/WHEP");
        ui.add_space(8.0);
        ui.label("WebRTC ingest (WHIP) and playback (WHEP) for low-latency streaming.");
        ui.add_space(16.0);

        // WHIP Ingest
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                let ingest_url = format!("{}/player/whip-ingest", server_base);
                ui.horizontal(|ui| {
                    if ui
                        .link(egui::RichText::new("WHIP Ingest").strong())
                        .on_hover_text(&ingest_url)
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab(&ingest_url));
                    }
                    if ui
                        .small_button(egui_phosphor::regular::COPY)
                        .on_hover_text("Copy URL to clipboard")
                        .clicked()
                    {
                        crate::clipboard::copy_text_with_ctx(ctx, &ingest_url);
                    }
                    Self::qr_toggle_button(ui, &ingest_url, qr_visible);
                });

                if qr_visible.contains(&ingest_url) {
                    Self::show_inline_qr(ui, ctx, &ingest_url, qr_cache, server_hostname);
                }

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Send audio/video from a browser to a Strom flow via WebRTC.",
                    )
                    .weak(),
                );
            });

        ui.add_space(12.0);

        // Combined streams player
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                let streams_url = format!("{}/player/whep-streams", server_base);
                ui.horizontal(|ui| {
                    if ui
                        .link(egui::RichText::new("WHEP Streams").strong())
                        .on_hover_text(&streams_url)
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab(&streams_url));
                    }
                    if ui
                        .small_button(egui_phosphor::regular::COPY)
                        .on_hover_text("Copy URL to clipboard")
                        .clicked()
                    {
                        crate::clipboard::copy_text_with_ctx(ctx, &streams_url);
                    }
                    Self::qr_toggle_button(ui, &streams_url, qr_visible);
                });

                if qr_visible.contains(&streams_url) {
                    Self::show_inline_qr(ui, ctx, &streams_url, qr_cache, server_hostname);
                }

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Page showing all active WHEP streams with mini-players.")
                        .weak(),
                );
            });

        ui.add_space(12.0);

        // Individual player base URL
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                let player_base = format!("{}/player/whep", server_base);
                ui.horizontal(|ui| {
                    if ui
                        .link(egui::RichText::new("Single Stream Player").strong())
                        .on_hover_text(&player_base)
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab(&player_base));
                    }
                    if ui
                        .small_button(egui_phosphor::regular::COPY)
                        .on_hover_text("Copy URL to clipboard")
                        .clicked()
                    {
                        crate::clipboard::copy_text_with_ctx(ctx, &player_base);
                    }
                    Self::qr_toggle_button(ui, &player_base, qr_visible);
                });

                if qr_visible.contains(&player_base) {
                    Self::show_inline_qr(ui, ctx, &player_base, qr_cache, server_hostname);
                }

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Use with ?endpoint=/whep/<endpoint_id> parameter.\n\
                         Individual player URLs are available from WHEP Output block properties.",
                    )
                    .weak(),
                );
            });
    }

    /// Render a QR toggle button. Call inside a `ui.horizontal` block.
    fn qr_toggle_button(ui: &mut Ui, url: &str, qr_visible: &mut HashSet<String>) {
        let is_visible = qr_visible.contains(url);
        if ui
            .small_button(egui_phosphor::regular::QR_CODE)
            .on_hover_text("Toggle QR code for mobile access")
            .clicked()
        {
            if is_visible {
                qr_visible.remove(url);
            } else {
                qr_visible.insert(url.to_string());
            }
        }
    }

    /// Show an inline QR code image below the URL.
    /// The QR code encodes the external URL (localhost replaced with hostname).
    fn show_inline_qr(
        ui: &mut Ui,
        ctx: &Context,
        url: &str,
        qr_cache: &mut QrCache,
        server_hostname: Option<&str>,
    ) {
        let external_url = crate::app::make_external_url(url, server_hostname);
        ui.add_space(4.0);
        if let Some(texture) = qr_cache.get_or_create(ctx, &external_url) {
            ui.image(egui::load::SizedTexture::new(
                texture.id(),
                egui::vec2(160.0, 160.0),
            ));
        }
        if external_url != url {
            ui.label(
                egui::RichText::new(&external_url)
                    .monospace()
                    .small()
                    .weak(),
            );
        }
    }

    fn render_srt_tab(
        &self,
        ui: &mut Ui,
        ctx: &Context,
        flows: &[Flow],
        relay_blocks: &[SrtRelayBlockInfo],
        srt_relay: &SrtRelayStatus,
    ) {
        ui.heading("MPEG-TS/SRT Streams");
        ui.add_space(8.0);
        ui.label(
            "SRT streams that can be played with VLC or other players: listeners on this server and outputs on the SRT relay.",
        );
        ui.add_space(16.0);

        let has_relay_blocks = !relay_blocks.is_empty();
        let relay_public = srt_relay.get().public_or_note().ok();
        let VlcStreams {
            streams,
            relay_outputs_without_public,
        } = Self::get_vlc_streams(flows, relay_public.as_deref());

        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                if streams.is_empty() {
                    let text = if relay_outputs_without_public > 0 {
                        "No SRT streams VLC can play yet.\n\n\
                         The SRT relay outputs are listed here once the SRT relay status is known (see Via SRT relay below)."
                    } else if has_relay_blocks {
                        "No SRT streams VLC can play here.\n\n\
                         The SRT relay blocks below are reached through the relay. Add an MPEGTSSRT Output block \
                         with mode=listener or srt+relay:// to play it in VLC."
                    } else {
                        "No SRT listener streams configured.\n\n\
                         Add an MPEGTSSRT Output block with mode=listener to see streams here."
                    };
                    ui.label(egui::RichText::new(text).weak());
                } else {
                    // Header with download all button
                    ui.horizontal(|ui| {
                        ui.strong(format!(
                            "{} stream{} available",
                            streams.len(),
                            if streams.len() == 1 { "" } else { "s" }
                        ));

                        if ui
                            .button(format!(
                                "{} Download All (VLC)",
                                egui_phosphor::regular::DOWNLOAD_SIMPLE
                            ))
                            .on_hover_text("Download a VLC playlist containing all SRT streams")
                            .clicked()
                        {
                            let content = Self::generate_combined_playlist(&streams);
                            download_file(
                                "strom-srt-streams.xspf",
                                &content,
                                "application/xspf+xml",
                            );
                        }
                    });

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // List individual streams
                    for stream in &streams {
                        ui.horizontal(|ui| {
                            ui.label(&stream.name);

                            let copy_hover = if stream.has_passphrase {
                                "Copy URL to clipboard (includes the passphrase)"
                            } else {
                                stream.vlc_uri.as_str()
                            };
                            if ui
                                .small_button(egui_phosphor::regular::COPY)
                                .on_hover_text(copy_hover)
                                .clicked()
                            {
                                crate::clipboard::copy_text_with_ctx(ctx, &stream.vlc_uri);
                            }

                            if ui
                                .small_button(format!(
                                    "{} VLC",
                                    egui_phosphor::regular::DOWNLOAD_SIMPLE
                                ))
                                .on_hover_text("Download VLC playlist")
                                .clicked()
                            {
                                let content = generate_vlc_playlist(
                                    &stream.vlc_uri,
                                    1000,
                                    &stream.name,
                                );
                                let safe_name: String = stream
                                    .name
                                    .chars()
                                    .map(|c| {
                                        if c.is_alphanumeric() || c == '-' || c == '_' {
                                            c
                                        } else {
                                            '_'
                                        }
                                    })
                                    .collect();
                                download_file(
                                    &format!("{}.xspf", safe_name),
                                    &content,
                                    "application/xspf+xml",
                                );
                            }
                        });
                    }
                }
            });
    }

    /// Blocks that reach the outside world through the SRT relay, with the
    /// URL the outside party dials.
    fn render_srt_relay_section(
        &mut self,
        ui: &mut Ui,
        ctx: &Context,
        api: &ApiClient,
        blocks: &[SrtRelayBlockInfo],
        server_hostname: Option<&str>,
        srt_relay: &SrtRelayStatus,
    ) {
        if blocks.is_empty() {
            return;
        }
        let relay = srt_relay.get();
        // (public address, note to show when it is missing)
        let (public, note): (Option<String>, Option<String>) = match relay.public_or_note() {
            Ok(public) => (Some(public), None),
            Err(note) if relay.configured() == Some(false) => {
                (None, Some(format!("{note} These blocks will not start.")))
            }
            Err(note) => (None, Some(note)),
        };

        ui.add_space(12.0);
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Via SRT relay");
                    if ui
                        .small_button(egui_phosphor::regular::ARROWS_CLOCKWISE)
                        .on_hover_text("Ask the server for the SRT relay status again")
                        .clicked()
                    {
                        srt_relay.refresh(api, ctx);
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Blocks with srt+relay://<id>. The other side dials the relay as an SRT caller with streamid=<id>.",
                    )
                    .weak(),
                );
                if let Some(note) = &note {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(note).weak());
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);

                for block in blocks {
                    let action = dial_in_action(block.role).to_lowercase();
                    ui.horizontal(|ui| {
                        ui.label(format!("{} / {}", block.flow_name, block.block_label));
                        match (&block.id, &public) {
                            (Err(e), _) => {
                                ui.colored_label(ui.visuals().error_fg_color, e);
                            }
                            (Ok(id), Some(public)) => {
                                let url = srt_relay_dial_in_uri(public, id);
                                ui.label(egui::RichText::new(&action).weak());
                                ui.label(egui::RichText::new(&url).monospace());
                                if ui
                                    .small_button(egui_phosphor::regular::COPY)
                                    .on_hover_text("Copy URL to clipboard")
                                    .clicked()
                                {
                                    crate::clipboard::copy_text_with_ctx(ctx, &url);
                                }
                                Self::qr_toggle_button(ui, &url, &mut self.qr_visible);
                                if block.has_passphrase {
                                    ui.label(egui::RichText::new("plus the passphrase").weak());
                                }
                            }
                            (Ok(id), None) => {
                                ui.label(egui::RichText::new(&action).weak());
                                ui.label(egui::RichText::new(format!("streamid={id}")).monospace());
                                if block.has_passphrase {
                                    ui.label(egui::RichText::new("plus the passphrase").weak());
                                }
                            }
                        }
                    });
                    if block.duplicate_id {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 180, 50),
                            "\u{26a0} Another SRT block uses the same relay id: only one of them \
                             will connect. Give one a new address in its properties.",
                        );
                    }
                    if let (Ok(id), Some(public)) = (&block.id, &public) {
                        let url = srt_relay_dial_in_uri(public, id);
                        if self.qr_visible.contains(&url) {
                            Self::show_inline_qr(
                                ui,
                                ctx,
                                &url,
                                &mut self.qr_cache,
                                server_hostname,
                            );
                        }
                    }
                }
            });
    }

    fn render_api_tab(&self, ui: &mut Ui, ctx: &Context, server_base: &str) {
        ui.heading("API Documentation");
        ui.add_space(8.0);
        ui.label("REST API endpoints and documentation.");
        ui.add_space(16.0);

        // Documentation section
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.strong("Documentation");
                ui.add_space(8.0);

                let swagger_url = format!("{}/swagger-ui/", server_base);
                Self::link_row(ui, ctx, "Swagger UI", &swagger_url);
                ui.add_space(4.0);
                let openapi_url = format!("{}/api-docs/openapi.json", server_base);
                Self::link_row(ui, ctx, "OpenAPI Spec", &openapi_url);
            });

        ui.add_space(12.0);

        // Endpoints section
        egui::Frame::group(ui.style())
            .inner_margin(12.0)
            .show(ui, |ui| {
                ui.strong("Endpoints");
                ui.add_space(8.0);

                let endpoints = [
                    ("Flows", format!("{}/api/flows", server_base)),
                    ("WHEP Streams", format!("{}/api/whep-streams", server_base)),
                    ("Version", format!("{}/api/version", server_base)),
                    ("SRT Relay", format!("{}/api/srt-relay", server_base)),
                    ("Blocks", format!("{}/api/blocks", server_base)),
                    ("Elements", format!("{}/api/elements", server_base)),
                ];

                for (i, (label, url)) in endpoints.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(4.0);
                    }
                    Self::link_row(ui, ctx, label, url);
                }
            });
    }

    /// Render a link row: clickable label (hover shows URL) + copy button.
    fn link_row(ui: &mut Ui, ctx: &Context, label: &str, url: &str) {
        ui.horizontal(|ui| {
            if ui.link(label).on_hover_text(url).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
            if ui
                .small_button(egui_phosphor::regular::COPY)
                .on_hover_text("Copy URL to clipboard")
                .clicked()
            {
                crate::clipboard::copy_text_with_ctx(ctx, url);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::block::{BlockInstance, Position};

    fn block(id: &str, definition: &str, srt_uri: &str) -> BlockInstance {
        BlockInstance {
            id: id.to_string(),
            block_definition_id: definition.to_string(),
            name: None,
            properties: [(
                "srt_uri".to_string(),
                PropertyValue::String(srt_uri.to_string()),
            )]
            .into(),
            position: Position { x: 0.0, y: 0.0 },
            runtime_data: None,
            computed_external_pads: None,
        }
    }

    fn flows() -> Vec<Flow> {
        let mut flow = Flow::new("Show");
        flow.blocks = vec![
            block(
                "out",
                "builtin.mpegtssrt_output",
                "srt://:5000?mode=listener",
            ),
            block(
                "relay_out",
                "builtin.mpegtssrt_output",
                "srt+relay://cam1?passphrase=abcdefghijkl",
            ),
            block("relay_in", "builtin.mpegtssrt_input", "srt+relay://cam2"),
            block("efp_out", "builtin.efpsrt_output", "srt+relay://cam3"),
            block(
                "caller",
                "builtin.mpegtssrt_output",
                "srt://192.0.2.1:5000?mode=caller",
            ),
        ];
        vec![flow]
    }

    #[test]
    fn listeners_and_relay_outputs_are_playable() {
        let found = LinksPage::get_vlc_streams(&flows(), Some("relay.example.com:9000"));
        let streams: Vec<_> = found
            .streams
            .iter()
            .map(|s| (s.name.as_str(), s.vlc_uri.as_str(), s.has_passphrase))
            .collect();
        assert_eq!(
            streams,
            vec![
                ("Show", "srt://127.0.0.1:5000?mode=caller", false),
                (
                    "Show / relay_out",
                    "srt://relay.example.com:9000?streamid=cam1&passphrase=abcdefghijkl",
                    true
                ),
            ]
        );
        assert_eq!(found.relay_outputs_without_public, 0);

        let playlist = LinksPage::generate_combined_playlist(&found.streams);
        assert!(playlist.contains(
            "<location>srt://relay.example.com:9000?streamid=cam1&amp;passphrase=abcdefghijkl</location>"
        ));
        assert!(playlist.contains("<location>srt://127.0.0.1:5000?mode=caller</location>"));
    }

    #[test]
    fn a_listener_passphrase_is_flagged_so_the_hover_hides_it() {
        let mut flow = Flow::new("Show");
        flow.blocks = vec![block(
            "out",
            "builtin.mpegtssrt_output",
            "srt://:5000?mode=listener&passphrase=topsecret123",
        )];
        let found = LinksPage::get_vlc_streams(&[flow], None);
        assert_eq!(found.streams.len(), 1);
        assert!(found.streams[0].vlc_uri.contains("topsecret123"));
        assert!(found.streams[0].has_passphrase);
    }

    #[test]
    fn playlist_titles_leave_out_the_passphrase() {
        let found = LinksPage::get_vlc_streams(&flows(), Some("relay.example.com:9000"));
        let playlist = LinksPage::generate_combined_playlist(&found.streams);
        assert!(playlist.contains(
            "<title>Show / relay_out (srt://relay.example.com:9000?streamid=cam1)</title>"
        ));
        assert_eq!(playlist.matches("abcdefghijkl").count(), 1, "{playlist}");
    }

    #[test]
    fn relay_blocks_flag_ids_used_twice_across_flows() {
        let mut other = Flow::new("Other");
        other.blocks = vec![
            block(
                "dup",
                "builtin.mpegtssrt_input",
                "srt+relay://cam1?passphrase=",
            ),
            block("own", "builtin.mpegtssrt_input", "srt+relay://cam9"),
        ];
        let mut all = flows();
        all.push(other);
        let found = LinksPage::get_srt_relay_blocks(&all);
        let flagged: Vec<_> = found
            .iter()
            .map(|b| {
                (
                    b.flow_name.as_str(),
                    b.block_label.as_str(),
                    b.duplicate_id,
                    b.has_passphrase,
                )
            })
            .collect();
        assert_eq!(
            flagged,
            vec![
                ("Other", "dup", true, false),
                ("Other", "own", false, false),
                ("Show", "efp_out", false, false),
                ("Show", "relay_in", false, false),
                ("Show", "relay_out", true, true),
            ]
        );
    }

    #[test]
    fn relay_outputs_wait_for_the_public_address() {
        let found = LinksPage::get_vlc_streams(&flows(), None);
        assert_eq!(found.streams.len(), 1);
        assert_eq!(found.streams[0].name, "Show");
        // Only the MPEG-TS relay output counts; inputs and EFP never play in VLC.
        assert_eq!(found.relay_outputs_without_public, 1);
    }
}
