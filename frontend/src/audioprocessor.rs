//! Audio Processor meters: input/output loudness, AGC gain and gain reduction,
//! and the stage meters (spike guard, expander, bass clipper, final clipper,
//! clip budget governor).

use crate::meter::BlockDataKey;
use egui::{Color32, Rect, Stroke, Ui, Vec2};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::audioprocessor::AudioProcessorGovernorState;
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

/// Meter values from one Audio Processor block, stage meters included.
#[derive(Debug, Clone)]
pub struct AudioProcessorData {
    pub input_momentary: f64,
    pub input_short_term: f64,
    pub output_momentary: f64,
    pub output_short_term: f64,
    pub output_integrated: f64,
    pub input_true_peak: f64,
    pub output_true_peak: f64,
    pub agc_gain: f64,
    pub band_gain_reduction: Vec<f64>,
    pub limiter_gain_reduction: f64,
    pub spike_events: u64,
    pub band_expander_gain_reduction: Vec<f64>,
    pub bass_clip_gain_reduction: f64,
    pub clip_depth: f64,
    pub loudness_deficit: f64,
    pub budget_excess: f64,
    pub governor_state: AudioProcessorGovernorState,
}

/// Storage for the meter data of every Audio Processor block, with the time
/// each block's data arrived.
#[derive(Debug, Clone, Default)]
pub struct AudioProcessorDataStore {
    data: HashMap<BlockDataKey, (AudioProcessorData, Instant)>,
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
            (data, Instant::now()),
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
            .filter(|(_, updated_at)| updated_at.elapsed() < DATA_TTL)
            .map(|(data, _)| data)
    }
}

fn fmt_level(v: f64, unit: &str) -> String {
    if v <= SILENCE_FLOOR {
        "---".to_string()
    } else {
        format!("{v:.1} {unit}").trim_end().to_string()
    }
}

fn lufs_fraction(lufs: f64) -> f32 {
    ((lufs - MIN_LUFS) / (MAX_LUFS - MIN_LUFS)).clamp(0.0, 1.0) as f32
}

/// Gain reduction is positive dB of reduction.
fn reduction_fraction(db: f64) -> f32 {
    (db / MAX_REDUCTION_DB).clamp(0.0, 1.0) as f32
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
            "I {}  AGC {:+.1}  LIM {:.1}",
            fmt_level(data.output_integrated, ""),
            data.agc_gain,
            data.limiter_gain_reduction
        ))
        .size(10.0),
    );
}

/// Governor state colour: amber when over budget or short of the target.
fn governor_color(stages: &AudioProcessorData) -> Color32 {
    if stages.budget_excess > 0.0
        || stages.loudness_deficit > 0.0
        || stages.governor_state == AudioProcessorGovernorState::OverBudget
    {
        REDUCTION_COLOR
    } else {
        Color32::from_gray(180)
    }
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

            ui.label("Integrated:");
            ui.label("");
            ui.label(fmt_level(data.output_integrated, "LUFS"));
            ui.end_row();

            ui.label("True peak:");
            ui.label(fmt_level(data.input_true_peak, "dBTP"));
            ui.label(fmt_level(data.output_true_peak, "dBTP"));
            ui.end_row();
        });

    ui.add_space(8.0);
    // Room for the label in front of a bar and the value after it. The
    // inspector scrolls sideways, so its available width is no limit.
    let width = (ui.available_width() - 110.0).clamp(40.0, 120.0);

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
                ui.label(format!("{:.1} dB", *gr));
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
        ui.label(format!("{:.1} dB", data.limiter_gain_reduction));
    });

    show_stages(ui, data, width);
}

