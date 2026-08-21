//! Audio analyzer visualization widgets (waveform oscilloscope and vectorscope).

use crate::meter::BlockDataKey;
use base64::{engine::general_purpose::STANDARD, Engine};
use egui::{Color32, Rect, Stroke, Ui, Vec2};
use egui_plot::{HLine, Legend, Line, Plot, PlotPoints, Points, VLine};
use instant::Instant;
use std::collections::HashMap;
use std::time::Duration;
use strom_types::FlowId;

/// Time-to-live for analyzer data before it's considered stale.
const ANALYZER_DATA_TTL: Duration = Duration::from_millis(1000);

/// Waveform color for L channel (teal).
const COLOR_L: Color32 = Color32::from_rgb(0, 180, 180);
/// Waveform color for R channel (orange).
const COLOR_R: Color32 = Color32::from_rgb(220, 140, 40);
/// Vectorscope dot color (green, classic scope look).
const COLOR_VECTOR: Color32 = Color32::from_rgb(100, 200, 100);
/// Reference line color.
const COLOR_REF: Color32 = Color32::from_rgb(60, 60, 60);

/// Audio analyzer data for a specific element.
#[derive(Debug, Clone)]
pub struct AudioAnalyzerData {
    /// Waveform min values per column for L channel (-1.0..1.0)
    pub waveform_l_min: Vec<f32>,
    /// Waveform max values per column for L channel
    pub waveform_l_max: Vec<f32>,
    /// Waveform min values per column for R channel
    pub waveform_r_min: Vec<f32>,
    /// Waveform max values per column for R channel
    pub waveform_r_max: Vec<f32>,
    /// Vectorscope L channel samples (-1.0..1.0)
    pub vectorscope_l: Vec<f32>,
    /// Vectorscope R channel samples (-1.0..1.0)
    pub vectorscope_r: Vec<f32>,
}

impl AudioAnalyzerData {
    /// Decode base64-encoded i8 samples and normalize to -1.0..1.0.
    pub fn from_base64(
        waveform_l_min: &str,
        waveform_l_max: &str,
        waveform_r_min: &str,
        waveform_r_max: &str,
        vectorscope_l: &str,
        vectorscope_r: &str,
    ) -> Self {
        Self {
            waveform_l_min: decode_and_normalize(waveform_l_min),
            waveform_l_max: decode_and_normalize(waveform_l_max),
            waveform_r_min: decode_and_normalize(waveform_r_min),
            waveform_r_max: decode_and_normalize(waveform_r_max),
            vectorscope_l: decode_and_normalize(vectorscope_l),
            vectorscope_r: decode_and_normalize(vectorscope_r),
        }
    }
}

/// Decode a base64 string of i8 bytes and normalize each to -1.0..1.0.
fn decode_and_normalize(b64: &str) -> Vec<f32> {
    let bytes = STANDARD.decode(b64).unwrap_or_default();
    bytes.iter().map(|&b| (b as i8) as f32 / 128.0).collect()
}

/// Analyzer data with timestamp for TTL tracking.
#[derive(Debug, Clone)]
struct TimestampedData {
    data: AudioAnalyzerData,
    updated_at: Instant,
}

/// Storage for all audio analyzer data in the application.
#[derive(Debug, Clone, Default)]
pub struct AudioAnalyzerDataStore {
    data: HashMap<BlockDataKey, TimestampedData>,
}

impl AudioAnalyzerDataStore {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Update analyzer data for a specific element.
    pub fn update(&mut self, flow_id: FlowId, element_id: String, data: AudioAnalyzerData) {
        let key = BlockDataKey {
            flow_id,
            element_id,
        };
        self.data.insert(
            key,
            TimestampedData {
                data,
                updated_at: Instant::now(),
            },
        );
    }

    /// Get analyzer data for a specific element.
    /// Returns None if the data is stale (older than TTL).
    pub fn get(&self, flow_id: &FlowId, element_id: &str) -> Option<&AudioAnalyzerData> {
        let key = BlockDataKey {
            flow_id: *flow_id,
            element_id: element_id.to_string(),
        };
        self.data.get(&key).and_then(|timestamped| {
            if timestamped.updated_at.elapsed() < ANALYZER_DATA_TTL {
                Some(&timestamped.data)
            } else {
                None
            }
        })
    }
}

/// Calculate the height needed for a compact audio analyzer display.
pub fn calculate_compact_height() -> f32 {
    // Waveform (two channels stacked) + some padding
    80.0
}

/// Persistent zoom state for the waveform plot, stored in egui memory.
#[derive(Clone, Debug)]
struct WaveformZoom {
    /// Amplitude range (symmetric around 0). 1.0 = full scale, 0.1 = zoomed in 10x.
    amplitude: f32,
    /// Fraction of total bins visible (1.0 = all, 0.1 = 10%).
    bin_fraction: f32,
}

