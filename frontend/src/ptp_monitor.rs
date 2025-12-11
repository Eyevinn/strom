//! PTP clock monitoring with history and graphs.

use egui::{Color32, Pos2, Rect, Stroke, Ui, Vec2};
use std::collections::{HashMap, VecDeque};
use strom_types::FlowId;

const HISTORY_SIZE: usize = 60; // Keep 60 seconds of history

/// PTP stats data received from WebSocket event.
#[derive(Clone, Debug)]
pub struct PtpStatsData {
    pub domain: u8,
    pub synced: bool,
    pub mean_path_delay_ns: Option<u64>,
    pub clock_offset_ns: Option<i64>,
    pub r_squared: Option<f64>,
    pub clock_rate: Option<f64>,
    pub grandmaster_id: Option<u64>,
    pub master_id: Option<u64>,
}

/// Data store for PTP statistics with history per flow.
#[derive(Clone, Default)]
pub struct PtpStatsStore {
    /// History per flow ID
    flows: HashMap<FlowId, PtpFlowHistory>,
}

/// PTP stats history for a single flow.
#[derive(Clone)]
pub struct PtpFlowHistory {
    /// Clock offset history in microseconds
    clock_offset_history: VecDeque<f64>,
    /// R-squared history (0.0-1.0)
    r_squared_history: VecDeque<f64>,
    /// Path delay history in microseconds
    path_delay_history: VecDeque<f64>,
    /// Latest stats
    latest: Option<PtpStatsData>,
}

impl PtpFlowHistory {
    /// Get the latest PTP stats.
    pub fn latest(&self) -> Option<&PtpStatsData> {
        self.latest.as_ref()
    }

    /// Get the clock offset history (in microseconds).
    pub fn clock_offset_history(&self) -> &VecDeque<f64> {
        &self.clock_offset_history
    }

    /// Get the R² history.
    pub fn r_squared_history(&self) -> &VecDeque<f64> {
        &self.r_squared_history
    }

    /// Get the path delay history (in microseconds).
    pub fn path_delay_history(&self) -> &VecDeque<f64> {
        &self.path_delay_history
    }
}

impl Default for PtpFlowHistory {
    fn default() -> Self {
        Self {
            clock_offset_history: VecDeque::with_capacity(HISTORY_SIZE),
            r_squared_history: VecDeque::with_capacity(HISTORY_SIZE),
            path_delay_history: VecDeque::with_capacity(HISTORY_SIZE),
            latest: None,
        }
    }
}

impl PtpStatsStore {
    /// Create a new PTP stats store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update stats for a flow.
    pub fn update(&mut self, flow_id: FlowId, stats: PtpStatsData) {
        let history = self.flows.entry(flow_id).or_default();

        // Update clock offset history (convert ns to µs)
        if let Some(offset_ns) = stats.clock_offset_ns {
            let offset_us = offset_ns as f64 / 1000.0;
            history.clock_offset_history.push_back(offset_us);
            if history.clock_offset_history.len() > HISTORY_SIZE {
                history.clock_offset_history.pop_front();
            }
        }

        // Update R-squared history
        if let Some(r_squared) = stats.r_squared {
            history.r_squared_history.push_back(r_squared);
            if history.r_squared_history.len() > HISTORY_SIZE {
                history.r_squared_history.pop_front();
            }
        }

        // Update path delay history (convert ns to µs)
        if let Some(delay_ns) = stats.mean_path_delay_ns {
            let delay_us = delay_ns as f64 / 1000.0;
            history.path_delay_history.push_back(delay_us);
            if history.path_delay_history.len() > HISTORY_SIZE {
                history.path_delay_history.pop_front();
            }
        }

        history.latest = Some(stats);
    }

    /// Get the history for a flow.
    pub fn get_history(&self, flow_id: &FlowId) -> Option<&PtpFlowHistory> {
        self.flows.get(flow_id).filter(|h| h.latest.is_some())
    }

    /// Check if we have stats for a flow.
    pub fn has_flow(&self, flow_id: &FlowId) -> bool {
        self.flows
            .get(flow_id)
            .map(|h| h.latest.is_some())
            .unwrap_or(false)
    }

    /// Remove stats for a flow (when flow is deleted or stopped).
    pub fn remove_flow(&mut self, flow_id: &FlowId) {
        self.flows.remove(flow_id);
    }
}

/// Inline graph size constants
const INLINE_GRAPH_WIDTH: f32 = 150.0;
const INLINE_GRAPH_HEIGHT: f32 = 50.0;

