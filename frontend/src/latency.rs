//! Audio latency measurement visualization widget.

use egui::{Color32, Rect, RichText, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for latency data before it's considered stale.
/// If no updates are received within this duration, the latency data is invalidated.
/// Set to 2000ms since latency measurements arrive every ~1 second.
const LATENCY_DATA_TTL: Duration = Duration::from_millis(2000);

/// Latency data for a specific element (block).
#[derive(Debug, Clone)]
pub struct LatencyData {
    /// Last measured latency in microseconds
    pub last_latency_us: i64,
    /// Running average latency in microseconds (last 5 measurements)
    pub average_latency_us: i64,
}

impl LatencyData {
    /// Get last latency in milliseconds
    pub fn last_latency_ms(&self) -> f64 {
        self.last_latency_us as f64 / 1000.0
    }

    /// Get average latency in milliseconds
    pub fn average_latency_ms(&self) -> f64 {
        self.average_latency_us as f64 / 1000.0
    }
}

/// Latency data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedLatencyData {
    data: LatencyData,
    updated_at: Instant,
}

/// Key for identifying latency data (flow + element).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LatencyKey {
    pub flow_id: FlowId,
    pub element_id: String,
}

/// Storage for all latency data in the application.
#[derive(Debug, Clone, Default)]
pub struct LatencyDataStore {
    data: HashMap<LatencyKey, TimestampedLatencyData>,
}