impl Default for WaveformZoom {
    fn default() -> Self {
        Self {
            amplitude: 1.0,
            bin_fraction: 1.0,
        }
    }
}

/// Persistent zoom state for the vectorscope plot, stored in egui memory.
#[derive(Clone, Debug)]
struct VectorscopeZoom {
    /// Scale (symmetric around 0). 1.0 = full scale, 0.5 = zoomed in 2x.
    scale: f32,
}

impl Default for VectorscopeZoom {
    fn default() -> Self {
        Self { scale: 1.0 }
    }
}

/// Render a full audio analyzer view (for property inspector panel) using egui_plot.
pub fn show_full(ui: &mut Ui, data: &AudioAnalyzerData) {
    ui.heading("Audio Analyzer");
    ui.separator();

    if data.waveform_l_min.is_empty() && data.vectorscope_l.is_empty() {
        ui.label("No signal detected");
        return;
    }

    let available_width = ui.available_width().max(200.0);

    // --- Waveform section ---
    ui.label("Waveform");

    // Zoom controls stored in egui temp memory
    let wf_zoom_id = ui.id().with("wf_zoom");
    let mut wf_zoom = ui
        .data_mut(|d| d.get_temp::<WaveformZoom>(wf_zoom_id))
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label("Amplitude:");
        ui.add(egui::Slider::new(&mut wf_zoom.amplitude, 0.01..=1.0).logarithmic(true));
        ui.label("Bins:");
        ui.add(egui::Slider::new(&mut wf_zoom.bin_fraction, 0.01..=1.0).logarithmic(true));
        if ui.small_button("Reset").clicked() {
            wf_zoom = WaveformZoom::default();
        }
    });

    ui.data_mut(|d| d.insert_temp(wf_zoom_id, wf_zoom.clone()));

    let n_l = data.waveform_l_min.len();
    let n_r = data.waveform_r_min.len();
    let n_max = n_l.max(n_r);

    let l_max_points: PlotPoints<'_> = (0..n_l)
        .map(|i| [i as f64, data.waveform_l_max[i] as f64])
        .collect();
    let l_min_points: PlotPoints<'_> = (0..n_l)
        .map(|i| [i as f64, data.waveform_l_min[i] as f64])
        .collect();
    let r_max_points: PlotPoints<'_> = (0..n_r)
        .map(|i| [i as f64, data.waveform_r_max[i] as f64])
        .collect();
    let r_min_points: PlotPoints<'_> = (0..n_r)
        .map(|i| [i as f64, data.waveform_r_min[i] as f64])
        .collect();

    let amp = wf_zoom.amplitude as f64;
    let bin_visible = (n_max as f64 * wf_zoom.bin_fraction as f64).max(1.0);

    let waveform_height = 150.0;
    Plot::new("waveform_full")
        .height(waveform_height)
        .width(available_width)
        .y_axis_label("Amplitude")
        .x_axis_label("Sample bin")
        .legend({
            let legend = Legend::default();
            // Only hide L min / R min on first render; after that the user controls visibility
            let init_id = ui.id().with("wf_legend_init");
            let already_init = ui.data_mut(|d| d.get_temp::<bool>(init_id).unwrap_or(false));
            if !already_init {
                ui.data_mut(|d| d.insert_temp(init_id, true));
                legend.hidden_items([egui::Id::new("L min"), egui::Id::new("R min")])
            } else {
                legend
            }
        })
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .show(ui, |plot_ui| {
            plot_ui.set_plot_bounds_y(-amp..=amp);
            plot_ui.set_plot_bounds_x(0.0..=bin_visible);

            plot_ui.hline(HLine::new("", 0.0).color(COLOR_REF).width(0.5_f32));

            plot_ui.line(Line::new("L", l_max_points).color(COLOR_L).width(1.0_f32));
            plot_ui.line(
                Line::new("L min", l_min_points)
                    .color(COLOR_L)
                    .width(1.0_f32),
            );

            plot_ui.line(Line::new("R", r_max_points).color(COLOR_R).width(1.0_f32));
            plot_ui.line(
                Line::new("R min", r_min_points)
                    .color(COLOR_R)
                    .width(1.0_f32),
            );
        });

    ui.add_space(8.0);

    // --- Vectorscope section ---
    ui.label("Vectorscope");

    let vs_zoom_id = ui.id().with("vs_zoom");
    let mut vs_zoom = ui
        .data_mut(|d| d.get_temp::<VectorscopeZoom>(vs_zoom_id))
        .unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label("Scale:");
        ui.add(egui::Slider::new(&mut vs_zoom.scale, 0.01..=1.0).logarithmic(true));
        if ui.small_button("Reset").clicked() {
            vs_zoom = VectorscopeZoom::default();
        }
    });

    ui.data_mut(|d| d.insert_temp(vs_zoom_id, vs_zoom.clone()));

    let scope_size = available_width.min(300.0);
    let scatter: PlotPoints<'_> = data
        .vectorscope_l
        .iter()
        .zip(data.vectorscope_r.iter())
        .map(|(&l, &r)| [l as f64, r as f64])
        .collect();

    let vs_scale = vs_zoom.scale as f64;

    Plot::new("vectorscope_full")
        .height(scope_size)
        .width(scope_size)
        .data_aspect(1.0)
        .x_axis_label("L")
        .y_axis_label("R")
        .legend(egui_plot::Legend::default())
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .show(ui, |plot_ui| {
            plot_ui.set_plot_bounds_x(-vs_scale..=vs_scale);
            plot_ui.set_plot_bounds_y(-vs_scale..=vs_scale);

            plot_ui.hline(HLine::new("", 0.0).color(COLOR_REF).width(0.5_f32));
            plot_ui.vline(VLine::new("", 0.0).color(COLOR_REF).width(0.5_f32));

            // +45deg (mono) diagonal
            plot_ui.line(
                Line::new("", PlotPoints::new(vec![[-1.0, -1.0], [1.0, 1.0]]))
                    .color(Color32::from_rgb(40, 40, 50))
                    .width(0.5_f32),
            );
            // -45deg (phase inversion) diagonal
            plot_ui.line(
                Line::new("", PlotPoints::new(vec![[-1.0, 1.0], [1.0, -1.0]]))
                    .color(Color32::from_rgb(40, 40, 50))
                    .width(0.5_f32),
            );

            plot_ui.points(
                Points::new("L/R", scatter)
                    .color(COLOR_VECTOR)
                    .radius(1.5_f32),
            );
        });
}

