//! Discovery page for browsing SAP/mDNS/AES67 streams.

use egui::{Color32, Context, Ui};
use serde::{Deserialize, Serialize};

use crate::list_navigator::{list_navigator, ListItem};

/// Response from the discovery API for a discovered stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredStream {
    pub id: String,
    pub name: String,
    pub source: String,
    pub multicast_address: String,
    pub port: u16,
    pub channels: u8,
    pub sample_rate: u32,
    pub encoding: String,
    pub origin_host: String,
    pub first_seen_secs_ago: u64,
    pub last_seen_secs_ago: u64,
    pub ttl_secs: u64,
}

/// Response from the discovery API for an announced stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnouncedStream {
    pub flow_id: String,
    pub block_id: String,
    pub origin_ip: String,
    pub sdp: String,
}

/// Type of selected stream
#[derive(Debug, Clone, PartialEq)]
pub enum SelectedStream {
    /// A discovered stream (from SAP/mDNS announcements)
    Discovered(String),
    /// An announced stream (flow_id, block_id)
    Announced(String, String),
}

/// Tab selection for stream list
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamTab {
    #[default]
    Discovered,
    Announced,
}

/// Discovery page state.
pub struct DiscoveryPage {
    /// Discovered streams from SAP/mDNS
    pub discovered_streams: Vec<DiscoveredStream>,
    /// Streams we're announcing
    pub announced_streams: Vec<AnnouncedStream>,
    /// Last fetch time
    pub last_fetch: instant::Instant,
    /// Whether we're currently loading
    pub loading: bool,
    /// Pending SDP for creating a new flow (set when "Create Flow" is clicked)
    pub pending_create_flow_sdp: Option<String>,
    /// Error message if any
    pub error: Option<String>,
    /// Search filter
    pub search_filter: String,
    /// Selected stream for details view
    pub selected_stream: Option<SelectedStream>,
    /// SDP content for selected stream
    pub selected_stream_sdp: Option<String>,
    /// Currently selected tab
    pub selected_tab: StreamTab,
    /// Request to focus the search box on next frame
    focus_search_requested: bool,
}

impl DiscoveryPage {
    pub fn new() -> Self {
        Self {
            discovered_streams: Vec::new(),
            announced_streams: Vec::new(),
            last_fetch: instant::Instant::now(),
            loading: false,
            pending_create_flow_sdp: None,
            error: None,
            search_filter: String::new(),
            selected_stream: None,
            selected_stream_sdp: None,
            selected_tab: StreamTab::default(),
            focus_search_requested: false,
        }
    }

    /// Request focus on the search box (will be applied on next frame).
    pub fn focus_search(&mut self) {
        self.focus_search_requested = true;
    }

