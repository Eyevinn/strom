use crate::info_page::{
    current_time_millis, format_datetime_local, format_uptime, parse_iso8601_to_millis,
};
use crate::properties::PropertyInspector;
use crate::state::AppMessage;
use egui::{CentralPanel, Color32, Context, SidePanel, TopBottomPanel};
use strom_types::Flow;

use super::*;
use super::{FocusTarget, ThemePreference};
impl StromApp {
    /// Render the top toolbar.
    pub(super) fn render_toolbar(&mut self, ctx: &Context) {
        // First top bar: System-wide controls
        TopBottomPanel::top("system_bar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0; // Add some vertical spacing between wrapped rows
                                                           // Strom logo and heading as clickable link to GitHub
                    if ui
                        .add(
                            egui::Image::from_bytes(
                                "bytes://strom-icon",
                                include_bytes!("../icon.png"),
                            )
                            .fit_to_exact_size(egui::vec2(24.0, 24.0))
                            .corner_radius(4.0),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Visit Strom on GitHub")
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
                    }
                    if ui
                        .heading("Strom")
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Visit Strom on GitHub")
                        .clicked()
                    {
                        ctx.open_url(egui::OpenUrl::new_tab("https://github.com/Eyevinn/strom"));
                    }

                    // Zoom controls
                    ui.separator();
                    let current_zoom = ctx.pixels_per_point();
                    let zoom_percent = (current_zoom * 100.0).round() as i32;

                    if ui
                        .small_button(egui_phosphor::regular::MAGNIFYING_GLASS_MINUS)
                        .on_hover_text("Zoom out")
                        .clicked()
                    {
                        let new_zoom = (current_zoom / 1.1).max(0.5);
                        ctx.set_pixels_per_point(new_zoom);
                        self.settings.zoom = Some(new_zoom);
                    }

                    if ui
                        .small_button(format!("{}%", zoom_percent))
                        .on_hover_text("Reset zoom")
                        .clicked()
                    {
                        ctx.set_pixels_per_point(self.native_pixels_per_point);
                        self.settings.zoom = None; // Reset to system default
                    }

                    if ui
                        .small_button(egui_phosphor::regular::MAGNIFYING_GLASS_PLUS)
                        .on_hover_text("Zoom in")
                        .clicked()
                    {
                        let new_zoom = (current_zoom * 1.1).min(5.0);
                        ctx.set_pixels_per_point(new_zoom);
                        self.settings.zoom = Some(new_zoom);
                    }

                    // Open Web GUI button (native mode only)
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        if ui
                            .button(format!(
                                "{} Web GUI",
                                egui_phosphor::regular::ARROW_SQUARE_OUT
                            ))
                            .on_hover_text("Open the web interface in your browser")
                            .clicked()
                        {
                            let scheme = if self.tls_enabled { "https" } else { "http" };
                            let url = format!("{}://localhost:{}", scheme, self.port);
                            ctx.open_url(egui::OpenUrl::new_tab(&url));
                        }
                    }

                    ui.separator();

                    // Navigation tabs
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Flows,
                            egui::RichText::new("Flows").size(16.0),
                        )
                        .clicked()
                    {
                        self.current_page = AppPage::Flows;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Discovery,
                            egui::RichText::new("Discovery").size(16.0),
                        )
                        .on_hover_text("Browse SAP/AES67 streams")
                        .clicked()
                    {
                        self.current_page = AppPage::Discovery;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Clocks,
                            egui::RichText::new("Clocks").size(16.0),
                        )
                        .on_hover_text("PTP clock synchronization")
                        .clicked()
                    {
                        self.current_page = AppPage::Clocks;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Media,
                            egui::RichText::new("Media").size(16.0),
                        )
                        .on_hover_text("Media file browser")
                        .clicked()
                    {
                        self.current_page = AppPage::Media;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Info,
                            egui::RichText::new("Info").size(16.0),
                        )
                        .on_hover_text("System and version information")
                        .clicked()
                    {
                        self.current_page = AppPage::Info;
                        self.focus_target = FocusTarget::None;
                    }
                    if ui
                        .selectable_label(
                            self.current_page == AppPage::Links,
                            egui::RichText::new("Links").size(16.0),
                        )
                        .on_hover_text("Quick links to streaming endpoints")
                        .clicked()
                    {
                        self.current_page = AppPage::Links;
                        self.focus_target = FocusTarget::None;
                    }

                    ui.separator();

                    // Theme dropdown menu
                    let theme_name = match self.settings.theme {
                        ThemePreference::EguiDark => "Dark",
                        ThemePreference::EguiLight => "Light",
                        ThemePreference::NordDark => "Nord Dark",
                        ThemePreference::NordLight => "Nord Light",
                        ThemePreference::TokyoNight => "Tokyo Night",
                        ThemePreference::TokyoNightStorm => "Tokyo Storm",
                        ThemePreference::TokyoNightLight => "Tokyo Light",
                        ThemePreference::ClaudeDark => "Claude Dark",
                        ThemePreference::ClaudeLight => "Claude Light",
                    };

                    egui::ComboBox::from_id_salt("theme_selector")
                        .selected_text(theme_name)
                        .show_ui(ui, |ui| {
                            let themes = [
                                (ThemePreference::EguiDark, "Dark (default)"),
                                (ThemePreference::EguiLight, "Light (default)"),
                                (ThemePreference::ClaudeDark, "Claude Dark"),
                                (ThemePreference::ClaudeLight, "Claude Light"),
                                (ThemePreference::NordDark, "Nord Dark"),
                                (ThemePreference::NordLight, "Nord Light"),
                                (ThemePreference::TokyoNight, "Tokyo Night"),
                                (ThemePreference::TokyoNightStorm, "Tokyo Night Storm"),
                                (ThemePreference::TokyoNightLight, "Tokyo Night Light"),
                            ];
                            for (theme, label) in themes {
                                if ui
                                    .selectable_label(self.settings.theme == theme, label)
                                    .clicked()
                                {
                                    self.settings.theme = theme;
                                    self.apply_theme(ctx.clone());
                                }
                            }
                        });

                    // Logout button (only show if auth is enabled and user is authenticated)
                    if let Some(ref status) = self.auth_status {
                        if status.auth_required
                            && status.authenticated
                            && ui
                                .button(egui_phosphor::regular::SIGN_OUT)
                                .on_hover_text("Logout")
                                .clicked()
                        {
                            self.handle_logout(ctx.clone());
                        }
                    }

