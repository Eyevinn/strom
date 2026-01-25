//! NDI source discovery using GStreamer DeviceMonitor.
//!
//! This module uses GStreamer's built-in NDI device provider to discover
//! NDI sources on the network. The NDI plugin handles all the complexity
//! of NDI discovery (mDNS, NDI SDK finder, etc.) internally.

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use utoipa::ToSchema;

/// Discovered NDI source from GStreamer DeviceMonitor.
#[derive(Debug, Clone)]
pub struct NdiSource {
    /// Unique ID for this source (hash of name).
    pub id: String,
    /// NDI source name (e.g., "HOSTNAME (Stream Name)").
    pub name: String,
    /// IP address of the NDI source (if available).
    pub ip_address: Option<String>,
    /// URL address for direct connection (if available).
    pub url_address: Option<String>,
    /// When this source was first seen.
    pub first_seen: Instant,
    /// When this source was last seen.
    pub last_seen: Instant,
}

impl NdiSource {
    /// Generate a unique ID from NDI name.
    pub fn generate_id(name: &str) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        format!("ndi-{:016x}", hasher.finish())
    }

    /// Convert to API response format.
    pub fn to_api_response(&self) -> NdiSourceResponse {
        NdiSourceResponse {
            id: self.id.clone(),
            name: self.name.clone(),
            ip_address: self.ip_address.clone(),
            url_address: self.url_address.clone(),
            first_seen_secs_ago: self.first_seen.elapsed().as_secs(),
            last_seen_secs_ago: self.last_seen.elapsed().as_secs(),
        }
    }
}

/// API response for an NDI source.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NdiSourceResponse {
    /// Unique ID for this source.
    pub id: String,
    /// NDI source name (use this for ndi-name property).
    pub name: String,
    /// IP address of the source (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    /// URL address for direct connection (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_address: Option<String>,
    /// Seconds since first discovery.
    pub first_seen_secs_ago: u64,
    /// Seconds since last seen.
    pub last_seen_secs_ago: u64,
}

/// NDI discovery service using GStreamer DeviceMonitor.
pub struct NdiDiscovery {
    /// Discovered NDI sources.
    sources: Arc<RwLock<HashMap<String, NdiSource>>>,
    /// GStreamer DeviceMonitor (stored to keep it alive).
    monitor: Option<gst::DeviceMonitor>,
    /// Whether NDI plugin is available.
    available: bool,
}

impl NdiDiscovery {
    /// Create a new NDI discovery service.
    pub fn new() -> Self {
        // Check if NDI plugin is available
        let available = Self::is_ndi_available();
        if !available {
            info!("NDI GStreamer plugin not available - NDI discovery disabled");
        }

        Self {
            sources: Arc::new(RwLock::new(HashMap::new())),
            monitor: None,
            available,
        }
    }

    /// Check if NDI GStreamer plugin is available.
    fn is_ndi_available() -> bool {
        let registry = gst::Registry::get();
        registry
            .find_feature(
                "ndideviceprovider",
                gst::DeviceProviderFactory::static_type(),
            )
            .is_some()
    }

    /// Check if NDI discovery is available.
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Start NDI discovery.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        if !self.available {
            info!("NDI discovery not started - plugin not available");
            return Ok(());
        }

        info!("Starting NDI discovery via GStreamer DeviceMonitor");

        // Create device monitor
        let monitor = gst::DeviceMonitor::new();

        // Add filter for NDI sources
        // The NDI device provider registers as Source/Network with application/x-ndi caps
        let caps = gst::Caps::builder("application/x-ndi").build();
        monitor.add_filter(Some("Source/Network"), Some(&caps));

        // Get the bus for device events
        let bus = monitor.bus();

        // Start monitoring
        if let Err(e) = monitor.start() {
            error!("Failed to start NDI device monitor: {}", e);
            return Err(anyhow::anyhow!("Failed to start NDI device monitor: {}", e));
        }

        // Do initial device enumeration
        let devices = monitor.devices();
        info!(
            "NDI discovery started, found {} initial sources",
            devices.len()
        );

        for device in devices {
            self.handle_device_added(&device).await;
        }

        // Store monitor to keep it alive
        self.monitor = Some(monitor);

        // Spawn task to handle device events
        let sources = self.sources.clone();
        tokio::spawn(async move {
            Self::run_event_loop(bus, sources).await;
        });

