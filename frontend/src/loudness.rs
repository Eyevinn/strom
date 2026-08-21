//! EBU R128 loudness meter visualization widget.

use crate::meter::BlockDataKey;
use egui::{Color32, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for loudness data before it's considered stale.
const LOUDNESS_DATA_TTL: Duration = Duration::from_millis(1000);

/// EBU R128 target loudness (broadcast standard).
const TARGET_LUFS: f64 = -23.0;

/// LUFS scale range for visualization.
const MIN_LUFS: f64 = -41.0;
const MAX_LUFS: f64 = -14.0;

/// True peak warning threshold in dBTP.
const TRUE_PEAK_WARN: f64 = -1.0;

/// Loudness data for a specific element.
#[derive(Debug, Clone)]
pub struct LoudnessData {
    /// Momentary loudness in LUFS (400ms window)
    pub momentary: f64,
    /// Short-term loudness in LUFS (3s window)
    pub shortterm: Option<f64>,
    /// Integrated (global) loudness in LUFS (from start)
    pub integrated: Option<f64>,
    /// Loudness range in LU
    pub loudness_range: Option<f64>,
    /// True peak per channel in dBTP
    pub true_peak: Vec<f64>,
}

/// Loudness data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedLoudnessData {
    data: LoudnessData,
    updated_at: Instant,
}

/// Storage for all loudness data in the application.
#[derive(Debug, Clone, Default)]
pub struct LoudnessDataStore {
    data: HashMap<BlockDataKey, TimestampedLoudnessData>,
}

