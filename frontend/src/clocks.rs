//! Clocks page for PTP synchronization monitoring.
//!
//! This page displays PTP clock statistics grouped by domain, since PTP clocks
//! are shared resources - one clock instance per domain regardless of how many
//! flows use it.

use egui::{Color32, RichText, Ui};
use std::collections::HashMap;

use crate::list_navigator::{list_navigator, ListItem};
use crate::ptp_monitor::{PtpStatsData, PtpStatsStore};

/// Clocks page state.
pub struct ClocksPage {
    /// Selected domain for detailed view
    selected_domain: Option<u8>,
}

impl ClocksPage {
    pub fn new() -> Self {
        Self {
            selected_domain: None,
        }
    }

    /// Render the clocks page.
    pub fn render(&mut self, ui: &mut Ui, ptp_stats: &PtpStatsStore, flows: &[strom_types::Flow]) {
        // Collect domain information
        let domain_info = self.collect_domain_info(ptp_stats, flows);

        if domain_info.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(50.0);
                ui.heading("No PTP Clocks Configured");
                ui.add_space(10.0);
                ui.label("Configure a flow to use PTP clock type in flow properties.");
                ui.add_space(10.0);
                ui.label("PTP statistics will be available once configured,");
                ui.label("regardless of whether the flow is running.");
            });
            return;
        }

        // Split view: domain list on left, details on right
        egui::SidePanel::left("ptp_domain_list")
            .default_width(350.0)
            .resizable(true)
            .show_inside(ui, |ui| {
                self.render_domain_list(ui, &domain_info);
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            self.render_details_panel(ui, ptp_stats, flows, &domain_info);
        });
    }

    /// Collect information about each PTP domain.
    fn collect_domain_info(
        &self,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
    ) -> HashMap<u8, DomainInfo> {
        let mut domains: HashMap<u8, DomainInfo> = HashMap::new();

        // First, find all flows configured for PTP
        for flow in flows {
            if flow.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp {
                let domain = flow.properties.ptp_domain.unwrap_or(0);
                let info = domains.entry(domain).or_insert_with(|| DomainInfo {
                    domain,
                    flow_count: 0,
                    stats: None,
                });
                info.flow_count += 1;

                // Get stats from this flow if available
                if info.stats.is_none() {
                    if let Some(history) = ptp_stats.get_history(&flow.id) {
                        info.stats = history.latest().cloned();
                    }
                }
            }
        }

        domains
    }

    fn render_domain_list(&mut self, ui: &mut Ui, domain_info: &HashMap<u8, DomainInfo>) {
        ui.label(format!("{} PTP domain(s) configured", domain_info.len()));
        ui.separator();

        // Sort domains by number and prepare data
        let mut domains: Vec<_> = domain_info.values().collect();
        domains.sort_by_key(|d| d.domain);

        // Build item data with owned strings
        let items_data: Vec<_> = domains
            .iter()
            .map(|info| {
                let id = info.domain.to_string();
                let label = format!("Domain {}", info.domain);

                // Build secondary text with stats
                let secondary = if let Some(ref stats) = info.stats {
                    let mut parts = vec![format!("{} flow(s)", info.flow_count)];
                    if let Some(offset_ns) = stats.clock_offset_ns {
                        parts.push(format!("Offset: {:.1}us", offset_ns as f64 / 1000.0));
                    }
                    if let Some(r_squared) = stats.r_squared {
                        parts.push(format!("R²: {:.4}", r_squared));
                    }
                    parts.join(" | ")
                } else {
                    format!("{} flow(s)", info.flow_count)
                };

                // Determine status
                let (status_text, status_color) = if let Some(ref stats) = info.stats {
                    if stats.synced {
                        ("SYNCED", Color32::from_rgb(100, 255, 100))
                    } else {
                        ("NOT SYNCED", Color32::from_rgb(255, 100, 100))
                    }
                } else {
                    ("No stats", Color32::GRAY)
                };

                (id, label, secondary, status_text, status_color)
            })
            .collect();

        // Get selected domain as string
        let selected_id = self.selected_domain.map(|d| d.to_string());

        let result = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let items =
                    items_data
                        .iter()
                        .map(|(id, label, secondary, status_text, status_color)| {
                            ListItem::new(id, label)
                                .with_secondary(secondary.clone())
                                .with_status(status_text, *status_color)
                        });

                list_navigator(ui, "ptp_domains", items, selected_id.as_deref())
            });

        if let Some(new_id) = result.inner.selected {
            if let Ok(domain) = new_id.parse::<u8>() {
                self.selected_domain = Some(domain);
            }
        }
    }

    fn render_details_panel(
        &mut self,
        ui: &mut Ui,
        ptp_stats: &PtpStatsStore,
        flows: &[strom_types::Flow],
        domain_info: &HashMap<u8, DomainInfo>,
    ) {
        ui.heading("Domain Details");
        ui.separator();

        let Some(domain) = self.selected_domain else {
            ui.label("Select a domain to view detailed statistics");
            return;
        };

        let Some(info) = domain_info.get(&domain) else {
            ui.label("Domain not found");
            self.selected_domain = None;
            return;
        };

        ui.label(RichText::new(format!("PTP Domain {}", domain)).heading());
        ui.add_space(10.0);

        if let Some(ref stats) = info.stats {
            // Sync status
            let (status_color, status_text) = if stats.synced {
                (Color32::from_rgb(100, 255, 100), "Synchronized")
            } else {
                (Color32::from_rgb(255, 100, 100), "Not Synchronized")
            };
            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.colored_label(status_color, RichText::new(status_text).strong());
            });

            egui::Grid::new("ptp_details_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Grandmaster:");
                    if let Some(gm_id) = stats.grandmaster_id {
                        ui.label(format!("{:016X}", gm_id));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Master:");
                    if let Some(master_id) = stats.master_id {
                        ui.label(format!("{:016X}", master_id));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Clock Offset:");
                    if let Some(offset_ns) = stats.clock_offset_ns {
                        let offset_us = offset_ns as f64 / 1000.0;
                        ui.label(format!("{:.2} us ({} ns)", offset_us, offset_ns));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Mean Path Delay:");
                    if let Some(delay_ns) = stats.mean_path_delay_ns {
                        let delay_us = delay_ns as f64 / 1000.0;
                        ui.label(format!("{:.2} us ({} ns)", delay_us, delay_ns));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("R2 (Quality):");
                    if let Some(r_squared) = stats.r_squared {
                        let color = if r_squared >= 0.99 {
                            Color32::from_rgb(100, 255, 100)
                        } else if r_squared >= 0.95 {
                            Color32::from_rgb(255, 200, 100)
                        } else {
                            Color32::from_rgb(255, 100, 100)
                        };
                        ui.colored_label(color, format!("{:.6}", r_squared));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();

                    ui.label("Clock Rate:");
                    if let Some(rate) = stats.clock_rate {
                        ui.label(format!("{:.9}", rate));
                    } else {
                        ui.label("-");
                    }
                    ui.end_row();
                });

            // Graphs
            ui.add_space(10.0);
            ui.separator();
            ui.heading("Graphs");
            ui.add_space(10.0);

            // Find a flow with this domain to get history
            let history = flows
                .iter()
                .filter(|f| {
                    f.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp
                        && f.properties.ptp_domain.unwrap_or(0) == domain
                })
                .find_map(|f| ptp_stats.get_history(&f.id));

            if let Some(history) = history {
                let graph_height = 100.0;
                let graph_width = ui.available_width() - 20.0;

                // Clock offset graph
                ui.label("Clock Offset (us):");
                let offset_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph(
                    ui.painter(),
                    offset_rect.1,
                    history.clock_offset_history(),
                    Color32::from_rgb(100, 200, 255),
                    true,
                );
                ui.add_space(10.0);

                // R² graph
                ui.label("R2 (Clock Estimation Quality):");
                let r2_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph_fixed(
                    ui.painter(),
                    r2_rect.1,
                    history.r_squared_history(),
                    Color32::from_rgb(100, 255, 100),
                    0.9,
                    1.0,
                );
                ui.add_space(10.0);

                // Path delay graph
                ui.label("Mean Path Delay (us):");
                let delay_rect = ui.allocate_space(egui::Vec2::new(graph_width, graph_height));
                draw_large_graph(
                    ui.painter(),
                    delay_rect.1,
                    history.path_delay_history(),
                    Color32::from_rgb(255, 150, 100),
                    false,
                );
            } else {
                ui.label("No historical data available");
            }
        } else {
            ui.label("No statistics available for this domain yet.");
            ui.label("Statistics will appear once PTP synchronization begins.");
        }

        // Show flows using this domain
        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("Flows using this domain:").strong());
        ui.add_space(5.0);

        let domain_flows: Vec<_> = flows
            .iter()
            .filter(|f| {
                f.properties.clock_type == strom_types::flow::GStreamerClockType::Ptp
                    && f.properties.ptp_domain.unwrap_or(0) == domain
            })
            .collect();

        if domain_flows.is_empty() {
            ui.label("No flows configured for this domain");
        } else {
            for flow in domain_flows {
                ui.label(format!("  - {}", flow.name));
            }
        }
    }
}

