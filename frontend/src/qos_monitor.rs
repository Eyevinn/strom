//! QoS (Quality of Service) monitoring.
//!
//! Tracks late-data drops per element, or per input pad of an element (an
//! audio mixer reports each input on its own), and provides visual indicators
//! for elements that drop.

use instant::Instant;
use std::collections::HashMap;
use strom_types::FlowId;

/// TTL for QoS reports: the backend only reports a source in a period where it
/// posted QoS messages, so without a new report within this period the health
/// status returns to Ok.
const QOS_TTL_SECS: f64 = 15.0;

/// QoS messages in one period (about a second) from which drops count as
/// critical rather than a warning. A dropping mixer input posts one per
/// buffer, dozens per second; an occasional late buffer posts one or two.
const CRITICAL_EVENTS_PER_PERIOD: u64 = 10;

/// QoS health status for an element
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoSHealth {
    /// No late data dropped
    Ok,
    /// Late data dropped now and then
    Warning,
    /// Late data dropped continuously
    Critical,
}

impl QoSHealth {
    /// Health from one period's report: drops, not the proportion. A mixer
    /// input that drops everything reports a proportion of 1.0, and an element
    /// that keeps up reports less than 1.0.
    pub fn from_drops(dropped_in_period: u64, event_count: u64) -> Self {
        if dropped_in_period == 0 {
            QoSHealth::Ok
        } else if event_count >= CRITICAL_EVENTS_PER_PERIOD {
            QoSHealth::Critical
        } else {
            QoSHealth::Warning
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
            QoSHealth::Ok => egui_phosphor::regular::CHECK,
            QoSHealth::Warning => egui_phosphor::regular::WARNING,
            QoSHealth::Critical => egui_phosphor::regular::PROHIBIT,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            QoSHealth::Critical => 0,
            QoSHealth::Warning => 1,
            QoSHealth::Ok => 2,
        }
    }
}

/// QoS data for a single element, or a single input pad of one
#[derive(Clone, Debug)]
pub struct QoSElementData {
    /// Element ID (or block ID if element is inside a block)
    pub element_id: String,
    /// Full GStreamer element name
    pub element_name: String,
    /// Input pad that dropped, when a pad reported it (e.g. `sink_1`)
    pub pad_name: Option<String>,
    /// Element feeding that pad, which tells which input was late
    pub upstream_element: Option<String>,
    /// Number of QoS messages in the last report
    pub event_count: u64,
    /// Dropped in the last report, in `stats_format` units
    pub dropped_in_period: u64,
    /// Unit of the dropped count
    pub stats_format: Option<String>,
    /// When the report arrived (WASM-compatible instant crate)
    pub last_update: Instant,
}

impl QoSElementData {
    /// Health of the last report, Ok once it is older than the TTL.
    pub fn health(&self) -> QoSHealth {
        self.health_at(Instant::now())
    }

    fn health_at(&self, now: Instant) -> QoSHealth {
        if (now - self.last_update).as_secs_f64() > QOS_TTL_SECS {
            return QoSHealth::Ok;
        }
        QoSHealth::from_drops(self.dropped_in_period, self.event_count)
    }

    /// One line for a tooltip: "mixer input sink_1 (fed by ...): 1920 samples dropped".
    pub fn describe(&self) -> String {
        let mut text = self.element_name.clone();
        if let Some(pad) = &self.pad_name {
            text.push_str(&format!(" input {pad}"));
        }
        if let Some(upstream) = &self.upstream_element {
            text.push_str(&format!(" (fed by {upstream})"));
        }
        text.push_str(&format!(
            ": {} {} dropped",
            self.dropped_in_period,
            self.stats_format.as_deref().unwrap_or("units")
        ));
        text
    }
}

/// Key of one QoS source: graph node id, element name, input pad.
type SourceKey = (String, String, Option<String>);

/// QoS stats for a single flow
#[derive(Clone, Default)]
pub struct QoSFlowStats {
    /// Latest report per source
    pub elements: HashMap<SourceKey, QoSElementData>,
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

