//! Property inspector for editing element properties.

use crate::graph::PropertyTab;
use egui::{Color32, ScrollArea, Ui};
use strom_types::{
    block::{EnumValue, ExposedProperty, DEFAULT_SRT_OUTPUT_URI},
    element::{ElementInfo, PropertyInfo, PropertyType},
    BlockDefinition, BlockInstance, Element, FlowId, PropertyValue,
};

/// A live property update to send to a running pipeline element.
pub struct LivePropertyUpdate {
    pub flow_id: FlowId,
    /// Full element ID: "{block_id}:{element_suffix}"
    pub element_id: String,
    pub property_name: String,
    pub value: PropertyValue,
}

/// Minimum interval between live property API calls for the same element+property.
pub const LIVE_PROPERTY_DEBOUNCE_MS: u64 = 80;

/// Debounce state for a single element+property combination.
/// Tracks when the last API call was sent and stores any pending update
/// that was suppressed by the debounce interval (so the final value is
/// always delivered).
pub struct LivePropertyDebounce {
    pub last_sent: instant::Instant,
    pub pending: Option<LivePropertyUpdate>,
}

/// Drain live property updates through the debounce filter.
///
/// For each incoming update, if enough time has elapsed since the last send
/// for that (element_id, property_name) pair, the update is returned
/// immediately. Otherwise it is stored as a pending update.
///
/// Additionally, any previously-pending updates whose debounce interval has
/// now expired are flushed — this ensures the final slider value is always
/// delivered even if no new `changed` event arrives.
pub fn drain_live_updates(
    debounce_map: &mut std::collections::HashMap<(String, String), LivePropertyDebounce>,
    incoming: Vec<LivePropertyUpdate>,
) -> Vec<LivePropertyUpdate> {
    let now = instant::Instant::now();
    let interval = std::time::Duration::from_millis(LIVE_PROPERTY_DEBOUNCE_MS);
    let mut to_send: Vec<LivePropertyUpdate> = Vec::new();

    // Keys that received a fresh incoming update this frame
    let mut touched_keys: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    // Process incoming updates
    for update in incoming {
        let key = (update.element_id.clone(), update.property_name.clone());
        touched_keys.insert(key.clone());

        let entry = debounce_map
            .entry(key)
            .or_insert_with(|| LivePropertyDebounce {
                // Set last_sent far enough in the past so the first update always goes through
                last_sent: now - interval,
                pending: None,
            });

        if now.duration_since(entry.last_sent) >= interval {
            // Enough time has passed — send immediately
            entry.last_sent = now;
            entry.pending = None;
            to_send.push(update);
        } else {
            // Too soon — store as pending (overwrites any previous pending value)
            entry.pending = Some(update);
        }
    }

    // Flush any previously-pending updates whose interval has expired
    // (but skip keys we already handled above to avoid double-sends)
    let expired_keys: Vec<(String, String)> = debounce_map
        .iter()
        .filter(|(k, v)| v.pending.is_some() && !touched_keys.contains(*k))
        .filter(|(_, v)| now.duration_since(v.last_sent) >= interval)
        .map(|(k, _)| k.clone())
        .collect();

    for key in expired_keys {
        if let Some(entry) = debounce_map.get_mut(&key) {
            if let Some(update) = entry.pending.take() {
                entry.last_sent = now;
                to_send.push(update);
            }
        }
    }

    to_send
}

/// Result from showing the block property inspector.
#[derive(Default)]
pub struct BlockInspectorResult {
    /// Whether delete was requested
    pub delete_requested: bool,
    /// Whether browse streams was requested (for AES67 Input SDP)
    pub browse_streams_requested: bool,
    /// Whether browse NDI sources was requested (for NDI Input)
    pub browse_ndi_sources_requested: bool,
    /// VLC playlist download requested (for MPEG-TS/SRT blocks) - contains (srt_uri, latency_ms)
    pub vlc_playlist_requested: Option<(String, i32)>,
    /// VLC playlist download-only requested (native mode) - contains (srt_uri, latency_ms)
    #[cfg(not(target_arch = "wasm32"))]
    pub vlc_playlist_download_only: Option<(String, i32)>,
    /// WHEP player endpoint_id (for WHEP Output blocks) - used to construct full player URL
    pub whep_player_url: Option<String>,
    /// Copy WHEP player URL to clipboard - contains endpoint_id
    pub copy_whep_url_requested: Option<String>,
    /// WHIP ingest endpoint_id (for WHIP Input blocks) - used to construct full ingest URL
    pub whip_ingest_url: Option<String>,
    /// Copy WHIP ingest URL to clipboard - contains endpoint_id
    pub copy_whip_url_requested: Option<String>,
    /// Show QR code for WHEP player URL - contains endpoint_id
    pub show_qr_whep: Option<String>,
    /// Show QR code for WHIP ingest URL - contains endpoint_id
    pub show_qr_whip: Option<String>,
    /// Loudness reset requested - contains (flow_id, block_id)
    pub loudness_reset_requested: Option<(FlowId, String)>,
    /// Recorder split-now requested - contains (flow_id, block_id)
    pub recorder_split_requested: Option<(FlowId, String)>,
    /// Recorder file download requested - contains relative path
    pub recorder_download_requested: Option<String>,
    /// Vision mixer control page requested - contains (flow_id, block_id)
    pub vision_mixer_url: Option<(FlowId, String)>,
    /// Live property updates to send to running pipeline elements
    pub live_property_updates: Vec<LivePropertyUpdate>,
    /// Inspector rendered a VideoDevice/AudioDevice property — caller should
    /// trigger device discovery if the cache is stale (TTL handled in the app).
    pub local_devices_needed: bool,
    /// Inspector requested a forced re-scan of capture devices (↻ button).
    pub local_devices_refresh_requested: bool,
}

/// Side-channel collected by the property editor when it renders a
/// VideoDevice / AudioDevice picker — bubbled back up so the App can
/// trigger device discovery and ↻ refresh calls.
#[derive(Default)]
pub(crate) struct DevicePickerActions {
    pub needed: bool,
    pub refresh_requested: bool,
}

/// Property inspector panel.
pub struct PropertyInspector;

impl PropertyInspector {
    /// Match an actual pad name (e.g., "sink_0") to a pad template (e.g., "sink_%u").
    /// Returns true if the actual pad name matches the template.
    fn matches_pad_template(actual_pad: &str, template: &str) -> bool {
        // First try exact match
        if actual_pad == template {
            return true;
        }

        // Check for request pad patterns like "sink_%u", "src_%u", "sink_%d", etc.
        // Replace common patterns with regex-like matching
        if template.contains("%u") || template.contains("%d") {
            // Extract the prefix before the pattern
            let prefix = if let Some(idx) = template.find("%u") {
                &template[..idx]
            } else if let Some(idx) = template.find("%d") {
                &template[..idx]
            } else {
                return false;
            };

            // Check if actual pad starts with the prefix
            if !actual_pad.starts_with(prefix) {
                return false;
            }

            // Check if the suffix is numeric
            let suffix = &actual_pad[prefix.len()..];
            suffix.chars().all(|c| c.is_ascii_digit() || c == '_')
        } else {
            false
        }
    }

    /// Show the property inspector for the given element with tabbed interface.
    /// Returns (new_active_tab, delete_requested).
    pub fn show(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        active_tab: PropertyTab,
        focused_pad: Option<String>,
        input_pads: Vec<String>,
        output_pads: Vec<String>,
    ) -> (PropertyTab, bool) {
        let mut new_tab = active_tab;
        let delete_requested = false;

        ui.push_id("selected_inspector", |ui| {
            // Outer scroll area for entire inspector
            ScrollArea::both()
                .id_salt("inspector_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // Element info in collapsible section
                    egui::CollapsingHeader::new(&element.element_type)
                        .default_open(false)
                        .show(ui, |ui| {
                            // Element ID (read-only)
                            ui.horizontal(|ui| {
                                ui.label("ID:");
                                ui.monospace(&element.id);
                            });

                            // Element description from element info
                            if let Some(info) = element_info {
                                if !info.description.is_empty() {
                                    ui.add_space(4.0);
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("Description:");
                                        ui.label(&info.description);
                                    });
                                }
                                if !info.category.is_empty() {
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.label("Category:");
                                        ui.label(&info.category);
                                    });
                                }
                            }
                        });

