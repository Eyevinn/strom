//! QoS (Quality of Service) statistics aggregation.
//!
//! This module provides types for collecting and aggregating QoS events from GStreamer
//! elements. QoS events indicate when elements are falling behind (proportion < 1.0)
//! or keeping up with real-time processing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Aggregated QoS statistics for a single element.
#[derive(Debug, Clone)]
pub(super) struct ElementQoSStats {
    pub event_count: u64,
    sum_proportion: f64,
    pub min_proportion: f64,
    pub max_proportion: f64,
    sum_jitter: i64,
    pub total_processed: u64,
}

impl ElementQoSStats {
    fn new() -> Self {
        Self {
            event_count: 0,
            sum_proportion: 0.0,
            min_proportion: f64::MAX,
            max_proportion: f64::MIN,
            sum_jitter: 0,
            total_processed: 0,
        }
    }

    fn add_event(&mut self, proportion: f64, jitter: i64, processed: u64) {
        self.event_count += 1;
        self.sum_proportion += proportion;
        self.min_proportion = self.min_proportion.min(proportion);
        self.max_proportion = self.max_proportion.max(proportion);
        self.sum_jitter += jitter;
        self.total_processed = processed; // Keep the latest value
    }

    pub fn avg_proportion(&self) -> f64 {
        if self.event_count > 0 {
            self.sum_proportion / self.event_count as f64
        } else {
            0.0
        }
    }

    pub fn avg_jitter(&self) -> i64 {
        if self.event_count > 0 {
            self.sum_jitter / self.event_count as i64
        } else {
            0
        }
    }
}

/// QoS statistics aggregator (collects QoS events and broadcasts periodically).
#[derive(Debug, Clone)]
pub(super) struct QoSAggregator {
    stats: Arc<Mutex<HashMap<String, ElementQoSStats>>>,
}

impl QoSAggregator {
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn add_event(&self, element_name: String, proportion: f64, jitter: i64, processed: u64) {
        let mut stats = self.stats.lock().unwrap();
        stats
            .entry(element_name)
            .or_insert_with(ElementQoSStats::new)
            .add_event(proportion, jitter, processed);
    }

    pub fn extract_and_reset(&self) -> HashMap<String, ElementQoSStats> {
        let mut stats = self.stats.lock().unwrap();
        std::mem::take(&mut *stats)
    }
}