    /// Update QoS stats for an element or one of its input pads
    pub fn update(&mut self, flow_id: FlowId, data: QoSElementData) {
        let key = (
            data.element_id.clone(),
            data.element_name.clone(),
            data.pad_name.clone(),
        );
        self.flows
            .entry(flow_id)
            .or_default()
            .elements
            .insert(key, data);
    }

    /// Get the worst health status for a flow (for navigator indicator)
    pub fn get_flow_health(&self, flow_id: &FlowId) -> Option<QoSHealth> {
        let now = Instant::now();
        self.flows.get(flow_id).and_then(|f| {
            f.elements
                .values()
                .map(|d| d.health_at(now))
                .min_by_key(QoSHealth::rank)
        })
    }

    /// Get all sources with current QoS issues in a flow
    pub fn get_problem_elements(&self, flow_id: &FlowId) -> Vec<&QoSElementData> {
        let now = Instant::now();
        self.flows
            .get(flow_id)
            .map(|f| {
                f.elements
                    .values()
                    .filter(|d| d.health_at(now) != QoSHealth::Ok)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Worst current health per graph node id in a flow (for graph rendering)
    pub fn get_element_health_map(&self, flow_id: &FlowId) -> HashMap<String, QoSHealth> {
        let now = Instant::now();
        let mut map: HashMap<String, QoSHealth> = HashMap::new();
        if let Some(f) = self.flows.get(flow_id) {
            for data in f.elements.values() {
                let health = data.health_at(now);
                map.entry(data.element_id.clone())
                    .and_modify(|h| {
                        if health.rank() < h.rank() {
                            *h = health;
                        }
                    })
                    .or_insert(health);
            }
        }
        map
    }

    /// Clear stats for a flow (when flow is stopped or deleted)
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        self.flows.remove(flow_id);
    }

    /// Clear stats for a graph node (when user dismisses a QoS log entry)
    pub fn clear_element(&mut self, flow_id: &FlowId, element_id: &str) {
        if let Some(flow_stats) = self.flows.get_mut(flow_id) {
            flow_stats.elements.retain(|(id, _, _), _| id != element_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn data(pad: &str, dropped: u64, events: u64, at: Instant) -> QoSElementData {
        QoSElementData {
            element_id: "mixer".into(),
            element_name: "mixer:audiomixer".into(),
            pad_name: Some(pad.into()),
            upstream_element: None,
            event_count: events,
            dropped_in_period: dropped,
            stats_format: Some("samples".into()),
            last_update: at,
        }
    }

    #[test]
    fn health_follows_drops_not_proportion() {
        assert_eq!(QoSHealth::from_drops(0, 3), QoSHealth::Ok);
        assert_eq!(QoSHealth::from_drops(1024, 1), QoSHealth::Warning);
        assert_eq!(
            QoSHealth::from_drops(40_000, CRITICAL_EVENTS_PER_PERIOD),
            QoSHealth::Critical
        );
    }

    #[test]
    fn a_report_expires_after_the_ttl() {
        let t0 = Instant::now();
        let d = data("sink_1", 1024, 40, t0);
        assert_eq!(d.health_at(t0), QoSHealth::Critical);
        assert_eq!(
            d.health_at(t0 + Duration::from_secs_f64(QOS_TTL_SECS + 1.0)),
            QoSHealth::Ok
        );
    }

    #[test]
    fn inputs_are_kept_apart_and_the_node_shows_the_worst() {
        let mut store = QoSStore::new();
        let flow = FlowId::new_v4();
        let now = Instant::now();
        store.update(flow, data("sink_0", 0, 1, now));
        store.update(flow, data("sink_1", 5000, 40, now));
        assert_eq!(store.get_problem_elements(&flow).len(), 1);
        assert_eq!(
            store.get_element_health_map(&flow).get("mixer"),
            Some(&QoSHealth::Critical)
        );
        assert_eq!(store.get_flow_health(&flow), Some(QoSHealth::Critical));
        store.clear_element(&flow, "mixer");
        assert_eq!(store.get_flow_health(&flow), None);
    }
}
