use crate::info_page::{
    current_time_millis, format_datetime_local, format_uptime, parse_iso8601_to_millis,
};
use crate::state::AppMessage;
use egui::Color32;

use super::*;

fn truncate_str(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

impl StromApp {
    /// Render the log panel showing errors, warnings, and info messages.
    pub(super) fn render_log_panel(&mut self, ui: &mut egui::Ui) {
        if !self.show_log_panel || self.log_entries.is_empty() {
            return;
        }

        // Calculate dynamic height based on number of entries (min 80px, max 200px)
        let panel_height = (self.log_entries.len() as f32 * 20.0).clamp(80.0, 200.0);

        // Collect actions to perform after rendering (to avoid borrow issues)
        let mut entry_to_remove: Option<usize> = None;
        let mut navigate_to: Option<(strom_types::FlowId, Option<String>)> = None;
        let mut copy_entry_text: Option<String> = None;

        // Pre-build flow name lookup
        let flow_names: std::collections::HashMap<strom_types::FlowId, String> =
            self.flows.iter().map(|f| (f.id, f.name.clone())).collect();

        egui::Panel::bottom("log_panel")
            .resizable(true)
            .min_size(80.0)
            .max_size(400.0)
            .default_size(panel_height)
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Pipeline Messages");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(egui_phosphor::regular::TRASH)
                            .on_hover_text("Clear all")
                            .clicked()
                        {
                            self.clear_log_entries();
                            self.qos_stats = crate::qos_monitor::QoSStore::new();
                        }
                        if ui
                            .button(egui_phosphor::regular::COPY)
                            .on_hover_text("Copy all entries to clipboard")
                            .clicked()
                        {
                            let all_text: String = self
                                .log_entries
                                .iter()
                                .rev()
                                .map(|e| {
                                    let flow = e
                                        .flow_id
                                        .and_then(|fid| flow_names.get(&fid))
                                        .map(|n| n.as_str())
                                        .unwrap_or("");
                                    let source = e.source.as_deref().unwrap_or("");
                                    format!("[{}] [{}] {}", flow, source, e.message)
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            crate::clipboard::copy_text_with_ctx(ui.ctx(), &all_text);
                        }
                        if ui
                            .button(egui_phosphor::regular::X)
                            .on_hover_text("Hide panel")
                            .clicked()
                        {
                            self.show_log_panel = false;
                        }
                    });
                });

                ui.separator();

                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let entries_len = self.log_entries.len();
                        for (rev_idx, entry) in self.log_entries.iter().rev().enumerate() {
                            let actual_idx = entries_len - 1 - rev_idx;

                            ui.horizontal(|ui| {
                                // Copy button (leftmost)
                                if ui
                                    .small_button(
                                        egui::RichText::new(egui_phosphor::regular::COPY)
                                            .size(14.0),
                                    )
                                    .on_hover_text("Copy")
                                    .clicked()
                                {
                                    let flow = entry
                                        .flow_id
                                        .and_then(|fid| flow_names.get(&fid))
                                        .map(|n| n.as_str())
                                        .unwrap_or("");
                                    let source = entry.source.as_deref().unwrap_or("");
                                    copy_entry_text =
                                        Some(format!("[{}] [{}] {}", flow, source, entry.message));
                                }

                                // Dismiss button
                                if ui
                                    .small_button(
                                        egui::RichText::new(egui_phosphor::regular::X).size(14.0),
                                    )
                                    .on_hover_text("Dismiss")
                                    .clicked()
                                {
                                    entry_to_remove = Some(actual_idx);
                                }

                                // Flow name (clickable, underline on hover)
                                if let Some(flow_id) = entry.flow_id {
                                    let flow_name = flow_names
                                        .get(&flow_id)
                                        .map(|n| n.as_str())
                                        .unwrap_or("unknown");
                                    let truncated_flow = truncate_str(flow_name, 15);

                                    let flow_label = ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&truncated_flow)
                                                .color(Color32::GRAY),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if flow_label.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        ui.painter().line_segment(
                                            [
                                                flow_label.rect.left_bottom(),
                                                flow_label.rect.right_bottom(),
                                            ],
                                            egui::Stroke::new(1.0_f32, Color32::GRAY),
                                        );
                                    }
                                    if flow_label.clicked() {
                                        navigate_to = Some((flow_id, entry.source.clone()));
                                    }
                                    flow_label.on_hover_text(format!("Navigate to {}", flow_name));
                                }

                                // Element/block ID (clickable, underline on hover)
                                if let Some(ref source) = entry.source {
                                    let truncated_source = truncate_str(source, 15);
                                    let source_label = ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(format!("[{}]", truncated_source))
                                                .color(Color32::from_rgb(150, 150, 255)),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if source_label.hovered() {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        ui.painter().line_segment(
                                            [
                                                source_label.rect.left_bottom(),
                                                source_label.rect.right_bottom(),
                                            ],
                                            egui::Stroke::new(
                                                1.0_f32,
                                                Color32::from_rgb(150, 150, 255),
                                            ),
                                        );
                                    }
                                    if source_label.clicked() {
                                        if let Some(flow_id) = entry.flow_id {
                                            navigate_to = Some((flow_id, Some(source.clone())));
                                        }
                                    }
                                    source_label.on_hover_text(format!("Navigate to {}", source));
                                }

                                // Message
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&entry.message).color(entry.color()),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                );
                            });
                        }
                    });
            });

        // Copy single entry
        if let Some(text) = copy_entry_text {
            crate::clipboard::copy_text_with_ctx(ui.ctx(), &text);
        }

        // Process deferred actions
        if let Some(idx) = entry_to_remove {
            // Check if this is a QoS entry - if so, clear from QoS store
            if idx < self.log_entries.len() {
                let entry = &self.log_entries[idx];
                if entry.message.starts_with("QoS:") {
                    if let (Some(flow_id), Some(ref element_id)) = (entry.flow_id, &entry.source) {
                        self.qos_stats.clear_element(&flow_id, element_id);
                    }
                }
                self.log_entries.remove(idx);
            }
        }

        if let Some((flow_id, element_id)) = navigate_to {
            // Navigate to the flow
            self.selected_flow_id = Some(flow_id);

            // Clear any existing focus before changing graph structure
            // to prevent accesskit panic when focused node is removed
            ui.ctx().memory_mut(|mem| {
                if let Some(focused_id) = mem.focused() {
                    mem.surrender_focus(focused_id);
                }
            });

            // Find and load the flow
            if let Some(flow) = self.flows.iter().find(|f| f.id == flow_id).cloned() {
                self.graph.deselect_all();
                self.graph.load(flow.elements.clone(), flow.links.clone());
                self.graph.load_blocks(flow.blocks.clone());

                // If we have an element ID, try to select it in the graph
                if let Some(ref elem_id) = element_id {
                    // ElementId is a String, so we can use it directly
                    // It will match either an element or a block
                    self.graph.select_node(elem_id.clone());
                    // Center the view on the selected element
                    self.graph.center_on_selected();
                }
            }
        }
    }

    /// Render the new flow dialog.
    pub(super) fn render_new_flow_dialog(&mut self, ui: &mut egui::Ui) {
        if !self.show_new_flow_dialog {
            return;
        }

        egui::Window::new("New Flow")
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.new_flow_name);
                });

                // Check for Enter key to create flow
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.new_flow_name.is_empty() {
                    self.create_flow(ui.ctx());
                }

                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        self.create_flow(ui.ctx());
                    }

                    if ui.button("Cancel").clicked() {
                        self.show_new_flow_dialog = false;
                        self.new_flow_name.clear();
                    }
                });
            });
    }

    /// Render the delete confirmation dialog.
    pub(super) fn render_delete_confirmation_dialog(&mut self, ui: &mut egui::Ui) {
        if self.flow_pending_deletion.is_none() {
            return;
        }

        let (flow_id, flow_name) = self.flow_pending_deletion.as_ref().unwrap().clone();

        egui::Window::new("Delete Flow")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Are you sure you want to delete this flow?");
                ui.add_space(5.0);
                ui.colored_label(Color32::YELLOW, format!("Flow: {}", flow_name));
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if ui
                        .button(format!("{} Delete", egui_phosphor::regular::TRASH))
                        .clicked()
                    {
                        self.delete_flow(flow_id, ui.ctx());
                        self.flow_pending_deletion = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.flow_pending_deletion = None;
                    }
                });
            });
    }

    /// Render the system monitor window.
    pub(super) fn render_system_monitor_window(&mut self, ui: &mut egui::Ui) {
        if !self.show_system_monitor {
            return;
        }

        egui::Window::new("System Monitoring")
            .collapsible(true)
            .resizable(true)
            .default_width(700.0)
            .default_height(500.0)
            .open(&mut self.show_system_monitor)
            .show(ui.ctx(), |ui| {
                // Build flow_id -> name mapping
                let flow_names: std::collections::HashMap<_, _> =
                    self.flows.iter().map(|f| (f.id, f.name.clone())).collect();

                let nav_action = crate::system_monitor::DetailedSystemMonitor::new(
                    &self.system_monitor,
                    &self.thread_monitor,
                    &mut self.system_monitor_tab,
                    &mut self.thread_sort_column,
                    &mut self.thread_sort_direction,
                    &flow_names,
                )
                .show(ui);

                // Store action to handle after closure (to avoid borrow conflict)
                if let Some(action) = nav_action {
                    self.pending_thread_nav_action = Some(action);
                }
            });

        // Handle navigation action outside the closure
        if let Some(action) = self.pending_thread_nav_action.take() {
            match action {
                crate::system_monitor::ThreadNavigationAction::Flow(flow_id) => {
                    self.select_flow(flow_id);
                }
                crate::system_monitor::ThreadNavigationAction::Block { flow_id, block_id } => {
                    self.select_flow(flow_id);
                    self.graph.select_node(block_id);
                }
                crate::system_monitor::ThreadNavigationAction::Element {
                    flow_id,
                    element_name,
                } => {
                    self.select_flow(flow_id);
                    self.graph.select_node(element_name);
                }
            }
        }
    }

    /// Render the flow properties dialog.
    pub(super) fn render_flow_properties_dialog(&mut self, ui: &mut egui::Ui) {
        let flow_id = match self.editing_properties_flow_id {
            Some(id) => id,
            None => return,
        };

        let flow = match self.flows.iter().find(|f| f.id == flow_id) {
            Some(f) => f,
            None => {
                self.editing_properties_flow_id = None;
                return;
            }
        };

        let flow_name = flow.name.clone();

        egui::Window::new(format!("{} {} - Properties", egui_phosphor::regular::GEAR, flow_name))
            .collapsible(false)
            .resizable(true)
            .movable(true)
            .default_width(400.0)
            .default_height(500.0)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ui.ctx().content_rect().center())
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 50.0) // Leave room for buttons
                    .show(ui, |ui| {
                ui.heading("Flow Properties");
                ui.add_space(5.0);

                // Persistence
                ui.label(egui::RichText::new("Persistence").strong());
                ui.checkbox(&mut self.properties_ephemeral_buffer, "Ephemeral (don't persist to storage)");
                ui.add_space(10.0);

                // Name
                ui.label("Name");
                ui.text_edit_singleline(&mut self.properties_name_buffer);
                ui.add_space(10.0);

                // Description
                ui.label("Description");
                ui.add(
                    egui::TextEdit::multiline(&mut self.properties_description_buffer)
                        .desired_width(f32::INFINITY)
                        .desired_rows(5)
                        .hint_text("Optional description for this flow..."),
                );

                ui.add_space(10.0);

                // Clock Type
                ui.label("Clock Type");
                ui.horizontal(|ui| {
                    use strom_types::flow::GStreamerClockType;

                    egui::ComboBox::from_id_salt("clock_type_selector")
                        .selected_text(self.properties_clock_type_buffer.label())
                        .show_ui(ui, |ui| {
                            for clock_type in GStreamerClockType::all() {
                                let label = if *clock_type == GStreamerClockType::Monotonic {
                                    format!("{} (recommended)", clock_type.label())
                                } else {
                                    clock_type.label().to_string()
                                };
                                ui.selectable_value(
                                    &mut self.properties_clock_type_buffer,
                                    *clock_type,
                                    label,
                                );
                            }
                        });
                });

                // Show description of selected clock type
                ui.label(self.properties_clock_type_buffer.description());

                // Show PTP domain field only when PTP is selected
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                ) {
                    ui.add_space(10.0);
                    ui.label("PTP Domain (0-255)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.properties_ptp_domain_buffer)
                            .desired_width(100.0)
                            .hint_text("0"),
                    );
                    ui.label("The PTP domain for clock synchronization");
                }

                // Show NTP server/port fields only when NTP is selected
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ntp
                ) {
                    ui.add_space(10.0);
                    ui.label("NTP Server");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.properties_ntp_server_buffer)
                            .desired_width(250.0)
                            .hint_text("pool.ntp.org"),
                    );
                    ui.label("NTP Port");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.properties_ntp_port_buffer)
                            .desired_width(100.0)
                            .hint_text("123"),
                    );
                }

                // Direct media timing selector (Auto / On / Off). Only meaningful
                // for wall-clock types — Monotonic has no external reference, so
                // base_time=0 would pin PTS to boot time.
                if self.properties_clock_type_buffer.supports_wall_clock_pts() {
                    ui.add_space(10.0);
                    ui.label("Direct Media Timing");
                    let is_ptp = matches!(
                        self.properties_clock_type_buffer,
                        strom_types::flow::GStreamerClockType::Ptp
                    );
                    let effective_default = if is_ptp { "On" } else { "Off" };
                    let current_label = match self.properties_direct_media_timing_buffer {
                        None => format!("Auto ({} for this clock type)", effective_default),
                        Some(true) => "On".to_string(),
                        Some(false) => "Off".to_string(),
                    };
                    egui::ComboBox::from_id_salt("direct_media_timing_selector")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.properties_direct_media_timing_buffer,
                                None,
                                format!("Auto ({} for this clock type)", effective_default),
                            );
                            ui.selectable_value(
                                &mut self.properties_direct_media_timing_buffer,
                                Some(true),
                                "On",
                            );
                            ui.selectable_value(
                                &mut self.properties_direct_media_timing_buffer,
                                Some(false),
                                "Off",
                            );
                        });
                    ui.label(
                        "Forces base_time=0, start_time=NONE. Required for AES67 \
                         (mediaclk:direct=0). Auto turns On for PTP, Off otherwise. \
                         Do not enable on flows with MPEG-TS demuxers, WHEP sinks, \
                         or other elements that assume running_time starts near 0.",
                    );
                }

                // Show clock sync status for PTP/NTP clocks
                if matches!(
                    self.properties_clock_type_buffer,
                    strom_types::flow::GStreamerClockType::Ptp
                        | strom_types::flow::GStreamerClockType::Ntp
                ) {
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        ui.label("Clock Status:");
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            if let Some(sync_status) = flow.properties.clock_sync_status {
                                use strom_types::flow::ClockSyncStatus;
                                match sync_status {
                                    ClockSyncStatus::Synced => {
                                        ui.colored_label(Color32::from_rgb(0, 200, 0), "[OK] Synced");
                                    }
                                    ClockSyncStatus::NotSynced => {
                                        ui.colored_label(
                                            Color32::from_rgb(200, 0, 0),
                                            "[!] Not Synced",
                                        );
                                    }
                                    ClockSyncStatus::Unknown => {
                                        ui.colored_label(Color32::GRAY, "[-] Unknown");
                                    }
                                }
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Unknown");
                            }
                        }
                    });

                    // Show PTP-specific options and link to Clocks page
                    if matches!(
                        self.properties_clock_type_buffer,
                        strom_types::flow::GStreamerClockType::Ptp
                    ) {
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                            ui.add_space(5.0);

                            // Show warning if restart needed - compare buffer with running domain
                            if let Some(ref ptp_info) = flow.properties.ptp_info {
                                let buffer_domain: u8 = self
                                    .properties_ptp_domain_buffer
                                    .parse()
                                    .unwrap_or(0);
                                let domain_changed = buffer_domain != ptp_info.domain;
                                if domain_changed {
                                    ui.colored_label(
                                        Color32::from_rgb(255, 165, 0),
                                        "! Restart needed - domain changed",
                                    );
                                }
                            }

                            // Button to open Clocks page for detailed stats
                            ui.add_space(5.0);
                            if ui
                                .button("View PTP Statistics")
                                .on_hover_text("Open Clocks page for detailed PTP statistics")
                                .clicked()
                            {
                                self.current_page = AppPage::Clocks;
                            }
                        }
                    }
                }

                ui.add_space(15.0);
                ui.separator();
                ui.add_space(10.0);

                // Thread Priority
                ui.label("Thread Priority");
                ui.horizontal(|ui| {
                    use strom_types::flow::ThreadPriority;

                    egui::ComboBox::from_id_salt("thread_priority_selector")
                        .selected_text(format!("{:?}", self.properties_thread_priority_buffer))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Normal,
                                "Normal",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::High,
                                "High (recommended)",
                            );
                            ui.selectable_value(
                                &mut self.properties_thread_priority_buffer,
                                ThreadPriority::Realtime,
                                "Realtime (requires privileges)",
                            );
                        });
                });

                // Show description of selected thread priority
                ui.label(self.properties_thread_priority_buffer.description());

                // Show thread priority status for running pipelines
                if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                    if let Some(ref status) = flow.properties.thread_priority_status {
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Status:");
                            if status.achieved {
                                ui.colored_label(
                                    Color32::from_rgb(0, 200, 0),
                                    format!("[OK] Achieved ({} threads)", status.threads_configured),
                                );
                            } else if let Some(ref err) = status.error {
                                ui.colored_label(Color32::from_rgb(255, 165, 0), "[!] Warning");
                                ui.label(format!("- {}", err));
                            } else {
                                ui.colored_label(Color32::GRAY, "[-] Not set");
                            }
                        });
                    }
                }

                ui.add_space(10.0);

                // CPU Affinity
                ui.label("CPU Affinity");
                ui.horizontal(|ui| {
                    use strom_types::flow::CpuAffinity;

                    egui::ComboBox::from_id_salt("cpu_affinity_selector")
                        .selected_text(match self.properties_cpu_affinity_buffer {
                            CpuAffinity::Off => "Off",
                            CpuAffinity::SingleCore => "Single Core",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.properties_cpu_affinity_buffer,
                                CpuAffinity::Off,
                                "Off (OS scheduler decides)",
                            );
                            ui.selectable_value(
                                &mut self.properties_cpu_affinity_buffer,
                                CpuAffinity::SingleCore,
                                "Single Core (max cache locality)",
                            );
                        });
                });

                // Show timestamps section
                if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter().find(|f| f.id == id)) {
                    let has_timestamps = flow.properties.created_at.is_some()
                        || flow.properties.last_modified.is_some()
                        || flow.properties.started_at.is_some();

                    if has_timestamps {
                        ui.add_space(15.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new("Timestamps").strong());

                        egui::Grid::new("timestamps_grid")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                if let Some(ref created) = flow.properties.created_at {
                                    ui.label("Created:");
                                    ui.label(format_datetime_local(created));
                                    ui.end_row();
                                }

                                if let Some(ref modified) = flow.properties.last_modified {
                                    ui.label("Last modified:");
                                    ui.label(format_datetime_local(modified));
                                    ui.end_row();
                                }

                                if let Some(ref started) = flow.properties.started_at {
                                    ui.label("Started:");
                                    ui.label(format_datetime_local(started));
                                    ui.end_row();

                                    // Show uptime
                                    if let Some(started_millis) = parse_iso8601_to_millis(started) {
                                        let uptime_millis = current_time_millis() - started_millis;
                                        ui.label("Uptime:");
                                        ui.label(format_uptime(uptime_millis));
                                        ui.end_row();
                                    }
                                }
                            });
                    }
                }

                }); // End ScrollArea

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(5.0);

                // Buttons (outside scroll area)
                ui.horizontal(|ui| {
                    if ui.button(format!("{} Save", egui_phosphor::regular::FLOPPY_DISK)).clicked() {
                        // Update flow properties
                        if let Some(flow) = self.editing_properties_flow_id.and_then(|id| self.flows.iter_mut().find(|f| f.id == id)) {
                            // Update flow name
                            flow.name = self.properties_name_buffer.clone();

                            flow.properties.description =
                                if self.properties_description_buffer.is_empty() {
                                    None
                                } else {
                                    Some(self.properties_description_buffer.clone())
                                };
                            flow.properties.clock_type = self.properties_clock_type_buffer;

                            // Parse and set PTP domain if PTP clock is selected
                            flow.properties.ptp_domain = if matches!(
                                self.properties_clock_type_buffer,
                                strom_types::flow::GStreamerClockType::Ptp
                            ) {
                                self.properties_ptp_domain_buffer.parse::<u8>().ok()
                            } else {
                                None
                            };

                            // Set NTP server/port if NTP clock is selected
                            let is_ntp = matches!(
                                self.properties_clock_type_buffer,
                                strom_types::flow::GStreamerClockType::Ntp
                            );
                            flow.properties.ntp_server = if is_ntp {
                                let trimmed = self.properties_ntp_server_buffer.trim();
                                if trimmed.is_empty() {
                                    None
                                } else {
                                    Some(trimmed.to_string())
                                }
                            } else {
                                None
                            };
                            flow.properties.ntp_port = if is_ntp {
                                self.properties_ntp_port_buffer.parse::<u16>().ok()
                            } else {
                                None
                            };

                            // Save direct media timing selection.
                            flow.properties.direct_media_timing =
                                self.properties_direct_media_timing_buffer;

                            // Set thread priority
                            flow.properties.thread_priority =
                                self.properties_thread_priority_buffer;

                            // Set CPU affinity
                            flow.properties.cpu_affinity =
                                self.properties_cpu_affinity_buffer;

                            // Set ephemeral
                            flow.properties.ephemeral = self.properties_ephemeral_buffer;

                            let flow_clone = flow.clone();
                            let api = self.api.clone();
                            let ctx_clone = ui.ctx().clone();

                            spawn_task(async move {
                                match api.update_flow(&flow_clone).await {
                                    Ok(_) => {
                                        tracing::info!("Flow properties updated successfully - WebSocket event will trigger refresh");
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to update flow properties: {}", e);
                                    }
                                }
                                ctx_clone.request_repaint();
                            });
                        }
                        self.editing_properties_flow_id = None;
                    }

                    if ui.button("Cancel").clicked() {
                        self.editing_properties_flow_id = None;
                    }
                });
            });
    }

    /// Render the stream picker modal for selecting discovered streams.
    pub(super) fn render_stream_picker_modal(&mut self, ui: &mut egui::Ui) {
        let Some(block_id) = self.show_stream_picker_for_block.clone() else {
            return;
        };

        let mut is_open = true;
        let mut selected_sdp: Option<String> = None;

        egui::Window::new("Select Discovered Stream")
            .open(&mut is_open)
            .collapsible(false)
            .resizable(true)
            .default_width(500.0)
            .default_height(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Select a stream to use its SDP:");
                ui.add_space(8.0);

                let streams = &self.discovery_page.discovered_streams;
                let is_loading = self.discovery_page.loading;

                if is_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading discovered streams...");
                    });
                } else if streams.is_empty() {
                    ui.label("No discovered streams available.");
                    ui.label("Make sure SAP discovery is running and streams are being announced on the network.");
                    ui.add_space(8.0);
                    if ui.button(egui_phosphor::regular::ARROWS_CLOCKWISE).on_hover_text("Refresh").clicked() {
                        self.discovery_page.refresh(&self.api, ui.ctx(), &self.channels.tx);
                    }
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for stream in streams {
                                let text = format!(
                                    "{} - {}:{} ({}ch {}Hz)",
                                    stream.name,
                                    stream.multicast_address,
                                    stream.port,
                                    stream.channels,
                                    stream.sample_rate
                                );

                                if ui.selectable_label(false, &text).clicked() {
                                    // Fetch SDP for this stream
                                    // For now, we'll construct it from the stream info
                                    // In a real implementation, we'd fetch the actual SDP
                                    selected_sdp = Some(stream.id.clone());
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();

                ui.horizontal(|ui| {
                    let refresh_clicked = ui.button(egui_phosphor::regular::ARROWS_CLOCKWISE).on_hover_text("Refresh").clicked();
                    if refresh_clicked {
                        self.discovery_page
                            .refresh(&self.api, ui.ctx(), &self.channels.tx);
                    }
                });
            });

        // Close modal if X button was clicked
        if !is_open {
            self.show_stream_picker_for_block = None;
        }

        // If a stream was selected, fetch its SDP and update the block
        if let Some(stream_id) = selected_sdp {
            self.show_stream_picker_for_block = None;

            // Fetch the SDP and update the block
            let api = self.api.clone();
            let tx = self.channels.sender();
            let ctx = ui.ctx().clone();

            spawn_task(async move {
                match api.get_stream_sdp(&stream_id).await {
                    Ok(sdp) => {
                        tracing::info!(
                            "Fetched SDP for stream {}, sending to block {}",
                            stream_id,
                            block_id
                        );
                        let _ = tx.send(AppMessage::StreamPickerSdpLoaded { block_id, sdp });
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch stream SDP for {}: {}", stream_id, e);
                        let _ = tx.send(AppMessage::FlowOperationError(format!(
                            "Failed to fetch stream SDP: {}",
                            e
                        )));
                    }
                }
                ctx.request_repaint();
            });
        }
    }

    /// Render the NDI picker modal for selecting discovered NDI sources.
    pub(super) fn render_ndi_picker_modal(&mut self, ui: &mut egui::Ui) {
        let Some(block_id) = self.show_ndi_picker_for_block.clone() else {
            return;
        };

        let mut is_open = true;
        let mut selected_ndi_name: Option<String> = None;

        // Get data before window closure to avoid borrowing issues
        let mut sources = self.discovery_page.get_ndi_sources().to_vec();
        let is_loading = self.discovery_page.loading;
        let ndi_available = self.discovery_page.ndi_available;

        // Sort sources alphabetically by name
        sources.sort_by_key(|a| a.name.to_lowercase());

        // Filter sources based on search
        let search_filter = self.ndi_search_filter.to_lowercase();
        let filtered_sources: Vec<_> = if search_filter.is_empty() {
            sources
        } else {
            sources
                .into_iter()
                .filter(|s| {
                    s.name.to_lowercase().contains(&search_filter)
                        || s.ip_address()
                            .map(|ip| ip.contains(&search_filter))
                            .unwrap_or(false)
                        || s.url_address()
                            .map(|url| url.to_lowercase().contains(&search_filter))
                            .unwrap_or(false)
                })
                .collect()
        };

        egui::Window::new("Select NDI Source")
            .open(&mut is_open)
            .collapsible(false)
            .resizable(true)
            .default_width(500.0)
            .default_height(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Select an NDI source:");
                ui.add_space(8.0);

                // Search filter input
                ui.horizontal(|ui| {
                    ui.label("Search:");
                    ui.text_edit_singleline(&mut self.ndi_search_filter);
                });
                ui.add_space(8.0);

                if !ndi_available {
                    ui.label("NDI discovery is not available.");
                    ui.label("Make sure the GStreamer NDI plugin is installed.");
                } else if is_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Loading NDI sources...");
                    });
                } else if filtered_sources.is_empty() {
                    if search_filter.is_empty() {
                        ui.label("No NDI sources discovered on the network.");
                    } else {
                        ui.label("No NDI sources match the search filter.");
                    }
                    ui.add_space(8.0);
                    if ui.button("Refresh").clicked() {
                        self.discovery_page
                            .refresh(&self.api, ui.ctx(), &self.channels.tx);
                    }
                } else {
                    // Scroll area for the source list
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(250.0)
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            for source in &filtered_sources {
                                let text = if let Some(ip) = source.ip_address() {
                                    format!("{} ({})", source.name, ip)
                                } else if let Some(url) = source.url_address() {
                                    format!("{} ({})", source.name, url)
                                } else {
                                    source.name.clone()
                                };

                                let clicked = ui.selectable_label(false, &text).clicked();
                                if clicked {
                                    selected_ndi_name = Some(source.name.clone());
                                }
                            }
                        });
                }

                ui.add_space(8.0);
                ui.separator();

                ui.horizontal(|ui| {
                    if ndi_available {
                        let refresh_clicked = ui.button("Refresh").clicked();
                        if refresh_clicked {
                            self.discovery_page
                                .refresh(&self.api, ui.ctx(), &self.channels.tx);
                        }
                    }
                });
            });

        // Close modal if X button was clicked
        if !is_open {
            self.show_ndi_picker_for_block = None;
            self.ndi_search_filter.clear();
        }

        // If an NDI source was selected, update the block's ndi_name property
        if let Some(ndi_name) = selected_ndi_name {
            self.show_ndi_picker_for_block = None;
            self.ndi_search_filter.clear();

            // Update the block's ndi_name property
            if let Some(block) = self.graph.get_block_by_id_mut(&block_id) {
                block.properties.insert(
                    "ndi_name".to_string(),
                    strom_types::PropertyValue::String(ndi_name.clone()),
                );
                self.status = format!("NDI source set to: {}", ndi_name);
                tracing::info!("NDI source '{}' selected for block {}", ndi_name, block_id);
            } else {
                tracing::warn!("Block {} not found when setting NDI source", block_id);
            }
        }
    }
}