    /// Render the discovery page.
    pub fn render(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        // Auto-refresh every 5 seconds
        if self.last_fetch.elapsed().as_secs() > 5 && !self.loading {
            self.refresh(api, ctx, tx);
        }

        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, format!("Error: {}", error));
            ui.separator();
        }

        // Split view: stream list on left, details on right
        egui::SidePanel::left("stream_list")
            .default_width(400.0)
            .resizable(true)
            .show_inside(ui, |ui| {
                // Search filter at top of list
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    let filter_id = egui::Id::new("discovery_search_filter");
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.search_filter).id(filter_id));
                    if self.focus_search_requested {
                        self.focus_search_requested = false;
                        response.request_focus();
                    }
                    if !self.search_filter.is_empty() && ui.small_button("✕").clicked() {
                        self.search_filter.clear();
                    }
                });
                ui.add_space(4.0);

                self.render_streams_list(ui, api, ctx, tx);
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_details_panel(ui);
        });
    }

    fn render_streams_list(
        &mut self,
        ui: &mut Ui,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let filter = self.search_filter.to_lowercase();

        // Tabs for Discovered / Announced
        ui.horizontal(|ui| {
            if ui
                .selectable_label(
                    self.selected_tab == StreamTab::Discovered,
                    format!("Discovered ({})", self.discovered_streams.len()),
                )
                .clicked()
            {
                self.selected_tab = StreamTab::Discovered;
            }
            if ui
                .selectable_label(
                    self.selected_tab == StreamTab::Announced,
                    format!("Announced ({})", self.announced_streams.len()),
                )
                .clicked()
            {
                self.selected_tab = StreamTab::Announced;
            }
        });

        ui.separator();

        // Get current selected ID for the list navigator
        let selected_id: Option<String> = match &self.selected_stream {
            Some(SelectedStream::Discovered(id)) if self.selected_tab == StreamTab::Discovered => {
                Some(id.clone())
            }
            Some(SelectedStream::Announced(fid, bid))
                if self.selected_tab == StreamTab::Announced =>
            {
                Some(format!("{}:{}", fid, bid))
            }
            _ => None,
        };

        // Stream list based on selected tab
        match self.selected_tab {
            StreamTab::Discovered => {
                if self.discovered_streams.is_empty() {
                    ui.label("No streams discovered yet. Waiting for SAP/mDNS announcements...");
                } else {
                    // Build list items data
                    let items_data: Vec<_> = self
                        .discovered_streams
                        .iter()
                        .filter(|stream| {
                            filter.is_empty()
                                || stream.name.to_lowercase().contains(&filter)
                                || stream.origin_host.to_lowercase().contains(&filter)
                                || stream.multicast_address.contains(&filter)
                        })
                        .map(|stream| {
                            (
                                stream.id.clone(),
                                stream.name.clone(),
                                format!(
                                    "{}:{} | {}",
                                    stream.multicast_address, stream.port, stream.origin_host
                                ),
                                format!(
                                    "{}ch {}Hz {}",
                                    stream.channels, stream.sample_rate, stream.encoding
                                ),
                            )
                        })
                        .collect();

                    let result = egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let items = items_data.iter().map(|(id, name, secondary, right)| {
                                ListItem::new(id, name)
                                    .with_tag("[RX]", Color32::from_rgb(100, 150, 255))
                                    .with_secondary(secondary.clone())
                                    .with_right_text(right.clone())
                            });

                            list_navigator(ui, "discovered_streams", items, selected_id.as_deref())
                        });

                    if let Some(new_id) = result.inner.selected {
                        self.selected_stream = Some(SelectedStream::Discovered(new_id.clone()));
                        self.selected_stream_sdp = None;
                        self.fetch_stream_sdp(&new_id, api, ctx, tx);
                    }
                }
            }
            StreamTab::Announced => {
                if self.announced_streams.is_empty() {
                    ui.label(
                        "No streams being announced. Start a flow with an AES67 output block.",
                    );
                } else {
                    // Pre-compute stream info from SDP
                    let streams_with_info: Vec<_> = self
                        .announced_streams
                        .iter()
                        .map(|stream| {
                            let stream_name = stream
                                .sdp
                                .lines()
                                .find(|l| l.starts_with("s="))
                                .map(|l| l.trim_start_matches("s="))
                                .unwrap_or("Unknown");

                            let multicast = stream
                                .sdp
                                .lines()
                                .find(|l| l.starts_with("c="))
                                .and_then(|l| l.split_whitespace().last())
                                .map(|s| s.split('/').next().unwrap_or(s))
                                .unwrap_or("?");

                            let port = stream
                                .sdp
                                .lines()
                                .find(|l| l.starts_with("m=audio"))
                                .and_then(|l| l.split_whitespace().nth(1))
                                .unwrap_or("?");

                            let id = format!("{}:{}", stream.flow_id, stream.block_id);

                            (
                                id,
                                stream_name.to_string(),
                                format!("{}:{} | {}", multicast, port, stream.origin_ip),
                                stream.sdp.clone(),
                            )
                        })
                        .filter(|(_, stream_name, secondary, _)| {
                            filter.is_empty()
                                || stream_name.to_lowercase().contains(&filter)
                                || secondary.to_lowercase().contains(&filter)
                        })
                        .collect();

                    let result = egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let items =
                                streams_with_info
                                    .iter()
                                    .map(|(id, stream_name, secondary, _)| {
                                        ListItem::new(id, stream_name)
                                            .with_tag("[TX]", Color32::from_rgb(100, 200, 100))
                                            .with_secondary(secondary.clone())
                                    });

                            list_navigator(ui, "announced_streams", items, selected_id.as_deref())
                        });

                    if let Some(new_id) = result.inner.selected {
                        // Parse the composite ID back to flow_id and block_id
                        if let Some((flow_id, block_id)) = new_id.split_once(':') {
                            self.selected_stream = Some(SelectedStream::Announced(
                                flow_id.to_string(),
                                block_id.to_string(),
                            ));
                            // Find and set the SDP
                            if let Some((_, _, _, sdp)) =
                                streams_with_info.iter().find(|(id, _, _, _)| id == &new_id)
                            {
                                self.selected_stream_sdp = Some(sdp.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn render_details_panel(&mut self, ui: &mut Ui) {
        ui.heading("Stream Details");
        ui.separator();

        let Some(selected) = &self.selected_stream else {
            ui.label("Select a stream to view details");
            return;
        };

        match selected {
            SelectedStream::Discovered(stream_id) => {
                let Some(stream) = self.discovered_streams.iter().find(|s| &s.id == stream_id)
                else {
                    ui.label("Stream not found");
                    return;
                };

                // Clone the data we need to avoid borrow issues
                let stream = stream.clone();

                egui::Grid::new("stream_details_grid")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Type:");
                        ui.colored_label(Color32::from_rgb(100, 150, 255), "[RX] Discovered");
                        ui.end_row();

                        ui.label("Name:");
                        ui.label(&stream.name);
                        ui.end_row();

                        ui.label("Source:");
                        ui.label(&stream.source);
                        ui.end_row();

                        ui.label("Multicast:");
                        ui.label(format!("{}:{}", stream.multicast_address, stream.port));
                        ui.end_row();

                        ui.label("Format:");
                        ui.label(format!(
                            "{}ch {}Hz {}",
                            stream.channels, stream.sample_rate, stream.encoding
                        ));
                        ui.end_row();

                        ui.label("Origin:");
                        ui.label(&stream.origin_host);
                        ui.end_row();

                        ui.label("First seen:");
                        ui.label(format!("{}s ago", stream.first_seen_secs_ago));
                        ui.end_row();

                        ui.label("Last seen:");
                        ui.label(format!("{}s ago", stream.last_seen_secs_ago));
                        ui.end_row();

                        ui.label("TTL:");
                        ui.label(format!("{}s", stream.ttl_secs));
                        ui.end_row();
                    });
            }
            SelectedStream::Announced(flow_id, block_id) => {
                let Some(stream) = self
                    .announced_streams
                    .iter()
                    .find(|s| &s.flow_id == flow_id && &s.block_id == block_id)
                else {
                    ui.label("Stream not found");
                    return;
                };

                // Parse details from SDP
                let stream_name = stream
                    .sdp
                    .lines()
                    .find(|l| l.starts_with("s="))
                    .map(|l| l.trim_start_matches("s="))
                    .unwrap_or("Unknown");

                let multicast = stream
                    .sdp
                    .lines()
                    .find(|l| l.starts_with("c="))
                    .and_then(|l| l.split_whitespace().last())
                    .map(|s| s.split('/').next().unwrap_or(s))
                    .unwrap_or("?");

                let port = stream
                    .sdp
                    .lines()
                    .find(|l| l.starts_with("m=audio"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("?");

                egui::Grid::new("stream_details_grid")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Type:");
                        ui.colored_label(Color32::from_rgb(100, 200, 100), "[TX] Announced");
                        ui.end_row();

                        ui.label("Name:");
                        ui.label(stream_name);
                        ui.end_row();

                        ui.label("Multicast:");
                        ui.label(format!("{}:{}", multicast, port));
                        ui.end_row();

                        ui.label("Origin:");
                        ui.label(&stream.origin_ip);
                        ui.end_row();

                        ui.label("Flow ID:");
                        ui.label(&stream.flow_id);
                        ui.end_row();

                        ui.label("Block ID:");
                        ui.label(&stream.block_id);
                        ui.end_row();
                    });
            }
        }

        ui.separator();
        ui.label("SDP:");

        if let Some(sdp) = &self.selected_stream_sdp {
            egui::ScrollArea::vertical()
                .max_height(200.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut sdp.as_str())
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("📋 Copy SDP").clicked() {
                    ui.ctx().copy_text(sdp.clone());
                }
                if ui.button("➕ Create Flow").clicked() {
                    self.pending_create_flow_sdp = Some(sdp.clone());
                }
            });
        } else {
            ui.label("Loading SDP...");
        }
    }

    /// Refresh streams from API.
    pub fn refresh(
        &mut self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        self.loading = true;
        self.last_fetch = instant::Instant::now();

        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();

        crate::app::spawn_task(async move {
            // Fetch discovered streams
            match api.get_discovered_streams().await {
                Ok(streams) => {
                    let _ = tx.send(crate::state::AppMessage::DiscoveredStreamsLoaded(streams));
                    ctx.request_repaint();
                }
                Err(e) => {
                    tracing::error!("Failed to fetch discovered streams: {}", e);
                }
            }

            // Fetch announced streams
            match api.get_announced_streams().await {
                Ok(streams) => {
                    let _ = tx.send(crate::state::AppMessage::AnnouncedStreamsLoaded(streams));
                    ctx.request_repaint();
                }
                Err(e) => {
                    tracing::error!("Failed to fetch announced streams: {}", e);
                }
            }
        });
    }

    /// Update discovered streams (called from message handler).
    pub fn set_discovered_streams(&mut self, streams: Vec<DiscoveredStream>) {
        self.discovered_streams = streams;
        self.loading = false;
        self.error = None;
    }

    /// Update announced streams (called from message handler).
    pub fn set_announced_streams(&mut self, streams: Vec<AnnouncedStream>) {
        self.announced_streams = streams;
    }

    /// Set error message.
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.loading = false;
    }

    /// Fetch SDP for a discovered stream.
    fn fetch_stream_sdp(
        &self,
        stream_id: &str,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<crate::state::AppMessage>,
    ) {
        let api = api.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();
        let stream_id = stream_id.to_string();

        crate::app::spawn_task(async move {
            match api.get_stream_sdp(&stream_id).await {
                Ok(sdp) => {
                    let _ = tx.send(crate::state::AppMessage::StreamSdpLoaded { stream_id, sdp });
                    ctx.request_repaint();
                }
                Err(e) => {
                    tracing::error!("Failed to fetch SDP for stream {}: {}", stream_id, e);
                }
            }
        });
    }

    /// Set SDP for selected stream.
    pub fn set_stream_sdp(&mut self, stream_id: String, sdp: String) {
        if let Some(SelectedStream::Discovered(id)) = &self.selected_stream {
            if id == &stream_id {
                self.selected_stream_sdp = Some(sdp);
            }
        }
    }

    /// Take pending create flow SDP if set.
    pub fn take_pending_create_flow_sdp(&mut self) -> Option<String> {
        self.pending_create_flow_sdp.take()
    }
}

impl Default for DiscoveryPage {
    fn default() -> Self {
        Self::new()
    }
}