                    // Tab buttons (wrap on small screens)
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .selectable_label(new_tab == PropertyTab::Element, "Element Properties")
                            .clicked()
                        {
                            new_tab = PropertyTab::Element;
                        }
                        if ui
                            .selectable_label(new_tab == PropertyTab::InputPads, "Input Pads")
                            .clicked()
                        {
                            new_tab = PropertyTab::InputPads;
                        }
                        if ui
                            .selectable_label(new_tab == PropertyTab::OutputPads, "Output Pads")
                            .clicked()
                        {
                            new_tab = PropertyTab::OutputPads;
                        }
                    });

                    ui.separator();

                    // Tab content
                    match new_tab {
                        PropertyTab::Element => {
                            Self::show_element_properties_tab(ui, element, element_info);
                        }
                        PropertyTab::InputPads => {
                            Self::show_input_pads_tab(
                                ui,
                                element,
                                element_info,
                                &input_pads,
                                focused_pad.as_deref(),
                            );
                        }
                        PropertyTab::OutputPads => {
                            Self::show_output_pads_tab(
                                ui,
                                element,
                                element_info,
                                &output_pads,
                                focused_pad.as_deref(),
                            );
                        }
                    }
                }); // outer ScrollArea
        });

        (new_tab, delete_requested)
    }

    /// Show the Element Properties tab content.
    fn show_element_properties_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("element_properties_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if let Some(info) = element_info {
                    if !info.properties.is_empty() {
                        for prop_info in &info.properties {
                            Self::show_property_from_info(ui, element, prop_info);
                        }
                    } else {
                        ui.label("No element properties available");
                    }
                } else {
                    ui.label("No element metadata available");
                }
            });
    }

    /// Show the Input Pads tab content.
    fn show_input_pads_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        actual_pads: &[String],
        focused_pad: Option<&str>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("input_pads_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if actual_pads.is_empty() {
                    ui.label("No input pads connected");
                    return;
                }

                for pad_name in actual_pads {
                    // Highlight focused pad
                    let is_focused = focused_pad == Some(pad_name.as_str());
                    if is_focused {
                        ui.colored_label(
                            Color32::from_rgb(255, 200, 100),
                            format!(
                                "{} Input Pad: {}",
                                egui_phosphor::regular::CARET_RIGHT,
                                pad_name
                            ),
                        );
                    } else {
                        ui.label(format!("Input Pad: {}", pad_name));
                    }

                    ui.indent(pad_name, |ui| {
                        // Find properties for this pad from element_info
                        if let Some(info) = element_info {
                            // Check if there's a matching sink pad in metadata (try template matching)
                            let pad_info = info
                                .sink_pads
                                .iter()
                                .find(|p| Self::matches_pad_template(pad_name, &p.name));

                            if let Some(pad_info) = pad_info {
                                if !pad_info.properties.is_empty() {
                                    for prop_info in &pad_info.properties {
                                        Self::show_pad_property_from_info(
                                            ui, element, pad_name, prop_info,
                                        );
                                    }
                                } else {
                                    ui.small("No configurable properties");
                                }
                            } else {
                                ui.small(format!(
                                    "No metadata for pad (tried matching: {})",
                                    pad_name
                                ));
                            }
                        } else {
                            ui.small("No element metadata available");
                        }
                    });
                    ui.add_space(8.0);
                }
            });
    }

    /// Show the Output Pads tab content.
    fn show_output_pads_tab(
        ui: &mut Ui,
        element: &mut Element,
        element_info: Option<&ElementInfo>,
        actual_pads: &[String],
        focused_pad: Option<&str>,
    ) {
        ui.label("💡 Only modified properties are saved");

        ScrollArea::both()
            .id_salt("output_pads_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if actual_pads.is_empty() {
                    ui.label("No output pads connected");
                    return;
                }

                for pad_name in actual_pads {
                    // Highlight focused pad
                    let is_focused = focused_pad == Some(pad_name.as_str());
                    if is_focused {
                        ui.colored_label(
                            Color32::from_rgb(255, 200, 100),
                            format!(
                                "{} Output Pad: {}",
                                egui_phosphor::regular::CARET_RIGHT,
                                pad_name
                            ),
                        );
                    } else {
                        ui.label(format!("Output Pad: {}", pad_name));
                    }

                    ui.indent(pad_name, |ui| {
                        // Find properties for this pad from element_info
                        if let Some(info) = element_info {
                            // Check if there's a matching source pad in metadata (try template matching)
                            let pad_info = info
                                .src_pads
                                .iter()
                                .find(|p| Self::matches_pad_template(pad_name, &p.name));

                            if let Some(pad_info) = pad_info {
                                if !pad_info.properties.is_empty() {
                                    for prop_info in &pad_info.properties {
                                        Self::show_pad_property_from_info(
                                            ui, element, pad_name, prop_info,
                                        );
                                    }
                                } else {
                                    ui.small("No configurable properties");
                                }
                            } else {
                                ui.small(format!(
                                    "No metadata for pad (tried matching: {})",
                                    pad_name
                                ));
                            }
                        } else {
                            ui.small("No element metadata available");
                        }
                    });
                    ui.add_space(8.0);
                }
            });
    }

    /// Show the property inspector for the given block.
    /// Returns actions requested by the user.
    #[allow(clippy::too_many_arguments)]
    pub fn show_block(
        ui: &mut Ui,
        block: &mut BlockInstance,
        definition: &BlockDefinition,
        flow_id: Option<strom_types::FlowId>,
        audioanalyzer_data_store: &crate::audioanalyzer::AudioAnalyzerDataStore,
        meter_data_store: &crate::meter::MeterDataStore,
        spectrum_data_store: &crate::spectrum::SpectrumDataStore,
        loudness_data_store: &crate::loudness::LoudnessDataStore,
        latency_data_store: &crate::latency::LatencyDataStore,
        mediaplayer_data_store: &crate::mediaplayer::MediaPlayerDataStore,
        webrtc_stats_store: &crate::webrtc_stats::WebRtcStatsStore,
        srt_stats_store: &crate::srt_stats::SrtStatsStore,
        rtp_stats: Option<&strom_types::api::FlowStatsResponse>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
        video_devices: &[strom_types::discovery::DeviceResponse],
        audio_devices: &[strom_types::discovery::DeviceResponse],
        local_devices_loading: bool,
        qr_inline: &mut Option<(String, String)>,
        qr_cache: &mut crate::qr::QrCache,
        recorder_filename: Option<&str>,
        recorder_start_time: Option<instant::Instant>,
        block_thumbnail: Option<&egui::TextureHandle>,
        taken_endpoint_ids: &std::collections::HashSet<String>,
    ) -> BlockInspectorResult {
        let block_id = block.id.clone();
        let mut result = BlockInspectorResult::default();
        let mut device_picker_actions = DevicePickerActions::default();

        ui.push_id("selected_inspector", |ui| {
            // Outer scroll area for entire block inspector
            ScrollArea::both()
                .id_salt("inspector_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
            // Block info in collapsible section
            egui::CollapsingHeader::new(&definition.name)
                .default_open(false)
                .show(ui, |ui| {
                    // Block ID (read-only)
                    ui.horizontal(|ui| {
                        ui.label("ID:");
                        ui.monospace(&block.id);
                    });

                    // Block description
                    if !definition.description.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Description:");
                            ui.label(&definition.description);
                        });
                    }
                });

            // Editable instance name
            ui.horizontal(|ui| {
                ui.label("Name:");
                let mut name_text = block.name.clone().unwrap_or_default();
                if ui.text_edit_singleline(&mut name_text).changed() {
                    block.name = if name_text.is_empty() {
                        None
                    } else {
                        Some(name_text)
                    };
                }
                if block.name.is_some()
                    && ui.small_button(egui_phosphor::regular::X).on_hover_text("Clear name").clicked()
                {
                    block.name = None;
                }
            });

            // Check if this block type has action buttons
            let has_action_buttons = matches!(
                definition.id.as_str(),
                "builtin.aes67_input"
                    | "builtin.ndi_input"
                    | "builtin.glcompositor"
                    | "builtin.compositor"
                    | "builtin.media_player"
                    | "builtin.mpegtssrt_output"
                    | "builtin.vision_mixer"
                    | "builtin.whep_output"
                    | "builtin.whip_input"
                    | "builtin.thumbnail"
            );

            // Only show separator before action buttons if there are any
            if has_action_buttons {
                ui.separator();
            }

            // Block-specific action buttons
            // Browse Streams button for AES67 Input blocks
            if definition.id == "builtin.aes67_input"
                && ui
                    .button(format!("{} Streams", egui_phosphor::regular::BROADCAST))
                    .on_hover_text("Select from discovered SAP streams")
                    .clicked()
            {
                result.browse_streams_requested = true;
            }

            // Browse NDI Sources button for NDI Input blocks
            if definition.id == "builtin.ndi_input"
                && ui
                    .button(format!("{} NDI Sources", egui_phosphor::regular::BROADCAST))
                    .on_hover_text("Select from discovered NDI sources")
                    .clicked()
            {
                result.browse_ndi_sources_requested = true;
            }

            // Open Mixer button for mixer blocks
            if definition.id == "builtin.mixer"
                && ui.button(format!("{} Mixer", egui_phosphor::regular::SLIDERS)).clicked()
            {
                crate::app::set_local_storage("open_mixer_editor", &block.id);
            }

            // Edit Layout button for compositor blocks
            if (definition.id == "builtin.glcompositor" || definition.id == "builtin.compositor")
                && ui.button(format!("{} Layout", egui_phosphor::regular::PENCIL_SIMPLE)).clicked()
            {
                crate::app::set_local_storage("open_compositor_editor", &block.id);
            }

            // Edit Playlist button for media player blocks
            if definition.id == "builtin.media_player"
                && ui.button(format!("{} Playlist", egui_phosphor::regular::PLAYLIST)).clicked()
            {
                crate::app::set_local_storage("open_playlist_editor", &block.id);
            }

            // Edit Routing Matrix button for Audio Router blocks
            if definition.id == "builtin.audiorouter"
                && ui.button(format!("{} Routing", egui_phosphor::regular::GRAPH)).clicked()
            {
                crate::app::set_local_storage("open_routing_editor", &block.id);
            }

            // Download VLC Playlist button for MPEG-TS/SRT output blocks (only in listener mode)
            if definition.id == "builtin.mpegtssrt_output" {
                // Get SRT URI from block properties
                let srt_uri = block
                    .properties
                    .get("srt_uri")
                    .and_then(|v| match v {
                        PropertyValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| DEFAULT_SRT_OUTPUT_URI.to_string());

                // Only show buttons if in listener mode (VLC can connect to us)
                // Default SRT mode is caller, so we need explicit mode=listener
                if srt_uri.contains("mode=listener") {
                    // Use fixed network-caching for VLC (not tied to SRT buffer latency)
                    // 1000ms is a reasonable default for smooth playback
                    let network_caching_ms = 1000;

                    ui.horizontal(|ui| {
                        // Open in VLC button (saves and opens automatically in native mode)
                        if ui
                            .button(format!("{} Open in VLC", egui_phosphor::regular::PLAY))
                            .on_hover_text("Download XSPF playlist and open in VLC")
                            .clicked()
                        {
                            result.vlc_playlist_requested =
                                Some((srt_uri.clone(), network_caching_ms));
                        }

                        // Download-only button (native mode only - lets user save to specific location)
                        #[cfg(not(target_arch = "wasm32"))]
                        if ui
                            .button(format!("{} Download", egui_phosphor::regular::DOWNLOAD_SIMPLE))
                            .on_hover_text("Download XSPF playlist file")
                            .clicked()
                        {
                            result.vlc_playlist_download_only =
                                Some((srt_uri.clone(), network_caching_ms));
                        }
                    });
                }
            }

            // Open WHEP Player button for WHEP Output blocks
            if definition.id == "builtin.whep_output" {
                // Get endpoint_id from runtime_data (set when flow starts)
                // or from properties if user configured it
                let endpoint_id = block
                    .runtime_data
                    .as_ref()
                    .and_then(|rd| rd.get("whep_endpoint_id").cloned())
                    .or_else(|| {
                        block.properties.get("endpoint_id").and_then(|v| match v {
                            PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        })
                    });

                if let Some(endpoint_id) = endpoint_id {
                    ui.horizontal(|ui| {
                        if ui
                            .button(egui_phosphor::regular::QR_CODE)
                            .on_hover_text("Toggle QR code for mobile access")
                            .clicked()
                        {
                            result.show_qr_whep = Some(endpoint_id.clone());
                        }
                        if ui
                            .button(format!("{} Player", egui_phosphor::regular::ARROW_SQUARE_OUT))
                            .on_hover_text("Open WHEP player in browser")
                            .clicked()
                        {
                            result.whep_player_url = Some(endpoint_id.clone());
                        }
                        if ui
                            .button(egui_phosphor::regular::COPY)
                            .on_hover_text("Copy player URL to clipboard")
                            .clicked()
                        {
                            result.copy_whep_url_requested = Some(endpoint_id.clone());
                        }
                    });

                    // Render inline QR code below buttons (only for this block)
                    if let Some((_, ref url)) = qr_inline.as_ref().filter(|(bid, _)| bid == &block_id) {
                        ui.add_space(4.0);
                        if let Some(texture) = qr_cache.get_or_create(ui.ctx(), url) {
                            ui.image(egui::load::SizedTexture::new(
                                texture.id(),
                                egui::vec2(200.0, 200.0),
                            ));
                        }
                        ui.label(egui::RichText::new(url.as_str()).monospace().small());
                    }
                } else {
                    // Flow not running, show disabled button with tooltip
                    ui.add_enabled_ui(false, |ui| {
                        ui.button(format!("{} Player", egui_phosphor::regular::ARROW_SQUARE_OUT))
                            .on_hover_text("Start the flow to enable player")
                            .on_disabled_hover_text("Start the flow to enable player");
                    });
                }
            }

            // Open Vision Mixer control page
            if definition.id == "builtin.vision_mixer" {
                if let Some(fid) = flow_id {
                    if ui
                        .button(format!(
                            "{} Vision Mixer",
                            egui_phosphor::regular::ARROW_SQUARE_OUT
                        ))
                        .on_hover_text("Open vision mixer control page in browser")
                        .clicked()
                    {
                        result.vision_mixer_url = Some((fid, block.id.clone()));
                    }
                }
            }

            // Open WHIP Ingest button for WHIP Input blocks
            if definition.id == "builtin.whip_input" {
                let endpoint_id = block
                    .runtime_data
                    .as_ref()
                    .and_then(|rd| rd.get("whip_endpoint_id").cloned())
                    .or_else(|| {
                        block.properties.get("endpoint_id").and_then(|v| match v {
                            PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
                            _ => None,
                        })
                    });

                if let Some(endpoint_id) = endpoint_id {
                    ui.horizontal(|ui| {
                        if ui
                            .button(egui_phosphor::regular::QR_CODE)
                            .on_hover_text("Toggle QR code for mobile access")
                            .clicked()
                        {
                            result.show_qr_whip = Some(endpoint_id.clone());
                        }
                        if ui
                            .button(format!("{} Ingest", egui_phosphor::regular::ARROW_SQUARE_OUT))
                            .on_hover_text("Open WHIP ingest page in browser")
                            .clicked()
                        {
                            result.whip_ingest_url = Some(endpoint_id.clone());
                        }
                        if ui
                            .button(egui_phosphor::regular::COPY)
                            .on_hover_text("Copy ingest URL to clipboard")
                            .clicked()
                        {
                            result.copy_whip_url_requested = Some(endpoint_id.clone());
                        }
                    });

                    // Render inline QR code below buttons (only for this block)
                    if let Some((_, ref url)) = qr_inline.as_ref().filter(|(bid, _)| bid == &block_id) {
                        ui.add_space(4.0);
                        if let Some(texture) = qr_cache.get_or_create(ui.ctx(), url) {
                            ui.image(egui::load::SizedTexture::new(
                                texture.id(),
                                egui::vec2(200.0, 200.0),
                            ));
                        }
                        ui.label(egui::RichText::new(url.as_str()).monospace().small());
                    }
                } else {
                    ui.add_enabled_ui(false, |ui| {
                        ui.button(format!("{} Ingest", egui_phosphor::regular::ARROW_SQUARE_OUT))
                            .on_hover_text("Start the flow to enable ingest page")
                            .on_disabled_hover_text("Start the flow to enable ingest page");
                    });
                }
            }

            // Thumbnail preview for thumbnail blocks
            if definition.id == "builtin.thumbnail" {
                if let Some(texture) = block_thumbnail {
                    ui.separator();
                    let available_width = ui.available_width();
                    let aspect = texture.size()[1] as f32 / texture.size()[0] as f32;
                    let size = egui::vec2(available_width, available_width * aspect);
                    ui.image(egui::load::SizedTexture::new(texture.id(), size));
                }
            }

            // Separator before properties section
            // (also serves as separator after action buttons if there were any)
            ui.separator();
            ui.label("💡 Only modified properties are saved");

            ScrollArea::both()
                .id_salt("block_properties_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if !definition.exposed_properties.is_empty() {
                        // Special handling for Audio Router - show only relevant properties
                        if definition.id == "builtin.audiorouter" {
                            Self::show_audiorouter_properties(
                                ui,
                                block,
                                definition,
                                flow_id,
                                network_interfaces,
                                available_channels,
                            );
                        } else if definition.id == "builtin.mixer" {
                            Self::show_mixer_properties(
                                ui,
                                block,
                                definition,
                                flow_id,
                                network_interfaces,
                                available_channels,
                                video_devices,
                                audio_devices,
                                local_devices_loading,
                                taken_endpoint_ids,
                                &mut device_picker_actions,
                                &mut result,
                            );
                        } else {
                            for exposed_prop in &definition.exposed_properties {
                                let changed = Self::show_exposed_property(
                                    ui,
                                    block,
                                    exposed_prop,
                                    definition,
                                    flow_id,
                                    network_interfaces,
                                    available_channels,
                                    video_devices,
                                    audio_devices,
                                    local_devices_loading,
                                    taken_endpoint_ids,
                                    &mut device_picker_actions,
                                );

                                // For live properties, send updates directly to the pipeline
                                if changed && exposed_prop.live {
                                    if let Some(fid) = flow_id {
                                        let element_id = format!(
                                            "{}:{}",
                                            block.id, exposed_prop.mapping.element_id
                                        );
                                        // Use the current value (or default) for the update
                                        let value = block
                                            .properties
                                            .get(&exposed_prop.name)
                                            .or(exposed_prop.default_value.as_ref())
                                            .cloned();
                                        if let Some(value) = value {
                                            result.live_property_updates.push(
                                                LivePropertyUpdate {
                                                    flow_id: fid,
                                                    element_id,
                                                    property_name: exposed_prop
                                                        .mapping
                                                        .property_name
                                                        .clone(),
                                                    value,
                                                },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        ui.label("This block has no configurable properties");
                    }

                    // Show audio analyzer visualization for audio analyzer blocks
                    if definition.id == "builtin.audioanalyzer" {
                        ui.separator();
                        if let Some(flow_id) = flow_id {
                            if let Some(analyzer_data) =
                                audioanalyzer_data_store.get(&flow_id, &block.id)
                            {
                                crate::audioanalyzer::show_full(ui, analyzer_data);
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "No audio analyzer data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Waveform and vectorscope will appear when audio is flowing through this block.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "No flow selected",
                            );
                        }
                    }

                    // Show meter visualization for meter blocks
                    if definition.id == "builtin.meter" {
                        ui.separator();

                        if let Some(flow_id) = flow_id {
                            if let Some(meter_data) = meter_data_store.get(&flow_id, &block.id) {
                                tracing::debug!("Found meter data, calling show_full");
                                crate::meter::show_full(ui, meter_data);
                            } else {
                                tracing::debug!("No meter data found for this block");
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ No audio level data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Meter data will appear when audio is flowing through this block.");
                            }
                        } else {
                            tracing::debug!("No flow_id available");
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ No flow selected",
                            );
                        }
                    }

                    // Show spectrum visualization for spectrum blocks
                    if definition.id == "builtin.spectrum" {
                        ui.separator();
                        if let Some(flow_id) = flow_id {
                            if let Some(spectrum_data) = spectrum_data_store.get(&flow_id, &block.id) {
                                crate::spectrum::show_full(ui, spectrum_data);
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "No spectrum data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Spectrum data will appear when audio is flowing through this block.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "No flow selected",
                            );
                        }
                    }

                    // Show loudness visualization for loudness blocks
                    if definition.id == "builtin.loudness" {
                        ui.separator();
                        if let Some(flow_id) = flow_id {
                            if ui.button("Reset Measurements").clicked() {
                                result.loudness_reset_requested =
                                    Some((flow_id, block.id.clone()));
                            }
                            if let Some(loudness_data) = loudness_data_store.get(&flow_id, &block.id) {
                                crate::loudness::show_full(ui, loudness_data);
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "No loudness data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Loudness data will appear when audio is flowing through this block.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "No flow selected",
                            );
                        }
                    }

                    // Show media player controls for media player blocks
                    if definition.id == "builtin.media_player" {
                        ui.separator();
                        if let Some(flow_id) = flow_id {
                            if let Some(player_data) =
                                mediaplayer_data_store.get(&flow_id, &block.id)
                            {
                                if let Some(action) =
                                    crate::mediaplayer::show_full(ui, player_data)
                                {
                                    let value = if let Some(pos) = action.1 {
                                        format!("{}:{}:{}", block.id, action.0, pos)
                                    } else {
                                        format!("{}:{}", block.id, action.0)
                                    };
                                    crate::app::set_local_storage("player_action", &value);
                                }
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "No media player data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Player controls will appear when the flow is running.");
                            }
                        }
                    }

                    // Show recording status, duration counter, and split button for recorder blocks
                    if definition.id == "builtin.recorder" {
                        ui.separator();
                        if let Some(start) = recorder_start_time {
                            let elapsed = start.elapsed();
                            let total_secs = elapsed.as_secs();
                            let h = total_secs / 3600;
                            let m = (total_secs % 3600) / 60;
                            let s = total_secs % 60;
                            ui.horizontal(|ui| {
                                ui.label("Recording:");
                                ui.monospace(format!("{:02}:{:02}:{:02}", h, m, s));
                            });
                        }
                        if let Some(filename) = recorder_filename {
                            let short_name = std::path::Path::new(filename)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(filename);
                            ui.horizontal(|ui| {
                                ui.monospace(short_name).on_hover_text(filename);
                                if ui
                                    .button(egui_phosphor::regular::DOWNLOAD_SIMPLE)
                                    .on_hover_text("Download recording")
                                    .clicked()
                                {
                                    result.recorder_download_requested =
                                        Some(filename.to_string());
                                }
                            });
                        }
                        if let Some(flow_id) = flow_id {
                            if ui.button("Split Now").clicked() {
                                result.recorder_split_requested =
                                    Some((flow_id, block.id.clone()));
                            }
                        } else {
                            ui.add_enabled(false, egui::Button::new("Split Now"));
                        }
                    }

                    // Show latency visualization for latency blocks
                    if definition.id == "builtin.latency" {
                        ui.separator();
                        tracing::debug!("Checking for latency data: flow_id={:?}, block_id={}", flow_id, block.id);
                        if let Some(flow_id) = flow_id {
                            if let Some(latency_data) = latency_data_store.get(&flow_id, &block.id) {
                                tracing::debug!("Found latency data, calling show_full");
                                crate::latency::show_full(ui, &block.id, latency_data);
                            } else {
                                tracing::debug!("No latency data found for this block");
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "No latency data available",
                                );
                                ui.add_space(4.0);
                                ui.small("Latency measurements will appear when audio is flowing through this block. Note: The audiolatency element measures round-trip latency using periodic ticks (1 second intervals).");
                            }
                        } else {
                            tracing::debug!("No flow_id available for latency block");
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "No flow selected",
                            );
                        }
                    }

                    // Show SDP for AES67 output blocks
                    if definition.id == "builtin.aes67_output" {
                        ui.separator();
                        ui.heading("📡 SDP (Session Description)");
                        ui.add_space(4.0);

                        // Get SDP from runtime_data (only available when flow is running)
                        let sdp = block
                            .runtime_data
                            .as_ref()
                            .and_then(|data| data.get("sdp"))
                            .map(|s| s.as_str());

                        if let Some(mut sdp_text) = sdp {
                            ui.horizontal(|ui| {
                                ui.label("Copy this SDP to configure receivers:");
                                if ui.button(egui_phosphor::regular::COPY)
                                    .on_hover_text("Copy SDP to clipboard")
                                    .clicked() {
                                    crate::clipboard::copy_text_with_ctx(ui.ctx(), sdp_text);
                                }
                            });
                            ui.add_space(4.0);

                            // Display SDP in a code-style text box
                            ui.add(
                                egui::TextEdit::multiline(&mut sdp_text)
                                    .desired_rows(12)
                                    .desired_width(f32::INFINITY)
                                    .code_editor()
                                    .interactive(false),
                            );
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ SDP is only available when the flow is running",
                            );
                            ui.add_space(4.0);
                            ui.small("Start the flow to generate SDP based on the actual stream capabilities.");
                        }
                    }

                    // Show WebRTC statistics for WHIP/WHEP blocks
                    if definition.id == "builtin.whep_input"
                        || definition.id == "builtin.whep_output"
                        || definition.id == "builtin.whip_output"
                        || definition.id == "builtin.whip_input"
                    {
                        ui.separator();
                        ui.heading("📊 WebRTC Statistics");
                        ui.add_space(4.0);

                        if let Some(flow_id) = flow_id {
                            if let Some(stats) = webrtc_stats_store.get(&flow_id) {
                                // Filter connections to only those belonging to this block
                                // Connection names are formatted as "block_id:element_name:..."
                                let block_prefix = format!("{}:", block.id);
                                let filtered_connections: std::collections::HashMap<_, _> = stats
                                    .connections
                                    .iter()
                                    .filter(|(name, _)| name.starts_with(&block_prefix))
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect();

                                let block_stats = strom_types::api::WebRtcStats {
                                    connections: filtered_connections,
                                };

                                if !block_stats.connections.is_empty() {
                                    crate::webrtc_stats::show_full(ui, &block_stats);
                                } else {
                                    ui.colored_label(
                                        Color32::from_rgb(200, 200, 100),
                                        "⚠ No WebRTC connections established",
                                    );
                                    ui.add_space(4.0);
                                    ui.small("WebRTC statistics will appear when the connection is established.");
                                }
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ WebRTC statistics not available",
                                );
                                ui.add_space(4.0);
                                ui.small("Start the flow to see WebRTC statistics.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ No flow selected",
                            );
                        }
                    }

                    // Show SRT statistics for SRT input/output blocks
                    if crate::srt_stats::is_srt_block_def(&definition.id) {
                        ui.separator();
                        ui.heading("📊 SRT Statistics");
                        ui.add_space(4.0);

                        if let Some(flow_id) = flow_id {
                            if let Some((block_stats, filtered_rates)) =
                                srt_stats_store.snapshot_for_block(&flow_id, &block.id)
                            {
                                let rates_opt = if filtered_rates.is_empty() {
                                    None
                                } else {
                                    Some(&filtered_rates)
                                };
                                crate::srt_stats::show_full(ui, &block_stats, rates_opt);
                            } else if srt_stats_store.get(&flow_id).is_some() {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ No SRT element data yet",
                                );
                                ui.add_space(4.0);
                                ui.small("SRT statistics appear once the pipeline is running.");
                            } else {
                                ui.colored_label(
                                    Color32::from_rgb(200, 200, 100),
                                    "⚠ SRT statistics not available",
                                );
                                ui.add_space(4.0);
                                ui.small("Start the flow to see SRT statistics.");
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ No flow selected",
                            );
                        }
                    }

                    // Show RTP statistics for AES67 input blocks
                    if definition.id == "builtin.aes67_input" {
                        ui.separator();
                        ui.heading("📊 RTP Statistics");
                        ui.add_space(4.0);

                        // Find RTP stats for this block
                        let block_stats = rtp_stats.and_then(|s| {
                            s.blocks.iter().find(|bs| bs.block_instance_id == block.id)
                        });

                        if let Some(block_stats) = block_stats {
                            // Group stats by jitterbuffer/SSRC
                            // Stats are prefixed with jitterbuffer name like "rtpjitterbuffer0_num_pushed"
                            // or have "(rtpjitterbuffer0)" in display_name
                            use std::collections::BTreeMap;
                            let mut grouped: BTreeMap<String, Vec<&strom_types::stats::Statistic>> =
                                BTreeMap::new();

                            for stat in &block_stats.stats {
                                // Extract jitterbuffer name from display_name "(rtpjitterbuffer0)"
                                // or from id prefix "rtpjitterbuffer0_"
                                let jb_name = if let Some(start) = stat.metadata.display_name.rfind('(')
                                {
                                    if let Some(end) = stat.metadata.display_name.rfind(')') {
                                        stat.metadata.display_name[start + 1..end].to_string()
                                    } else {
                                        "default".to_string()
                                    }
                                } else if let Some(underscore) = stat.id.find('_') {
                                    // Check if prefix looks like a jitterbuffer name
                                    let prefix = &stat.id[..underscore];
                                    if prefix.starts_with("rtpjitterbuffer") {
                                        prefix.to_string()
                                    } else {
                                        "default".to_string()
                                    }
                                } else {
                                    "default".to_string()
                                };

                                grouped.entry(jb_name).or_default().push(stat);
                            }

                            if grouped.len() <= 1 {
                                // Single jitterbuffer - show flat list
                                egui::Grid::new("rtp_stats_grid")
                                    .num_columns(2)
                                    .spacing([20.0, 4.0])
                                    .show(ui, |ui| {
                                        for stat in &block_stats.stats {
                                            let label = ui.label(&stat.metadata.display_name);
                                            label.on_hover_text(&stat.metadata.description);
                                            let formatted = stat.value.format();
                                            ui.monospace(&formatted);
                                            ui.end_row();
                                        }
                                    });
                            } else {
                                // Multiple jitterbuffers - show each in a collapsible (all open)
                                // Reverse order so newest (highest number) appears first
                                for (jb_name, stats) in grouped.iter().rev() {
                                    egui::CollapsingHeader::new(jb_name)
                                        .id_salt(format!("rtp_stats_{}", jb_name))
                                        .default_open(true)
                                        .show(ui, |ui| {
                                            egui::Grid::new(format!("rtp_stats_grid_{}", jb_name))
                                                .num_columns(2)
                                                .spacing([20.0, 4.0])
                                                .show(ui, |ui| {
                                                    for stat in stats {
                                                        // Remove jitterbuffer suffix from display name
                                                        let display_name = stat
                                                            .metadata
                                                            .display_name
                                                            .split(" (")
                                                            .next()
                                                            .unwrap_or(&stat.metadata.display_name);
                                                        let label = ui.label(display_name);
                                                        label.on_hover_text(&stat.metadata.description);
                                                        let formatted = stat.value.format();
                                                        ui.monospace(&formatted);
                                                        ui.end_row();
                                                    }
                                                });
                                        });
                                }
                            }
                        } else {
                            ui.colored_label(
                                Color32::from_rgb(200, 200, 100),
                                "⚠ Statistics are only available when the flow is running",
                            );
                            ui.add_space(4.0);
                            ui.small("Start the flow to see RTP jitterbuffer statistics.");
                        }
                    }

                });
            }); // outer ScrollArea
        });

        result.local_devices_needed = device_picker_actions.needed;
        result.local_devices_refresh_requested = device_picker_actions.refresh_requested;
        result
    }

    /// Show mixer properties grouped into collapsing sections.
    ///
    /// Properties are bucketed by name pattern into Config / Main Bus /
    /// per-Channel / per-Aux / per-Group sections. Channel sections also have
    /// nested sub-sections for Gate, Compressor, EQ, Aux Sends and Group
    /// Routing. Children of closed `CollapsingHeader`s are not rendered, which
    /// is what avoids the per-frame cost of drawing every property for
    /// high-channel-count configurations.
    ///
    /// Properties for inactive channels/aux/groups (beyond the configured
    /// counts) are filtered out during bucketing.
    #[allow(clippy::too_many_arguments)]
    fn show_mixer_properties(
        ui: &mut Ui,
        block: &mut BlockInstance,
        definition: &BlockDefinition,
        flow_id: Option<strom_types::FlowId>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
        video_devices: &[strom_types::discovery::DeviceResponse],
        audio_devices: &[strom_types::discovery::DeviceResponse],
        local_devices_loading: bool,
        taken_endpoint_ids: &std::collections::HashSet<String>,
        device_picker_actions: &mut DevicePickerActions,
        result: &mut BlockInspectorResult,
    ) {
        use strom_types::mixer::{MAX_AUX_BUSES, MAX_CHANNELS, MAX_GROUPS};

        let get_uint = |key: &str, default: usize| -> usize {
            block
                .properties
                .get(key)
                .and_then(|v| match v {
                    PropertyValue::String(s) => s.parse().ok(),
                    PropertyValue::UInt(n) => Some(*n as usize),
                    PropertyValue::Int(n) => Some(*n as usize),
                    _ => None,
                })
                .unwrap_or(default)
        };
        let num_ch = get_uint("num_channels", 8).min(MAX_CHANNELS);
        let num_aux = get_uint("num_aux_buses", 0).min(MAX_AUX_BUSES);
        let num_grp = get_uint("num_groups", 0).min(MAX_GROUPS);

        // Index buckets into definition.exposed_properties
        let mut config_idx: Vec<usize> = Vec::new();
        let mut main_idx: Vec<usize> = Vec::new();
        let mut ch_basic: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_hpf: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_gate: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_comp: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_eq: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_aux_sends: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut ch_groups: Vec<Vec<usize>> = vec![Vec::new(); num_ch];
        let mut aux_buckets: Vec<Vec<usize>> = vec![Vec::new(); num_aux];
        let mut grp_buckets: Vec<Vec<usize>> = vec![Vec::new(); num_grp];

        // Extract leading digits after `prefix`. Returns None if prefix doesn't match.
        fn extract_index(s: &str, prefix: &str) -> Option<usize> {
            let rest = s.strip_prefix(prefix)?;
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..end].parse().ok()
        }

        for (i, prop) in definition.exposed_properties.iter().enumerate() {
            let name = prop.name.as_str();

            // Mixer-level configuration
            if matches!(
                name,
                "num_channels"
                    | "num_aux_buses"
                    | "num_groups"
                    | "dsp_backend"
                    | "solo_mode"
                    | "force_live"
                    | "latency"
                    | "min_upstream_latency"
                    | "pfl_level"
            ) {
                config_idx.push(i);
                continue;
            }

            // Main bus (compressor, EQ, limiter, fader)
            if name.starts_with("main_") {
                main_idx.push(i);
                continue;
            }

            // ch{N}_...  — channel-scoped property
            if let Some(rest) = name.strip_prefix("ch") {
                if let Some(underscore) = rest.find('_') {
                    if let Ok(n) = rest[..underscore].parse::<usize>() {
                        // Recognized as a channel property: always consume (drop
                        // it entirely if the channel is inactive — never let it
                        // fall through into Configuration).
                        if !(1..=num_ch).contains(&n) {
                            continue;
                        }
                        let ch_i = n - 1;
                        let suffix = &rest[underscore + 1..];

                        let bucket = if suffix.starts_with("aux") {
                            // ch{N}_aux{M}_*  — only include if aux M is active
                            match extract_index(suffix, "aux") {
                                Some(m) if (1..=num_aux).contains(&m) => &mut ch_aux_sends[ch_i],
                                _ => continue,
                            }
                        } else if suffix.starts_with("to_grp") {
                            // ch{N}_to_grp{M} — only include if group M is active
                            match extract_index(suffix, "to_grp") {
                                Some(m) if (1..=num_grp).contains(&m) => &mut ch_groups[ch_i],
                                _ => continue,
                            }
                        } else if suffix.starts_with("gate") {
                            &mut ch_gate[ch_i]
                        } else if suffix.starts_with("comp") {
                            &mut ch_comp[ch_i]
                        } else if suffix.starts_with("eq") {
                            &mut ch_eq[ch_i]
                        } else if suffix.starts_with("hpf") {
                            &mut ch_hpf[ch_i]
                        } else {
                            &mut ch_basic[ch_i]
                        };
                        bucket.push(i);
                        continue;
                    }
                }
            }

            // aux{N}_*  — aux master property
            if let Some(rest) = name.strip_prefix("aux") {
                if let Some(underscore) = rest.find('_') {
                    if let Ok(n) = rest[..underscore].parse::<usize>() {
                        // Recognized as aux property: consume regardless of range.
                        if (1..=num_aux).contains(&n) {
                            aux_buckets[n - 1].push(i);
                        }
                        continue;
                    }
                }
            }

            // group{N}_*  — group master property
            if let Some(rest) = name.strip_prefix("group") {
                if let Some(underscore) = rest.find('_') {
                    if let Ok(n) = rest[..underscore].parse::<usize>() {
                        // Recognized as group property: consume regardless of range.
                        if (1..=num_grp).contains(&n) {
                            grp_buckets[n - 1].push(i);
                        }
                        continue;
                    }
                }
            }

            // Truly unrecognized — surface it under Configuration so a newly
            // added property is never silently hidden from the user.
            config_idx.push(i);
        }

        // Channel header labels: use the user-set ch{N}_label if present,
        // otherwise fall back to "Channel {N}". Precomputed so the immutable
        // borrow of `block.properties` is released before we re-borrow it
        // mutably to render properties.
        let channel_labels: Vec<String> = (1..=num_ch)
            .map(|ch| {
                let key = format!("ch{}_label", ch);
                match block.properties.get(&key) {
                    Some(PropertyValue::String(s)) if !s.is_empty() => {
                        format!("Channel {} — {}", ch, s)
                    }
                    _ => format!("Channel {}", ch),
                }
            })
            .collect();

        // --- Render ---

        // Configuration: open by default (small, always relevant)
        if !config_idx.is_empty() {
            egui::CollapsingHeader::new("Configuration")
                .id_salt("mixer_config")
                .default_open(true)
                .show(ui, |ui| {
                    Self::render_mixer_bucket(
                        ui,
                        block,
                        &config_idx,
                        definition,
                        flow_id,
                        network_interfaces,
                        available_channels,
                        video_devices,
                        audio_devices,
                        local_devices_loading,
                        taken_endpoint_ids,
                        device_picker_actions,
                        result,
                    );
                });
        }

        // Main bus
        if !main_idx.is_empty() {
            egui::CollapsingHeader::new("Main Bus")
                .id_salt("mixer_main")
                .default_open(false)
                .show(ui, |ui| {
                    Self::render_mixer_bucket(
                        ui,
                        block,
                        &main_idx,
                        definition,
                        flow_id,
                        network_interfaces,
                        available_channels,
                        video_devices,
                        audio_devices,
                        local_devices_loading,
                        taken_endpoint_ids,
                        device_picker_actions,
                        result,
                    );
                });
        }

        // Channels — one collapsing header per channel, with nested sub-sections
        if num_ch > 0 {
            egui::CollapsingHeader::new(format!("Channels ({})", num_ch))
                .id_salt("mixer_channels")
                .default_open(false)
                .show(ui, |ui| {
                    for ch in 1..=num_ch {
                        let ch_i = ch - 1;
                        egui::CollapsingHeader::new(&channel_labels[ch_i])
                            .id_salt(format!("mixer_ch_{}", ch))
                            .default_open(false)
                            .show(ui, |ui| {
                                // Basic controls (label, gain, pan, fader, mute, pfl, to_main)
                                Self::render_mixer_bucket(
                                    ui,
                                    block,
                                    &ch_basic[ch_i],
                                    definition,
                                    flow_id,
                                    network_interfaces,
                                    available_channels,
                                    video_devices,
                                    audio_devices,
                                    local_devices_loading,
                                    taken_endpoint_ids,
                                    device_picker_actions,
                                    result,
                                );

                                // High-pass filter
                                if !ch_hpf[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("High-Pass Filter")
                                        .id_salt(format!("mixer_ch_{}_hpf", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_hpf[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }

                                // Gate
                                if !ch_gate[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("Gate")
                                        .id_salt(format!("mixer_ch_{}_gate", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_gate[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }

                                // Compressor
                                if !ch_comp[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("Compressor")
                                        .id_salt(format!("mixer_ch_{}_comp", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_comp[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }

                                // EQ
                                if !ch_eq[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("EQ")
                                        .id_salt(format!("mixer_ch_{}_eq", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_eq[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }

                                // Aux sends
                                if !ch_aux_sends[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("Aux Sends")
                                        .id_salt(format!("mixer_ch_{}_aux", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_aux_sends[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }

                                // Group routing
                                if !ch_groups[ch_i].is_empty() {
                                    egui::CollapsingHeader::new("Group Routing")
                                        .id_salt(format!("mixer_ch_{}_groups", ch))
                                        .default_open(false)
                                        .show(ui, |ui| {
                                            Self::render_mixer_bucket(
                                                ui,
                                                block,
                                                &ch_groups[ch_i],
                                                definition,
                                                flow_id,
                                                network_interfaces,
                                                available_channels,
                                                video_devices,
                                                audio_devices,
                                                local_devices_loading,
                                                taken_endpoint_ids,
                                                device_picker_actions,
                                                result,
                                            );
                                        });
                                }
                            });
                    }
                });
        }

        // Aux Buses
        if num_aux > 0 {
            egui::CollapsingHeader::new(format!("Aux Buses ({})", num_aux))
                .id_salt("mixer_aux")
                .default_open(false)
                .show(ui, |ui| {
                    for aux in 1..=num_aux {
                        let bucket = &aux_buckets[aux - 1];
                        if bucket.is_empty() {
                            continue;
                        }
                        egui::CollapsingHeader::new(format!("Aux {}", aux))
                            .id_salt(format!("mixer_aux_{}", aux))
                            .default_open(false)
                            .show(ui, |ui| {
                                Self::render_mixer_bucket(
                                    ui,
                                    block,
                                    bucket,
                                    definition,
                                    flow_id,
                                    network_interfaces,
                                    available_channels,
                                    video_devices,
                                    audio_devices,
                                    local_devices_loading,
                                    taken_endpoint_ids,
                                    device_picker_actions,
                                    result,
                                );
                            });
                    }
                });
        }

        // Groups
        if num_grp > 0 {
            egui::CollapsingHeader::new(format!("Groups ({})", num_grp))
                .id_salt("mixer_groups")
                .default_open(false)
                .show(ui, |ui| {
                    for grp in 1..=num_grp {
                        let bucket = &grp_buckets[grp - 1];
                        if bucket.is_empty() {
                            continue;
                        }
                        egui::CollapsingHeader::new(format!("Group {}", grp))
                            .id_salt(format!("mixer_grp_{}", grp))
                            .default_open(false)
                            .show(ui, |ui| {
                                Self::render_mixer_bucket(
                                    ui,
                                    block,
                                    bucket,
                                    definition,
                                    flow_id,
                                    network_interfaces,
                                    available_channels,
                                    video_devices,
                                    audio_devices,
                                    local_devices_loading,
                                    taken_endpoint_ids,
                                    device_picker_actions,
                                    result,
                                );
                            });
                    }
                });
        }
    }

    /// Render a list of property indices into `definition.exposed_properties`,
    /// collecting any live-update events into `result`.
    #[allow(clippy::too_many_arguments)]
    fn render_mixer_bucket(
        ui: &mut Ui,
        block: &mut BlockInstance,
        indices: &[usize],
        definition: &BlockDefinition,
        flow_id: Option<strom_types::FlowId>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
        video_devices: &[strom_types::discovery::DeviceResponse],
        audio_devices: &[strom_types::discovery::DeviceResponse],
        local_devices_loading: bool,
        taken_endpoint_ids: &std::collections::HashSet<String>,
        device_picker_actions: &mut DevicePickerActions,
        result: &mut BlockInspectorResult,
    ) {
        for &i in indices {
            let exposed_prop = &definition.exposed_properties[i];
            let changed = Self::show_exposed_property(
                ui,
                block,
                exposed_prop,
                definition,
                flow_id,
                network_interfaces,
                available_channels,
                video_devices,
                audio_devices,
                local_devices_loading,
                taken_endpoint_ids,
                device_picker_actions,
            );

            if changed && exposed_prop.live {
                if let Some(fid) = flow_id {
                    let element_id = format!("{}:{}", block.id, exposed_prop.mapping.element_id);
                    let value = block
                        .properties
                        .get(&exposed_prop.name)
                        .or(exposed_prop.default_value.as_ref())
                        .cloned();
                    if let Some(value) = value {
                        result.live_property_updates.push(LivePropertyUpdate {
                            flow_id: fid,
                            element_id,
                            property_name: exposed_prop.mapping.property_name.clone(),
                            value,
                        });
                    }
                }
            }
        }
    }

    /// Show Audio Router properties with filtered view.
    fn show_audiorouter_properties(
        ui: &mut Ui,
        block: &mut BlockInstance,
        definition: &BlockDefinition,
        flow_id: Option<strom_types::FlowId>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
    ) {
        // Helper to get property value (from block or default)
        let get_uint_prop = |name: &str| -> usize {
            block
                .properties
                .get(name)
                .and_then(|v| match v {
                    PropertyValue::UInt(u) => Some(*u as usize),
                    PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                    _ => None,
                })
                .or_else(|| {
                    definition
                        .exposed_properties
                        .iter()
                        .find(|p| p.name == name)
                        .and_then(|p| p.default_value.as_ref())
                        .and_then(|v| match v {
                            PropertyValue::UInt(u) => Some(*u as usize),
                            PropertyValue::Int(i) if *i > 0 => Some(*i as usize),
                            _ => None,
                        })
                })
                .unwrap_or(2)
        };

        // Get current num_inputs and num_outputs
        let num_inputs = get_uint_prop("num_inputs").clamp(1, 8);
        let num_outputs = get_uint_prop("num_outputs").clamp(1, 8);

        // Show num_inputs property
        if let Some(prop) = definition
            .exposed_properties
            .iter()
            .find(|p| p.name == "num_inputs")
        {
            let mut sink = DevicePickerActions::default();
            Self::show_exposed_property(
                ui,
                block,
                prop,
                definition,
                flow_id,
                network_interfaces,
                available_channels,
                &[],
                &[],
                false,
                &std::collections::HashSet::new(),
                &mut sink,
            );
        }

        // Show relevant input channel properties
        for i in 0..num_inputs {
            let prop_name = format!("input_{}_channels", i);
            if let Some(prop) = definition
                .exposed_properties
                .iter()
                .find(|p| p.name == prop_name)
            {
                let mut sink = DevicePickerActions::default();
                Self::show_exposed_property(
                    ui,
                    block,
                    prop,
                    definition,
                    flow_id,
                    network_interfaces,
                    available_channels,
                    &[],
                    &[],
                    false,
                    &std::collections::HashSet::new(),
                    &mut sink,
                );
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // Show num_outputs property
        if let Some(prop) = definition
            .exposed_properties
            .iter()
            .find(|p| p.name == "num_outputs")
        {
            let mut sink = DevicePickerActions::default();
            Self::show_exposed_property(
                ui,
                block,
                prop,
                definition,
                flow_id,
                network_interfaces,
                available_channels,
                &[],
                &[],
                false,
                &std::collections::HashSet::new(),
                &mut sink,
            );
        }

        // Show relevant output channel properties
        for i in 0..num_outputs {
            let prop_name = format!("output_{}_channels", i);
            if let Some(prop) = definition
                .exposed_properties
                .iter()
                .find(|p| p.name == prop_name)
            {
                let mut sink = DevicePickerActions::default();
                Self::show_exposed_property(
                    ui,
                    block,
                    prop,
                    definition,
                    flow_id,
                    network_interfaces,
                    available_channels,
                    &[],
                    &[],
                    false,
                    &std::collections::HashSet::new(),
                    &mut sink,
                );
            }
        }

        // Note: routing_matrix property is NOT shown as a text field
        // The modal routing editor handles all routing configuration
    }

    /// Show an exposed property editor. Returns true if the value was changed.
    #[allow(clippy::too_many_arguments)]
    fn show_exposed_property(
        ui: &mut Ui,
        block: &mut BlockInstance,
        exposed_prop: &ExposedProperty,
        definition: &BlockDefinition,
        _flow_id: Option<strom_types::FlowId>,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        available_channels: &[strom_types::api::AvailableOutput],
        video_devices: &[strom_types::discovery::DeviceResponse],
        audio_devices: &[strom_types::discovery::DeviceResponse],
        local_devices_loading: bool,
        taken_endpoint_ids: &std::collections::HashSet<String>,
        device_picker_actions: &mut DevicePickerActions,
    ) -> bool {
        let prop_name = &exposed_prop.name;
        let display_label = &exposed_prop.label;
        let default_value = exposed_prop.default_value.as_ref();
        let is_multiline = matches!(
            exposed_prop.property_type,
            strom_types::block::PropertyType::Multiline
        );

        // Get current value or use default
        let mut current_value = block.properties.get(prop_name).cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();
        }

        // For string properties without a value, initialize to empty string
        if current_value.is_none() {
            if let strom_types::block::PropertyType::String = &exposed_prop.property_type {
                current_value = Some(PropertyValue::String(String::new()));
            }
        }

        let mut property_changed = false;
        let is_live = exposed_prop.live;

        // For multiline, use vertical layout
        if is_multiline {
            // Property label with indicator
            ui.horizontal(|ui| {
                if has_custom_value {
                    ui.colored_label(
                        Color32::from_rgb(150, 100, 255), // Purple for blocks
                        format!("• {}:", display_label),
                    );
                } else {
                    ui.label(format!("{}:", display_label));
                }

                // Reset button if modified
                if has_custom_value
                    && ui
                        .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                        .on_hover_text("Reset to default")
                        .clicked()
                {
                    block.properties.remove(prop_name);
                }
            });

            // Multiline editor
            let mut text = match current_value {
                Some(PropertyValue::String(s)) => s,
                _ => String::new(),
            };

            let response = ui.add(
                egui::TextEdit::multiline(&mut text)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .code_editor(),
            );

            if response.changed() {
                property_changed = true;
                // Only save if different from default
                if let Some(PropertyValue::String(default)) = default_value {
                    if text != *default {
                        block
                            .properties
                            .insert(prop_name.clone(), PropertyValue::String(text));
                    } else {
                        block.properties.remove(prop_name);
                    }
                } else if !text.is_empty() {
                    block
                        .properties
                        .insert(prop_name.clone(), PropertyValue::String(text));
                } else {
                    block.properties.remove(prop_name);
                }
            }
        } else {
            // For non-multiline, use horizontal layout
            let changed_in_row = ui.horizontal(|ui| {
                // Show property label with indicator if modified
                if has_custom_value {
                    ui.colored_label(
                        Color32::from_rgb(150, 100, 255), // Purple for blocks
                        format!("• {}:", display_label),
                    );
                } else {
                    ui.label(format!("{}:", display_label));
                }

                // Show LIVE badge for real-time properties
                if is_live {
                    let badge = ui.colored_label(
                        Color32::from_rgb(50, 200, 50),
                        "LIVE",
                    );
                    badge.on_hover_text(
                        "This property updates the audio pipeline in real-time.\nSave the flow to persist changes across restarts.",
                    );
                }

                let mut changed = false;

                if let Some(mut value) = current_value {
                    // Special handling for InterInput channel property - show dropdown with available channels
                    let is_inter_input_channel =
                        definition.id == "builtin.inter_input" && prop_name == "channel";

                    // Special handling for AudioGain gain property - show dB slider
                    let is_audiogain_gain =
                        definition.id == "builtin.audiogain" && prop_name == "gain";

                    changed = if is_inter_input_channel {
                        Self::show_inter_channel_editor(ui, &mut value, available_channels)
                    } else if is_audiogain_gain {
                        Self::show_db_gain_editor(ui, &mut value)
                    } else {
                        // Check property type for special handling
                        match &exposed_prop.property_type {
                            strom_types::block::PropertyType::Enum { values } => {
                                Self::show_block_enum_editor(ui, &mut value, values)
                            }
                            strom_types::block::PropertyType::NetworkInterface => {
                                Self::show_network_interface_editor(
                                    ui,
                                    &mut value,
                                    network_interfaces,
                                )
                            }
                            strom_types::block::PropertyType::Device { category } => {
                                device_picker_actions.needed = true;
                                use strom_types::discovery::DeviceCategory;
                                let (devices, kind_label): (&[_], &str) = match category {
                                    DeviceCategory::VideoSource => {
                                        (video_devices, "video source")
                                    }
                                    DeviceCategory::AudioSource => {
                                        (audio_devices, "audio source")
                                    }
                                    // No UI lists for these yet — picker will
                                    // show an empty list with the refresh
                                    // button, which is intentional until we
                                    // wire up extra fetches.
                                    DeviceCategory::AudioSink
                                    | DeviceCategory::NetworkSource
                                    | DeviceCategory::Other => (&[], "device"),
                                };
                                Self::show_local_device_editor(
                                    ui,
                                    &mut value,
                                    devices,
                                    kind_label,
                                    local_devices_loading,
                                    device_picker_actions,
                                )
                            }
                            _ => {
                                // Convert block::PropertyType to element::PropertyType for other types
                                let prop_type =
                                    Self::convert_block_prop_type(&exposed_prop.property_type);
                                Self::show_property_editor(
                                    ui,
                                    &mut value,
                                    prop_type.as_ref(),
                                    default_value,
                                    false, // Block properties are always writable
                                )
                            }
                        }
                    };

                    if changed {
                        // Only save if different from default
                        if let Some(default) = default_value {
                            if !Self::values_equal(&value, default) {
                                block.properties.insert(prop_name.clone(), value);
                            } else {
                                block.properties.remove(prop_name);
                            }
                        } else {
                            block.properties.insert(prop_name.clone(), value);
                        }
                    }
                }

                // Reset button if modified
                if has_custom_value
                    && ui
                        .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                        .on_hover_text("Reset to default")
                        .clicked()
                {
                    block.properties.remove(prop_name);
                    changed = true;
                }

                changed
            });
            property_changed = changed_in_row.inner;
        }

        // Show description
        if !exposed_prop.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&exposed_prop.description);
            });
        }

        // Show warning if endpoint_id is already taken by another block
        if prop_name == "endpoint_id"
            && (definition.id == "builtin.whip_input" || definition.id == "builtin.whep_output")
        {
            let trimmed = block.properties.get("endpoint_id").and_then(|v| match v {
                PropertyValue::String(s) => {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t.to_string())
                    }
                }
                _ => None,
            });
            if let Some(val) = &trimmed {
                if taken_endpoint_ids.contains(val) {
                    ui.indent(prop_name, |ui| {
                        ui.colored_label(
                            Color32::from_rgb(255, 180, 50),
                            format!(
                                "\u{26a0} Endpoint '{}' is already in use by another block",
                                val
                            ),
                        );
                    });
                }
            }
        }

        // Add spacing after each property
        ui.add_space(8.0);

        property_changed
    }

    fn show_pad_property_from_info(
        ui: &mut Ui,
        element: &mut Element,
        pad_name: &str,
        prop_info: &PropertyInfo,
    ) {
        let prop_name = &prop_info.name;
        let default_value = prop_info.default_value.as_ref();

        // Get current value from pad_properties or use default
        let mut current_value = element
            .pad_properties
            .get(pad_name)
            .and_then(|props| props.get(prop_name))
            .cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();

            // For enum properties without default value, initialize to first option
            if current_value.is_none() {
                if let PropertyType::Enum { values } = &prop_info.property_type {
                    if let Some(first_value) = values.first() {
                        current_value = Some(PropertyValue::String(first_value.clone()));
                    }
                }
            }
        }

        ui.horizontal(|ui| {
            // Show property name with indicator if modified
            if has_custom_value {
                ui.colored_label(
                    Color32::from_rgb(255, 150, 100), // Orange for pad properties
                    format!("• {}:", prop_name),
                );
            } else {
                ui.label(format!("{}:", prop_name));
            }

            if let Some(mut value) = current_value {
                let changed = Self::show_property_editor(
                    ui,
                    &mut value,
                    Some(&prop_info.property_type),
                    default_value,
                    !prop_info.writable, // Read-only if not writable
                );

                if changed {
                    // Ensure the pad_properties map exists
                    element
                        .pad_properties
                        .entry(pad_name.to_string())
                        .or_default();

                    // Only save if different from default
                    if let Some(default) = default_value {
                        if !Self::values_equal(&value, default) {
                            element
                                .pad_properties
                                .get_mut(pad_name)
                                .unwrap()
                                .insert(prop_name.clone(), value);
                        } else {
                            // Remove if same as default
                            if let Some(props) = element.pad_properties.get_mut(pad_name) {
                                props.remove(prop_name);
                                // Clean up empty pad property maps
                                if props.is_empty() {
                                    element.pad_properties.remove(pad_name);
                                }
                            }
                        }
                    } else {
                        element
                            .pad_properties
                            .get_mut(pad_name)
                            .unwrap()
                            .insert(prop_name.clone(), value);
                    }
                }
            }

            // Reset button if modified (only show for writable properties)
            if has_custom_value
                && prop_info.writable
                && ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset to default")
                    .clicked()
            {
                if let Some(props) = element.pad_properties.get_mut(pad_name) {
                    props.remove(prop_name);
                    // Clean up empty pad property maps
                    if props.is_empty() {
                        element.pad_properties.remove(pad_name);
                    }
                }
            }
        });

        // Show description
        if !prop_info.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&prop_info.description);
            });
        }

        // Add spacing after each property
        ui.add_space(8.0);
    }

    fn show_property_from_info(ui: &mut Ui, element: &mut Element, prop_info: &PropertyInfo) {
        let prop_name = &prop_info.name;
        let default_value = prop_info.default_value.as_ref();

        // Debug logging for location property
        if prop_name == "location" {
            tracing::debug!(
                "Rendering property '{}' for element '{}': writable={}, construct_only={}, type={:?}",
                prop_name,
                element.element_type,
                prop_info.writable,
                prop_info.construct_only,
                prop_info.property_type
            );
        }

        // Get current value or use default
        let mut current_value = element.properties.get(prop_name).cloned();
        let has_custom_value = current_value.is_some();

        if current_value.is_none() {
            current_value = default_value.cloned();

            // For enum properties without default value, initialize to first option
            if current_value.is_none() {
                if let PropertyType::Enum { values } = &prop_info.property_type {
                    if let Some(first_value) = values.first() {
                        current_value = Some(PropertyValue::String(first_value.clone()));
                    }
                }
            }

            // For writable properties without a value, create an empty/default value
            if current_value.is_none() && prop_info.writable {
                current_value = Some(match &prop_info.property_type {
                    PropertyType::String => PropertyValue::String(String::new()),
                    PropertyType::Int { min, .. } => PropertyValue::Int(*min),
                    PropertyType::UInt { min, .. } => PropertyValue::UInt(*min),
                    PropertyType::Float { min, .. } => PropertyValue::Float(*min),
                    PropertyType::Bool => PropertyValue::Bool(false),
                    PropertyType::Enum { values } => {
                        PropertyValue::String(values.first().cloned().unwrap_or_default())
                    }
                });
            }
        }

        ui.horizontal(|ui| {
            // Show property name with indicator if modified
            if has_custom_value {
                ui.colored_label(
                    Color32::from_rgb(100, 200, 255),
                    format!("• {}:", prop_name),
                );
            } else {
                ui.label(format!("{}:", prop_name));
            }

            if let Some(mut value) = current_value {
                let changed = Self::show_property_editor(
                    ui,
                    &mut value,
                    Some(&prop_info.property_type),
                    default_value,
                    !prop_info.writable, // Read-only if not writable
                );

                if changed {
                    // Only save if different from default
                    if let Some(default) = default_value {
                        if !Self::values_equal(&value, default) {
                            element.properties.insert(prop_name.clone(), value);
                        } else {
                            element.properties.remove(prop_name);
                        }
                    } else {
                        element.properties.insert(prop_name.clone(), value);
                    }
                }
            }

            // Reset button if modified (only show for writable properties)
            if has_custom_value
                && prop_info.writable
                && ui
                    .small_button(egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE)
                    .on_hover_text("Reset to default")
                    .clicked()
            {
                element.properties.remove(prop_name);
            }
        });

        // Show description
        if !prop_info.description.is_empty() {
            ui.indent(prop_name, |ui| {
                ui.small(&prop_info.description);
            });
        }

        // Add spacing after each property
        ui.add_space(8.0);
    }

    fn values_equal(a: &PropertyValue, b: &PropertyValue) -> bool {
        match (a, b) {
            (PropertyValue::String(a), PropertyValue::String(b)) => a == b,
            (PropertyValue::Int(a), PropertyValue::Int(b)) => a == b,
            (PropertyValue::UInt(a), PropertyValue::UInt(b)) => a == b,
            (PropertyValue::Float(a), PropertyValue::Float(b)) => (a - b).abs() < 0.0001,
            (PropertyValue::Bool(a), PropertyValue::Bool(b)) => a == b,
            _ => false,
        }
    }

    /// Convert block::PropertyType to element::PropertyType for the property editor.
    fn convert_block_prop_type(
        block_prop: &strom_types::block::PropertyType,
    ) -> Option<PropertyType> {
        match block_prop {
            strom_types::block::PropertyType::Enum { values } => Some(PropertyType::Enum {
                values: values.iter().map(|ev| ev.value.clone()).collect(),
            }),
            // Other types don't need conversion (no constraints)
            _ => None,
        }
    }

    /// Show enum editor for block properties with labels.
    fn show_block_enum_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        enum_values: &[EnumValue],
    ) -> bool {
        // Normalize numeric values to strings so they match enum variant values.
        // This handles values deserialized from JSON as Int/UInt (e.g. `4` instead of `"4"`).
        match value {
            PropertyValue::Int(i) => *value = PropertyValue::String(i.to_string()),
            PropertyValue::UInt(u) => *value = PropertyValue::String(u.to_string()),
            _ => {}
        }
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Find the label for the current value
            let current_label = enum_values
                .iter()
                .find(|ev| ev.value == *s)
                .and_then(|ev| ev.label.as_ref())
                .cloned()
                .unwrap_or_else(|| s.clone());

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&current_label)
                .show_ui(ui, |ui| {
                    for enum_val in enum_values {
                        // Display label if available, otherwise just the value
                        let display_text = enum_val.label.as_deref().unwrap_or(&enum_val.value);

                        if ui
                            .selectable_label(*s == enum_val.value, display_text)
                            .clicked()
                        {
                            *s = enum_val.value.clone();
                            changed = true;
                        }
                    }
                });
            changed
        } else {
            false
        }
    }

    /// Show network interface selector dropdown.
    fn show_network_interface_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        interfaces: &[strom_types::NetworkInterfaceInfo],
    ) -> bool {
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Build display text for current selection
            let selected_display = if s.is_empty() {
                "Default (all interfaces)".to_string()
            } else {
                // Find interface to show with IP info
                interfaces
                    .iter()
                    .find(|iface| iface.name == *s)
                    .map(|iface| {
                        let ip = iface
                            .ipv4_addresses
                            .first()
                            .map(|addr| addr.address.as_str())
                            .unwrap_or("no IP");
                        format!("{} ({})", iface.name, ip)
                    })
                    .unwrap_or_else(|| s.clone())
            };

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&selected_display)
                .show_ui(ui, |ui| {
                    // Default option - empty string
                    if ui
                        .selectable_label(s.is_empty(), "Default (all interfaces)")
                        .clicked()
                    {
                        *s = String::new();
                        changed = true;
                    }

                    // List all available interfaces
                    for iface in interfaces {
                        // Skip loopback interfaces
                        if iface.is_loopback {
                            continue;
                        }

                        // Build display with IP info
                        let ip_info = iface
                            .ipv4_addresses
                            .first()
                            .map(|addr| addr.address.as_str())
                            .unwrap_or("no IP");
                        let display = format!("{} ({})", iface.name, ip_info);

                        if ui.selectable_label(*s == iface.name, &display).clicked() {
                            *s = iface.name.clone();
                            changed = true;
                        }
                    }
                });

            changed
        } else {
            false
        }
    }

    /// Show a picker for a local capture/playback device (Video/Source,
    /// Audio/Source, ...). Empty string = OS default
    /// (autovideosrc/autoaudiosrc on the backend).
    #[allow(clippy::too_many_arguments)]
    fn show_local_device_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        devices: &[strom_types::discovery::DeviceResponse],
        kind_label: &str,
        loading: bool,
        actions: &mut DevicePickerActions,
    ) -> bool {
        let PropertyValue::String(s) = value else {
            return false;
        };
        let mut changed = false;

        ui.horizontal(|ui| {
            let selected_display = if s.is_empty() {
                format!("OS default ({})", kind_label)
            } else {
                devices
                    .iter()
                    .find(|d| d.id == *s)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| format!("{} (not found — refresh)", s))
            };

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(selected_display)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(s.is_empty(), format!("OS default ({})", kind_label))
                        .clicked()
                    {
                        s.clear();
                        changed = true;
                    }
                    if devices.is_empty() {
                        if loading {
                            ui.add_enabled(false, egui::Label::new("Scanning…"));
                        } else {
                            ui.add_enabled(
                                false,
                                egui::Label::new(format!("(no {} devices found)", kind_label)),
                            );
                        }
                    }
                    for d in devices {
                        if ui.selectable_label(*s == d.id, &d.name).clicked() {
                            *s = d.id.clone();
                            changed = true;
                        }
                    }
                });

            let refresh = ui
                .add_enabled(
                    !loading,
                    egui::Button::new(egui_phosphor::regular::ARROWS_CLOCKWISE).small(),
                )
                .on_hover_text(if loading {
                    "Scanning local devices…"
                } else {
                    "Re-scan local devices"
                });
            if refresh.clicked() {
                actions.refresh_requested = true;
            }
        });

        changed
    }

    /// Show a dB gain slider for the AudioGain block.
    /// The value is stored directly in dB — no conversion needed here.
    fn show_db_gain_editor(ui: &mut Ui, value: &mut PropertyValue) -> bool {
        if let PropertyValue::Float(db) = value {
            ui.add(
                egui::Slider::new(db, -60.0..=20.0)
                    .suffix(" dB")
                    .fixed_decimals(1),
            )
            .changed()
        } else {
            false
        }
    }

    /// Show channel selector for InterInput blocks.
    /// Shows a dropdown with available channels (from all flows with InterOutput blocks).
    fn show_inter_channel_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        available_channels: &[strom_types::api::AvailableOutput],
    ) -> bool {
        if let PropertyValue::String(s) = value {
            let mut changed = false;

            // Helper to format display text for a channel
            let format_channel_display = |ch: &strom_types::api::AvailableOutput| -> String {
                let name_part = ch
                    .description
                    .as_ref()
                    .filter(|d| !d.is_empty())
                    .map(|d| d.as_str())
                    .unwrap_or(&ch.name);
                let status = if ch.is_active {
                    egui_phosphor::regular::PLAY
                } else {
                    egui_phosphor::regular::STOP
                };
                format!("{} {} / {}", status, ch.flow_name, name_part)
            };

            // Build display text for current selection
            let selected_display = if s.is_empty() {
                "(select channel)".to_string()
            } else {
                // Find channel to show with more info
                available_channels
                    .iter()
                    .find(|ch| ch.channel_name == *s)
                    .map(format_channel_display)
                    .unwrap_or_else(|| format!("(unknown: {})", s))
            };

            egui::ComboBox::from_id_salt(ui.next_auto_id())
                .selected_text(&selected_display)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if available_channels.is_empty() {
                        ui.label("No Inter Output blocks found");
                        ui.small("Add Inter Output blocks to flows to publish streams");
                    } else {
                        for channel in available_channels {
                            let display = format_channel_display(channel);
                            let response =
                                ui.selectable_label(*s == channel.channel_name, &display);

                            // Show tooltip with channel details
                            response.clone().on_hover_ui(|ui| {
                                ui.label(format!("Flow: {}", channel.flow_name));
                                if let Some(desc) = &channel.description {
                                    ui.label(format!("Description: {}", desc));
                                }
                                ui.label(format!("Block ID: {}", channel.name));
                                ui.label(format!(
                                    "Status: {}",
                                    if channel.is_active {
                                        "Active"
                                    } else {
                                        "Inactive"
                                    }
                                ));
                                ui.small(&channel.channel_name);
                            });

                            if response.clicked() {
                                *s = channel.channel_name.clone();
                                changed = true;
                            }
                        }
                    }
                });

            changed
        } else {
            false
        }
    }

    fn show_property_editor(
        ui: &mut Ui,
        value: &mut PropertyValue,
        prop_type: Option<&PropertyType>,
        _default_value: Option<&PropertyValue>,
        read_only: bool,
    ) -> bool {
        if read_only {
            // Display as non-editable text with a subtle background
            let text = match value {
                PropertyValue::String(s) => s.clone(),
                PropertyValue::Int(i) => i.to_string(),
                PropertyValue::UInt(u) => u.to_string(),
                PropertyValue::Float(f) => format!("{:.3}", f),
                PropertyValue::Bool(b) => b.to_string(),
            };
            ui.label(egui::RichText::new(text).color(Color32::from_rgb(150, 150, 150)))
                .on_hover_text("Read-only property");
            false
        } else {
            match (value, prop_type) {
                (PropertyValue::String(s), Some(PropertyType::Enum { values })) => {
                    // Enum dropdown
                    let mut changed = false;
                    egui::ComboBox::from_id_salt(ui.next_auto_id())
                        .selected_text(s.as_str())
                        .show_ui(ui, |ui| {
                            for val in values {
                                if ui.selectable_label(s == val, val).clicked() {
                                    *s = val.clone();
                                    changed = true;
                                }
                            }
                        });
                    changed
                }
                (PropertyValue::String(s), _) => ui.text_edit_singleline(s).changed(),
                (PropertyValue::Int(i), Some(PropertyType::Int { min, max })) => {
                    ui.add(egui::Slider::new(i, *min..=*max)).changed()
                }
                (PropertyValue::Int(i), _) => ui.add(egui::DragValue::new(i)).changed(),
                (PropertyValue::UInt(u), Some(PropertyType::UInt { min, max })) => {
                    ui.add(egui::Slider::new(u, *min..=*max)).changed()
                }
                (PropertyValue::UInt(u), _) => ui.add(egui::DragValue::new(u)).changed(),
                (PropertyValue::Float(f), Some(PropertyType::Float { min, max })) => {
                    ui.add(egui::Slider::new(f, *min..=*max)).changed()
                }
                (PropertyValue::Float(f), _) => ui
                    .add(egui::DragValue::new(f).speed(0.1).fixed_decimals(1))
                    .changed(),
                (PropertyValue::Bool(b), _) => ui.checkbox(b, "").changed(),
            }
        }
    }
}