impl LatencyDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update latency data for a specific element.
    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: LatencyData) {
        let key = LatencyKey {
            flow_id,
            element_id,
        };
        self.data.insert(
            key,
            TimestampedLatencyData {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Get latency data for a specific element.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&LatencyData> {
        let key = LatencyKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < LATENCY_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }

    /// Clear all latency data.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Remove latency data for a specific flow.
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        self.data.retain(|k, _| k.flow_id != *flow_id);
    }

    /// Remove stale latency data entries (older than TTL).
    #[allow(dead_code)]
    pub fn expire_stale(&mut self) {
        self.data
            .retain(|_, v| v.updated_at.elapsed() < LATENCY_DATA_TTL);
    }
}

/// Get color based on latency value (lower is better).
/// - Green: < 10ms (excellent)
/// - Yellow: 10-50ms (good)
/// - Orange: 50-100ms (acceptable)
/// - Red: > 100ms (poor)
fn latency_to_color(latency_ms: f64) -> Color32 {
    if latency_ms < 10.0 {
        Color32::from_rgb(0, 220, 0) // Green
    } else if latency_ms < 50.0 {
        Color32::from_rgb(255, 220, 0) // Yellow
    } else if latency_ms < 100.0 {
        Color32::from_rgb(255, 165, 0) // Orange
    } else {
        Color32::from_rgb(255, 0, 0) // Red
    }
}

/// Get dim color based on latency value.
fn latency_to_color_dim(latency_ms: f64) -> Color32 {
    if latency_ms < 10.0 {
        Color32::from_rgb(0, 60, 0)
    } else if latency_ms < 50.0 {
        Color32::from_rgb(60, 60, 0)
    } else if latency_ms < 100.0 {
        Color32::from_rgb(60, 45, 0)
    } else {
        Color32::from_rgb(60, 0, 0)
    }
}

/// Calculate the required height for a compact latency display.
pub fn calculate_compact_height() -> f32 {
    40.0 // Height for latency display
}

/// Render a compact latency widget (for graph nodes).
pub fn show_compact(ui: &mut Ui, latency_data: &LatencyData) {
    let last_ms = latency_data.last_latency_ms();
    let avg_ms = latency_data.average_latency_ms();

    tracing::trace!(
        "Latency show_compact: last={:.2}ms, avg={:.2}ms",
        last_ms,
        avg_ms
    );

    let total_height = calculate_compact_height();
    let bar_height = 12.0;

    let (rect, _response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(80.0), total_height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();

    // Background
    painter.rect(
        rect,
        2.0,
        Color32::from_gray(30),
        Stroke::new(1.0, Color32::from_gray(60)),
        egui::epaint::StrokeKind::Inside,
    );

    // Draw latency bar (normalized to 200ms max for visualization)
    let max_latency_ms = 200.0;
    let bar_level = (avg_ms / max_latency_ms).min(1.0) as f32;

    let bar_rect = Rect::from_min_size(
        egui::pos2(rect.min.x + 4.0, rect.min.y + 4.0),
        Vec2::new(rect.width() - 8.0, bar_height),
    );

    // Draw background bar
    painter.rect(
        bar_rect,
        2.0,
        latency_to_color_dim(avg_ms),
        Stroke::NONE,
        egui::epaint::StrokeKind::Inside,
    );

    // Draw filled bar
    if bar_level > 0.0 {
        let filled_rect = Rect::from_min_max(
            bar_rect.min,
            egui::pos2(
                bar_rect.min.x + bar_rect.width() * bar_level,
                bar_rect.max.y,
            ),
        );
        painter.rect(
            filled_rect,
            2.0,
            latency_to_color(avg_ms),
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }

    // Draw last measurement marker
    let last_level = (last_ms / max_latency_ms).min(1.0) as f32;
    if last_level > 0.0 {
        let marker_x = bar_rect.min.x + bar_rect.width() * last_level;
        painter.line_segment(
            [
                egui::pos2(marker_x, bar_rect.min.y),
                egui::pos2(marker_x, bar_rect.max.y),
            ],
            Stroke::new(2.0, Color32::WHITE),
        );
    }

    // Draw text labels
    let text_y = rect.min.y + bar_height + 8.0;
    let text_color = latency_to_color(avg_ms);

    // Average latency (main display)
    painter.text(
        egui::pos2(rect.min.x + 4.0, text_y),
        egui::Align2::LEFT_TOP,
        format!("Avg: {:.1}ms", avg_ms),
        egui::FontId::proportional(11.0),
        text_color,
    );

    // Last latency
    painter.text(
        egui::pos2(rect.max.x - 4.0, text_y),
        egui::Align2::RIGHT_TOP,
        format!("Last: {:.1}ms", last_ms),
        egui::FontId::proportional(10.0),
        Color32::from_gray(180),
    );
}

/// Render a full latency widget (for property inspector).
pub fn show_full(ui: &mut Ui, element_id: &str, latency_data: &LatencyData) {
    let last_ms = latency_data.last_latency_ms();
    let avg_ms = latency_data.average_latency_ms();

    tracing::trace!(
        "Latency show_full for element {}: last={:.2}ms, avg={:.2}ms",
        element_id,
        last_ms,
        avg_ms
    );

    ui.heading("Audio Latency");
    ui.separator();

    // Main latency display
    ui.horizontal(|ui| {
        ui.label("Average Latency:");
        let color = latency_to_color(avg_ms);
        ui.label(
            RichText::new(format!("{:.2} ms", avg_ms))
                .color(color)
                .strong(),
        );
    });

    ui.horizontal(|ui| {
        ui.label("Last Measurement:");
        let color = latency_to_color(last_ms);
        ui.label(RichText::new(format!("{:.2} ms", last_ms)).color(color));
    });

    ui.add_space(10.0);

    // Visual bar representation
    let bar_height = 20.0;
    let max_latency_ms = 200.0;

    let (rect, _response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(100.0), bar_height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();

    // Draw zone backgrounds
    let zones = [
        (0.0, 10.0 / max_latency_ms, Color32::from_rgb(0, 60, 0)), // Excellent
        (
            10.0 / max_latency_ms,
            50.0 / max_latency_ms,
            Color32::from_rgb(60, 60, 0),
        ), // Good
        (
            50.0 / max_latency_ms,
            100.0 / max_latency_ms,
            Color32::from_rgb(60, 45, 0),
        ), // Acceptable
        (100.0 / max_latency_ms, 1.0, Color32::from_rgb(60, 0, 0)), // Poor
    ];

    for (start, end, color) in zones {
        let x_start = rect.min.x + rect.width() * start as f32;
        let x_end = rect.min.x + rect.width() * end as f32;
        let zone_rect = Rect::from_min_max(
            egui::pos2(x_start, rect.min.y),
            egui::pos2(x_end, rect.max.y),
        );
        painter.rect(
            zone_rect,
            2.0,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }

    // Draw average latency bar
    let avg_level = (avg_ms / max_latency_ms).min(1.0);
    if avg_level > 0.0 {
        let filled_rect = Rect::from_min_max(
            rect.min,
            egui::pos2(rect.min.x + rect.width() * avg_level as f32, rect.max.y),
        );
        painter.rect(
            filled_rect,
            2.0,
            latency_to_color(avg_ms),
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }

    // Draw last measurement marker
    let last_level = (last_ms / max_latency_ms).min(1.0);
    if last_level > 0.0 {
        let marker_x = rect.min.x + rect.width() * last_level as f32;
        painter.line_segment(
            [
                egui::pos2(marker_x, rect.min.y),
                egui::pos2(marker_x, rect.max.y),
            ],
            Stroke::new(2.0, Color32::WHITE),
        );
    }

    // Border
    painter.rect(
        rect,
        2.0,
        Color32::TRANSPARENT,
        Stroke::new(1.0, Color32::from_gray(100)),
        egui::epaint::StrokeKind::Inside,
    );

    ui.add_space(5.0);

    // Scale labels
    ui.horizontal(|ui| {
        ui.label("0ms");
        ui.add_space(ui.available_width() / 4.0 - 20.0);
        ui.label("50ms");
        ui.add_space(ui.available_width() / 2.0 - 20.0);
        ui.label("100ms");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label("200ms");
        });
    });

    ui.add_space(5.0);

    // Quality indicator
    let quality = if avg_ms < 10.0 {
        ("Excellent", Color32::from_rgb(0, 220, 0))
    } else if avg_ms < 50.0 {
        ("Good", Color32::from_rgb(255, 220, 0))
    } else if avg_ms < 100.0 {
        ("Acceptable", Color32::from_rgb(255, 165, 0))
    } else {
        ("Poor", Color32::from_rgb(255, 0, 0))
    };

    ui.horizontal(|ui| {
        ui.label("Quality:");
        ui.label(RichText::new(quality.0).color(quality.1).strong());
    });

    ui.add_space(5.0);
    ui.label(format!("Element: {}", element_id));
}