/// Widget for displaying PTP stats graphs inline in the properties panel.
pub struct PtpStatsGraphs<'a> {
    store: &'a PtpStatsStore,
    flow_id: FlowId,
}

impl<'a> PtpStatsGraphs<'a> {
    pub fn new(store: &'a PtpStatsStore, flow_id: FlowId) -> Self {
        Self { store, flow_id }
    }

    /// Draw inline clock offset graph (to be placed next to the value).
    pub fn draw_offset_graph(&self, ui: &mut Ui) {
        let Some(history) = self.store.get_history(&self.flow_id) else {
            return;
        };
        if history.clock_offset_history.is_empty() {
            return;
        }
        let rect = ui.allocate_space(Vec2::new(INLINE_GRAPH_WIDTH, INLINE_GRAPH_HEIGHT));
        draw_ptp_graph(
            ui.painter(),
            rect.1,
            &history.clock_offset_history,
            Color32::from_rgb(100, 200, 255),
            true,
        );
    }

    /// Draw inline R² graph (to be placed next to the value).
    pub fn draw_r_squared_graph(&self, ui: &mut Ui) {
        let Some(history) = self.store.get_history(&self.flow_id) else {
            return;
        };
        if history.r_squared_history.is_empty() {
            return;
        }
        let rect = ui.allocate_space(Vec2::new(INLINE_GRAPH_WIDTH, INLINE_GRAPH_HEIGHT));
        draw_ptp_graph_fixed_range(
            ui.painter(),
            rect.1,
            &history.r_squared_history,
            Color32::from_rgb(100, 255, 100),
            0.9,
            1.0,
        );
    }

    /// Draw inline path delay graph (to be placed next to the value).
    pub fn draw_delay_graph(&self, ui: &mut Ui) {
        let Some(history) = self.store.get_history(&self.flow_id) else {
            return;
        };
        if history.path_delay_history.is_empty() {
            return;
        }
        let rect = ui.allocate_space(Vec2::new(INLINE_GRAPH_WIDTH, INLINE_GRAPH_HEIGHT));
        draw_ptp_graph(
            ui.painter(),
            rect.1,
            &history.path_delay_history,
            Color32::from_rgb(255, 150, 100),
            false,
        );
    }
}

/// Draw a graph with auto-scaled range for signed values.
fn draw_ptp_graph(
    painter: &egui::Painter,
    rect: Rect,
    data: &VecDeque<f64>,
    color: Color32,
    signed: bool,
) {
    // Draw background
    painter.rect_filled(rect, 2.0, Color32::from_gray(20));

    if data.is_empty() {
        return;
    }

    // Calculate range
    let (min_val, max_val) = if signed {
        // For signed values, center around zero with symmetric range
        let max_abs = data.iter().map(|v| v.abs()).fold(0.0_f64, f64::max);
        let range = max_abs.max(1.0) * 1.1; // Add 10% margin
        (-range, range)
    } else {
        // For unsigned values, use 0 to max
        let max = data.iter().fold(0.0_f64, |a, &b| a.max(b));
        (0.0, max.max(1.0) * 1.1)
    };

    // Draw center line for signed values
    if signed {
        let y_center = rect.center().y;
        painter.line_segment(
            [
                Pos2::new(rect.min.x, y_center),
                Pos2::new(rect.max.x, y_center),
            ],
            Stroke::new(0.5, Color32::from_gray(80)),
        );
    }

    // Draw grid lines
    for i in 0..=4 {
        let y = rect.min.y + (i as f32 / 4.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(0.5, Color32::from_gray(40)),
        );
    }

    // Draw data line
    let range = max_val - min_val;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (HISTORY_SIZE - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );
}

/// Draw a graph with fixed range (useful for R-squared which is always 0-1).
fn draw_ptp_graph_fixed_range(
    painter: &egui::Painter,
    rect: Rect,
    data: &VecDeque<f64>,
    color: Color32,
    min_val: f64,
    max_val: f64,
) {
    // Draw background
    painter.rect_filled(rect, 2.0, Color32::from_gray(20));

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
    let range = max_val - min_val;
    let points: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (HISTORY_SIZE - 1).max(1) as f32) * rect.width();
            let normalized = ((value - min_val) / range) as f32;
            let y = rect.max.y - normalized * rect.height();
            Pos2::new(x, y.clamp(rect.min.y, rect.max.y))
        })
        .collect();

    if points.len() >= 2 {
        painter.add(egui::Shape::line(points, Stroke::new(1.5, color)));
    }

    // Draw border
    painter.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );
}
