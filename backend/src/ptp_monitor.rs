//! PTP clock monitoring service.
//!
//! This module provides centralized PTP clock monitoring that is independent of pipeline
//! lifecycle. In GStreamer, PTP clocks are shared resources - there's only one PTP clock
//! instance per domain, regardless of how many pipelines use it.
//!
//! The monitor:
//! - Initializes PTP once when first needed
//! - Registers a single global statistics callback
//! - Tracks stats for all active domains
//! - Broadcasts stats via the event system

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

use gstreamer::glib;
use gstreamer::prelude::ClockExt;
use gstreamer_net as gst_net;

use crate::events::EventBroadcaster;
use strom_types::{FlowId, StromEvent};

/// Statistics for a single PTP domain.
#[derive(Debug, Clone, Default)]
pub struct PtpDomainStats {
    /// PTP domain number
    pub domain: u8,
    /// Whether the clock is synchronized
    pub synced: bool,
    /// Mean path delay to master in nanoseconds
    pub mean_path_delay_ns: Option<u64>,
    /// Clock offset/correction in nanoseconds
    pub clock_offset_ns: Option<i64>,
    /// R-squared (clock estimation quality, 0.0-1.0)
    pub r_squared: Option<f64>,
    /// Clock rate ratio
    pub clock_rate: Option<f64>,
    /// Last update timestamp (Unix seconds)
    pub last_update: u64,
    /// Grandmaster clock ID (if available)
    pub grandmaster_id: Option<u64>,
    /// Master clock ID (if available)
    pub master_id: Option<u64>,
}

/// Inner state for the PTP monitor (shared across threads).
/// Uses std::sync::RwLock because it's accessed from both Tokio tasks and GLib callbacks.
struct PtpMonitorInner {
    /// PTP initialization state
    initialized: bool,
    /// Statistics per domain
    domain_stats: HashMap<u8, PtpDomainStats>,
    /// PTP clock instances per domain (for querying sync status)
    clocks: HashMap<u8, gst_net::PtpClock>,
    /// Flows interested in each domain (flow_id -> domain)
    flow_domains: HashMap<FlowId, u8>,
    /// Event broadcaster for sending stats
    event_broadcaster: Option<Arc<EventBroadcaster>>,
}

/// Centralized PTP clock monitoring service.
///
/// This service manages PTP clock monitoring independently of pipeline lifecycle.
/// It should be created once at application startup and shared across all flows.
pub struct PtpMonitor {
    /// Inner state - uses std::sync::RwLock for GLib callback compatibility
    inner: Arc<RwLock<PtpMonitorInner>>,
    /// Statistics callback handle (must be kept alive)
    #[allow(dead_code)]
    stats_callback: Arc<std::sync::Mutex<Option<gst_net::PtpStatisticsCallback>>>,
}

