//! Buffer age monitoring data store.
//!
//! Tracks buffer age warnings and manual probe data for display in the UI.

use instant::Instant;
use std::collections::HashMap;
use strom_types::FlowId;

/// TTL for buffer age warnings: if no new warning arrives within this period,
/// the health status returns to Ok.
const WARNING_TTL_SECS: f64 = 15.0;

/// Buffer age health status for an element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferAgeHealth {
    /// No buffer age issues
    Ok,
    /// Buffer age exceeds threshold (warning level)
    Warning,
    /// Buffer age is critically high (>2x threshold)
    Critical,
}

impl BufferAgeHealth {
    pub fn from_age(age_ms: u64, threshold_ms: u64) -> Self {
        if age_ms >= threshold_ms * 2 {
            BufferAgeHealth::Critical
        } else if age_ms >= threshold_ms {
            BufferAgeHealth::Warning
        } else {
            BufferAgeHealth::Ok
        }
    }

    pub fn color(&self) -> egui::Color32 {
        match self {
            BufferAgeHealth::Ok => egui::Color32::from_rgb(0, 200, 0),
            BufferAgeHealth::Warning => egui::Color32::from_rgb(255, 165, 0),
            BufferAgeHealth::Critical => egui::Color32::from_rgb(255, 50, 50),
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            BufferAgeHealth::Ok => egui_phosphor::regular::CHECK,
            BufferAgeHealth::Warning => egui_phosphor::regular::CLOCK,
            BufferAgeHealth::Critical => egui_phosphor::regular::CLOCK_COUNTDOWN,
        }
    }
}

/// A buffer age warning entry for an element+pad.
#[derive(Debug, Clone)]
pub struct BufferAgeWarningEntry {
    pub age_ms: u64,
    pub threshold_ms: u64,
    pub last_seen: Instant,
}

/// Data from a manual buffer age probe.
#[derive(Debug, Clone)]
pub struct ProbeData {
    pub probe_id: String,
    pub element_id: String,
    pub pad_name: String,
    pub current_age_ms: u64,
    pub max_age_ms: u64,
    pub sum_age_ms: u64,
    pub sample_count: u64,
    pub last_update: Instant,
}

impl ProbeData {
    pub fn avg_age_ms(&self) -> u64 {
        self.sum_age_ms.checked_div(self.sample_count).unwrap_or(0)
    }
}

/// Data store for buffer age warnings and probes.
#[derive(Clone, Default)]
pub struct BufferAgeStore {
    /// Automatic monitor warnings keyed by (flow_id, element_id)
    warnings: HashMap<(FlowId, String), BufferAgeWarningEntry>,
    /// Manual probe data keyed by probe_id
    probes: HashMap<String, ProbeData>,
}

impl BufferAgeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with a buffer age warning from the monitor.
    pub fn update_warning(
        &mut self,
        flow_id: FlowId,
        element_id: String,
        age_ms: u64,
        threshold_ms: u64,
    ) {
        let key = (flow_id, element_id);
        self.warnings.insert(
            key,
            BufferAgeWarningEntry {
                age_ms,
                threshold_ms,
                last_seen: Instant::now(),
            },
        );
    }

    /// Update with a probe measurement.
    pub fn update_probe(
        &mut self,
        probe_id: String,
        element_id: String,
        pad_name: String,
        age_ms: u64,
    ) {
        let entry = self
            .probes
            .entry(probe_id.clone())
            .or_insert_with(|| ProbeData {
                probe_id,
                element_id: element_id.clone(),
                pad_name: pad_name.clone(),
                current_age_ms: 0,
                max_age_ms: 0,
                sum_age_ms: 0,
                sample_count: 0,
                last_update: Instant::now(),
            });
        entry.current_age_ms = age_ms;
        entry.max_age_ms = entry.max_age_ms.max(age_ms);
        entry.sum_age_ms += age_ms;
        entry.sample_count += 1;
        entry.last_update = Instant::now();
    }

    /// Record probe activation.
    pub fn probe_activated(&mut self, probe_id: String, element_id: String, pad_name: String) {
        self.probes.insert(
            probe_id.clone(),
            ProbeData {
                probe_id,
                element_id,
                pad_name,
                current_age_ms: 0,
                max_age_ms: 0,
                sum_age_ms: 0,
                sample_count: 0,
                last_update: Instant::now(),
            },
        );
    }

    /// Record probe deactivation.
    pub fn probe_deactivated(&mut self, probe_id: &str) {
        self.probes.remove(probe_id);
    }

    /// Build a health map for all elements with active warnings in a flow.
    pub fn get_element_health_map(&self, flow_id: &FlowId) -> HashMap<String, BufferAgeHealth> {
        let now = Instant::now();
        self.warnings
            .iter()
            .filter(|((fid, _), entry)| {
                fid == flow_id && (now - entry.last_seen).as_secs_f64() <= WARNING_TTL_SECS
            })
            .map(|((_, eid), entry)| {
                (
                    eid.clone(),
                    BufferAgeHealth::from_age(entry.age_ms, entry.threshold_ms),
                )
            })
            .filter(|(_, health)| *health != BufferAgeHealth::Ok)
            .collect()
    }

    /// Get active probes for an element.
    pub fn get_probes_for_element(&self, element_id: &str) -> Vec<&ProbeData> {
        self.probes
            .values()
            .filter(|p| p.element_id == element_id)
            .collect()
    }

    /// Clear all data for a flow (when flow is stopped or deleted).
    pub fn clear_flow(&mut self, flow_id: &FlowId) {
        self.warnings.retain(|(fid, _), _| fid != flow_id);
    }
}