impl Default for ClocksPage {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a PTP domain.
struct DomainInfo {
    domain: u8,
    flow_count: usize,
    stats: Option<PtpStatsData>,
}

/// Draw a larger graph with labels.
fn draw_large_graph(
    painter: &egui::Painter,
    rect: egui::Rect,
    data: &std::collections::VecDeque<f64>,
    color: Color32,
    signed: bool,
) {
    use egui::{Pos2, Stroke};

    // Draw background
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));

    if data.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No data",
            egui::FontId::default(),
            Color32::GRAY,
        );
        return;
    }

    // Calculate range
    let (min_val, max_val) = if signed {
        let max_abs = data.iter().map(|v| v.abs()).fold(0.0_f64, f64::max);
        let range = max_abs.max(1.0) * 1.1;
        (-range, range)
    } else {
        let max = data.iter().fold(0.0_f64, |a, &b| a.max(b));
        (0.0, max.max(1.0) * 1.1)
    };

    // Draw grid lines
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5, Color32::from_gray(40)),
        );
    }

    // Draw center line for signed values
    if signed {
        let y_center = rect.center().y;
        painter.line_segment(
            [
                Pos2::new(rect.min.x, y_center),
                Pos2::new(rect.max.x, y_center),
            ],
            Stroke::new(1.0, Color32::from_gray(80)),
        );
    }

    // Draw data line
    let range = max_val - min_val;
    let history_size = 60;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (history_size - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );

    // Draw current value
    if let Some(&last) = data.back() {
        let text = format!("{:.2}", last);
        painter.text(
            Pos2::new(rect.max.x - 5.0, rect.min.y + 15.0),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::default(),
            color,
        );
    }
}

/// Draw a graph with fixed range.
fn draw_large_graph_fixed(
    painter: &egui::Painter,
    rect: egui::Rect,
    data: &std::collections::VecDeque<f64>,
    color: Color32,
    min_val: f64,
    max_val: f64,
) {
    use egui::{Pos2, Stroke};

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
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No data",
            egui::FontId::default(),
            Color32::GRAY,
        );
        return;
    }

    // Draw data line
    let range = max_val - min_val;
    let history_size = 60;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (history_size - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );

    // Draw current value
    if let Some(&last) = data.back() {
        let text = format!("{:.4}", last);
        painter.text(
            Pos2::new(rect.max.x - 5.0, rect.min.y + 15.0),
            egui::Align2::RIGHT_CENTER,
            text,
            egui::FontId::default(),
            color,
        );
    }
}