/// Render a compact audio analyzer (waveform + vectorscope side by side).
pub fn show_compact(ui: &mut Ui, data: &AudioAnalyzerData) {
    let available = ui.available_size();
    let total_width = available.x.max(120.0);
    let total_height = available.y.max(60.0);

    // Allocate space for vectorscope (square, using height as size)
    let scope_size = total_height.min(total_width * 0.35);
    let waveform_width = (total_width - scope_size - 4.0).max(60.0);

    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(total_width, total_height), egui::Sense::hover());

    let painter = ui.painter_at(rect);

    // Waveform area (left side)
    let waveform_rect = Rect::from_min_size(rect.min, Vec2::new(waveform_width, total_height));
    draw_waveform(&painter, waveform_rect, data);

    // Vectorscope area (right side, square)
    let scope_rect = Rect::from_min_size(
        egui::pos2(
            rect.min.x + waveform_width + 4.0,
            rect.min.y + (total_height - scope_size) / 2.0,
        ),
        Vec2::new(scope_size, scope_size),
    );
    draw_vectorscope(&painter, scope_rect, data);
}

/// Draw the waveform oscilloscope display.
fn draw_waveform(painter: &egui::Painter, rect: Rect, data: &AudioAnalyzerData) {
    // Background
    painter.rect(
        rect,
        1.0,
        Color32::from_rgb(15, 15, 20),
        Stroke::new(1.0_f32, Color32::from_gray(50)),
        egui::epaint::StrokeKind::Inside,
    );

    let half_h = rect.height() / 2.0;

    // L channel: top half, R channel: bottom half
    let l_rect = Rect::from_min_size(rect.min, Vec2::new(rect.width(), half_h));
    let r_rect = Rect::from_min_size(
        egui::pos2(rect.min.x, rect.min.y + half_h),
        Vec2::new(rect.width(), half_h),
    );

    // Center lines (zero crossing)
    let l_center_y = l_rect.center().y;
    let r_center_y = r_rect.center().y;
    painter.line_segment(
        [
            egui::pos2(rect.min.x, l_center_y),
            egui::pos2(rect.max.x, l_center_y),
        ],
        Stroke::new(0.5_f32, COLOR_REF),
    );
    painter.line_segment(
        [
            egui::pos2(rect.min.x, r_center_y),
            egui::pos2(rect.max.x, r_center_y),
        ],
        Stroke::new(0.5_f32, COLOR_REF),
    );

    // Divider between L and R
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.min.y + half_h),
            egui::pos2(rect.max.x, rect.min.y + half_h),
        ],
        Stroke::new(0.5_f32, COLOR_REF),
    );

    // Draw L waveform
    draw_channel_waveform(
        painter,
        l_rect,
        &data.waveform_l_min,
        &data.waveform_l_max,
        COLOR_L,
    );

    // Draw R waveform
    draw_channel_waveform(
        painter,
        r_rect,
        &data.waveform_r_min,
        &data.waveform_r_max,
        COLOR_R,
    );
}

