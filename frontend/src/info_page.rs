//! Info page for displaying system and version information.

use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use std::collections::VecDeque;

use crate::api::VersionInfo;
use crate::system_monitor::SystemMonitorStore;

const HISTORY_SIZE: usize = 60;
const MARGIN: f32 = 16.0;
const GAP: f32 = 12.0;
const GRAPH_HEIGHT: f32 = 60.0;
// Minimum content width to ensure readability
const MIN_CONTENT_WIDTH: f32 = 800.0;
// Frame inner margin (used in render_box)
const BOX_INNER_MARGIN: f32 = 12.0;

/// Info page state.
pub struct InfoPage {
    /// Whether we've requested network interfaces load
    requested_network_load: bool,
}

impl InfoPage {
    pub fn new() -> Self {
        Self {
            requested_network_load: false,
        }
    }

    /// Check if network interfaces should be loaded (call once on page show).
    pub fn should_load_network(&mut self) -> bool {
        if !self.requested_network_load {
            self.requested_network_load = true;
            true
        } else {
            false
        }
    }

    /// Render the info page.
    pub fn render(
        &mut self,
        ui: &mut Ui,
        version_info: Option<&VersionInfo>,
        system_monitor: &SystemMonitorStore,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
        flows: &[strom_types::Flow],
    ) {
        // Get available width and use minimum if window is too small
        // Subtract extra padding to account for scrollbar and frame overhead
        let available_width = ui.available_width() - 60.0;
        let content_width = (available_width - 2.0 * MARGIN).max(MIN_CONTENT_WIDTH);

        // Calculate box widths
        // Row 1: 3 boxes with 2 gaps → box = (content - 2*gap) / 3
        let box_width_3 = (content_width - 2.0 * GAP) / 3.0;
        // Row 2+3: 2 boxes with 1 gap → box = (content - gap) / 2
        let box_width_2 = (content_width - GAP) / 2.0;

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Disable default item spacing in horizontal layouts
                ui.spacing_mut().item_spacing.x = 0.0;

                ui.add_space(MARGIN);

                // Row 1: Version & Build | System | GStreamer
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "Version & Build", box_width_3, |ui| {
                        self.render_version_content(ui, version_info);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "System", box_width_3, |ui| {
                        self.render_system_content(ui, version_info, flows);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "GStreamer", box_width_3, |ui| {
                        self.render_gstreamer_content(ui, version_info);
                    });
                });

                ui.add_space(GAP);