impl LoudnessDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update loudness data for a specific element.
    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: LoudnessData) {
        let key = BlockDataKey {
            flow_id,
            element_id,
        };
        self.data.insert(
            key,
            TimestampedLoudnessData {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Get loudness data for a specific element.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&LoudnessData> {
        let key = BlockDataKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < LOUDNESS_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }
}

/// Convert LUFS value to a 0.0-1.0 range for visualization.
fn lufs_to_level(lufs: f64) -> f32 {
    ((lufs - MIN_LUFS) / (MAX_LUFS - MIN_LUFS)).clamp(0.0, 1.0) as f32
}

/// Get color for a LUFS value based on deviation from -23 LUFS target.
/// Green: within +/-2 LU (-25 to -21 LUFS)
/// Yellow: within +/-5 LU (-28 to -18 LUFS)
/// Red: outside +/-5 LU
fn lufs_to_color(lufs: f64) -> Color32 {
    let deviation = (lufs - TARGET_LUFS).abs();
    if deviation <= 2.0 {
        Color32::from_rgb(0, 220, 0) // Green: on target
    } else if deviation <= 5.0 {
        Color32::from_rgb(255, 220, 0) // Yellow: acceptable
    } else {
        Color32::from_rgb(255, 0, 0) // Red: out of range
    }
}

/// Get dim color for background based on LUFS zone.
fn lufs_zone_dim_color(lufs: f64) -> Color32 {
    let deviation = (lufs - TARGET_LUFS).abs();
    if deviation <= 2.0 {
        Color32::from_rgb(0, 60, 0)
    } else if deviation <= 5.0 {
        Color32::from_rgb(60, 60, 0)
    } else {
        Color32::from_rgb(60, 0, 0)
    }
}

/// Draw horizontal background zones for LUFS scale.
fn draw_lufs_background(painter: &egui::Painter, rect: Rect, radius: f32) {
    // Define zone boundaries in LUFS
    let zones: [(f64, f64); 5] = [
        (MIN_LUFS, -28.0), // Red (too quiet)
        (-28.0, -25.0),    // Yellow (quiet side)
        (-25.0, -21.0),    // Green (on target)
        (-21.0, -18.0),    // Yellow (loud side)
        (-18.0, MAX_LUFS), // Red (too loud)
    ];

    for (start_lufs, end_lufs) in zones {
        let mid_lufs = (start_lufs + end_lufs) / 2.0;
        let color = lufs_zone_dim_color(mid_lufs);
        let x_start = rect.min.x + rect.width() * lufs_to_level(start_lufs);
        let x_end = rect.min.x + rect.width() * lufs_to_level(end_lufs);
        let zone_rect = Rect::from_min_max(
            egui::pos2(x_start, rect.min.y),
            egui::pos2(x_end, rect.max.y),
        );
        painter.rect(
            zone_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Draw horizontal level bar for LUFS value with color based on target deviation.
fn draw_lufs_bar(painter: &egui::Painter, rect: Rect, lufs: f64, radius: f32) {
    let level = lufs_to_level(lufs);
    if level <= 0.0 {
        return;
    }

    // Define zone boundaries
    let zones: [(f64, f64); 5] = [
        (MIN_LUFS, -28.0),
        (-28.0, -25.0),
        (-25.0, -21.0),
        (-21.0, -18.0),
        (-18.0, MAX_LUFS),
    ];

    for (start_lufs, end_lufs) in zones {
        let zone_start = lufs_to_level(start_lufs);
        let zone_end = lufs_to_level(end_lufs);

        if level <= zone_start {
            break;
        }

        let fill_end = level.min(zone_end);
        let mid_lufs = (start_lufs + end_lufs) / 2.0;
        let color = lufs_to_color(mid_lufs);

        let x_start = rect.min.x + rect.width() * zone_start;
        let x_end = rect.min.x + rect.width() * fill_end;
        let bar_rect = Rect::from_min_max(
            egui::pos2(x_start, rect.min.y),
            egui::pos2(x_end, rect.max.y),
        );
        painter.rect(
            bar_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Draw vertical background zones for LUFS scale.
fn draw_lufs_background_vertical(painter: &egui::Painter, rect: Rect, radius: f32) {
    let zones: [(f64, f64); 5] = [
        (MIN_LUFS, -28.0),
        (-28.0, -25.0),
        (-25.0, -21.0),
        (-21.0, -18.0),
        (-18.0, MAX_LUFS),
    ];

    for (start_lufs, end_lufs) in zones {
        let mid_lufs = (start_lufs + end_lufs) / 2.0;
        let color = lufs_zone_dim_color(mid_lufs);
        let y_start = rect.max.y - rect.height() * lufs_to_level(end_lufs);
        let y_end = rect.max.y - rect.height() * lufs_to_level(start_lufs);
        let zone_rect = Rect::from_min_max(
            egui::pos2(rect.min.x, y_start),
            egui::pos2(rect.max.x, y_end),
        );
        painter.rect(
            zone_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Draw vertical level bar for LUFS value.
fn draw_lufs_bar_vertical(painter: &egui::Painter, rect: Rect, lufs: f64, radius: f32) {
    let level = lufs_to_level(lufs);
    if level <= 0.0 {
        return;
    }

    let zones: [(f64, f64); 5] = [
        (MIN_LUFS, -28.0),
        (-28.0, -25.0),
        (-25.0, -21.0),
        (-21.0, -18.0),
        (-18.0, MAX_LUFS),
    ];

    for (start_lufs, end_lufs) in zones {
        let zone_start = lufs_to_level(start_lufs);
        let zone_end = lufs_to_level(end_lufs);

        if level <= zone_start {
            break;
        }

        let fill_end = level.min(zone_end);
        let mid_lufs = (start_lufs + end_lufs) / 2.0;
        let color = lufs_to_color(mid_lufs);

        let y_start = rect.max.y - rect.height() * fill_end;
        let y_end = rect.max.y - rect.height() * zone_start;
        let bar_rect = Rect::from_min_max(
            egui::pos2(rect.min.x, y_start),
            egui::pos2(rect.max.x, y_end),
        );
        painter.rect(
            bar_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Calculate the required height for a compact loudness meter.
pub fn calculate_compact_height() -> f32 {
    // Two bars (M, S) + integrated label + true peak warning
    let bar_height = 8.0;
    let spacing = 2.0;
    let label_height = 14.0;
    2.0 * (bar_height + spacing) + label_height
}

/// Render a compact loudness widget (for graph nodes).
///
/// Shows two horizontal bars (M=momentary, S=short-term) with LUFS coloring,
/// integrated loudness as a text value, and a true peak warning indicator.
pub fn show_compact(ui: &mut Ui, data: &LoudnessData) {
    let available_height = ui.available_height();
    let bar_count = 2.0; // M and S bars
    let label_space = 14.0;
    let bar_area = available_height - label_space;
    let per_bar = bar_area / bar_count;
    let spacing = per_bar * 0.2;
    let bar_height = (per_bar - spacing).max(1.0);
    let total_height = bar_count * per_bar + label_space;

    let available_width = ui.available_width().max(60.0);
    let (rect, _response) = ui.allocate_exact_size(
        Vec2::new(available_width, total_height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();
    let radius = (bar_height * 0.15).clamp(0.5, 1.0);

    let bars: [(&str, Option<f64>); 2] = [("M", Some(data.momentary)), ("S", data.shortterm)];

    for (i, (_label, lufs)) in bars.iter().enumerate() {
        let y = rect.min.y + i as f32 * per_bar;
        let bar_rect = Rect::from_min_size(
            egui::pos2(rect.min.x, y),
            Vec2::new(rect.width(), bar_height),
        );

        // Background zones
        draw_lufs_background(painter, bar_rect, radius);

        // Level bar
        if let Some(lufs) = lufs {
            draw_lufs_bar(painter, bar_rect, *lufs, radius);
        }

        // Border
        if bar_height >= 3.0 {
            painter.rect(
                bar_rect,
                radius,
                Color32::TRANSPARENT,
                Stroke::new(1.0_f32, Color32::from_gray(80)),
                egui::epaint::StrokeKind::Inside,
            );
        }
    }

    // Integrated loudness text and true peak warning
    let text_y = rect.min.y + bar_count * per_bar + 2.0;
    let integrated_text = match data.integrated {
        Some(v) => format!("I: {:.1} LUFS", v),
        None => "I: ---".to_string(),
    };

    let text_color = match data.integrated {
        Some(v) => lufs_to_color(v),
        None => Color32::GRAY,
    };

    painter.text(
        egui::pos2(rect.min.x + 2.0, text_y),
        egui::Align2::LEFT_TOP,
        &integrated_text,
        egui::FontId::proportional(10.0),
        text_color,
    );

    // True peak warning
    let tp_warn = data.true_peak.iter().any(|tp| *tp > TRUE_PEAK_WARN);
    if tp_warn {
        painter.text(
            egui::pos2(rect.max.x - 2.0, text_y),
            egui::Align2::RIGHT_TOP,
            "TP!",
            egui::FontId::proportional(10.0),
            Color32::from_rgb(255, 0, 0),
        );
    }
}

/// Render a full loudness widget (for property inspector).
pub fn show_full(ui: &mut Ui, data: &LoudnessData) {
    ui.heading("EBU R128 Loudness Meter");
    ui.separator();

    // Numeric readouts
    let format_opt_lufs = |v: Option<f64>| -> String {
        match v {
            Some(v) => format!("{:.1} LUFS", v),
            None => "---".to_string(),
        }
    };

    egui::Grid::new("loudness_values")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label("Momentary (M):");
            ui.colored_label(
                lufs_to_color(data.momentary),
                format!("{:.1} LUFS", data.momentary),
            );
            ui.end_row();

            ui.label("Short-term (S):");
            ui.colored_label(
                data.shortterm.map(lufs_to_color).unwrap_or(Color32::GRAY),
                format_opt_lufs(data.shortterm),
            );
            ui.end_row();

            ui.label("Integrated (I):");
            ui.colored_label(
                data.integrated.map(lufs_to_color).unwrap_or(Color32::GRAY),
                format_opt_lufs(data.integrated),
            );
            ui.end_row();

            ui.label("Loudness Range:");
            let lra_text = match data.loudness_range {
                Some(v) => format!("{:.1} LU", v),
                None => "---".to_string(),
            };
            ui.label(lra_text);
            ui.end_row();

            // True peak per channel
            for (i, tp) in data.true_peak.iter().enumerate() {
                ui.label(format!("True Peak Ch {}:", i + 1));
                let tp_color = if *tp > TRUE_PEAK_WARN {
                    Color32::from_rgb(255, 0, 0)
                } else {
                    Color32::from_rgb(0, 220, 0)
                };
                ui.colored_label(tp_color, format!("{:.1} dBTP", tp));
                ui.end_row();
            }
        });

    ui.add_space(10.0);

    // Vertical EBU-style gauge with M and S bars side by side
    let meter_width = 30.0;
    let meter_height = 120.0;
    let gap = 8.0;
    let total_width = 2.0 * meter_width + gap;

    ui.horizontal(|ui| {
        // M and S vertical bars
        let (rect, _response) =
            ui.allocate_exact_size(Vec2::new(total_width, meter_height), egui::Sense::hover());

        let painter = ui.painter();

        // M bar
        let m_rect = Rect::from_min_size(rect.min, Vec2::new(meter_width, meter_height));
        draw_lufs_background_vertical(painter, m_rect, 2.0);
        draw_lufs_bar_vertical(painter, m_rect, data.momentary, 2.0);
        painter.rect(
            m_rect,
            2.0,
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, Color32::from_gray(100)),
            egui::epaint::StrokeKind::Inside,
        );

        // S bar
        let s_rect = Rect::from_min_size(
            egui::pos2(rect.min.x + meter_width + gap, rect.min.y),
            Vec2::new(meter_width, meter_height),
        );
        draw_lufs_background_vertical(painter, s_rect, 2.0);
        if let Some(shortterm) = data.shortterm {
            draw_lufs_bar_vertical(painter, s_rect, shortterm, 2.0);
        }
        painter.rect(
            s_rect,
            2.0,
            Color32::TRANSPARENT,
            Stroke::new(1.0_f32, Color32::from_gray(100)),
            egui::epaint::StrokeKind::Inside,
        );

        // Integrated marker line across both bars
        if let Some(integrated) = data.integrated {
            let i_level = lufs_to_level(integrated);
            let i_y = rect.max.y - rect.height() * i_level;
            painter.line_segment(
                [
                    egui::pos2(rect.min.x - 2.0, i_y),
                    egui::pos2(rect.min.x + total_width + 2.0, i_y),
                ],
                Stroke::new(2.0_f32, Color32::WHITE),
            );
        }

        // Loudness range bracket
        if let (Some(integrated), Some(lra)) = (data.integrated, data.loudness_range) {
            if lra > 0.0 {
                let half_range = lra / 2.0;
                let lra_top = lufs_to_level(integrated + half_range);
                let lra_bottom = lufs_to_level(integrated - half_range);
                let bracket_x = rect.min.x + total_width + 4.0;
                let top_y = rect.max.y - rect.height() * lra_top;
                let bottom_y = rect.max.y - rect.height() * lra_bottom;
                let bracket_color = Color32::from_rgb(180, 180, 255);

                // Vertical line
                painter.line_segment(
                    [
                        egui::pos2(bracket_x, top_y),
                        egui::pos2(bracket_x, bottom_y),
                    ],
                    Stroke::new(1.5_f32, bracket_color),
                );
                // Top cap
                painter.line_segment(
                    [
                        egui::pos2(bracket_x - 3.0, top_y),
                        egui::pos2(bracket_x + 3.0, top_y),
                    ],
                    Stroke::new(1.5_f32, bracket_color),
                );
                // Bottom cap
                painter.line_segment(
                    [
                        egui::pos2(bracket_x - 3.0, bottom_y),
                        egui::pos2(bracket_x + 3.0, bottom_y),
                    ],
                    Stroke::new(1.5_f32, bracket_color),
                );
            }
        }

        // Labels below
        painter.text(
            egui::pos2(m_rect.center().x, rect.max.y + 4.0),
            egui::Align2::CENTER_TOP,
            "M",
            egui::FontId::proportional(11.0),
            Color32::GRAY,
        );
        painter.text(
            egui::pos2(s_rect.center().x, rect.max.y + 4.0),
            egui::Align2::CENTER_TOP,
            "S",
            egui::FontId::proportional(11.0),
            Color32::GRAY,
        );
    });

    ui.add_space(20.0);

    // True peak bars (small horizontal bars per channel)
    if !data.true_peak.is_empty() {
        ui.label("True Peak (dBTP):");
        for (i, tp) in data.true_peak.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("Ch {}:", i + 1));
                let tp_color = if *tp > TRUE_PEAK_WARN {
                    Color32::from_rgb(255, 0, 0)
                } else {
                    Color32::from_rgb(0, 220, 0)
                };
                // Map true peak: -20 dBTP to 0 dBTP range
                let tp_level = ((*tp + 20.0) / 20.0).clamp(0.0, 1.0) as f32;
                let bar_width = 100.0;
                let bar_height = 10.0;
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(bar_width, bar_height), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect(
                    rect,
                    2.0,
                    Color32::from_gray(40),
                    Stroke::new(1.0_f32, Color32::from_gray(80)),
                    egui::epaint::StrokeKind::Inside,
                );
                if tp_level > 0.0 {
                    let fill_rect = Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.min.x + rect.width() * tp_level, rect.max.y),
                    );
                    painter.rect(
                        fill_rect,
                        2.0,
                        tp_color,
                        Stroke::NONE,
                        egui::epaint::StrokeKind::Inside,
                    );
                }
                ui.label(format!("{:.1}", tp));
            });
        }
    }
}