/// Draw a single channel's waveform in the given rect.
///
/// When there are more data columns than pixels, adjacent columns are merged
/// (taking min-of-mins and max-of-maxes). Draws vertical min/max bars plus
/// line segments connecting midpoints of adjacent columns to eliminate gaps.
fn draw_channel_waveform(
    painter: &egui::Painter,
    rect: Rect,
    mins: &[f32],
    maxs: &[f32],
    color: Color32,
) {
    let num_columns = mins.len();
    if num_columns == 0 {
        return;
    }

    let center_y = rect.center().y;
    let half_h = rect.height() / 2.0 * 0.9; // 90% to leave a small margin
    let pixel_cols = (rect.width() as usize).max(1);

    // Number of render columns is the smaller of data columns and pixel columns
    let render_cols = pixel_cols.min(num_columns);
    let data_per_render = num_columns as f64 / render_cols as f64;
    let px_per_col = rect.width() / render_cols as f32;

    let mut prev_mid: Option<egui::Pos2> = None;

    for i in 0..render_cols {
        // Map render column back to data range
        let d_start = (i as f64 * data_per_render) as usize;
        let d_end = (((i + 1) as f64) * data_per_render) as usize;
        let d_end = d_end.min(num_columns);

        let mut col_min: f32 = 0.0;
        let mut col_max: f32 = 0.0;
        for j in d_start..d_end {
            if mins[j] < col_min {
                col_min = mins[j];
            }
            if maxs[j] > col_max {
                col_max = maxs[j];
            }
        }

        let x = rect.min.x + (i as f32 + 0.5) * px_per_col;

        // Map -1.0..1.0 to pixel Y (inverted: -1.0 is bottom, 1.0 is top)
        let y_top = center_y - col_max * half_h;
        let y_bot = center_y - col_min * half_h;

        // Vertical min/max bar
        painter.line_segment(
            [egui::pos2(x, y_top), egui::pos2(x, y_bot)],
            Stroke::new(1.0_f32, color),
        );

        // Connect midpoints of adjacent columns to fill gaps
        let mid = egui::pos2(x, (y_top + y_bot) / 2.0);
        if let Some(prev) = prev_mid {
            painter.line_segment([prev, mid], Stroke::new(1.0_f32, color));
        }
        prev_mid = Some(mid);
    }
}

/// Draw the vectorscope (Lissajous) display.
fn draw_vectorscope(painter: &egui::Painter, rect: Rect, data: &AudioAnalyzerData) {
    // Background
    painter.rect(
        rect,
        1.0,
        Color32::from_rgb(15, 15, 20),
        Stroke::new(1.0_f32, Color32::from_gray(50)),
        egui::epaint::StrokeKind::Inside,
    );

    let center = rect.center();
    let half_size = rect.width().min(rect.height()) / 2.0 * 0.9;

    // Reference lines: crosshairs
    painter.line_segment(
        [
            egui::pos2(rect.min.x, center.y),
            egui::pos2(rect.max.x, center.y),
        ],
        Stroke::new(0.5_f32, COLOR_REF),
    );
    painter.line_segment(
        [
            egui::pos2(center.x, rect.min.y),
            egui::pos2(center.x, rect.max.y),
        ],
        Stroke::new(0.5_f32, COLOR_REF),
    );

    // Reference lines: +45deg (mono) and -45deg (phase inversion)
    let diag_len = half_size * 0.95;
    painter.line_segment(
        [
            egui::pos2(center.x - diag_len, center.y - diag_len),
            egui::pos2(center.x + diag_len, center.y + diag_len),
        ],
        Stroke::new(0.5_f32, Color32::from_rgb(40, 40, 50)),
    );
    painter.line_segment(
        [
            egui::pos2(center.x - diag_len, center.y + diag_len),
            egui::pos2(center.x + diag_len, center.y - diag_len),
        ],
        Stroke::new(0.5_f32, Color32::from_rgb(40, 40, 50)),
    );

    // Draw points: X = L, Y = R (standard vectorscope convention)
    for (&l, &r) in data.vectorscope_l.iter().zip(data.vectorscope_r.iter()) {
        let x = center.x + l * half_size;
        let y = center.y - r * half_size; // Invert Y for screen coords

        painter.rect_filled(
            Rect::from_center_size(egui::pos2(x, y), Vec2::splat(1.5)),
            0.0,
            COLOR_VECTOR,
        );
    }
}
