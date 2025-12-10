//! QoS (Quality of Service) monitoring with history and visualization.
//!
//! Tracks buffer drop statistics per element and provides visual indicators
//! for elements that are falling behind.

use instant::Instant;
use std::collections::{HashMap, VecDeque};
use strom_types::FlowId;

/// History size for graphs (60 seconds at 1 update/second)
const HISTORY_SIZE: usize = 60;

/// Threshold for "warning" status (proportion below this = warning)
const WARNING_THRESHOLD: f64 = 0.95;
/// Threshold for "critical" status (proportion below this = critical)
const CRITICAL_THRESHOLD: f64 = 0.8;

/// QoS health status for an element
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoSHealth {
    /// No QoS issues (proportion >= 0.95)
    Ok,
    /// Minor issues, slightly falling behind (0.8 <= proportion < 0.95)
    Warning,
    /// Significant issues, falling behind badly (proportion < 0.8)
    Critical,
}

impl QoSHealth {
    pub fn from_proportion(proportion: f64) -> Self {
        if proportion >= WARNING_THRESHOLD {
            QoSHealth::Ok
        } else if proportion >= CRITICAL_THRESHOLD {
            QoSHealth::Warning
        } else {
            QoSHealth::Critical
        }
    }

    pub fn color(&self) -> egui::Color32 {
        match self {
            QoSHealth::Ok => egui::Color32::from_rgb(0, 200, 0),
            QoSHealth::Warning => egui::Color32::from_rgb(255, 165, 0),
            QoSHealth::Critical => egui::Color32::from_rgb(255, 50, 50),
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            QoSHealth::Ok => "✓",
            QoSHealth::Warning => "⚠",
            QoSHealth::Critical => "⛔",
        }
    }
}

/// QoS data for a single element
#[derive(Clone, Debug)]
pub struct QoSElementData {
    /// Element ID (or block ID if element is inside a block)
    pub element_id: String,
    /// Block ID if this element is inside a block
    pub block_id: Option<String>,
    /// Full GStreamer element name
    pub element_name: String,
    /// Internal element type if part of a block
    pub internal_element_type: Option<String>,
    /// Average proportion (< 1.0 = falling behind)
    pub avg_proportion: f64,
    /// Minimum proportion seen
    pub min_proportion: f64,
    /// Maximum proportion seen
    pub max_proportion: f64,
    /// Average jitter in nanoseconds
    pub avg_jitter_ns: i64,
    /// Number of QoS events in the last update
    pub event_count: u64,
    /// Total buffers processed
    pub total_processed: u64,
    /// Whether currently falling behind
    pub is_falling_behind: bool,
    /// Timestamp of last update (using WASM-compatible instant crate)
    pub last_update: Instant,
}

impl QoSElementData {
    pub fn health(&self) -> QoSHealth {
        QoSHealth::from_proportion(self.avg_proportion)
    }
}

/// QoS history for a single element
#[derive(Clone, Default)]
pub struct QoSElementHistory {
    /// Proportion history (for graphing)
    pub proportion_history: VecDeque<f64>,
    /// Latest data
    pub latest: Option<QoSElementData>,
}

/// QoS stats for a single flow
#[derive(Clone, Default)]
pub struct QoSFlowStats {
    /// Per-element stats (element_id -> history)
    pub elements: HashMap<String, QoSElementHistory>,
    /// Worst health status across all elements
    pub worst_health: Option<QoSHealth>,
}

impl QoSFlowStats {
    /// Update the worst health status based on all elements
    fn update_worst_health(&mut self) {
        self.worst_health = self
            .elements
            .values()
            .filter_map(|h| h.latest.as_ref())
            .map(|data| data.health())
            .min_by_key(|health| match health {
                QoSHealth::Critical => 0,
                QoSHealth::Warning => 1,
                QoSHealth::Ok => 2,
            });
    }
}

/// Data store for QoS statistics
#[derive(Clone, Default)]
pub struct QoSStore {
    /// Stats per flow
    flows: HashMap<FlowId, QoSFlowStats>,
}