        Ok(())
    }

    /// Stop NDI discovery.
    pub async fn stop(&mut self) {
        if let Some(monitor) = self.monitor.take() {
            info!("Stopping NDI discovery");
            monitor.stop();
        }

        // Clear sources
        let mut sources = self.sources.write().await;
        sources.clear();
    }

    /// Get all discovered NDI sources.
    pub async fn get_sources(&self) -> Vec<NdiSource> {
        let sources = self.sources.read().await;
        sources.values().cloned().collect()
    }

    /// Get a specific NDI source by ID.
    pub async fn get_source(&self, id: &str) -> Option<NdiSource> {
        let sources = self.sources.read().await;
        sources.get(id).cloned()
    }

    /// Get NDI source by name.
    pub async fn get_source_by_name(&self, name: &str) -> Option<NdiSource> {
        let sources = self.sources.read().await;
        sources.values().find(|s| s.name == name).cloned()
    }

    /// Handle a device added event.
    async fn handle_device_added(&self, device: &gst::Device) {
        let display_name = device.display_name().to_string();
        let device_class = device.device_class().to_string();

        debug!(
            "NDI device added: {} (class: {})",
            display_name, device_class
        );

        // Extract properties from device
        let mut ip_address = None;
        let mut url_address = None;

        if let Some(props) = device.properties() {
            // Try to get IP address
            if let Ok(ip) = props.get::<String>("ip") {
                ip_address = Some(ip);
            }
            // Try to get URL address
            if let Ok(url) = props.get::<String>("url-address") {
                url_address = Some(url);
            }
            // The ndi-name is typically the display name
            debug!("NDI device properties: {:?}", props);
        }

        let id = NdiSource::generate_id(&display_name);
        let now = Instant::now();

        let source = NdiSource {
            id: id.clone(),
            name: display_name.clone(),
            ip_address,
            url_address,
            first_seen: now,
            last_seen: now,
        };

        let mut sources = self.sources.write().await;
        let is_new = !sources.contains_key(&id);

        if is_new {
            info!("Discovered new NDI source: {}", display_name);
        } else {
            debug!("Updated NDI source: {}", display_name);
        }

        // Update or insert
        sources
            .entry(id)
            .and_modify(|s| s.last_seen = now)
            .or_insert(source);
    }

    /// Handle a device removed event.
    async fn handle_device_removed(
        sources: &Arc<RwLock<HashMap<String, NdiSource>>>,
        device: &gst::Device,
    ) {
        let display_name = device.display_name().to_string();
        let id = NdiSource::generate_id(&display_name);

        let mut sources = sources.write().await;
        if sources.remove(&id).is_some() {
            info!("NDI source removed: {}", display_name);
        }
    }

    /// Run the event loop for device monitor bus messages.
    async fn run_event_loop(bus: gst::Bus, sources: Arc<RwLock<HashMap<String, NdiSource>>>) {
        loop {
            // Use timeout to allow checking for shutdown
            let msg = match bus.timed_pop(gst::ClockTime::from_mseconds(500)) {
                Some(msg) => msg,
                None => continue,
            };

            match msg.view() {
                gst::MessageView::DeviceAdded(device_added) => {
                    let device = device_added.device();
                    let display_name = device.display_name().to_string();
                    let device_class = device.device_class().to_string();

                    debug!(
                        "NDI device added event: {} (class: {})",
                        display_name, device_class
                    );

                    // Extract properties
                    let mut ip_address = None;
                    let mut url_address = None;

                    if let Some(props) = device.properties() {
                        if let Ok(ip) = props.get::<String>("ip") {
                            ip_address = Some(ip);
                        }
                        if let Ok(url) = props.get::<String>("url-address") {
                            url_address = Some(url);
                        }
                    }

                    let id = NdiSource::generate_id(&display_name);
                    let now = Instant::now();

                    let source = NdiSource {
                        id: id.clone(),
                        name: display_name.clone(),
                        ip_address,
                        url_address,
                        first_seen: now,
                        last_seen: now,
                    };

                    let mut sources_guard = sources.write().await;
                    let is_new = !sources_guard.contains_key(&id);

                    if is_new {
                        info!("Discovered new NDI source: {}", display_name);
                    }

                    sources_guard
                        .entry(id)
                        .and_modify(|s| s.last_seen = now)
                        .or_insert(source);
                }
                gst::MessageView::DeviceRemoved(device_removed) => {
                    let device = device_removed.device();
                    Self::handle_device_removed(&sources, &device).await;
                }
                // Note: DeviceChanged is not commonly used for NDI,
                // and the GStreamer Rust bindings API varies by version.
                // We rely on DeviceAdded/DeviceRemoved for updates.
                _ => {}
            }
        }
    }

    /// Refresh NDI sources by re-querying the device monitor.
    pub async fn refresh(&self) {
        if let Some(monitor) = &self.monitor {
            let devices = monitor.devices();
            debug!("Refreshing NDI sources, found {} devices", devices.len());

            for device in devices {
                self.handle_device_added(&device).await;
            }
        }
    }
}

impl Default for NdiDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ndi_source_generate_id() {
        let id1 = NdiSource::generate_id("HOSTNAME (Stream)");
        let id2 = NdiSource::generate_id("HOSTNAME (Stream)");
        let id3 = NdiSource::generate_id("OTHER (Stream)");

        // Same name should produce same ID
        assert_eq!(id1, id2);
        // Different name should produce different ID
        assert_ne!(id1, id3);
        // ID should start with "ndi-"
        assert!(id1.starts_with("ndi-"));
    }

    #[test]
    fn test_ndi_source_to_api_response() {
        let source = NdiSource {
            id: "ndi-123".to_string(),
            name: "TEST (Source)".to_string(),
            ip_address: Some("192.168.1.100".to_string()),
            url_address: None,
            first_seen: Instant::now(),
            last_seen: Instant::now(),
        };

        let response = source.to_api_response();
        assert_eq!(response.id, "ndi-123");
        assert_eq!(response.name, "TEST (Source)");
        assert_eq!(response.ip_address, Some("192.168.1.100".to_string()));
        assert!(response.url_address.is_none());
    }
}