impl PtpMonitor {
    /// Create a new PTP monitor.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(PtpMonitorInner {
                initialized: false,
                domain_stats: HashMap::new(),
                clocks: HashMap::new(),
                flow_domains: HashMap::new(),
                event_broadcaster: None,
            })),
            stats_callback: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Set the event broadcaster for sending PTP stats events.
    pub fn set_event_broadcaster(&self, broadcaster: Arc<EventBroadcaster>) {
        let mut inner = self.inner.write().unwrap();
        inner.event_broadcaster = Some(broadcaster);
    }

    /// Initialize PTP if not already initialized.
    fn ensure_initialized(&self) -> Result<(), String> {
        let mut inner = self.inner.write().unwrap();

        if inner.initialized {
            return Ok(());
        }

        // Initialize PTP globally (listens on all interfaces)
        match gst_net::PtpClock::init(None, &[]) {
            Ok(_) => {
                info!("PTP clock system initialized");
            }
            Err(e) => {
                // This is often just "already initialized" which is fine
                debug!(
                    "PTP clock init returned: {} (may already be initialized)",
                    e
                );
            }
        }

        inner.initialized = true;
        drop(inner);

        // Register global statistics callback
        self.register_stats_callback();

        Ok(())
    }

    /// Register the global PTP statistics callback.
    fn register_stats_callback(&self) {
        let inner = self.inner.clone();

        let callback = gst_net::PtpClock::add_statistics_callback(move |domain, stats| {
            let name = stats.name();

            // Only process TIME_UPDATED events (they have the fields we want)
            if name == "GstPtpStatisticsTimeUpdated" {
                let mean_path_delay_ns = stats.get::<u64>("mean-path-delay-avg").ok();
                let clock_offset_ns = stats.get::<i64>("discontinuity").ok();
                let r_squared = stats.get::<f64>("r-squared").ok();
                let clock_rate = stats.get::<f64>("rate").ok();

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                // Update stats synchronously (we're in a GLib callback, using std::sync::RwLock)
                if let Ok(mut guard) = inner.write() {
                    // Get sync status from clock first (if we have one)
                    let clock_info = guard.clocks.get(&domain).map(|clock| {
                        (
                            clock.is_synced(),
                            clock.grandmaster_clock_id(),
                            clock.master_clock_id(),
                        )
                    });

                    // Get or create stats for this domain
                    let domain_stats =
                        guard
                            .domain_stats
                            .entry(domain)
                            .or_insert_with(|| PtpDomainStats {
                                domain,
                                ..Default::default()
                            });

                    // Update the stats
                    domain_stats.mean_path_delay_ns = mean_path_delay_ns;
                    domain_stats.clock_offset_ns = clock_offset_ns;
                    domain_stats.r_squared = r_squared;
                    domain_stats.clock_rate = clock_rate;
                    domain_stats.last_update = now;

                    // Update sync status from clock if we have one
                    if let Some((synced, grandmaster_id, master_id)) = clock_info {
                        domain_stats.synced = synced;
                        domain_stats.grandmaster_id = Some(grandmaster_id);
                        domain_stats.master_id = Some(master_id);
                    }

                    debug!(
                        "PTP stats updated for domain {}: synced={}, offset={:?}ns, r²={:?}",
                        domain,
                        domain_stats.synced,
                        domain_stats.clock_offset_ns,
                        domain_stats.r_squared
                    );
                }
            }

            glib::ControlFlow::Continue
        });

        let mut guard = self.stats_callback.lock().unwrap();
        *guard = Some(callback);
        info!("Global PTP statistics callback registered");
    }

    /// Register a flow's interest in a PTP domain.
    ///
    /// This should be called when a flow is created or updated with PTP configuration.
    /// The monitor will start tracking that domain.
    pub fn register_flow(&self, flow_id: FlowId, domain: u8) -> Result<(), String> {
        self.ensure_initialized()?;

        let mut inner = self.inner.write().unwrap();

        // Check if this flow was already registered for a different domain
        if let Some(old_domain) = inner.flow_domains.get(&flow_id) {
            if *old_domain == domain {
                return Ok(()); // Already registered for this domain
            }
            // Flow changed domains - will be handled by unregister + register
        }

        inner.flow_domains.insert(flow_id, domain);

        // Create a clock for this domain if we don't have one
        if !inner.clocks.contains_key(&domain) {
            match gst_net::PtpClock::new(None, domain as u32) {
                Ok(clock) => {
                    info!(
                        "Created PTP clock for domain {} (synced: {})",
                        domain,
                        clock.is_synced()
                    );

                    // Initialize stats for this domain
                    let stats = PtpDomainStats {
                        domain,
                        synced: clock.is_synced(),
                        grandmaster_id: Some(clock.grandmaster_clock_id()),
                        master_id: Some(clock.master_clock_id()),
                        last_update: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                        ..Default::default()
                    };
                    inner.domain_stats.insert(domain, stats);
                    inner.clocks.insert(domain, clock);
                }
                Err(e) => {
                    warn!("Failed to create PTP clock for domain {}: {}", domain, e);
                    // Still track the flow, stats will update when available
                }
            }
        }

        info!(
            "Flow {} registered for PTP domain {} ({} flows using this domain)",
            flow_id,
            domain,
            inner
                .flow_domains
                .values()
                .filter(|d| **d == domain)
                .count()
        );

        Ok(())
    }

    /// Unregister a flow's interest in PTP.
    ///
    /// This should be called when a flow is deleted or PTP is disabled.
    pub fn unregister_flow(&self, flow_id: FlowId) {
        let mut inner = self.inner.write().unwrap();

        if let Some(domain) = inner.flow_domains.remove(&flow_id) {
            // Check if any other flows still use this domain
            let domain_still_used = inner.flow_domains.values().any(|d| *d == domain);

            if !domain_still_used {
                // No more flows using this domain - we could clean up the clock
                // But keeping it alive doesn't hurt and avoids re-initialization
                info!(
                    "No more flows using PTP domain {}, but keeping clock alive",
                    domain
                );
            }

            info!("Flow {} unregistered from PTP domain {}", flow_id, domain);
        }
    }

    /// Get current stats for all monitored domains.
    pub fn get_all_stats(&self) -> Vec<PtpDomainStats> {
        let inner = self.inner.read().unwrap();

        // Update sync status from clocks before returning
        let mut stats: Vec<PtpDomainStats> = inner.domain_stats.values().cloned().collect();

        for stat in &mut stats {
            if let Some(clock) = inner.clocks.get(&stat.domain) {
                stat.synced = clock.is_synced();
                stat.grandmaster_id = Some(clock.grandmaster_clock_id());
                stat.master_id = Some(clock.master_clock_id());
            }
        }

        stats
    }

    /// Get stats for a specific domain.
    pub fn get_domain_stats(&self, domain: u8) -> Option<PtpDomainStats> {
        let inner = self.inner.read().unwrap();

        inner.domain_stats.get(&domain).cloned().map(|mut stats| {
            // Update sync status from clock
            if let Some(clock) = inner.clocks.get(&domain) {
                stats.synced = clock.is_synced();
                stats.grandmaster_id = Some(clock.grandmaster_clock_id());
                stats.master_id = Some(clock.master_clock_id());
            }
            stats
        })
    }

    /// Get the set of domains that are currently being monitored.
    pub fn get_monitored_domains(&self) -> HashSet<u8> {
        let inner = self.inner.read().unwrap();
        inner.flow_domains.values().copied().collect()
    }

    /// Get which flows are using each domain.
    pub fn get_domain_flows(&self) -> HashMap<u8, Vec<FlowId>> {
        let inner = self.inner.read().unwrap();
        let mut result: HashMap<u8, Vec<FlowId>> = HashMap::new();

        for (flow_id, domain) in &inner.flow_domains {
            result.entry(*domain).or_default().push(*flow_id);
        }

        result
    }

    /// Get PTP stats events for broadcasting via WebSocket.
    ///
    /// Returns events for all monitored domains.
    pub fn get_stats_events(&self) -> Vec<StromEvent> {
        let stats = self.get_all_stats();
        let domain_flows = self.get_domain_flows();

        let mut events = Vec::new();

        for domain_stat in stats {
            // Get flows using this domain
            let flows = domain_flows
                .get(&domain_stat.domain)
                .cloned()
                .unwrap_or_default();

            // Emit one event per flow (for backward compatibility with frontend)
            // TODO: Consider changing to domain-based events in the future
            for flow_id in flows {
                events.push(StromEvent::PtpStats {
                    flow_id,
                    domain: domain_stat.domain,
                    synced: domain_stat.synced,
                    mean_path_delay_ns: domain_stat.mean_path_delay_ns,
                    clock_offset_ns: domain_stat.clock_offset_ns,
                    r_squared: domain_stat.r_squared,
                    clock_rate: domain_stat.clock_rate,
                    grandmaster_id: domain_stat.grandmaster_id,
                    master_id: domain_stat.master_id,
                });
            }
        }

        events
    }

    /// Check if a specific domain is synchronized.
    pub fn is_domain_synced(&self, domain: u8) -> bool {
        let inner = self.inner.read().unwrap();
        inner
            .clocks
            .get(&domain)
            .map(|c| c.is_synced())
            .unwrap_or(false)
    }

    /// Get the PTP clock for a domain (for pipelines to use).
    pub fn get_clock(&self, domain: u8) -> Option<gst_net::PtpClock> {
        let inner = self.inner.read().unwrap();
        inner.clocks.get(&domain).cloned()
    }
}

impl Default for PtpMonitor {
    fn default() -> Self {
        Self::new()
    }
}
