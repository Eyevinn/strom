//! Thread CPU monitoring for GStreamer streaming threads.

use std::collections::{HashMap, VecDeque};
use strom_types::{FlowId, ThreadCpuStats, ThreadStats};

const HISTORY_SIZE: usize = 60; // Keep 60 seconds of history

/// History and latest data for a single thread.
#[derive(Clone)]
pub struct ThreadHistory {
    /// CPU usage history (0-100%)
    pub cpu_history: VecDeque<f32>,
    /// Latest stats for this thread
    pub latest: Option<ThreadCpuStats>,
}

impl Default for ThreadHistory {
    fn default() -> Self {
        Self {
            cpu_history: VecDeque::with_capacity(HISTORY_SIZE),
            latest: None,
        }
    }
}

impl ThreadHistory {
    /// Update with new CPU usage value.
    fn update(&mut self, stats: ThreadCpuStats) {
        self.cpu_history.push_back(stats.cpu_usage);
        if self.cpu_history.len() > HISTORY_SIZE {
            self.cpu_history.pop_front();
        }
        self.latest = Some(stats);
    }
}

/// Data store for thread CPU monitoring statistics.
#[derive(Clone, Default)]
pub struct ThreadMonitorStore {
    /// Thread histories indexed by thread ID
    threads: HashMap<u64, ThreadHistory>,
    /// Thread IDs indexed by flow ID (for filtering)
    by_flow: HashMap<FlowId, Vec<u64>>,
    /// Last update timestamp
    last_timestamp: i64,
}

impl ThreadMonitorStore {
    /// Create a new thread monitor store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with new thread statistics.
    pub fn update(&mut self, stats: ThreadStats) {
        self.last_timestamp = stats.timestamp;

        // Track which threads we've seen in this update
        let mut seen_thread_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();

        // Update by_flow mapping
        self.by_flow.clear();

        for thread_stats in stats.threads {
            let thread_id = thread_stats.thread_id;
            let flow_id = thread_stats.flow_id;

            seen_thread_ids.insert(thread_id);

            // Update thread history
            self.threads
                .entry(thread_id)
                .or_default()
                .update(thread_stats);

            // Update flow -> threads mapping
            self.by_flow.entry(flow_id).or_default().push(thread_id);
        }

        // Remove threads that are no longer active
        self.threads.retain(|id, _| seen_thread_ids.contains(id));
    }

    /// Get all threads sorted by CPU usage (highest first).
    pub fn get_sorted_threads(&self) -> Vec<&ThreadHistory> {
        let mut threads: Vec<&ThreadHistory> = self.threads.values().collect();
        threads.sort_by(|a, b| {
            let a_cpu = a.latest.as_ref().map(|s| s.cpu_usage).unwrap_or(0.0);
            let b_cpu = b.latest.as_ref().map(|s| s.cpu_usage).unwrap_or(0.0);
            b_cpu
                .partial_cmp(&a_cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        threads
    }

    /// Get threads for a specific flow, sorted by CPU usage.
    pub fn get_threads_for_flow(&self, flow_id: &FlowId) -> Vec<&ThreadHistory> {
        let Some(thread_ids) = self.by_flow.get(flow_id) else {
            return Vec::new();
        };

        let mut threads: Vec<&ThreadHistory> = thread_ids
            .iter()
            .filter_map(|id| self.threads.get(id))
            .collect();

        threads.sort_by(|a, b| {
            let a_cpu = a.latest.as_ref().map(|s| s.cpu_usage).unwrap_or(0.0);
            let b_cpu = b.latest.as_ref().map(|s| s.cpu_usage).unwrap_or(0.0);
            b_cpu
                .partial_cmp(&a_cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        threads
    }

    /// Get the total number of active threads.
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    /// Get total CPU usage across all threads.
    pub fn total_cpu_usage(&self) -> f32 {
        self.threads
            .values()
            .filter_map(|h| h.latest.as_ref())
            .map(|s| s.cpu_usage)
            .sum()
    }

    /// Check if there are any threads being monitored.
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// Get the last update timestamp.
    pub fn last_timestamp(&self) -> i64 {
        self.last_timestamp
    }
}
