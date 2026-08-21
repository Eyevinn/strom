//! Audio spectrum analyzer visualization widget.

use crate::meter::BlockDataKey;
use egui::{Color32, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for spectrum data before it's considered stale.
const SPECTRUM_DATA_TTL: Duration = Duration::from_millis(1000);

/// Spectrum data for a specific element (block).
#[derive(Debug, Clone)]
pub struct SpectrumData {
    /// Magnitude values in dB per channel; each inner Vec is one channel's frequency bands.
    /// Single-element outer Vec when multi-channel is off (mono mix).
    pub magnitudes: Vec<Vec<f32>>,
}

/// Spectrum data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedSpectrumData {
    data: SpectrumData,
    updated_at: Instant,
}

/// Storage for all spectrum data in the application.
#[derive(Debug, Clone, Default)]
pub struct SpectrumDataStore {
    data: HashMap<BlockDataKey, TimestampedSpectrumData>,
}

impl SpectrumDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update spectrum data for a specific element.
    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: SpectrumData) {
        let key = BlockDataKey {
            flow_id,
            element_id,
        };
        self.data.insert(
            key,
            TimestampedSpectrumData {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Get spectrum data for a specific element.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&SpectrumData> {
        let key = BlockDataKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < SPECTRUM_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }
}

/// Convert dB magnitude to a 0.0-1.0 range for visualization.
fn db_to_level(db: f32, threshold: f32) -> f32 {
    ((db - threshold) / (0.0 - threshold)).clamp(0.0, 1.0)
}

/// Zone boundaries for spectrum bar coloring (same as meter).
const ZONE_GREEN_END: f32 = 0.7;
const ZONE_YELLOW_END: f32 = 0.85;
const ZONE_ORANGE_END: f32 = 0.9;

/// Get color for a spectrum bar level.
fn level_to_color(level: f32) -> Color32 {
    if level < ZONE_GREEN_END {
        Color32::from_rgb(0, 220, 0)
    } else if level < ZONE_YELLOW_END {
        Color32::from_rgb(255, 220, 0)
    } else if level < ZONE_ORANGE_END {
        Color32::from_rgb(255, 165, 0)
    } else {
        Color32::from_rgb(255, 0, 0)
    }
}

/// Per-channel tint colors for distinguishing channels in multi-channel mode.
const CHANNEL_COLORS: &[Color32] = &[
    Color32::from_rgb(100, 200, 255), // Ch1 - blue tint
    Color32::from_rgb(255, 150, 100), // Ch2 - orange tint
    Color32::from_rgb(100, 255, 150), // Ch3 - green tint
    Color32::from_rgb(255, 100, 255), // Ch4 - magenta tint
    Color32::from_rgb(255, 255, 100), // Ch5 - yellow tint
    Color32::from_rgb(100, 255, 255), // Ch6 - cyan tint
];

/// Get dim background color for a spectrum bar level.
fn level_to_color_dim(level: f32) -> Color32 {
    if level < ZONE_GREEN_END {
        Color32::from_rgb(0, 40, 0)
    } else if level < ZONE_YELLOW_END {
        Color32::from_rgb(40, 40, 0)
    } else if level < ZONE_ORANGE_END {
        Color32::from_rgb(40, 30, 0)
    } else {
        Color32::from_rgb(40, 0, 0)
    }
}

/// Detect the threshold from magnitude data across all channels.
fn detect_threshold(magnitudes: &[Vec<f32>]) -> f32 {
    magnitudes
        .iter()
        .flat_map(|ch| ch.iter().copied())
        .fold(f32::INFINITY, f32::min)
        .min(-40.0)
}

/// Draw a single spectrum chart into the given rect.
fn draw_spectrum_bars(
    painter: &egui::Painter,
    rect: Rect,
    bands: &[f32],
    threshold: f32,
    color_fn: impl Fn(f32) -> Color32,
) {
    let band_count = bands.len();
    if band_count == 0 {
        return;
    }

    let bar_spacing = 1.0;
    let total_spacing = bar_spacing * (band_count as f32 - 1.0).max(0.0);
    let bar_width = ((rect.width() - total_spacing) / band_count as f32).max(1.0);

    for (i, &mag) in bands.iter().enumerate() {
        let x = rect.min.x + i as f32 * (bar_width + bar_spacing);
        let level = db_to_level(mag, threshold);

        let bar_rect = Rect::from_min_max(
            egui::pos2(x, rect.min.y),
            egui::pos2((x + bar_width).min(rect.max.x), rect.max.y),
        );

        // Dim background
        painter.rect(
            bar_rect,
            0.0,
            level_to_color_dim(level),
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );

        // Active bar (from bottom up)
        if level > 0.0 {
            let bar_top = rect.max.y - rect.height() * level;
            let active_rect = Rect::from_min_max(
                egui::pos2(x, bar_top),
                egui::pos2((x + bar_width).min(rect.max.x), rect.max.y),
            );
            painter.rect(
                active_rect,
                0.0,
                color_fn(level),
                Stroke::NONE,
                egui::epaint::StrokeKind::Inside,
            );
        }
    }
}

/// Render a compact spectrum widget (for graph nodes).
/// Shows one stacked row per channel when multi-channel is active.
pub fn show_compact(ui: &mut Ui, spectrum_data: &SpectrumData) {
    let channel_count = spectrum_data.magnitudes.len();
    if channel_count == 0 || spectrum_data.magnitudes[0].is_empty() {
        ui.label("No signal");
        return;
    }

    let available_width = ui.available_width().max(60.0);
    let available_height = ui.available_height().max(20.0);

    let (rect, _response) = ui.allocate_exact_size(
        Vec2::new(available_width, available_height),
        egui::Sense::hover(),
    );

    let painter = ui.painter();

    // Dark background
    painter.rect(
        rect,
        1.0,
        Color32::from_rgb(10, 10, 10),
        Stroke::NONE,
        egui::epaint::StrokeKind::Inside,
    );

    let threshold = detect_threshold(&spectrum_data.magnitudes);
    let channel_spacing = if channel_count > 1 { 1.0 } else { 0.0 };
    let total_channel_spacing = channel_spacing * (channel_count as f32 - 1.0).max(0.0);
    let channel_height = ((rect.height() - total_channel_spacing) / channel_count as f32).max(4.0);

    for (ch, bands) in spectrum_data.magnitudes.iter().enumerate() {
        let y = rect.min.y + ch as f32 * (channel_height + channel_spacing);
        let ch_rect = Rect::from_min_size(
            egui::pos2(rect.min.x, y),
            Vec2::new(rect.width(), channel_height.min(rect.max.y - y)),
        );

        if channel_count > 1 {
            let ch_color = CHANNEL_COLORS[ch % CHANNEL_COLORS.len()];
            draw_spectrum_bars(painter, ch_rect, bands, threshold, |_level| ch_color);
        } else {
            draw_spectrum_bars(painter, ch_rect, bands, threshold, level_to_color);
        }
    }

    // Border
    painter.rect(
        rect,
        1.0,
        Color32::TRANSPARENT,
        Stroke::new(1.0_f32, Color32::from_gray(60)),
        egui::epaint::StrokeKind::Inside,
    );
}

/// Render a full spectrum widget (for property inspector).
/// Shows separate charts per channel when multi-channel is active.
pub fn show_full(ui: &mut Ui, spectrum_data: &SpectrumData) {
    let channel_count = spectrum_data.magnitudes.len();

    ui.heading("Spectrum Analyzer");
    ui.separator();

    if channel_count == 0 || spectrum_data.magnitudes[0].is_empty() {
        ui.label("No signal detected");
        return;
    }

    let band_count = spectrum_data.magnitudes[0].len();
    if channel_count > 1 {
        ui.label(format!(
            "Channels: {} | Bands: {}",
            channel_count, band_count
        ));
    } else {
        ui.label(format!("Bands: {}", band_count));
    }
    ui.add_space(5.0);

    let threshold = detect_threshold(&spectrum_data.magnitudes);
    let chart_width = ui.available_width().max(200.0);

    for (ch, bands) in spectrum_data.magnitudes.iter().enumerate() {
        if channel_count > 1 {
            ui.label(format!("Channel {}", ch + 1));
        }

        let chart_height = if channel_count > 1 { 100.0 } else { 150.0 };

        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(chart_width, chart_height), egui::Sense::hover());

        let painter = ui.painter();

        // Dark background
        painter.rect(
            rect,
            2.0,
            Color32::from_rgb(15, 15, 15),
            Stroke::new(1.0_f32, Color32::from_gray(80)),
            egui::epaint::StrokeKind::Inside,
        );

        let bar_spacing = 1.0;
        let total_spacing = bar_spacing * (band_count as f32 - 1.0).max(0.0);
        let bar_width = ((rect.width() - 4.0 - total_spacing) / band_count as f32).max(1.0);
        let inner_left = rect.min.x + 2.0;
        let inner_bottom = rect.max.y - 2.0;
        let inner_height = inner_bottom - (rect.min.y + 2.0);

        // Draw horizontal guide lines at -20, -40, -60 dB
        for &db_line in &[-20.0_f32, -40.0, -60.0] {
            let level = db_to_level(db_line, threshold);
            if level > 0.0 && level < 1.0 {
                let y = inner_bottom - inner_height * level;
                painter.line_segment(
                    [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                    Stroke::new(0.5_f32, Color32::from_gray(40)),
                );
                painter.text(
                    egui::pos2(rect.max.x - 28.0, y - 6.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}", db_line as i32),
                    egui::FontId::proportional(8.0),
                    Color32::from_gray(80),
                );
            }
        }

        // Hover: show band index and magnitude
        let hover_band = if response.hovered() {
            response.hover_pos().map(|pos| {
                let rel_x = pos.x - inner_left;
                let band = (rel_x / (bar_width + bar_spacing)) as usize;
                band.min(band_count - 1)
            })
        } else {
            None
        };

        let ch_color = if channel_count > 1 {
            Some(CHANNEL_COLORS[ch % CHANNEL_COLORS.len()])
        } else {
            None
        };

        for (i, &mag) in bands.iter().enumerate() {
            let x = inner_left + i as f32 * (bar_width + bar_spacing);
            let level = db_to_level(mag, threshold);

            if level > 0.0 {
                let bar_top = inner_bottom - inner_height * level;
                let active_rect = Rect::from_min_max(
                    egui::pos2(x, bar_top),
                    egui::pos2((x + bar_width).min(rect.max.x - 2.0), inner_bottom),
                );

                let color = if hover_band == Some(i) {
                    Color32::WHITE
                } else if let Some(c) = ch_color {
                    c
                } else {
                    level_to_color(level)
                };

                painter.rect(
                    active_rect,
                    0.0,
                    color,
                    Stroke::NONE,
                    egui::epaint::StrokeKind::Inside,
                );
            }
        }

        // Show tooltip for hovered band
        if let Some(band) = hover_band {
            if band < band_count {
                let mag = bands[band];
                if channel_count > 1 {
                    response.on_hover_text(format!("Ch {} Band {}: {:.1} dB", ch + 1, band, mag));
                } else {
                    response.on_hover_text(format!("Band {}: {:.1} dB", band, mag));
                }
            }
        }

        ui.add_space(4.0);
    }

    // Approximate frequency labels below the charts
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let nyquist = 24000.0_f32;
        let labels = [0.0, 0.25, 0.5, 0.75, 1.0];
        let section_width = chart_width / (labels.len() - 1) as f32;
        for (i, &frac) in labels.iter().enumerate() {
            let freq = nyquist * frac;
            let label = if freq >= 1000.0 {
                format!("{:.0}k", freq / 1000.0)
            } else {
                format!("{:.0}", freq)
            };
            if i > 0 {
                let remaining = section_width - ui.spacing().item_spacing.x;
                ui.add_space(remaining.max(0.0));
            }
            ui.small(label);
        }
    });
}
