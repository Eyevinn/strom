//! Audio meter visualization widget.

use egui::{Color32, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for meter data before it's considered stale.
/// If no updates are received within this duration, the meter data is invalidated.
/// Set to 1000ms to allow for brief network hiccups while meter updates typically arrive every ~100ms.
const METER_DATA_TTL: Duration = Duration::from_millis(1000);

/// Meter data for a specific element (block or element).
#[derive(Debug, Clone)]
pub struct MeterData {
    /// RMS values in dB for each channel
    pub rms: Vec<f64>,
    /// Peak values in dB for each channel
    pub peak: Vec<f64>,
    /// Decay values in dB for each channel
    pub decay: Vec<f64>,
}

/// Meter data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedMeterData {
    data: MeterData,
    updated_at: Instant,
}

/// Key for identifying meter data (flow + element).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct MeterKey {
    pub flow_id: FlowId,
    pub element_id: String,
}

/// Storage for all meter data in the application.
#[derive(Debug, Clone, Default)]
pub struct MeterDataStore {
    data: HashMap<MeterKey, TimestampedMeterData>,
}

impl MeterDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update meter data for a specific element.
    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: MeterData) {
        let key = MeterKey {
            flow_id,
            element_id,
        };
        self.data.insert(
            key,
            TimestampedMeterData {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Get meter data for a specific element.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&MeterData> {
        let key = MeterKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < METER_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }

    /// Clear all meter data.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Remove meter data for a specific flow.
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        self.data.retain(|k, _| k.flow_id != *flow_id);
    }

    /// Remove stale meter data entries (older than TTL).
    /// Can be called periodically to clean up memory.
    #[allow(dead_code)]
    pub fn expire_stale(&mut self) {
        self.data
            .retain(|_, v| v.updated_at.elapsed() < METER_DATA_TTL);
    }
}

/// Convert dB value to a 0.0-1.0 range for visualization.
/// -60 dB (very quiet) maps to 0.0
/// 0 dB (digital full scale) maps to 1.0
fn db_to_level(db: f64) -> f32 {
    let min_db = -60.0;
    let max_db = 0.0;
    ((db - min_db) / (max_db - min_db)).clamp(0.0, 1.0) as f32
}

/// Zone boundaries for professional audio metering.
const ZONE_GREEN_END: f32 = 0.7; // -18 dB
const ZONE_YELLOW_END: f32 = 0.85; // -9 dB
const ZONE_ORANGE_END: f32 = 0.9; // -6 dB

/// Get bright color for a meter level based on professional audio metering standards.
///
/// Industry standard color zones (based on EBU R128 and broadcast standards):
/// - Green: -60 dB to -18 dB (safe operational level)
/// - Yellow: -18 dB to -9 dB (loud but acceptable)
/// - Orange: -9 dB to -6 dB (getting hot)
/// - Red: -6 dB to 0 dB (danger zone, risk of clipping)
///
/// With our mapping: -60 dB = 0.0, 0 dB = 1.0
/// - Green: 0.0 to 0.7 (-60 to -18 dB)
/// - Yellow: 0.7 to 0.85 (-18 to -9 dB)
/// - Orange: 0.85 to 0.9 (-9 to -6 dB)
/// - Red: 0.9 to 1.0 (-6 to 0 dB)
fn level_to_color_bright(level: f32) -> Color32 {
    if level < ZONE_GREEN_END {
        // Green zone: -60 dB to -18 dB (safe level)
        Color32::from_rgb(0, 220, 0)
    } else if level < ZONE_YELLOW_END {
        // Yellow zone: -18 dB to -9 dB (loud but acceptable)
        Color32::from_rgb(255, 220, 0)
    } else if level < ZONE_ORANGE_END {
        // Orange zone: -9 dB to -6 dB (getting hot)
        Color32::from_rgb(255, 165, 0)
    } else {
        // Red zone: -6 dB to 0 dB (danger, clipping risk)
        Color32::from_rgb(255, 0, 0)
    }
}

/// Get dim color for background zones.
fn level_to_color_dim(level: f32) -> Color32 {
    if level < ZONE_GREEN_END {
        Color32::from_rgb(0, 60, 0)
    } else if level < ZONE_YELLOW_END {
        Color32::from_rgb(60, 60, 0)
    } else if level < ZONE_ORANGE_END {
        Color32::from_rgb(60, 45, 0)
    } else {
        Color32::from_rgb(60, 0, 0)
    }
}

/// Get light color for RMS bars (lighter than bright, darker than dim).
fn level_to_color_light(level: f32) -> Color32 {
    if level < ZONE_GREEN_END {
        // Very light green
        Color32::from_rgb(180, 255, 180)
    } else if level < ZONE_YELLOW_END {
        // Very light yellow
        Color32::from_rgb(255, 255, 200)
    } else if level < ZONE_ORANGE_END {
        // Very light orange
        Color32::from_rgb(255, 230, 180)
    } else {
        // Very light red/pink
        Color32::from_rgb(255, 200, 200)
    }
}

/// Draw horizontal background zones (dim colors).
fn draw_horizontal_zones_background(painter: &egui::Painter, rect: Rect, radius: f32) {
    let zones = [
        (0.0, ZONE_GREEN_END, level_to_color_dim(0.0)),
        (
            ZONE_GREEN_END,
            ZONE_YELLOW_END,
            level_to_color_dim(ZONE_GREEN_END),
        ),
        (
            ZONE_YELLOW_END,
            ZONE_ORANGE_END,
            level_to_color_dim(ZONE_YELLOW_END),
        ),
        (ZONE_ORANGE_END, 1.0, level_to_color_dim(ZONE_ORANGE_END)),
    ];

    for (start, end, color) in zones {
        let x_start = rect.min.x + rect.width() * start;
        let x_end = rect.min.x + rect.width() * end;
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

/// Draw horizontal level bar (bright colors, multicolored).
fn draw_horizontal_level(painter: &egui::Painter, rect: Rect, level: f32, radius: f32) {
    if level <= 0.0 {
        return;
    }

    let zones = [
        (0.0, ZONE_GREEN_END, level_to_color_bright(0.0)),
        (
            ZONE_GREEN_END,
            ZONE_YELLOW_END,
            level_to_color_bright(ZONE_GREEN_END),
        ),
        (
            ZONE_YELLOW_END,
            ZONE_ORANGE_END,
            level_to_color_bright(ZONE_YELLOW_END),
        ),
        (ZONE_ORANGE_END, 1.0, level_to_color_bright(ZONE_ORANGE_END)),
    ];

    for (start, end, color) in zones {
        if level <= start {
            break;
        }
        let zone_level = level.min(end);
        let x_start = rect.min.x + rect.width() * start;
        let x_end = rect.min.x + rect.width() * zone_level;
        let level_rect = Rect::from_min_max(
            egui::pos2(x_start, rect.min.y),
            egui::pos2(x_end, rect.max.y),
        );
        painter.rect(
            level_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Draw vertical background zones (dim colors).
fn draw_vertical_zones_background(painter: &egui::Painter, rect: Rect, radius: f32) {
    let zones = [
        (0.0, ZONE_GREEN_END, level_to_color_dim(0.0)),
        (
            ZONE_GREEN_END,
            ZONE_YELLOW_END,
            level_to_color_dim(ZONE_GREEN_END),
        ),
        (
            ZONE_YELLOW_END,
            ZONE_ORANGE_END,
            level_to_color_dim(ZONE_YELLOW_END),
        ),
        (ZONE_ORANGE_END, 1.0, level_to_color_dim(ZONE_ORANGE_END)),
    ];

    for (start, end, color) in zones {
        let y_start = rect.max.y - rect.height() * end;
        let y_end = rect.max.y - rect.height() * start;
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

/// Draw vertical level bar (bright colors, multicolored).
fn draw_vertical_level(painter: &egui::Painter, rect: Rect, level: f32, radius: f32) {
    if level <= 0.0 {
        return;
    }

    let zones = [
        (0.0, ZONE_GREEN_END, level_to_color_bright(0.0)),
        (
            ZONE_GREEN_END,
            ZONE_YELLOW_END,
            level_to_color_bright(ZONE_GREEN_END),
        ),
        (
            ZONE_YELLOW_END,
            ZONE_ORANGE_END,
            level_to_color_bright(ZONE_YELLOW_END),
        ),
        (ZONE_ORANGE_END, 1.0, level_to_color_bright(ZONE_ORANGE_END)),
    ];

    for (start, end, color) in zones {
        if level <= start {
            break;
        }
        let zone_level = level.min(end);
        let y_start = rect.max.y - rect.height() * zone_level;
        let y_end = rect.max.y - rect.height() * start;
        let level_rect = Rect::from_min_max(
            egui::pos2(rect.min.x, y_start),
            egui::pos2(rect.max.x, y_end),
        );
        painter.rect(
            level_rect,
            radius,
            color,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Calculate the required height for a compact meter with the given number of channels.
pub fn calculate_compact_height(channel_count: usize) -> f32 {
    if channel_count == 0 {
        20.0 // Height for "No signal" message
    } else {
        let bar_height = 8.0;
        let spacing = 2.0;
        (bar_height + spacing) * channel_count as f32
    }
}

/// Render a compact meter widget (for graph nodes).
pub fn show_compact(ui: &mut Ui, meter_data: &MeterData) {
    let channel_count = meter_data.rms.len();
    tracing::trace!(
        "📊 show_compact called: channels={}, rms={:?}",
        channel_count,
        meter_data.rms
    );
    if channel_count == 0 {
        ui.label("No signal");
        return;
    }

    // Compact: show horizontal bars, one per channel
    let bar_height = 8.0;
    let spacing = 2.0;
    let total_height = calculate_compact_height(channel_count);

    let (rect, _response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(60.0), total_height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();

    for (i, (rms, peak, decay)) in meter_data
        .rms
        .iter()
        .zip(&meter_data.peak)
        .zip(&meter_data.decay)
        .map(|((r, p), d)| (r, p, d))
        .enumerate()
    {
        let y = rect.min.y + i as f32 * (bar_height + spacing);
        let bar_rect = Rect::from_min_size(
            egui::pos2(rect.min.x, y),
            Vec2::new(rect.width(), bar_height),
        );

        // Background zones (dim colors)
        draw_horizontal_zones_background(painter, bar_rect, 1.0);

        // Peak level (bright multicolored bar)
        let peak_level = db_to_level(*peak);
        draw_horizontal_level(painter, bar_rect, peak_level, 1.0);

        // RMS level (darker inner bar within the peak bar)
        let rms_level = db_to_level(*rms);
        if rms_level > 0.0 {
            let zones = [
                (0.0, ZONE_GREEN_END, level_to_color_dim(0.0)),
                (
                    ZONE_GREEN_END,
                    ZONE_YELLOW_END,
                    level_to_color_dim(ZONE_GREEN_END),
                ),
                (
                    ZONE_YELLOW_END,
                    ZONE_ORANGE_END,
                    level_to_color_dim(ZONE_YELLOW_END),
                ),
                (ZONE_ORANGE_END, 1.0, level_to_color_dim(ZONE_ORANGE_END)),
            ];

            for (start, end, color) in zones {
                if rms_level <= start {
                    break;
                }
                let zone_level = rms_level.min(end);
                let x_start = bar_rect.min.x + bar_rect.width() * start;
                let x_end = bar_rect.min.x + bar_rect.width() * zone_level;
                let rms_rect = Rect::from_min_max(
                    egui::pos2(x_start, bar_rect.min.y + 2.0),
                    egui::pos2(x_end, bar_rect.max.y - 2.0),
                );
                painter.rect(
                    rms_rect,
                    0.5,
                    color,
                    Stroke::NONE,
                    egui::epaint::StrokeKind::Inside,
                );
            }
        }

        // Decay indicator (thin white line)
        let decay_level = db_to_level(*decay);
        if decay_level > 0.0 {
            let decay_x = bar_rect.min.x + bar_rect.width() * decay_level;
            painter.line_segment(
                [
                    egui::pos2(decay_x, bar_rect.min.y),
                    egui::pos2(decay_x, bar_rect.max.y),
                ],
                Stroke::new(2.0, Color32::WHITE),
            );
        }

        // Border
        painter.rect(
            bar_rect,
            1.0,
            Color32::TRANSPARENT,
            Stroke::new(1.0, Color32::from_gray(80)),
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Render a full meter widget (for property inspector).
pub fn show_full(ui: &mut Ui, element_id: &str, meter_data: &MeterData) {
    let channel_count = meter_data.rms.len();
    tracing::trace!(
        "📊 show_full called for element {}: channels={}, rms={:?}",
        element_id,
        channel_count,
        meter_data.rms
    );

    ui.heading("Audio Level Meter");
    ui.separator();

    if channel_count == 0 {
        ui.label("No signal detected");
        return;
    }

    ui.label(format!("Channels: {}", channel_count));
    ui.add_space(5.0);

    // Show vertical meters horizontally side-by-side
    ui.horizontal_wrapped(|ui| {
        for (i, (rms, peak, decay)) in meter_data
            .rms
            .iter()
            .zip(&meter_data.peak)
            .zip(&meter_data.decay)
            .map(|((r, p), d)| (r, p, d))
            .enumerate()
        {
            ui.vertical(|ui| {
                // Channel number above
                ui.label(format!("Ch {}", i + 1));

                // Vertical meter
                let meter_width = 30.0;
                let meter_height = 100.0;

                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(meter_width, meter_height),
                    egui::Sense::hover(),
                );

                let painter = ui.painter();

                // Background zones (dim colors)
                draw_vertical_zones_background(painter, rect, 2.0);

                // Peak level (bright multicolored bar)
                let peak_level = db_to_level(*peak);
                draw_vertical_level(painter, rect, peak_level, 2.0);

                // RMS level (darker inner bar within the peak bar)
                let rms_level = db_to_level(*rms);
                if rms_level > 0.0 {
                    let zones = [
                        (0.0, ZONE_GREEN_END, level_to_color_dim(0.0)),
                        (
                            ZONE_GREEN_END,
                            ZONE_YELLOW_END,
                            level_to_color_dim(ZONE_GREEN_END),
                        ),
                        (
                            ZONE_YELLOW_END,
                            ZONE_ORANGE_END,
                            level_to_color_dim(ZONE_YELLOW_END),
                        ),
                        (ZONE_ORANGE_END, 1.0, level_to_color_dim(ZONE_ORANGE_END)),
                    ];

                    for (start, end, color) in zones {
                        if rms_level <= start {
                            break;
                        }
                        let zone_level = rms_level.min(end);
                        let y_start = rect.max.y - rect.height() * zone_level;
                        let y_end = rect.max.y - rect.height() * start;
                        let rms_rect = Rect::from_min_max(
                            egui::pos2(rect.min.x + 6.0, y_start),
                            egui::pos2(rect.max.x - 6.0, y_end),
                        );
                        painter.rect(
                            rms_rect,
                            1.0,
                            color,
                            Stroke::NONE,
                            egui::epaint::StrokeKind::Inside,
                        );
                    }
                }

                // Decay indicator (horizontal white line)
                let decay_level = db_to_level(*decay);
                if decay_level > 0.0 {
                    let decay_y = rect.max.y - rect.height() * decay_level;
                    painter.line_segment(
                        [
                            egui::pos2(rect.min.x, decay_y),
                            egui::pos2(rect.max.x, decay_y),
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

                // dB value below
                if response.hovered() {
                    ui.label(format!("RMS: {:.1}", rms));
                    ui.label(format!("Peak: {:.1}", peak));
                    ui.label(format!("Decay: {:.1}", decay));
                } else {
                    ui.label(format!("{:.1}", peak));
                }
            });
        }
    });

    ui.add_space(5.0);
    ui.label(format!("Element: {}", element_id));
}