                    // System monitoring widget (graphs only, no text)
                    let monitor_response = ui.add(
                        crate::system_monitor::CompactSystemMonitor::new(&self.system_monitor)
                            .width(80.0)
                            .height(18.0),
                    );
                    if monitor_response.clicked() {
                        self.show_system_monitor = !self.show_system_monitor;
                    }
                    monitor_response.on_hover_text("Click to show detailed system monitoring");
                });
            });

        // Second top bar: Page-specific controls
        self.render_page_toolbar(ctx);
    }

    /// Render the page-specific toolbar (second row)
    pub(super) fn render_page_toolbar(&mut self, ctx: &Context) {
        match self.current_page {
            AppPage::Flows => self.render_flows_toolbar(ctx),
            AppPage::Discovery => self.render_discovery_toolbar(ctx),
            AppPage::Clocks => self.render_clocks_toolbar(ctx),
            AppPage::Media => self.render_media_toolbar(ctx),
            AppPage::Info => self.render_info_toolbar(ctx),
            AppPage::Links => self.render_links_toolbar(ctx),
        }
    }

    /// Render the flows page toolbar
    pub(super) fn render_flows_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(8, 4)))
            .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Flows").heading());
                ui.separator();

                if ui
                    .button(format!("{} New Flow", egui_phosphor::regular::PLUS))
                    .on_hover_text(format!("Create a new flow ({})", Self::format_shortcut("Ctrl+N")))
                    .clicked()
                {
                    self.show_new_flow_dialog = true;
                }

                if ui
                    .button(format!("{} Import", egui_phosphor::regular::DOWNLOAD_SIMPLE))
                    .on_hover_text(format!("Import flow from JSON ({})", Self::format_shortcut("Ctrl+O")))
                    .clicked()
                {
                    self.show_import_dialog = true;
                    self.import_json_buffer.clear();
                    self.import_error = None;
                }

                if ui
                    .button(format!("{} Refresh", egui_phosphor::regular::ARROWS_CLOCKWISE))
                    .on_hover_text("Reload flows from server (F5 or Ctrl+R)")
                    .clicked()
                {
                    self.needs_refresh = true;
                }

                if ui
                    .button(format!("{} Save", egui_phosphor::regular::FLOPPY_DISK))
                    .on_hover_text(format!("Save current flow ({})", Self::format_shortcut("Ctrl+S")))
                    .clicked()
                {
                    self.save_current_flow(ctx);
                }

                // Flow controls - only show when a flow is selected
                let flow_info = self.current_flow().map(|f| (f.id, f.running));

                if let Some((flow_id, running)) = flow_info {
                    ui.separator();

                    let (state_text, state_color) = if running {
                        ("Started", Color32::GREEN)
                    } else {
                        ("Stopped", Color32::GRAY)
                    };

                    ui.colored_label(state_color, format!("State: {}", state_text));

                    // Show minimum pipeline latency for running live flows
                    if running {
                        if let Some(latency) = self.latency_cache.get(&flow_id.to_string()) {
                            if latency.live {
                                ui.label(format!(
                                    "Min latency: {}",
                                    latency.min_latency_formatted
                                ));
                            }
                        }
                    }

                    ui.separator();

                    // Show Start or Restart button depending on state
                    let button_text = if running {
                        format!("{} Restart", egui_phosphor::regular::ARROWS_CLOCKWISE)
                    } else {
                        "▶ Start".to_string()
                    };

                    if ui
                        .button(button_text)
                        .on_hover_text(if running {
                            "Restart pipeline (F9)"
                        } else {
                            "Start pipeline (F9)"
                        })
                        .clicked()
                    {
                        if running {
                            // For restart: stop first, then start
                            let api = self.api.clone();
                            let tx = self.channels.sender();
                            let ctx_clone = ctx.clone();

                            self.status = "Restarting flow...".to_string();

                            spawn_task(async move {
                                // First stop the flow
                                match api.stop_flow(flow_id).await {
                                    Ok(_) => {
                                        tracing::info!("Flow stopped, now starting...");
                                        // Then start it again
                                        match api.start_flow(flow_id).await {
                                            Ok(_) => {
                                                tracing::info!("Flow restarted successfully - WebSocket events will trigger refresh");
                                                let _ = tx.send(AppMessage::FlowOperationSuccess("Flow restarted".to_string()));
                                            }
                                            Err(e) => {
                                                tracing::error!(
                                                    "Failed to start flow after stop: {}",
                                                    e
                                                );
                                                let _ = tx.send(AppMessage::FlowOperationError(format!("Failed to restart flow: {}", e)));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to stop flow for restart: {}", e);
                                        let _ = tx.send(AppMessage::FlowOperationError(format!("Failed to restart flow: {}", e)));
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        } else {
                            self.start_flow(ctx);
                        }
                    }

                    if ui
                        .button("■ Stop")
                        .on_hover_text("Stop pipeline (Shift+F9)")
                        .clicked()
                    {
                        self.stop_flow(ctx);
                    }

                    if ui
                        .button(format!("{} Debug Graph", egui_phosphor::regular::GRAPH))
                        .on_hover_text(format!(
                            "View pipeline debug graph ({})",
                            Self::format_shortcut("Ctrl+D")
                        ))
                        .clicked()
                    {
                        let url = self.api.get_debug_graph_url(flow_id);
                        ctx.open_url(egui::OpenUrl::new_tab(&url));
                    }

                    // Show flow uptime on the right side (only for running flows)
                    if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id) {
                        if let Some(ref started_at) = flow.properties.started_at {
                            if let Some(started_millis) = parse_iso8601_to_millis(started_at) {
                                let uptime_millis = current_time_millis() - started_millis;

                                // Push to right side
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // Build tooltip text
                                    let mut tooltip = format!("Started: {}", format_datetime_local(started_at));
                                    if let Some(ref modified) = flow.properties.last_modified {
                                        tooltip.push_str(&format!("\nLast modified: {}", format_datetime_local(modified)));
                                    }

                                    ui.label(
                                        egui::RichText::new(format!("Flow uptime: {}", format_uptime(uptime_millis)))
                                            .color(Color32::GREEN)
                                    ).on_hover_text(tooltip);
                                });
                            }
                        }
                    }
                }

            });
        });
    }

    /// Render the discovery page toolbar
    pub(super) fn render_discovery_toolbar(&mut self, ctx: &Context) {
        let is_loading = self.discovery_page.loading;

        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Discovery").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.discovery_page
                            .refresh(&self.api, ctx, &self.channels.sender());
                    }
                    if is_loading {
                        ui.spinner();
                    }
                });
            });
    }

    /// Render the clocks page toolbar
    pub(super) fn render_clocks_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Clocks").heading());
                    ui.separator();
                    ui.label("PTP clocks are shared per domain");
                });
            });
    }

    /// Render the media page toolbar
    pub(super) fn render_media_toolbar(&mut self, ctx: &Context) {
        let is_loading = self.media_page.loading;

        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Media Files").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.media_page
                            .refresh(&self.api, ctx, &self.channels.sender());
                    }
                    if is_loading {
                        ui.spinner();
                    }
                });
            });
    }

    /// Render the info page toolbar
    pub(super) fn render_info_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("System Information").heading());
                    ui.separator();

                    if ui.button("Refresh").clicked() {
                        self.load_version(ctx.clone());
                        // Force reload of network interfaces
                        self.network_interfaces_loaded = false;
                        self.load_network_interfaces(ctx.clone());
                    }
                });
            });
    }

    /// Render the links page toolbar
    pub(super) fn render_links_toolbar(&mut self, ctx: &Context) {
        TopBottomPanel::top("page_toolbar")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(8, 4)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new("Links").heading());
                });
            });
    }

    /// Render the flow list sidebar.
    pub(super) fn render_flow_list(&mut self, ctx: &Context) {
        if !self.show_flow_list_panel {
            return;
        }
        // Max width is 40% of screen width
        #[allow(deprecated)]
        let max_width = ctx.screen_rect().width() * 0.4;
        SidePanel::left("flow_list")
            .default_width(200.0)
            .max_width(max_width)
            .resizable(true)
            .show(ctx, |ui| {
                // Filter input at top
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    let filter_id = egui::Id::new("flow_list_filter");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.flow_filter)
                            .id(filter_id)
                            .desired_width(100.0),
                    );
                    if self.focus_flow_filter_requested {
                        self.focus_flow_filter_requested = false;
                        response.request_focus();
                    }
                    if !self.flow_filter.is_empty()
                        && ui.small_button(egui_phosphor::regular::X).on_hover_text("Clear search").clicked()
                    {
                        self.flow_filter.clear();
                    }
                });
                ui.add_space(4.0);

                // Scroll area for the flow list
                egui::ScrollArea::both()
                    .id_salt("flow_list_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.flows.is_empty() {
                            ui.label("No flows yet");
                            ui.label("Click 'New Flow' to get started");
                        } else {
                            // Create sorted and filtered list of flows (by name)
                            let filter_lower = self.flow_filter.to_lowercase();
                            let mut sorted_flows: Vec<&Flow> = self
                                .flows
                                .iter()
                                .filter(|f| {
                                    filter_lower.is_empty()
                                        || f.name.to_lowercase().contains(&filter_lower)
                                })
                                .collect();
                            sorted_flows
                                .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

                            if sorted_flows.is_empty() {
                                ui.label("No matching flows");
                                return;
                            }

                            // Handle keyboard navigation
                            let list_id = ui.id().with("flow_list_nav");
                            let has_focus = ui.memory(|mem| mem.has_focus(list_id));

                            if has_focus {
                                let current_idx = self
                                    .selected_flow_id
                                    .and_then(|sel| sorted_flows.iter().position(|f| f.id == sel));

                                ui.input(|input| {
                                    if input.key_pressed(egui::Key::ArrowDown) {
                                        if let Some(idx) = current_idx {
                                            if idx + 1 < sorted_flows.len() {
                                                let flow = sorted_flows[idx + 1];
                                                self.selected_flow_id = Some(flow.id);
                                                self.graph.deselect_all();
                                                self.graph.clear_runtime_dynamic_pads();
                                                self.graph.load(
                                                    flow.elements.clone(),
                                                    flow.links.clone(),
                                                );
                                                self.graph.load_blocks(flow.blocks.clone());
                                            }
                                        } else {
                                            let flow = sorted_flows[0];
                                            self.selected_flow_id = Some(flow.id);
                                            self.graph.deselect_all();
                                            self.graph.clear_runtime_dynamic_pads();
                                            self.graph
                                                .load(flow.elements.clone(), flow.links.clone());
                                            self.graph.load_blocks(flow.blocks.clone());
                                        }
                                    } else if input.key_pressed(egui::Key::ArrowUp) {
                                        if let Some(idx) = current_idx {
                                            if idx > 0 {
                                                let flow = sorted_flows[idx - 1];
                                                self.selected_flow_id = Some(flow.id);
                                                self.graph.deselect_all();
                                                self.graph.clear_runtime_dynamic_pads();
                                                self.graph.load(
                                                    flow.elements.clone(),
                                                    flow.links.clone(),
                                                );
                                                self.graph.load_blocks(flow.blocks.clone());
                                            }
                                        } else if !sorted_flows.is_empty() {
                                            let flow = sorted_flows[sorted_flows.len() - 1];
                                            self.selected_flow_id = Some(flow.id);
                                            self.graph.deselect_all();
                                            self.graph.clear_runtime_dynamic_pads();
                                            self.graph
                                                .load(flow.elements.clone(), flow.links.clone());
                                            self.graph.load_blocks(flow.blocks.clone());
                                        }
                                    }
                                });
                            }

                            for flow in sorted_flows {
                                let selected = self.selected_flow_id == Some(flow.id);

                                // Create full-width selectable area
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 20.0),
                                    egui::Sense::click(),
                                );

                                if response.clicked() {
                                    // Defer flow selection to next frame to avoid accesskit panic
                                    // when focused node is removed during UI update
                                    self.pending_flow_selection = Some(flow.id);
                                }

                                // Check for QoS issues to tint the background
                                let qos_health = self.qos_stats.get_flow_health(&flow.id);
                                let has_qos_issues = qos_health
                                    .map(|h| h != crate::qos_monitor::QoSHealth::Ok)
                                    .unwrap_or(false);

                                // Draw background for selected/hovered item with QoS tint
                                if selected {
                                    let mut bg_color = ui.visuals().selection.bg_fill;
                                    if has_qos_issues {
                                        // Blend selection color with warning/critical color
                                        let qos_color = qos_health.unwrap().color();
                                        bg_color = Color32::from_rgba_unmultiplied(
                                            ((bg_color.r() as u16 + qos_color.r() as u16) / 2)
                                                as u8,
                                            ((bg_color.g() as u16 + qos_color.g() as u16) / 2)
                                                as u8,
                                            ((bg_color.b() as u16 + qos_color.b() as u16) / 2)
                                                as u8,
                                            bg_color.a(),
                                        );
                                    }
                                    ui.painter().rect_filled(rect, 2.0, bg_color);
                                } else if has_qos_issues {
                                    // Draw QoS warning/critical background
                                    let qos_color = qos_health.unwrap().color();
                                    let bg_color = Color32::from_rgba_unmultiplied(
                                        qos_color.r(),
                                        qos_color.g(),
                                        qos_color.b(),
                                        40, // Semi-transparent
                                    );
                                    ui.painter().rect_filled(rect, 2.0, bg_color);
                                    // Also draw a left border for emphasis
                                    let border_rect = egui::Rect::from_min_size(
                                        rect.min,
                                        egui::vec2(3.0, rect.height()),
                                    );
                                    ui.painter().rect_filled(border_rect, 0.0, qos_color);
                                } else if response.hovered() {
                                    ui.painter().rect_filled(
                                        rect,
                                        2.0,
                                        ui.visuals().widgets.hovered.bg_fill,
                                    );
                                }

                                // Draw flow name and buttons
                                // Use a horizontal layout with the name truncating to fit
                                let text_color = if selected {
                                    ui.visuals().selection.stroke.color
                                } else {
                                    ui.visuals().text_color()
                                };

                                // Calculate space for right-side elements dynamically
                                let mut right_side_width = 18.0; // menu button "..."

                                // Clock sync indicator shown for PTP/NTP
                                if matches!(
                                    flow.properties.clock_type,
                                    strom_types::flow::GStreamerClockType::Ptp
                                        | strom_types::flow::GStreamerClockType::Ntp
                                ) {
                                    right_side_width += 18.0;
                                }

                                // Thread priority warning indicator
                                if flow
                                    .properties
                                    .thread_priority_status
                                    .as_ref()
                                    .map(|s| !s.achieved && s.error.is_some())
                                    .unwrap_or(false)
                                {
                                    right_side_width += 18.0;
                                }

                                // CPU affinity indicator
                                if flow.properties.cpu_affinity
                                    != strom_types::flow::CpuAffinity::default()
                                {
                                    right_side_width += 18.0;
                                }

                                // Left side: state icon (always) + QoS indicator (conditional)
                                let mut left_icons_width = 20.0; // state icon + spacing
                                let has_qos_issues = self
                                    .qos_stats
                                    .get_flow_health(&flow.id)
                                    .map(|h| h != crate::qos_monitor::QoSHealth::Ok)
                                    .unwrap_or(false);
                                if has_qos_issues {
                                    left_icons_width += 18.0;
                                }
                                let available_name_width =
                                    (rect.width() - right_side_width - left_icons_width).max(20.0);

                                let mut child_ui = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(rect)
                                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                                );
                                child_ui.add_space(4.0);

                                // Show running state icon
                                let state_icon = if flow.running { "▶" } else { "■" };
                                let state_color = if flow.running {
                                    Color32::from_rgb(0, 200, 0)
                                } else {
                                    Color32::GRAY
                                };
                                child_ui.colored_label(state_color, state_icon);

                                // Show QoS indicator if there are issues - make it clickable to open log
                                if let Some(qos_health) = self.qos_stats.get_flow_health(&flow.id) {
                                    if qos_health != crate::qos_monitor::QoSHealth::Ok {
                                        let qos_label = child_ui
                                            .colored_label(qos_health.color(), qos_health.icon())
                                            .interact(egui::Sense::click());

                                        // Click to open log panel
                                        if qos_label.clicked() {
                                            self.show_log_panel = true;
                                        }

                                        // Show tooltip with problem elements
                                        let problem_elements =
                                            self.qos_stats.get_problem_elements(&flow.id);
                                        if !problem_elements.is_empty() {
                                            qos_label.on_hover_ui(|ui| {
                                                ui.label(
                                                    egui::RichText::new(
                                                        "QoS Issues (click to view log)",
                                                    )
                                                    .strong(),
                                                );
                                                ui.separator();
                                                for (element_id, data) in &problem_elements {
                                                    let health = data.health();
                                                    ui.horizontal(|ui| {
                                                        ui.colored_label(
                                                            health.color(),
                                                            health.icon(),
                                                        );
                                                        ui.label(format!(
                                                            "{}: {:.1}%",
                                                            element_id,
                                                            data.avg_proportion * 100.0
                                                        ));
                                                    });
                                                }
                                            });
                                        }
                                    }
                                }

                                child_ui.add_space(4.0);

                                // Show flow name with truncation - constrain width first
                                let name_label = child_ui
                                    .allocate_ui_with_layout(
                                        egui::vec2(available_name_width, rect.height()),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&flow.name)
                                                        .color(text_color),
                                                )
                                                .truncate()
                                                .sense(egui::Sense::click()),
                                            )
                                        },
                                    )
                                    .inner;

                                // Handle click on the text itself (in addition to the background)
                                if name_label.clicked() {
                                    // Defer flow selection to next frame to avoid accesskit panic
                                    self.pending_flow_selection = Some(flow.id);
                                }

                                // Add hover tooltip with flow details (shows full name)
                                name_label.on_hover_ui(|ui| {
                                    ui.label(egui::RichText::new(&flow.name).strong());
                                    ui.separator();

                                    if let Some(ref desc) = flow.properties.description {
                                        if !desc.is_empty() {
                                            ui.label("Description:");
                                            ui.label(desc);
                                            ui.add_space(5.0);
                                        }
                                    }

                                    ui.label(format!("Clock: {:?}", flow.properties.clock_type));

                                    if let Some(domain) = flow.properties.ptp_domain {
                                        ui.label(format!("PTP Domain: {}", domain));
                                    }

                                    if let Some(sync_status) = flow.properties.clock_sync_status {
                                        use strom_types::flow::ClockSyncStatus;
                                        let status_text = match sync_status {
                                            ClockSyncStatus::Synced => "Synced",
                                            ClockSyncStatus::NotSynced => "Not Synced",
                                            ClockSyncStatus::Unknown => "Unknown",
                                        };
                                        ui.label(format!("Sync Status: {}", status_text));
                                    }

                                    // Display PTP grandmaster info if available
                                    if let Some(ref ptp_info) = flow.properties.ptp_info {
                                        if let Some(ref gm) = ptp_info.grandmaster_clock_id {
                                            ui.label(format!("Grandmaster: {}", gm));
                                        }
                                    }

                                    // Show pinned CPUs if available from thread monitor
                                    let pinned_cpus = self.thread_monitor
                                        .get_sorted_threads()
                                        .iter()
                                        .filter_map(|h| h.latest.as_ref())
                                        .find(|s| s.flow_id == flow.id)
                                        .and_then(|s| s.pinned_cpus.clone());
                                    if let Some(ref cpus) = pinned_cpus {
                                        ui.label(format!("Pinned to CPUs: {}", crate::system_monitor::format_cpu_range(cpus)));
                                    }

                                    ui.add_space(5.0);
                                    let state_text = if flow.running {
                                        "Running"
                                    } else {
                                        "Stopped"
                                    };
                                    ui.label(format!("State: {}", state_text));

                                    // Show timestamps
                                    if flow.properties.started_at.is_some()
                                        || flow.properties.last_modified.is_some()
                                        || flow.properties.created_at.is_some()
                                    {
                                        ui.add_space(5.0);
                                        ui.separator();

                                        if let Some(ref started_at) = flow.properties.started_at {
                                            ui.label(format!(
                                                "Started: {}",
                                                format_datetime_local(started_at)
                                            ));
                                            if let Some(started_millis) =
                                                parse_iso8601_to_millis(started_at)
                                            {
                                                let uptime_millis =
                                                    current_time_millis() - started_millis;
                                                ui.label(format!(
                                                    "Uptime: {}",
                                                    format_uptime(uptime_millis)
                                                ));
                                            }
                                        }

                                        if let Some(ref modified) = flow.properties.last_modified {
                                            ui.label(format!(
                                                "Last modified: {}",
                                                format_datetime_local(modified)
                                            ));
                                        }

                                        if let Some(ref created) = flow.properties.created_at {
                                            ui.label(format!(
                                                "Created: {}",
                                                format_datetime_local(created)
                                            ));
                                        }
                                    }
                                });

                                // Buttons on the right
                                child_ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.add_space(4.0);

                                        // Single menu button with dropdown
                                        ui.menu_button(egui_phosphor::regular::DOTS_THREE_VERTICAL, |ui| {
                                            ui.set_min_width(150.0);

                                            // Properties
                                            if ui
                                                .button(format!(
                                                    "{} Properties",
                                                    egui_phosphor::regular::GEAR
                                                ))
                                                .clicked()
                                            {
                                                self.editing_properties_flow_id = Some(flow.id);
                                                self.properties_name_buffer = flow.name.clone();
                                                self.properties_description_buffer = flow
                                                    .properties
                                                    .description
                                                    .clone()
                                                    .unwrap_or_default();
                                                self.properties_clock_type_buffer =
                                                    flow.properties.clock_type;
                                                self.properties_ptp_domain_buffer = flow
                                                    .properties
                                                    .ptp_domain
                                                    .map(|d| d.to_string())
                                                    .unwrap_or_else(|| "0".to_string());
                                                self.properties_thread_priority_buffer =
                                                    flow.properties.thread_priority;
                                                self.properties_cpu_affinity_buffer =
                                                    flow.properties.cpu_affinity;
                                                ui.close();
                                            }

                                            ui.separator();

                                            // Export as JSON
                                            if ui
                                                .button(format!(
                                                    "{} Export as JSON",
                                                    egui_phosphor::regular::UPLOAD_SIMPLE
                                                ))
                                                .clicked()
                                            {
                                                match serde_json::to_string_pretty(flow) {
                                                    Ok(json) => {
                                                        crate::clipboard::copy_text_with_ctx(
                                                            ui.ctx(),
                                                            &json,
                                                        );
                                                        self.status = format!(
                                                    "Flow '{}' exported to clipboard as JSON",
                                                    flow.name
                                                );
                                                    }
                                                    Err(e) => {
                                                        self.error = Some(format!(
                                                            "Failed to export flow: {}",
                                                            e
                                                        ));
                                                    }
                                                }
                                                ui.close();
                                            }

                                            // Export to gst-launch (only if flow has elements, not blocks)
                                            let has_only_elements =
                                                !flow.elements.is_empty() && flow.blocks.is_empty();
                                            let tooltip = if has_only_elements {
                                                "Export as gst-launch-1.0 pipeline"
                                            } else {
                                                "Only available for flows with elements, not blocks"
                                            };
                                            if ui
                                                .add_enabled(
                                                    has_only_elements,
                                                    egui::Button::new(format!("{} Export as gst-launch", egui_phosphor::regular::TERMINAL)),
                                                )
                                                .on_hover_text(tooltip)
                                                .clicked()
                                                && has_only_elements
                                            {
                                                self.pending_gst_launch_export = Some((
                                                    flow.elements.clone(),
                                                    flow.links.clone(),
                                                    flow.name.clone(),
                                                ));
                                                ui.close();
                                            }

                                            ui.separator();

                                            // Copy flow
                                            if ui
                                                .button(format!(
                                                    "{} Copy",
                                                    egui_phosphor::regular::COPY
                                                ))
                                                .clicked()
                                            {
                                                self.flow_pending_copy = Some(flow.clone());
                                                ui.close();
                                            }

                                            // Delete flow
                                            if ui
                                                .button(format!(
                                                    "{} Delete",
                                                    egui_phosphor::regular::TRASH
                                                ))
                                                .clicked()
                                            {
                                                self.flow_pending_deletion =
                                                    Some((flow.id, flow.name.clone()));
                                                ui.close();
                                            }
                                        });

                                        // Show clock sync indicator for PTP/NTP
                                        use strom_types::flow::{
                                            ClockSyncStatus, GStreamerClockType,
                                        };
                                        if matches!(
                                            flow.properties.clock_type,
                                            GStreamerClockType::Ptp | GStreamerClockType::Ntp
                                        ) {
                                            let (text_color, tooltip) =
                                                match flow.properties.clock_sync_status {
                                                    Some(ClockSyncStatus::Synced) => (
                                                        Color32::from_rgb(0, 200, 0),
                                                        format!(
                                                            "{:?} - Synchronized",
                                                            flow.properties.clock_type
                                                        ),
                                                    ),
                                                    Some(ClockSyncStatus::NotSynced) => (
                                                        Color32::from_rgb(200, 0, 0),
                                                        format!(
                                                            "{:?} - Not Synchronized",
                                                            flow.properties.clock_type
                                                        ),
                                                    ),
                                                    Some(ClockSyncStatus::Unknown) | None => (
                                                        Color32::GRAY,
                                                        format!(
                                                            "{:?} - Unknown",
                                                            flow.properties.clock_type
                                                        ),
                                                    ),
                                                };

                                            ui.add_space(4.0);
                                            ui.add(egui::Label::new(
                                                egui::RichText::new(
                                                    egui_phosphor::regular::CLOCK_CLOCKWISE,
                                                )
                                                    .size(12.0)
                                                    .color(text_color),
                                            ))
                                            .on_hover_text(tooltip);
                                        }

                                        // Show thread priority warning indicator if priority not achieved
                                        if let Some(ref status) =
                                            flow.properties.thread_priority_status
                                        {
                                            if !status.achieved && status.error.is_some() {
                                                let warning_color = Color32::from_rgb(255, 165, 0);
                                                let tooltip = status
                                                    .error
                                                    .as_ref()
                                                    .map(|e| {
                                                        format!("Thread priority not set: {}", e)
                                                    })
                                                    .unwrap_or_else(|| {
                                                        "Thread priority warning".to_string()
                                                    });

                                                ui.add_space(2.0);
                                                ui.add(
                                                    egui::Label::new(
                                                        egui::RichText::new(
                                                            egui_phosphor::regular::WARNING,
                                                        )
                                                            .size(12.0)
                                                            .color(warning_color),
                                                    )
                                                    .sense(egui::Sense::hover()),
                                                )
                                                .on_hover_text(tooltip);
                                            }
                                        }

                                        // Show CPU icon when affinity is not the default (Off)
                                        if flow.properties.cpu_affinity
                                            != strom_types::flow::CpuAffinity::default()
                                        {
                                            let tooltip = match flow.properties.cpu_affinity {
                                                strom_types::flow::CpuAffinity::SingleCore => {
                                                    "CPU affinity: Single Core"
                                                }
                                                strom_types::flow::CpuAffinity::Off => {
                                                    unreachable!()
                                                }
                                            };
                                            ui.add_space(2.0);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(
                                                        egui_phosphor::regular::CPU,
                                                    )
                                                    .size(12.0)
                                                    .color(Color32::GRAY),
                                                )
                                                .sense(egui::Sense::hover()),
                                            )
                                            .on_hover_text(tooltip);
                                        }
                                    },
                                );
                            }
                        }
                    }); // ScrollArea
            });
    }

    /// Render the element palette sidebar.
    pub(super) fn render_palette(&mut self, ctx: &Context) {
        if !self.show_palette_panel {
            return;
        }
        // Max width is 40% of screen width
        #[allow(deprecated)]
        let max_width = ctx.screen_rect().width() * 0.4;
        SidePanel::right("palette")
            .default_width(250.0)
            .max_width(max_width)
            .resizable(true)
            .show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                // Check if an element is selected and trigger property loading if needed
                // Do this BEFORE getting mutable reference to avoid borrow checker issues
                if let Some((selected_element_type, active_tab)) = self
                    .graph
                    .get_selected_element()
                    .map(|e| (e.element_type.clone(), self.graph.active_property_tab))
                {
                    // Trigger lazy loading if properties not cached
                    if !self.palette.has_properties_cached(&selected_element_type) {
                        tracing::info!(
                            "Element '{}' selected but properties not cached, triggering lazy load",
                            selected_element_type
                        );
                        self.load_element_properties(selected_element_type.clone(), ctx);
                    }

                    // Trigger pad properties loading if on Input/Output Pads tabs
                    use crate::graph::PropertyTab;
                    if matches!(active_tab, PropertyTab::InputPads | PropertyTab::OutputPads)
                        && !self.palette.has_pad_properties_cached(&selected_element_type)
                    {
                        tracing::info!(
                            "Element '{}' showing pad tab but pad properties not cached, triggering lazy load",
                            selected_element_type
                        );
                        self.load_element_pad_properties(selected_element_type.clone(), ctx);
                    }
                }

                // Show either the palette or the property inspector, not both
                // Clone selected ID before any borrows for probe section
                let selected_id_for_probe = self.graph.selected.clone();
                // Collect data BEFORE getting mutable reference to avoid borrow checker issues
                let selected_element_data = self.graph.get_selected_element().map(|element| {
                    let active_tab = self.graph.active_property_tab;

                    // Use pad properties if showing pad tabs, otherwise regular properties
                    use crate::graph::PropertyTab;
                    let element_info = if matches!(active_tab, PropertyTab::InputPads | PropertyTab::OutputPads) {
                        self.palette.get_element_info_with_pads(&element.element_type).cloned()
                    } else {
                        self.palette.get_element_info(&element.element_type).cloned()
                    };

                    let element_id = element.id.clone();
                    let focused_pad = self.graph.focused_pad.clone();
                    let input_pads = self.graph.get_actual_input_pads(&element_id);
                    let output_pads = self.graph.get_actual_output_pads(&element_id);
                    (element_info, active_tab, focused_pad, input_pads, output_pads)
                });

                if let Some((element_info, active_tab, focused_pad, input_pads, output_pads)) = selected_element_data {
                    // Element selected: show ONLY property inspector
                    let mut delete_element = false;
                    ui.horizontal(|ui| {
                        ui.heading("Properties");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(format!(
                                    "{} Delete",
                                    egui_phosphor::regular::TRASH
                                ))
                                .on_hover_text("Delete element")
                                .clicked()
                            {
                                delete_element = true;
                            }
                        });
                    });
                    ui.separator();

                    // Probe section
                    if let Some(ref eid) = selected_id_for_probe {
                        self.render_probe_button(ui, eid);
                        // render_probe_details adds its own separator when active
                        if !self.render_probe_details(ui, eid) {
                            ui.separator();
                        }
                    }

                    if delete_element {
                        self.graph.remove_selected();
                    } else {
                        // Split borrow: get mutable access to graph fields separately
                        let graph = &mut self.graph;
                        if let Some(element) = graph.get_selected_element_mut() {
                            let (new_tab, _) = PropertyInspector::show(
                                ui,
                                element,
                                element_info.as_ref(),
                                active_tab,
                                focused_pad,
                                input_pads,
                                output_pads,
                            );
                            graph.active_property_tab = new_tab;
                        }
                    }
                } else if let Some(block_def_id) = self
                    .graph
                    .get_selected_block()
                    .map(|b| b.block_definition_id.clone())
                {
                    // Block selected: show block property inspector
                    let mut delete_block = false;
                    ui.horizontal(|ui| {
                        ui.heading("Block Properties");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(format!(
                                    "{} Delete",
                                    egui_phosphor::regular::TRASH
                                ))
                                .on_hover_text("Delete block")
                                .clicked()
                            {
                                delete_block = true;
                            }
                        });
                    });
                    ui.separator();

                    // Probe section
                    if let Some(ref eid) = selected_id_for_probe {
                        self.render_probe_button(ui, eid);
                        // render_probe_details adds its own separator when active
                        if !self.render_probe_details(ui, eid) {
                            ui.separator();
                        }
                    }

                    if delete_block {
                        self.graph.remove_selected();
                    }

                    // Clone definition to avoid borrow checker issues
                    let definition_opt = self
                        .graph
                        .get_block_definition_by_id(&block_def_id)
                        .cloned();
                    let flow_id = self.current_flow().map(|f| f.id);

                    // Load network interfaces if block has NetworkInterface properties
                    if let Some(ref def) = definition_opt {
                        let has_network_prop = def.exposed_properties.iter().any(|prop| {
                            matches!(
                                prop.property_type,
                                strom_types::block::PropertyType::NetworkInterface
                            )
                        });
                        if has_network_prop {
                            self.load_network_interfaces(ctx.clone());
                        }

                        // Load available channels if this is an InterInput block
                        // Only refresh once when selection changes to this block
                        if def.id == "builtin.inter_input" {
                            if let Some(block) = self.graph.get_selected_block() {
                                let block_id = block.id.clone();
                                if self.last_inter_input_refresh.as_ref() != Some(&block_id) {
                                    self.last_inter_input_refresh = Some(block_id);
                                    self.refresh_available_channels();
                                }
                            }
                            self.load_available_channels(ctx.clone());
                        }
                    }

                    // Get RTP stats for this flow if available
                    let rtp_stats = flow_id
                        .map(|fid| fid.to_string())
                        .and_then(|fid| self.rtp_stats_cache.get(&fid));

                    // Then get mutable reference to block
                    if let (Some(block), Some(def)) =
                        (self.graph.get_selected_block_mut(), definition_opt)
                    {
                        let block_id = block.id.clone();
                        let recorder_filename = flow_id
                            .and_then(|fid| self.recorder_filenames.get(&(fid, block_id.clone())))
                            .map(|s| s.as_str());
                        let recorder_start_time = flow_id
                            .and_then(|fid| self.recorder_start_times.get(&(fid, block_id.clone())))
                            .copied();
                        let block_thumbnail = flow_id
                            .and_then(|fid| self.block_thumbnails.get(&(fid, block.id.clone())));
                        let result = PropertyInspector::show_block(
                            ui,
                            block,
                            &def,
                            flow_id,
                            &self.audioanalyzer_data,
                            &self.meter_data,
                            &self.spectrum_data,
                            &self.loudness_data,
                            &self.latency_data,
                            &self.webrtc_stats,
                            rtp_stats,
                            &self.network_interfaces,
                            &self.available_channels,
                            &mut self.qr_inline,
                            &mut self.qr_cache,
                            recorder_filename,
                            recorder_start_time,
                            block_thumbnail,
                        );

                        // Handle deletion request
                        if result.delete_requested {
                            self.graph.remove_selected();
                        }

                        // Handle browse streams request (for AES67 Input)
                        if result.browse_streams_requested {
                            self.show_stream_picker_for_block = Some(block_id.clone());
                            // Refresh discovered streams for the picker
                            self.discovery_page.refresh(&self.api, ctx, &self.channels.tx);
                        }

                        // Handle browse NDI sources request (for NDI Input)
                        if result.browse_ndi_sources_requested {
                            self.show_ndi_picker_for_block = Some(block_id.clone());
                            // Refresh NDI sources for the picker
                            self.discovery_page.refresh(&self.api, ctx, &self.channels.tx);
                        }

                        // Handle VLC playlist download request (for MPEG-TS/SRT Output)
                        if let Some((srt_uri, latency_ms)) = result.vlc_playlist_requested {
                            // Get flow name for the stream title
                            let stream_name = self
                                .current_flow()
                                .map(|f| f.name.clone())
                                .unwrap_or_else(|| "SRT Stream".to_string());

                            let playlist_content =
                                generate_vlc_playlist(&srt_uri, latency_ms, &stream_name);

                            // Generate filename based on flow name
                            let safe_name: String = stream_name
                                .chars()
                                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                                .collect();
                            let filename = format!("{}.xspf", safe_name);

                            download_file(&filename, &playlist_content, "application/xspf+xml");
                        }

                        // Handle VLC playlist download-only request (native mode)
                        #[cfg(not(target_arch = "wasm32"))]
                        if let Some((srt_uri, latency_ms)) = result.vlc_playlist_download_only {
                            let stream_name = self
                                .current_flow()
                                .map(|f| f.name.clone())
                                .unwrap_or_else(|| "SRT Stream".to_string());

                            let playlist_content =
                                generate_vlc_playlist(&srt_uri, latency_ms, &stream_name);

                            let safe_name: String = stream_name
                                .chars()
                                .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                                .collect();
                            let filename = format!("{}.xspf", safe_name);

                            // Save to current directory (download only, don't open)
                            let path = std::path::PathBuf::from(&filename);
                            match std::fs::write(&path, &playlist_content) {
                                Ok(_) => {
                                    let abs_path = std::fs::canonicalize(&path)
                                        .unwrap_or(path.clone());
                                    self.status = format!("Saved: {}", abs_path.display());
                                    tracing::info!("Saved VLC playlist to: {}", abs_path.display());
                                }
                                Err(e) => {
                                    self.status = format!("Failed to save: {}", e);
                                    tracing::error!("Failed to save VLC playlist: {}", e);
                                }
                            }
                        }

                        // Handle WHEP player request (for WHEP Output)
                        if let Some(endpoint_id) = result.whep_player_url {
                            let player_url = self.api.get_whep_player_url(&endpoint_id);
                            ctx.open_url(egui::OpenUrl::new_tab(&player_url));
                        }

                        // Handle copy WHEP URL to clipboard
                        if let Some(endpoint_id) = result.copy_whep_url_requested {
                            let player_url = self.api.get_whep_player_url(&endpoint_id);
                            crate::clipboard::copy_text_with_ctx(ctx, &player_url);
                            self.status = "Player URL copied to clipboard".to_string();
                        }

                        // Handle vision mixer control page request
                        if let Some(flow_id) = result.vision_mixer_url {
                            let url = self.api.get_vision_mixer_url(&flow_id);
                            ctx.open_url(egui::OpenUrl::new_tab(&url));
                        }

                        // Handle WHIP ingest request (for WHIP Input)
                        if let Some(endpoint_id) = result.whip_ingest_url {
                            let ingest_url = self.api.get_whip_ingest_url(&endpoint_id);
                            ctx.open_url(egui::OpenUrl::new_tab(&ingest_url));
                        }

                        // Handle copy WHIP ingest URL to clipboard
                        if let Some(endpoint_id) = result.copy_whip_url_requested {
                            let ingest_url = self.api.get_whip_ingest_url(&endpoint_id);
                            crate::clipboard::copy_text_with_ctx(ctx, &ingest_url);
                            self.status = "Ingest URL copied to clipboard".to_string();
                        }

                        // Handle QR code toggle for WHEP player
                        if let Some(endpoint_id) = result.show_qr_whep {
                            let server_hostname = self.system_info.as_ref().map(|s| s.hostname.as_str());
                            let player_url = make_external_url(
                                &self.api.get_whep_player_url(&endpoint_id),
                                server_hostname,
                            );
                            if self.qr_inline.as_ref().is_some_and(|(bid, _)| bid == &block_id) {
                                self.qr_inline = None;
                            } else {
                                self.qr_inline = Some((block_id.clone(), player_url));
                            }
                        }

                        // Handle QR code toggle for WHIP ingest
                        if let Some(endpoint_id) = result.show_qr_whip {
                            let server_hostname = self.system_info.as_ref().map(|s| s.hostname.as_str());
                            let ingest_url = make_external_url(
                                &self.api.get_whip_ingest_url(&endpoint_id),
                                server_hostname,
                            );
                            if self.qr_inline.as_ref().is_some_and(|(bid, _)| bid == &block_id) {
                                self.qr_inline = None;
                            } else {
                                self.qr_inline = Some((block_id.clone(), ingest_url));
                            }
                        }

                        // Handle loudness reset request
                        if let Some((flow_id, block_id)) = result.loudness_reset_requested {
                            let api = self.api.clone();
                            spawn_task(async move {
                                if let Err(e) = api.reset_loudness(&flow_id, &block_id).await {
                                    tracing::warn!("Failed to reset loudness: {}", e);
                                }
                            });
                        }

                        // Handle recorder split-now request
                        if let Some((flow_id, block_id)) = result.recorder_split_requested {
                            let api = self.api.clone();
                            spawn_task(async move {
                                if let Err(e) = api.recorder_split_now(&flow_id, &block_id).await {
                                    tracing::warn!("Failed to trigger recorder split: {}", e);
                                }
                            });
                        }

                        // Handle recorder file download request
                        if let Some(relative_path) = result.recorder_download_requested {
                            let url = self.api.get_media_download_url(&relative_path);
                            ctx.open_url(egui::OpenUrl::new_tab(&url));
                        }
                    } else {
                        ui.label("Block definition not found");
                    }

                } else if let Some(link) = self.graph.get_selected_link().cloned() {
                    // Link selected: show connection info
                    let mut delete_link = false;
                    let (from_ref, to_ref) = link.to_pad_refs();

                    ui.horizontal(|ui| {
                        ui.heading("Connection");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(format!(
                                    "{} Delete",
                                    egui_phosphor::regular::TRASH
                                ))
                                .on_hover_text("Delete connection")
                                .clicked()
                            {
                                delete_link = true;
                            }
                        });
                    });
                    ui.separator();

                    let from_label = self.graph.node_label(&from_ref.element_id);
                    let to_label = self.graph.node_label(&to_ref.element_id);

                    egui::Grid::new("connection_info")
                        .num_columns(2)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("From:");
                            if let Some(ref pad) = from_ref.pad_name {
                                ui.label(format!("{} ({})", from_label, pad));
                            } else {
                                ui.label(&from_label);
                            }
                            ui.end_row();

                            ui.strong("To:");
                            if let Some(ref pad) = to_ref.pad_name {
                                ui.label(format!("{} ({})", to_label, pad));
                            } else {
                                ui.label(&to_label);
                            }
                            ui.end_row();
                        });

                    if delete_link {
                        self.graph.remove_selected_link();
                    }
                } else {
                    // No element, block, or link selected: show ONLY the palette
                    self.palette.show(ui);
                }
            }); // ScrollArea
            });
    }

    /// Render the main canvas area.
    pub(super) fn render_canvas(&mut self, ctx: &Context) {
        CentralPanel::default().show(ctx, |ui| {
            // Panel toggle buttons at the edges (use clip_rect for full area including margins)
            let panel_rect = ui.clip_rect();

            // Left panel toggle (flow list)
            let left_toggle_pos = egui::pos2(panel_rect.left(), panel_rect.top() + 4.0);
            egui::Area::new(egui::Id::new("left_panel_toggle"))
                .fixed_pos(left_toggle_pos)
                .order(egui::Order::Middle)
                .show(ctx, |ui| {
                    let icon = if self.show_flow_list_panel {
                        egui_phosphor::regular::CARET_LEFT
                    } else {
                        egui_phosphor::regular::CARET_RIGHT
                    };
                    let tooltip = if self.show_flow_list_panel {
                        "Hide flow list"
                    } else {
                        "Show flow list"
                    };
                    // No rounding on left side (flush with edge)
                    let corner_radius = egui::CornerRadius {
                        nw: 0,
                        sw: 0,
                        ne: 4,
                        se: 4,
                    };
                    let button = egui::Button::new(egui::RichText::new(icon).size(24.0))
                        .corner_radius(corner_radius)
                        .min_size(egui::vec2(32.0, 32.0));
                    if ui.add(button).on_hover_text(tooltip).clicked() {
                        self.show_flow_list_panel = !self.show_flow_list_panel;
                    }
                });

            // Right panel toggle (palette) - only show when a flow is selected
            if self.current_flow().is_some() {
                let right_toggle_pos =
                    egui::pos2(panel_rect.right() - 32.0, panel_rect.top() + 4.0);
                egui::Area::new(egui::Id::new("right_panel_toggle"))
                    .fixed_pos(right_toggle_pos)
                    .order(egui::Order::Middle)
                    .show(ctx, |ui| {
                        let icon = if self.show_palette_panel {
                            egui_phosphor::regular::CARET_RIGHT
                        } else {
                            egui_phosphor::regular::CARET_LEFT
                        };
                        let tooltip = if self.show_palette_panel {
                            "Hide palette"
                        } else {
                            "Show palette"
                        };
                        // No rounding on right side (flush with edge)
                        let corner_radius = egui::CornerRadius {
                            nw: 4,
                            sw: 4,
                            ne: 0,
                            se: 0,
                        };
                        let button = egui::Button::new(egui::RichText::new(icon).size(24.0))
                            .corner_radius(corner_radius)
                            .min_size(egui::vec2(32.0, 32.0));
                        if ui.add(button).on_hover_text(tooltip).clicked() {
                            self.show_palette_panel = !self.show_palette_panel;
                        }
                    });
            }
            if self.current_flow().is_some() {
                // Setup dynamic content for meter blocks before rendering
                self.graph.clear_block_content();
                if let Some(flow_id) = self.current_flow().map(|f| f.id) {
                    // Clone block IDs to avoid borrowing issues
                    let meter_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.meter")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in meter_blocks {
                        if let Some(meter_data) = self.meter_data.get(&flow_id, &block_id) {
                            let channel_count = meter_data.rms.len();
                            let meter_data_clone = meter_data.clone();

                            // For 1-2 channels the meters fit inside the base
                            // block height — no need to expand the node.
                            let additional_height = if channel_count <= 2 {
                                0.0
                            } else {
                                crate::meter::calculate_compact_height(channel_count)
                            };

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::meter::show_compact(ui, &meter_data_clone);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for spectrum blocks
                    let spectrum_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.spectrum")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in spectrum_blocks {
                        if let Some(spectrum_data) = self.spectrum_data.get(&flow_id, &block_id) {
                            let channel_count = spectrum_data.magnitudes.len();
                            let spectrum_data_clone = spectrum_data.clone();

                            // Single channel fits in the base block height.
                            // Multi-channel: add 30px per extra channel.
                            let additional_height = if channel_count <= 1 {
                                0.0
                            } else {
                                (channel_count - 1) as f32 * 30.0
                            };

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::spectrum::show_compact(ui, &spectrum_data_clone);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for loudness blocks
                    let loudness_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.loudness")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in loudness_blocks {
                        if let Some(loudness_data) = self.loudness_data.get(&flow_id, &block_id) {
                            let loudness_data_clone = loudness_data.clone();
                            let height = crate::loudness::calculate_compact_height();

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: height + 10.0,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::loudness::show_compact(ui, &loudness_data_clone);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for audio analyzer blocks
                    let analyzer_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.audioanalyzer")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in analyzer_blocks {
                        if let Some(analyzer_data) =
                            self.audioanalyzer_data.get(&flow_id, &block_id)
                        {
                            let analyzer_data_clone = analyzer_data.clone();
                            let height = crate::audioanalyzer::calculate_compact_height();

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: height + 10.0,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::audioanalyzer::show_compact(
                                            ui,
                                            &analyzer_data_clone,
                                        );
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for latency blocks
                    let latency_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.latency")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in latency_blocks {
                        if let Some(latency_data) = self.latency_data.get(&flow_id, &block_id) {
                            let height = crate::latency::calculate_compact_height();
                            let latency_data_clone = latency_data.clone();

                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: height + 10.0,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        crate::latency::show_compact(ui, &latency_data_clone);
                                    })),
                                },
                            );
                        }
                    }

                    // Setup dynamic content for WHIP/WHEP blocks
                    let webrtc_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| {
                            b.block_definition_id == "builtin.whep_input"
                                || b.block_definition_id == "builtin.whep_output"
                                || b.block_definition_id == "builtin.whip_output"
                                || b.block_definition_id == "builtin.whip_input"
                        })
                        .map(|b| b.id.clone())
                        .collect();

                    if let Some(stats) = self.webrtc_stats.get(&flow_id) {
                        for block_id in webrtc_blocks {
                            // Filter connections to only those belonging to this block
                            // Connection names are formatted as "block_id:element_name:..."
                            let block_prefix = format!("{}:", block_id);
                            let filtered_connections: std::collections::HashMap<_, _> = stats
                                .connections
                                .iter()
                                .filter(|(name, _)| name.starts_with(&block_prefix))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();

                            let stats_for_block = strom_types::api::WebRtcStats {
                                connections: filtered_connections,
                            };

                            // Only show content if there are connections for this block
                            if !stats_for_block.connections.is_empty() {
                                self.graph.set_block_content(
                                    block_id,
                                    crate::graph::BlockContentInfo {
                                        additional_height: 25.0,
                                        render_callback: Some(Box::new(move |ui, _rect| {
                                            crate::webrtc_stats::show_compact(ui, &stats_for_block);
                                        })),
                                    },
                                );
                            }
                        }
                    }

                    // Setup dynamic content for Media Player blocks
                    let player_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.media_player")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in player_blocks {
                        // Get player data or use default
                        let player_data = self
                            .mediaplayer_data
                            .get(&flow_id, &block_id)
                            .cloned()
                            .unwrap_or_default();

                        let height = crate::mediaplayer::calculate_compact_height();
                        let player_data_clone = player_data.clone();
                        let block_id_for_action = block_id.clone();

                        self.graph.set_block_content(
                            block_id,
                            crate::graph::BlockContentInfo {
                                additional_height: height + 10.0,
                                render_callback: Some(Box::new(move |ui, rect| {
                                    // Double-click anywhere on the media player node opens the playlist
                                    let interact_id = egui::Id::new("media_player_dblclick")
                                        .with(&block_id_for_action);
                                    if ui
                                        .interact(rect, interact_id, egui::Sense::click())
                                        .double_clicked()
                                    {
                                        set_local_storage(
                                            "player_action",
                                            &format!("{}:playlist", block_id_for_action),
                                        );
                                    }

                                    if let Some((action, seek_pos)) =
                                        crate::mediaplayer::show_compact(ui, &player_data_clone)
                                    {
                                        // Use local storage to signal actions
                                        let action_data = if let Some(pos) = seek_pos {
                                            format!("{}:{}:{}", block_id_for_action, action, pos)
                                        } else {
                                            format!("{}:{}", block_id_for_action, action)
                                        };
                                        tracing::debug!("Setting player_action: {}", action_data);
                                        set_local_storage("player_action", &action_data);
                                    }
                                })),
                            },
                        );
                    }

                    // Setup dynamic content for thumbnail blocks
                    let thumbnail_blocks: Vec<_> = self
                        .graph
                        .blocks
                        .iter()
                        .filter(|b| b.block_definition_id == "builtin.thumbnail")
                        .map(|b| b.id.clone())
                        .collect();

                    for block_id in thumbnail_blocks {
                        if let Some(texture) =
                            self.block_thumbnails.get(&(flow_id, block_id.clone()))
                        {
                            let texture_id = texture.id();
                            let tex_size = texture.size_vec2();
                            let aspect = tex_size.y / tex_size.x;
                            // Inline thumbnail: fit within block width (~120px)
                            let thumb_h = 120.0 * aspect;
                            self.graph.set_block_content(
                                block_id,
                                crate::graph::BlockContentInfo {
                                    additional_height: thumb_h,
                                    render_callback: Some(Box::new(move |ui, _rect| {
                                        let w = ui.available_width();
                                        let h = w * aspect;
                                        ui.image(egui::load::SizedTexture::new(
                                            texture_id,
                                            egui::vec2(w, h),
                                        ));
                                    })),
                                },
                            );
                        }
                    }
                }

                // Update QoS and buffer age health maps for the current flow before rendering
                if let Some(flow_id) = self.selected_flow_id {
                    let qos_health_map = self.qos_stats.get_element_health_map(&flow_id);
                    self.graph.set_qos_health_map(qos_health_map);
                    let ba_health_map = self.buffer_age_data.get_element_health_map(&flow_id);
                    self.graph.set_buffer_age_health_map(ba_health_map);
                }

                // Show graph editor
                let response = self.graph.show(ui);

                // Open palette panel when something is selected
                if self.graph.has_selection() && !self.show_palette_panel {
                    self.show_palette_panel = true;
                }

                // Check if a QoS marker in the graph was clicked - open log panel
                if self.graph.was_qos_marker_clicked() {
                    self.show_log_panel = true;
                }

                // Check if user double-clicked on background - open palette and focus search
                if self.graph.take_open_palette_request() {
                    self.show_palette_panel = true;
                    self.palette.focus_search();
                }

                // Toggle right pane when clicking background while nothing is selected
                if self.graph.take_toggle_right_pane_request() {
                    self.show_palette_panel = !self.show_palette_panel;
                }

                // Handle adding elements from palette
                if let Some(element_type) = self.palette.take_dragging_element() {
                    // Add element at center of visible area
                    let center = response.rect.center();
                    let world_pos = ((center - response.rect.min - self.graph.pan_offset)
                        / self.graph.zoom)
                        .to_pos2();
                    self.graph.add_element(element_type.clone(), world_pos);

                    // Trigger pad info loading if not already cached
                    if !self.palette.has_pad_properties_cached(&element_type) {
                        self.load_element_pad_properties(element_type, ctx);
                    }
                }

                // Handle adding blocks from palette
                if let Some(block_id) = self.palette.take_dragging_block() {
                    // Add block at center of visible area
                    let center = response.rect.center();
                    let world_pos = ((center - response.rect.min - self.graph.pan_offset)
                        / self.graph.zoom)
                        .to_pos2();

                    // Set default description for InterOutput blocks
                    if block_id == "builtin.inter_output" {
                        // Count existing inter_output blocks to get next number
                        let counter = self
                            .graph
                            .blocks
                            .iter()
                            .filter(|b| b.block_definition_id == "builtin.inter_output")
                            .count()
                            + 1;
                        let mut props = std::collections::HashMap::new();
                        props.insert(
                            "description".to_string(),
                            strom_types::PropertyValue::String(format!("stream_{}", counter)),
                        );
                        self.graph.add_block_with_props(block_id, world_pos, props);
                    } else {
                        self.graph.add_block(block_id, world_pos);
                    }
                }

                // Handle delete key for elements and links
                // Only process delete if no text edit widget has focus
                if ui.input(|i| i.key_pressed(egui::Key::Delete))
                    && !ui.ctx().wants_keyboard_input()
                {
                    self.graph.remove_selected(); // Remove selected element (if any)
                    self.graph.remove_selected_link(); // Remove selected link (if any)
                }
            } else {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("Welcome to Strom");
                    ui.label("Select a flow from the sidebar or create a new one");
                });
            }
        });
    }

    /// Render the status bar.
    pub(super) fn render_status_bar(&mut self, ctx: &Context) {
        TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                ui.separator();
                ui.label(format!("Flows: {}", self.flows.len()));

                // Log message counts with toggle button
                let (errors, warnings, _infos) = self.log_counts();
                if errors > 0 || warnings > 0 {
                    ui.separator();
                    let toggle_text = if self.show_log_panel {
                        format!("Messages: {} errors, {} warnings [hide]", errors, warnings)
                    } else {
                        format!("Messages: {} errors, {} warnings [show]", errors, warnings)
                    };
                    let color = if errors > 0 {
                        Color32::from_rgb(255, 80, 80)
                    } else {
                        Color32::from_rgb(255, 200, 50)
                    };
                    if ui
                        .add(
                            egui::Label::new(egui::RichText::new(&toggle_text).color(color))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Click to toggle message panel")
                        .clicked()
                    {
                        self.show_log_panel = !self.show_log_panel;
                    }
                }

                // Version info on the right side
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Debug console button (WASM only)
                    #[cfg(target_arch = "wasm32")]
                    {
                        if ui
                            .small_button(egui_phosphor::regular::BUG)
                            .on_hover_text("Toggle debug console")
                            .clicked()
                        {
                            crate::wasm_utils::toggle_debug_console_panel();
                        }
                        ui.separator();
                    }

                    if let Some(ref version_info) = self.system_info {
                        let version_text = if !version_info.git_tag.is_empty() {
                            // On a tagged release
                            version_info.git_tag.to_string()
                        } else {
                            // Development version
                            format!("v{}-{}", version_info.version, version_info.git_hash)
                        };

                        let color = if version_info.git_dirty {
                            Color32::from_rgb(255, 165, 0) // Orange for dirty
                        } else if !version_info.git_tag.is_empty() {
                            Color32::from_rgb(0, 200, 0) // Green for release
                        } else {
                            Color32::GRAY // Gray for dev
                        };

                        let full_version_text = if version_info.git_dirty {
                            format!("{} (modified)", version_text)
                        } else {
                            version_text
                        };

                        ui.colored_label(color, full_version_text)
                            .on_hover_ui(|ui| {
                                ui.label(format!("Version: v{}", version_info.version));
                                ui.label(format!("Git: {}", version_info.git_hash));
                                if !version_info.git_tag.is_empty() {
                                    ui.label(format!("Tag: {}", version_info.git_tag));
                                }
                                ui.label(format!("Branch: {}", version_info.git_branch));
                                ui.label(format!("Built: {}", version_info.build_timestamp));
                                if !version_info.gstreamer_version.is_empty() {
                                    ui.label(format!(
                                        "GStreamer: {}",
                                        version_info.gstreamer_version
                                    ));
                                }
                                if !version_info.os_info.is_empty() {
                                    let os_text = if version_info.in_docker {
                                        format!("{} (Docker)", version_info.os_info)
                                    } else {
                                        version_info.os_info.clone()
                                    };
                                    ui.label(format!("OS: {}", os_text));
                                }
                                if version_info.git_dirty {
                                    ui.colored_label(
                                        Color32::YELLOW,
                                        "Working directory had uncommitted changes",
                                    );
                                }
                            });
                    }
                });
            });
        });
    }
}