impl QoSStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update QoS stats for an element
    pub fn update(&mut self, flow_id: FlowId, data: QoSElementData) {
        let flow_stats = self.flows.entry(flow_id).or_default();

        // Use element_id as the key (this is the visible node ID in the graph)
        let key = data.element_id.clone();
        let history = flow_stats.elements.entry(key).or_default();

        // Add proportion to history
        history.proportion_history.push_back(data.avg_proportion);
        if history.proportion_history.len() > HISTORY_SIZE {
            history.proportion_history.pop_front();
        }

        history.latest = Some(data);

        // Update worst health for the flow
        flow_stats.update_worst_health();
    }

    /// Get the worst health status for a flow (for navigator indicator)
    pub fn get_flow_health(&self, flow_id: &FlowId) -> Option<QoSHealth> {
        self.flows.get(flow_id).and_then(|f| f.worst_health)
    }

    /// Get QoS stats for a specific element
    pub fn get_element_stats(
        &self,
        flow_id: &FlowId,
        element_id: &str,
    ) -> Option<&QoSElementHistory> {
        self.flows
            .get(flow_id)
            .and_then(|f| f.elements.get(element_id))
    }

    /// Get all elements with QoS issues in a flow
    pub fn get_problem_elements(&self, flow_id: &FlowId) -> Vec<(&str, &QoSElementData)> {
        self.flows
            .get(flow_id)
            .map(|f| {
                f.elements
                    .iter()
                    .filter_map(|(id, h)| {
                        h.latest.as_ref().and_then(|data| {
                            if data.health() != QoSHealth::Ok {
                                Some((id.as_str(), data))
                            } else {
                                None
                            }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get all element IDs with their health status for a flow (for graph rendering)
    pub fn get_element_health_map(&self, flow_id: &FlowId) -> HashMap<String, QoSHealth> {
        self.flows
            .get(flow_id)
            .map(|f| {
                f.elements
                    .iter()
                    .filter_map(|(id, h)| h.latest.as_ref().map(|data| (id.clone(), data.health())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Check if any element in a flow has QoS issues
    pub fn flow_has_issues(&self, flow_id: &FlowId) -> bool {
        self.get_flow_health(flow_id)
            .map(|h| h != QoSHealth::Ok)
            .unwrap_or(false)
    }

    /// Clear stats for a flow (when flow is stopped or deleted)
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        self.flows.remove(flow_id);
    }

    /// Clear stats for a specific element (when user dismisses a QoS log entry)
    pub fn clear_element(&mut self, flow_id: &FlowId, element_id: &str) {
        if let Some(flow_stats) = self.flows.get_mut(flow_id) {
            flow_stats.elements.remove(element_id);
            flow_stats.update_worst_health();
        }
    }

    /// Clear stale entries (elements that haven't updated recently)
    pub fn clear_stale(&mut self, max_age: std::time::Duration) {
        for flow_stats in self.flows.values_mut() {
            flow_stats.elements.retain(|_, h| {
                h.latest
                    .as_ref()
                    .map(|d| d.last_update.elapsed() < max_age)
                    .unwrap_or(false)
            });
            flow_stats.update_worst_health();
        }
    }
}

/// Graph size constants for inline QoS graphs
const INLINE_GRAPH_WIDTH: f32 = 120.0;
const INLINE_GRAPH_HEIGHT: f32 = 40.0;

/// Draw an inline QoS proportion graph
pub fn draw_qos_graph(ui: &mut egui::Ui, history: &VecDeque<f64>) {
    if history.is_empty() {
        return;
    }

    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::new(INLINE_GRAPH_WIDTH, INLINE_GRAPH_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter();

    // Background
    painter.rect_filled(rect, 2.0, egui::Color32::from_gray(20));

    // Draw threshold lines
    let warning_y = rect.max.y - (WARNING_THRESHOLD as f32) * rect.height();
    let critical_y = rect.max.y - (CRITICAL_THRESHOLD as f32) * rect.height();

    painter.line_segment(
        [
            egui::Pos2::new(rect.min.x, warning_y),
            egui::Pos2::new(rect.max.x, warning_y),
        ],
        egui::Stroke::new(
            0.5,
            egui::Color32::from_rgb(255, 165, 0).gamma_multiply(0.5),
        ),
    );
    painter.line_segment(
        [
            egui::Pos2::new(rect.min.x, critical_y),
            egui::Pos2::new(rect.max.x, critical_y),
        ],
        egui::Stroke::new(
            0.5,
            egui::Color32::from_rgb(255, 50, 50).gamma_multiply(0.5),
        ),
    );

    // Draw data line with color based on value
    let points: Vec<egui::Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let x = rect.min.x + (i as f32 / (HISTORY_SIZE - 1).max(1) as f32) * rect.width();
            // Clamp to 0.0-1.0 range for display
            let normalized = value.clamp(0.0, 1.0) as f32;
            let y = rect.max.y - normalized * rect.height();
            egui::Pos2::new(x, y)
        })
        .collect();

    if points.len() >= 2 {
        // Color based on latest value
        let latest = history.back().copied().unwrap_or(1.0);
        let color = QoSHealth::from_proportion(latest).color();
        painter.add(egui::Shape::line(points, egui::Stroke::new(1.5, color)));
    }

    // Border
    painter.rect_stroke(
        rect,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
        egui::StrokeKind::Outside,
    );
}