                // Row 2: CPU | Memory
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "CPU", box_width_2, |ui| {
                        self.render_cpu_content(ui, system_monitor, box_width_2);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "Memory", box_width_2, |ui| {
                        self.render_memory_content(ui, system_monitor, box_width_2);
                    });
                });

                ui.add_space(GAP);

                // Row 3: GPU | Network
                ui.horizontal(|ui| {
                    ui.add_space(MARGIN);

                    render_box(ui, "GPU", box_width_2, |ui| {
                        self.render_gpu_content(ui, system_monitor, box_width_2);
                    });

                    ui.add_space(GAP);

                    render_box(ui, "Network Interfaces", box_width_2, |ui| {
                        self.render_network_content(ui, network_interfaces);
                    });
                });

                ui.add_space(MARGIN);
            });
    }

    fn render_version_content(&self, ui: &mut Ui, version_info: Option<&VersionInfo>) {
        if let Some(info) = version_info {
            egui::Grid::new("version_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    ui.label("Version:");
                    ui.label(egui::RichText::new(format!("v{}", info.version)).strong());
                    ui.end_row();

                    if !info.git_tag.is_empty() {
                        ui.label("Tag:");
                        ui.label(&info.git_tag);
                        ui.end_row();
                    }

                    ui.label("Git Hash:");
                    ui.label(egui::RichText::new(&info.git_hash).monospace());
                    ui.end_row();

                    ui.label("Branch:");
                    ui.label(&info.git_branch);
                    ui.end_row();

                    if info.git_dirty {
                        ui.label("Status:");
                        ui.colored_label(Color32::YELLOW, "Modified");
                        ui.end_row();
                    }

                    ui.label("Build:");
                    ui.label(&info.build_timestamp);
                    ui.end_row();
                });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
        }
    }

    fn render_system_content(
        &self,
        ui: &mut Ui,
        version_info: Option<&VersionInfo>,
        flows: &[strom_types::Flow],
    ) {
        egui::Grid::new("system_grid")
            .num_columns(2)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                if let Some(info) = version_info {
                    ui.label("OS:");
                    ui.label(&info.os_info);
                    ui.end_row();

                    ui.label("Environment:");
                    if info.in_docker {
                        ui.colored_label(Color32::from_rgb(100, 180, 255), "Docker");
                    } else {
                        ui.label("Native");
                    }
                    ui.end_row();
                }

                // Get hostname from browser location in WASM
                #[cfg(target_arch = "wasm32")]
                {
                    if let Some(window) = web_sys::window() {
                        if let Ok(host) = window.location().host() {
                            ui.label("Backend:");
                            ui.label(host);
                            ui.end_row();
                        }
                    }
                }

                // Flow statistics
                let running_flows = flows
                    .iter()
                    .filter(|f| f.state == Some(strom_types::PipelineState::Playing))
                    .count();
                let total_flows = flows.len();

                ui.label("Flows:");
                ui.label(format!("{} / {}", running_flows, total_flows));
                ui.end_row();
            });
    }

    fn render_gstreamer_content(&self, ui: &mut Ui, version_info: Option<&VersionInfo>) {
        if let Some(info) = version_info {
            egui::Grid::new("gstreamer_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .show(ui, |ui| {
                    ui.label("Version:");
                    ui.label(egui::RichText::new(&info.gstreamer_version).monospace());
                    ui.end_row();
                });
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading...");
            });
        }
    }

    fn render_cpu_content(&self, ui: &mut Ui, system_monitor: &SystemMonitorStore, box_width: f32) {
        if let Some(stats) = system_monitor.latest() {
            ui.label(format!("Usage: {:.1}%", stats.cpu_usage));
            ui.add_space(4.0);

            // Graph width = box width minus inner margin on both sides
            let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
            let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
            draw_graph(
                ui.painter(),
                rect,
                system_monitor.cpu_history(),
                Color32::from_rgb(100, 200, 255),
            );
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_memory_content(
        &self,
        ui: &mut Ui,
        system_monitor: &SystemMonitorStore,
        box_width: f32,
    ) {
        if let Some(stats) = system_monitor.latest() {
            let mem_percent = if stats.total_memory > 0 {
                (stats.used_memory as f32 / stats.total_memory as f32) * 100.0
            } else {
                0.0
            };

            ui.label(format!(
                "{:.1}% ({:.1} / {:.1} GB)",
                mem_percent,
                stats.used_memory as f64 / 1_073_741_824.0,
                stats.total_memory as f64 / 1_073_741_824.0
            ));
            ui.add_space(4.0);

            let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
            let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
            draw_graph(
                ui.painter(),
                rect,
                system_monitor.memory_history(),
                Color32::from_rgb(100, 255, 100),
            );
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_gpu_content(&self, ui: &mut Ui, system_monitor: &SystemMonitorStore, box_width: f32) {
        if let Some(stats) = system_monitor.latest() {
            if stats.gpu_stats.is_empty() {
                ui.label("No GPU detected");
                return;
            }

            for (i, gpu) in stats.gpu_stats.iter().enumerate() {
                if i > 0 {
                    ui.separator();
                    ui.add_space(4.0);
                }

                // GPU name
                ui.label(egui::RichText::new(&gpu.name).strong());
                ui.add_space(4.0);

                // GPU stats in a grid
                egui::Grid::new(format!("gpu_{}_grid", i))
                    .num_columns(2)
                    .spacing([8.0, 2.0])
                    .show(ui, |ui| {
                        ui.label("Utilization:");
                        ui.label(format!("{:.1}%", gpu.utilization));
                        ui.end_row();

                        ui.label("Memory:");
                        ui.label(format!("{:.1}%", gpu.memory_utilization));
                        ui.end_row();

                        if let Some(temp) = gpu.temperature {
                            ui.label("Temperature:");
                            let temp_color = if temp > 80.0 {
                                Color32::RED
                            } else if temp > 70.0 {
                                Color32::YELLOW
                            } else {
                                Color32::GREEN
                            };
                            ui.colored_label(temp_color, format!("{:.0}°C", temp));
                            ui.end_row();
                        }

                        if let Some(power) = gpu.power_usage {
                            ui.label("Power:");
                            ui.label(format!("{:.0}W", power));
                            ui.end_row();
                        }
                    });

                ui.add_space(4.0);

                // Full-width graph
                let graph_width = box_width - 2.0 * BOX_INNER_MARGIN;
                if let Some(gpu_hist) = system_monitor.gpu_history(i) {
                    let (_, rect) = ui.allocate_space(Vec2::new(graph_width, GRAPH_HEIGHT));
                    draw_graph(
                        ui.painter(),
                        rect,
                        gpu_hist,
                        Color32::from_rgb(255, 150, 100),
                    );
                }
            }
        } else {
            ui.label("Waiting for data...");
        }
    }

    fn render_network_content(
        &self,
        ui: &mut Ui,
        network_interfaces: &[strom_types::NetworkInterfaceInfo],
    ) {
        if network_interfaces.is_empty() {
            ui.label("No network interfaces available.");
        } else {
            egui::Grid::new("network_grid")
                .num_columns(4)
                .spacing([16.0, 2.0])
                .striped(true)
                .show(ui, |ui| {
                    // Header
                    ui.strong("Interface");
                    ui.strong("IPv4");
                    ui.strong("Netmask");
                    ui.strong("Status");
                    ui.end_row();

                    for iface in network_interfaces {
                        // Skip loopback interfaces
                        if iface.is_loopback {
                            continue;
                        }

                        // Get the first IPv4 address if available
                        let (ip, netmask) = if let Some(addr) = iface.ipv4_addresses.first() {
                            (
                                addr.address.as_str(),
                                addr.netmask.as_deref().unwrap_or("-"),
                            )
                        } else {
                            ("-", "-")
                        };

                        ui.label(egui::RichText::new(&iface.name).monospace());
                        ui.label(egui::RichText::new(ip).monospace());
                        ui.label(egui::RichText::new(netmask).monospace());

                        if iface.is_up {
                            ui.colored_label(Color32::GREEN, "up");
                        } else {
                            ui.colored_label(Color32::GRAY, "down");
                        }
                        ui.end_row();
                    }
                });
        }
    }
}

impl Default for InfoPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a box with header and content.
fn render_box(ui: &mut Ui, title: &str, width: f32, content: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(Color32::from_gray(30))
        .corner_radius(8.0)
        .stroke(Stroke::new(1.0, Color32::from_gray(60)))
        .inner_margin(BOX_INNER_MARGIN)
        .show(ui, |ui| {
            ui.set_width(width);

            ui.vertical(|ui| {
                ui.strong(title);
                ui.add_space(8.0);
                content(ui);
            });
        });
}

/// Draw a graph with background and grid lines.
fn draw_graph(painter: &egui::Painter, rect: Rect, data: &VecDeque<f32>, color: Color32) {
    // Draw background
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));

    // Draw grid lines
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5, Color32::from_gray(40)),
        );
    }

    if data.is_empty() {
        return;
    }

    // Draw data line
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (HISTORY_SIZE - 1) as f32) * rect.width();
            let y = rect.max.y - (value / 100.0) * rect.height();
            Pos2::new(x, y.max(rect.min.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(50)),
        egui::StrokeKind::Outside,
    );
}
