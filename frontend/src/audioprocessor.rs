//! Audio Processor meters: input/output loudness, AGC gain and gain reduction.

use crate::meter::BlockDataKey;
use egui::{Color32, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for meter data before it's considered stale.
const DATA_TTL: Duration = Duration::from_millis(1000);

/// LUFS scale of the loudness bars.
const MIN_LUFS: f64 = -41.0;
const MAX_LUFS: f64 = -5.0;

/// Gain reduction scale of the reduction bars (dB).
const MAX_REDUCTION_DB: f64 = 12.0;

/// Below this the element is reporting silence (it sends -200).
const SILENCE_FLOOR: f64 = -100.0;

/// Meter values from one Audio Processor block.
#[derive(Debug, Clone)]
pub struct AudioProcessorData {
    pub input_momentary: f64,
    pub input_short_term: f64,
    pub output_momentary: f64,
    pub output_short_term: f64,
    pub output_true_peak: f64,
    pub agc_gain: f64,
    pub band_gain_reduction: Vec<f64>,
    pub limiter_gain_reduction: f64,
}

#[derive(Debug, Clone)]
struct Timestamped {
    data: AudioProcessorData,
    updated_at: Instant,
}

/// Storage for the meter data of every Audio Processor block.
#[derive(Debug, Clone, Default)]
pub struct AudioProcessorDataStore {
    data: HashMap<BlockDataKey, Timestamped>,
}

impl AudioProcessorDataStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: AudioProcessorData) {
        self.data.insert(
            BlockDataKey {
                flow_id,
                element_id,
            },
            Timestamped {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Meter data for a block, or `None` when it is stale.
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&AudioProcessorData> {
        let key = BlockDataKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data
            .get(&key)
            .filter(|t| t.updated_at.elapsed() < DATA_TTL)
            .map(|t| &t.data)
    }
}

fn fmt_level(v: f64, unit: &str) -> String {
    if v <= SILENCE_FLOOR {
        "---".to_string()
    } else {
        format!("{v:.1} {unit}")
    }
}

fn lufs_fraction(lufs: f64) -> f32 {
    ((lufs - MIN_LUFS) / (MAX_LUFS - MIN_LUFS)).clamp(0.0, 1.0) as f32
}

fn reduction_fraction(db: f64) -> f32 {
    (db.abs() / MAX_REDUCTION_DB).clamp(0.0, 1.0) as f32
}

/// One horizontal bar filled to `fraction` of its width.
fn bar(ui: &mut Ui, size: Vec2, fraction: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 1.0, Color32::from_gray(35));
    let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height()));
    painter.rect_filled(fill, 1.0, color);
    painter.rect(
        rect,
        1.0,
        Color32::TRANSPARENT,
        Stroke::new(1.0_f32, Color32::from_gray(80)),
        egui::epaint::StrokeKind::Inside,
    );
}

const LOUDNESS_COLOR: Color32 = Color32::from_rgb(0, 200, 120);
const REDUCTION_COLOR: Color32 = Color32::from_rgb(255, 160, 0);

/// Height of the compact (graph node) view.
pub fn calculate_compact_height() -> f32 {
    2.0 * 10.0 + 14.0
}

/// Compact view for the graph node: input and output short-term loudness
/// bars, and the AGC gain and limiter reduction as text.
pub fn show_compact(ui: &mut Ui, data: &AudioProcessorData) {
    let width = ui.available_width().max(60.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    bar(
        ui,
        Vec2::new(width, 8.0),
        lufs_fraction(data.input_short_term),
        Color32::from_gray(150),
    );
    bar(
        ui,
        Vec2::new(width, 8.0),
        lufs_fraction(data.output_short_term),
        LOUDNESS_COLOR,
    );
    ui.label(
        egui::RichText::new(format!(
            "AGC {:+.1} dB  LIM {:.1} dB",
            data.agc_gain,
            -data.limiter_gain_reduction.abs()
        ))
        .size(10.0),
    );
}

/// Full view for the property inspector.
pub fn show_full(ui: &mut Ui, data: &AudioProcessorData) {
    ui.heading("Audio Processor");
    ui.separator();

    egui::Grid::new("audioprocessor_values")
        .num_columns(3)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            ui.label("");
            ui.label("Input");
            ui.label("Output");
            ui.end_row();

            ui.label("Momentary:");
            ui.label(fmt_level(data.input_momentary, "LUFS"));
            ui.label(fmt_level(data.output_momentary, "LUFS"));
            ui.end_row();

            ui.label("Short-term:");
            ui.label(fmt_level(data.input_short_term, "LUFS"));
            ui.label(fmt_level(data.output_short_term, "LUFS"));
            ui.end_row();

            ui.label("True peak:");
            ui.label("");
            ui.label(fmt_level(data.output_true_peak, "dBTP"));
            ui.end_row();
        });

    ui.add_space(8.0);
    let width = ui.available_width().clamp(80.0, 240.0);

    ui.label(format!("AGC gain: {:+.1} dB", data.agc_gain));

    if !data.band_gain_reduction.is_empty() {
        ui.add_space(4.0);
        ui.label("Multiband gain reduction:");
        for (i, gr) in data.band_gain_reduction.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("Band {}", i + 1));
                bar(
                    ui,
                    Vec2::new(width, 8.0),
                    reduction_fraction(*gr),
                    REDUCTION_COLOR,
                );
                ui.label(format!("{:.1} dB", -gr.abs()));
            });
        }
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Limiter");
        bar(
            ui,
            Vec2::new(width, 8.0),
            reduction_fraction(data.limiter_gain_reduction),
            REDUCTION_COLOR,
        );
        ui.label(format!("{:.1} dB", -data.limiter_gain_reduction.abs()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_reads_as_no_value() {
        assert_eq!(fmt_level(-200.0, "LUFS"), "---");
        assert_eq!(fmt_level(-23.04, "LUFS"), "-23.0 LUFS");
    }

    #[test]
    fn bars_clamp_to_their_scale() {
        assert_eq!(lufs_fraction(-200.0), 0.0);
        assert_eq!(lufs_fraction(0.0), 1.0);
        assert_eq!(reduction_fraction(-24.0), 1.0);
        assert_eq!(reduction_fraction(-6.0), 0.5);
    }
}