/// The stage meters, compactly: one line per stage, with bars only for
/// the per-band expander and the bass clipper.
fn show_stages(ui: &mut Ui, stages: &AudioProcessorData, width: f32) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Bass clip");
        bar(
            ui,
            Vec2::new(width, 8.0),
            reduction_fraction(stages.bass_clip_gain_reduction),
            REDUCTION_COLOR,
        );
        ui.label(format!("{:.1} dB", stages.bass_clip_gain_reduction));
    });

    if !stages.band_expander_gain_reduction.is_empty() {
        ui.add_space(4.0);
        // The values on the label's line, the bars on their own line, so
        // five bands fit the bar width.
        ui.label(format!(
            "Expander: {}",
            fmt_band_list(&stages.band_expander_gain_reduction)
        ));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = EXPANDER_BAR_GAP;
            let band_width = expander_bar_width(width, stages.band_expander_gain_reduction.len());
            for gr in &stages.band_expander_gain_reduction {
                bar(
                    ui,
                    Vec2::new(band_width, 8.0),
                    reduction_fraction(*gr),
                    REDUCTION_COLOR,
                );
            }
        });
    }

    ui.add_space(4.0);
    ui.label(format!(
        "Spikes removed: {}   Clip depth: {}",
        stages.spike_events,
        fmt_level(stages.clip_depth, "dB")
    ));
    ui.colored_label(
        governor_color(stages),
        format!(
            "Governor: {}   Budget excess: {:.1} dB   Deficit: {:.1} LU",
            stages.governor_state.label(),
            stages.budget_excess,
            stages.loudness_deficit
        ),
    );
}

/// Gap between the per-band expander bars.
const EXPANDER_BAR_GAP: f32 = 4.0;

/// Width of each of `bands` expander bars so that, with the gaps, they fill
/// `width` exactly and never overflow it.
fn expander_bar_width(width: f32, bands: usize) -> f32 {
    let n = bands.max(1) as f32;
    ((width - EXPANDER_BAR_GAP * (n - 1.0)) / n).max(1.0)
}

/// Per-band values as "a / b / c dB".
fn fmt_band_list(values: &[f64]) -> String {
    let parts: Vec<String> = values.iter().map(|v| format!("{v:.1}")).collect();
    format!("{} dB", parts.join(" / "))
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
    fn band_lists_and_governor_colour() {
        assert_eq!(fmt_band_list(&[2.0, 0.0, 4.25]), "2.0 / 0.0 / 4.2 dB");
        let mut stages = AudioProcessorData {
            input_momentary: -23.0,
            input_short_term: -23.0,
            output_momentary: -23.0,
            output_short_term: -23.0,
            output_integrated: -23.0,
            input_true_peak: -2.0,
            output_true_peak: -3.0,
            agc_gain: 0.0,
            band_gain_reduction: vec![],
            limiter_gain_reduction: 0.0,
            spike_events: 0,
            band_expander_gain_reduction: vec![],
            bass_clip_gain_reduction: 0.0,
            clip_depth: -200.0,
            loudness_deficit: 0.0,
            budget_excess: 0.0,
            governor_state: AudioProcessorGovernorState::Free,
        };
        assert_eq!(governor_color(&stages), Color32::from_gray(180));
        stages.budget_excess = 0.5;
        assert_eq!(governor_color(&stages), REDUCTION_COLOR);
        // Nothing clipped reads as no value, not as -200 dB.
        assert_eq!(fmt_level(stages.clip_depth, "dB"), "---");
    }

    /// Five bands fit the narrowest bar width the inspector uses (40) and
    /// the widest (120), gaps included.
    #[test]
    fn expander_bars_fit_the_width() {
        for width in [40.0f32, 120.0] {
            for bands in 1..=5 {
                let total = expander_bar_width(width, bands) * bands as f32
                    + EXPANDER_BAR_GAP * (bands as f32 - 1.0);
                assert!(total <= width + 0.01, "{bands} bands at {width}: {total}");
            }
        }
    }

    #[test]
    fn bars_clamp_to_their_scale() {
        assert_eq!(lufs_fraction(-200.0), 0.0);
        assert_eq!(lufs_fraction(0.0), 1.0);
        assert_eq!(reduction_fraction(24.0), 1.0);
        assert_eq!(reduction_fraction(6.0), 0.5);
        assert_eq!(reduction_fraction(-1.0), 0.0);
    }
}
