//! Flows page rendering for the Strom application.
//!
//! This module contains the flow list sidebar rendering logic,
//! separated from the main app.rs for better code organization.

use egui::{Color32, Context, SidePanel};
use strom_types::{Flow, PipelineState};

use crate::app::StromApp;

/// Implementation of flows page specific rendering for StromApp.
impl StromApp {
    /// Render the flow list sidebar.
    pub(crate) fn render_flow_list(&mut self, ctx: &Context) {
        SidePanel::left("flow_list")
            .default_width(200.0)
            .resizable(true)
            .show(ctx, |ui| {
                // Filter input at top
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    let filter_id = egui::Id::new("flow_list_filter");
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.flow_filter).id(filter_id));
                    if self.focus_flow_filter_requested {
                        self.focus_flow_filter_requested = false;
                        response.request_focus();
                    }
                    if !self.flow_filter.is_empty() && ui.small_button("✕").clicked() {
                        self.flow_filter.clear();
                    }
                });
                ui.add_space(4.0);

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
                            filter_lower.is_empty() || f.name.to_lowercase().contains(&filter_lower)
                        })
                        .collect();
                    sorted_flows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

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
                                        self.graph.load(flow.elements.clone(), flow.links.clone());
                                        self.graph.load_blocks(flow.blocks.clone());
                                    }
                                } else {
                                    let flow = sorted_flows[0];
                                    self.selected_flow_id = Some(flow.id);
                                    self.graph.deselect_all();
                                    self.graph.clear_runtime_dynamic_pads();
                                    self.graph.load(flow.elements.clone(), flow.links.clone());
                                    self.graph.load_blocks(flow.blocks.clone());
                                }
                            } else if input.key_pressed(egui::Key::ArrowUp) {
                                if let Some(idx) = current_idx {
                                    if idx > 0 {
                                        let flow = sorted_flows[idx - 1];
                                        self.selected_flow_id = Some(flow.id);
                                        self.graph.deselect_all();
                                        self.graph.clear_runtime_dynamic_pads();
                                        self.graph.load(flow.elements.clone(), flow.links.clone());
                                        self.graph.load_blocks(flow.blocks.clone());
                                    }
                                } else if !sorted_flows.is_empty() {
                                    let flow = sorted_flows[sorted_flows.len() - 1];
                                    self.selected_flow_id = Some(flow.id);
                                    self.graph.deselect_all();
                                    self.graph.clear_runtime_dynamic_pads();
                                    self.graph.load(flow.elements.clone(), flow.links.clone());
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
                            // Select the flow by ID
                            self.selected_flow_id = Some(flow.id);
                            // Clear graph selection when switching flows
                            self.graph.deselect_all();
                            // Clear runtime dynamic pads (will be re-fetched if flow is running)
                            self.graph.clear_runtime_dynamic_pads();
                            // Load flow into graph editor
                            self.graph.load(flow.elements.clone(), flow.links.clone());
                            self.graph.load_blocks(flow.blocks.clone());
                            // Request focus for keyboard navigation
                            ui.memory_mut(|mem| mem.request_focus(list_id));
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
                                    ((bg_color.r() as u16 + qos_color.r() as u16) / 2) as u8,
                                    ((bg_color.g() as u16 + qos_color.g() as u16) / 2) as u8,
                                    ((bg_color.b() as u16 + qos_color.b() as u16) / 2) as u8,
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
                            let border_rect =
                                egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
                            ui.painter().rect_filled(border_rect, 0.0, qos_color);
                        } else if response.hovered() {
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                ui.visuals().widgets.hovered.bg_fill,
                            );
                        }

                        // Draw flow name and buttons
                        let mut child_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        );
                        child_ui.add_space(4.0);

                        let text_color = if selected {
                            ui.visuals().selection.stroke.color
                        } else {
                            ui.visuals().text_color()
                        };

                        // Show running state icon
                        let state_icon = match flow.state {
                            Some(PipelineState::Playing) => "▶",
                            Some(PipelineState::Paused) => "⏸",
                            Some(PipelineState::Ready) | Some(PipelineState::Null) | None => "⏹",
                        };
                        let state_color = match flow.state {
                            Some(PipelineState::Playing) => Color32::from_rgb(0, 200, 0),
                            Some(PipelineState::Paused) => Color32::from_rgb(255, 165, 0),
                            Some(PipelineState::Ready) | Some(PipelineState::Null) | None => {
                                Color32::GRAY
                            }
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
                                            egui::RichText::new("QoS Issues (click to view log)")
                                                .strong(),
                                        );
                                        ui.separator();
                                        for (element_id, data) in &problem_elements {
                                            let health = data.health();
                                            ui.horizontal(|ui| {
                                                ui.colored_label(health.color(), health.icon());
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

                        // Show flow name with hover tooltip - make it clickable too
                        let name_label = child_ui
                            .colored_label(text_color, &flow.name)
                            .interact(egui::Sense::click());

                        // Handle click on the text itself (in addition to the background)
                        if name_label.clicked() {
                            self.selected_flow_id = Some(flow.id);
                            // Clear graph selection when switching flows
                            self.graph.deselect_all();
                            self.graph.load(flow.elements.clone(), flow.links.clone());
                            self.graph.load_blocks(flow.blocks.clone());
                        }

                        // Add hover tooltip with flow details
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

                            ui.add_space(5.0);
                            let state_text = match flow.state {
                                Some(PipelineState::Playing) => "Running",
                                Some(PipelineState::Paused) => "Paused",
                                Some(PipelineState::Ready) | Some(PipelineState::Null) | None => {
                                    "Stopped"
                                }
                            };
                            ui.label(format!("State: {}", state_text));
                        });

                        // Buttons on the right
                        child_ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add_space(4.0);

                                // Single menu button with dropdown
                                ui.menu_button("...", |ui| {
                                    ui.set_min_width(150.0);

                                    // Properties
                                    if ui.button("⚙  Properties").clicked() {
                                        self.editing_properties_flow_id = Some(flow.id);
                                        self.properties_name_buffer = flow.name.clone();
                                        self.properties_description_buffer =
                                            flow.properties.description.clone().unwrap_or_default();
                                        self.properties_clock_type_buffer =
                                            flow.properties.clock_type;
                                        self.properties_ptp_domain_buffer = flow
                                            .properties
                                            .ptp_domain
                                            .map(|d| d.to_string())
                                            .unwrap_or_else(|| "0".to_string());
                                        self.properties_thread_priority_buffer =
                                            flow.properties.thread_priority;
                                        ui.close();
                                    }

                                    ui.separator();

                                    // Export as JSON
                                    if ui.button("📤  Export as JSON").clicked() {
                                        match serde_json::to_string_pretty(flow) {
                                            Ok(json) => {
                                                ui.ctx().copy_text(json);
                                                self.status = format!(
                                                    "Flow '{}' exported to clipboard as JSON",
                                                    flow.name
                                                );
                                            }
                                            Err(e) => {
                                                self.error =
                                                    Some(format!("Failed to export flow: {}", e));
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
                                            egui::Button::new("🖥  Export as gst-launch"),
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
                                    if ui.button("📋  Copy").clicked() {
                                        self.flow_pending_copy = Some(flow.clone());
                                        ui.close();
                                    }

                                    // Delete flow
                                    if ui.button("🗑  Delete").clicked() {
                                        self.flow_pending_deletion =
                                            Some((flow.id, flow.name.clone()));
                                        ui.close();
                                    }
                                });

                                // Show clock sync indicator for PTP/NTP (small colored dot)
                                use strom_types::flow::{ClockSyncStatus, GStreamerClockType};
                                if matches!(
                                    flow.properties.clock_type,
                                    GStreamerClockType::Ptp | GStreamerClockType::Ntp
                                ) {
                                    let (text_color, tooltip) = match flow
                                        .properties
                                        .clock_sync_status
                                    {
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
                                            format!("{:?} - Unknown", flow.properties.clock_type),
                                        ),
                                    };

                                    // Small colored dot indicator
                                    ui.add_space(4.0);
                                    ui.add(egui::Label::new(
                                        egui::RichText::new("*").size(12.0).color(text_color),
                                    ))
                                    .on_hover_text(tooltip);
                                }

                                // Show thread priority warning indicator if priority not achieved
                                if let Some(ref status) = flow.properties.thread_priority_status {
                                    if !status.achieved && status.error.is_some() {
                                        let warning_color = Color32::from_rgb(255, 165, 0);
                                        let tooltip = status
                                            .error
                                            .as_ref()
                                            .map(|e| format!("Thread priority not set: {}", e))
                                            .unwrap_or_else(|| {
                                                "Thread priority warning".to_string()
                                            });

                                        ui.add_space(2.0);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new("⚠")
                                                    .size(12.0)
                                                    .color(warning_color),
                                            )
                                            .sense(egui::Sense::hover()),
                                        )
                                        .on_hover_text(tooltip);
                                    }
                                }
                            },
                        );
                    }
                }
            });
    }
}
