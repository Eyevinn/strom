//! Generic device discovery using GStreamer DeviceMonitor.
//!
//! This module uses GStreamer's DeviceMonitor to discover various types of
//! devices including audio devices, video devices, and network sources like NDI.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Sync, shareable map of discovery-id → live `gst::Device` handle.
/// Mirrored alongside `DiscoveredDevice` so block builders (which run on
/// blocking threads where we can't await an async lock) can resolve a
/// device id to the underlying `GstDevice` without starting a transient
/// `DeviceMonitor` of their own — that was crashing on macOS in
/// `gst_device_provider_stop`.
pub type GstDeviceMap = Arc<Mutex<HashMap<String, gst::Device>>>;

pub use strom_types::discovery::{DeviceCategory, DeviceResponse};

/// Discovered device from GStreamer DeviceMonitor.
#[derive(Debug, Clone)]
pub struct DiscoveredDevice {
    /// Unique ID for this device.
    pub id: String,
    /// Display name of the device.
    pub name: String,
    /// Device class (e.g., "Audio/Source", "Video/Source", "Source/Network").
    pub device_class: String,
    /// Device category.
    pub category: DeviceCategory,
    /// Provider that discovered this device (e.g., "pulsedeviceprovider", "ndideviceprovider").
    pub provider: String,
    /// Additional properties from the device.
    pub properties: HashMap<String, String>,
    /// When this device was first seen.
    pub first_seen: Instant,
    /// When this device was last seen.
    pub last_seen: Instant,
}

impl DiscoveredDevice {
    /// Generate a unique ID from device class, provider, and name.
    pub fn generate_id(device_class: &str, provider: &str, name: &str) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        device_class.hash(&mut hasher);
        provider.hash(&mut hasher);
        name.hash(&mut hasher);
        format!("dev-{:016x}", hasher.finish())
    }

    /// Convert to API response format.
    pub fn to_api_response(&self) -> DeviceResponse {
        DeviceResponse {
            id: self.id.clone(),
            name: self.name.clone(),
            device_class: self.device_class.clone(),
            category: self.category,
            provider: self.provider.clone(),
            properties: self.properties.clone(),
            first_seen_secs_ago: self.first_seen.elapsed().as_secs(),
            last_seen_secs_ago: self.last_seen.elapsed().as_secs(),
        }
    }

    /// Check if this is an NDI device.
    pub fn is_ndi(&self) -> bool {
        // Check provider name or NDI-specific property
        self.provider.contains("ndi") || self.properties.contains_key("ndi-name")
    }
}

/// Device discovery service using GStreamer DeviceMonitor.
pub struct DeviceDiscovery {
    /// Discovered devices (metadata, serialisable via API).
    devices: Arc<RwLock<HashMap<String, DiscoveredDevice>>>,
    /// Live `gst::Device` handles keyed by the same id. Kept in a sync
    /// `Mutex` so block builders running on blocking threads can do a
    /// non-async lookup. Shared with the event loop so add/remove keeps
    /// it in step with `devices`.
    gst_devices: GstDeviceMap,
    /// GStreamer DeviceMonitor.
    monitor: Option<gst::DeviceMonitor>,
    /// Shutdown flag for the event loop.
    shutdown: Arc<AtomicBool>,
    /// Whether discovery is running.
    running: bool,
}

impl DeviceDiscovery {
    /// Create a new device discovery service.
    pub fn new() -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            gst_devices: Arc::new(Mutex::new(HashMap::new())),
            monitor: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            running: false,
        }
    }

    /// Get a shared handle to the live `gst::Device` map.
    /// Block builders clone this and read from it synchronously.
    pub fn gst_device_map(&self) -> GstDeviceMap {
        self.gst_devices.clone()
    }

    /// Look up a live `gst::Device` by its discovery id.
    pub fn get_gst_device(&self, id: &str) -> Option<gst::Device> {
        self.gst_devices.lock().ok()?.get(id).cloned()
    }

    /// Check if a specific device provider is available.
    pub fn is_provider_available(provider_name: &str) -> bool {
        let registry = gst::Registry::get();
        registry
            .find_feature(provider_name, gst::DeviceProviderFactory::static_type())
            .is_some()
    }

    /// Check if NDI device provider is available.
    pub fn is_ndi_available() -> bool {
        Self::is_provider_available("ndideviceprovider")
    }

    /// Check if discovery is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Start device discovery for all device types.
    pub async fn start(&mut self) -> anyhow::Result<()> {
        if self.running {
            return Ok(());
        }

        info!("Starting device discovery via GStreamer DeviceMonitor");

        // Create device monitor
        let monitor = gst::DeviceMonitor::new();

        // Add filters for device types we care about.
        // Audio/Source + Video/Source are needed by the Camera/Mic Input block
        // to populate its device picker. On Windows the underlying providers
        // (WASAPI, DirectSound, MF) do hardware probing on enumeration which
        // adds a small one-time cost at startup; this is the trade-off for
        // letting users pick local capture devices from the UI.
        monitor.add_filter(Some("Audio/Source"), None);
        monitor.add_filter(Some("Video/Source"), None);
        monitor.add_filter(Some("Source/Network"), None);

        // Get the bus for device events
        let bus = monitor.bus();

        // Start monitoring
        if let Err(e) = monitor.start() {
            return Err(anyhow::anyhow!("Failed to start device monitor: {}", e));
        }

        // Do initial device enumeration
        let initial_devices = monitor.devices();
        info!(
            "Device discovery started, found {} initial devices",
            initial_devices.len()
        );

        for device in initial_devices {
            self.handle_device_added(&device).await;
        }

        // Store monitor
        self.monitor = Some(monitor);
        self.running = true;

        // Spawn task to handle device events
        let devices = self.devices.clone();
        let gst_devices = self.gst_devices.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            Self::run_event_loop(bus, devices, gst_devices, shutdown).await;
        });

        Ok(())
    }

    /// Stop device discovery.
    pub async fn stop(&mut self) {
        if !self.running {
            return;
        }

        // Signal the event loop to stop
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(monitor) = self.monitor.take() {
            info!("Stopping device discovery");
            monitor.stop();
        }

        self.running = false;

        // Clear devices
        let mut devices = self.devices.write().await;
        devices.clear();
        if let Ok(mut gst_devices) = self.gst_devices.lock() {
            gst_devices.clear();
        }
    }

    /// Get all discovered devices.
    pub async fn get_devices(&self) -> Vec<DiscoveredDevice> {
        let devices = self.devices.read().await;
        devices.values().cloned().collect()
    }

    /// Get devices filtered by category.
    pub async fn get_devices_by_category(&self, category: DeviceCategory) -> Vec<DiscoveredDevice> {
        let devices = self.devices.read().await;
        devices
            .values()
            .filter(|d| d.category == category)
            .cloned()
            .collect()
    }

    /// Get NDI devices specifically.
    pub async fn get_ndi_devices(&self) -> Vec<DiscoveredDevice> {
        let devices = self.devices.read().await;
        devices.values().filter(|d| d.is_ndi()).cloned().collect()
    }

    /// Get a specific device by ID.
    pub async fn get_device(&self, id: &str) -> Option<DiscoveredDevice> {
        let devices = self.devices.read().await;
        devices.get(id).cloned()
    }

    /// Compute the discovery ID for a `gst::Device` using the same hash inputs
    /// (device class, provider, display name) as `DiscoveredDevice::generate_id`.
    /// Used by block builders to match a user-selected device id back to the
    /// underlying `gst::Device` returned from a `gst::DeviceMonitor`.
    pub fn device_id_for(device: &gst::Device) -> String {
        let class = device.device_class().to_string();
        let provider = Self::get_provider_name(device, &class);
        let name = device.display_name().to_string();
        DiscoveredDevice::generate_id(&class, &provider, &name)
    }

    /// Get provider name from device properties or infer from device class.
    fn get_provider_name(device: &gst::Device, device_class: &str) -> String {
        if let Some(props) = device.properties() {
            // Try to get from device.api property
            if let Ok(api) = props.get::<String>("device.api") {
                return format!("{}provider", api.to_lowercase());
            }

            // Check for NDI-specific property
            if props.get::<String>("ndi-name").is_ok() {
                return "ndideviceprovider".to_string();
            }
        }

        // Infer from device class (fallback)
        match device_class {
            "Audio/Source" | "Audio/Sink" => "pulsedeviceprovider".to_string(),
            "Video/Source" => "v4l2deviceprovider".to_string(),
            // Don't assume Source/Network is NDI - could be other network protocols
            _ => "unknown".to_string(),
        }
    }

    /// Handle a device added event.
    async fn handle_device_added(&self, device: &gst::Device) {
        let display_name = device.display_name().to_string();
        let device_class = device.device_class().to_string();
        let category = DeviceCategory::from_device_class(&device_class);

        // Try to infer provider from device properties or class
        let provider = Self::get_provider_name(device, &device_class);

        debug!(
            "Device added: {} (class: {}, provider: {})",
            display_name, device_class, provider
        );

        // Extract all properties from device
        let mut properties = HashMap::new();
        if let Some(props) = device.properties() {
            // Common properties we're interested in
            let prop_names = [
                "device.api",
                "device.class",
                "device.name",
                "device.description",
                "device.nick",
                "device.icon_name",
                "device.path",
                "device.serial",
                "device.vendor",
                "device.product",
                "device.bus",
                "node.name",
                "node.description",
                "object.path",
                "api.alsa.card",
                "api.alsa.card.name",
                "api.alsa.path",
                "api.v4l2.path",
                // NDI-specific
                "ip",
                "url-address",
                "ndi-name",
            ];

            for prop_name in prop_names {
                if let Ok(value) = props.get::<String>(prop_name) {
                    properties.insert(prop_name.to_string(), value);
                }
            }

            debug!("Device properties for {}: {:?}", display_name, properties);
        }

        let id = DiscoveredDevice::generate_id(&device_class, &provider, &display_name);
        let now = Instant::now();

        let discovered = DiscoveredDevice {
            id: id.clone(),
            name: display_name.clone(),
            device_class,
            category,
            provider: provider.clone(),
            properties,
            first_seen: now,
            last_seen: now,
        };

        let mut devices = self.devices.write().await;
        let is_new = !devices.contains_key(&id);

        if is_new {
            info!(
                "Discovered new device: {} (provider: {})",
                display_name, provider
            );
        } else {
            debug!("Updated device: {}", display_name);
        }

        devices
            .entry(id.clone())
            .and_modify(|d| d.last_seen = now)
            .or_insert(discovered);
        drop(devices);

        if let Ok(mut gst_devices) = self.gst_devices.lock() {
            gst_devices.insert(id, device.clone());
        }
    }

    /// Handle a device removed event.
    async fn handle_device_removed(
        devices: &Arc<RwLock<HashMap<String, DiscoveredDevice>>>,
        gst_devices: &GstDeviceMap,
        device: &gst::Device,
    ) {
        let display_name = device.display_name().to_string();
        let device_class = device.device_class().to_string();
        let provider = Self::get_provider_name(device, &device_class);

        let id = DiscoveredDevice::generate_id(&device_class, &provider, &display_name);

        let mut devices = devices.write().await;
        if devices.remove(&id).is_some() {
            info!("Device removed: {}", display_name);
        }
        if let Ok(mut gst_devices) = gst_devices.lock() {
            gst_devices.remove(&id);
        }
    }

    /// Run the event loop for device monitor bus messages.
    async fn run_event_loop(
        bus: gst::Bus,
        devices: Arc<RwLock<HashMap<String, DiscoveredDevice>>>,
        gst_devices: GstDeviceMap,
        shutdown: Arc<AtomicBool>,
    ) {
        loop {
            // Check if shutdown was requested
            if shutdown.load(Ordering::SeqCst) {
                debug!("Device discovery event loop shutting down");
                break;
            }

            // Use non-blocking pop to avoid blocking the async runtime
            let msg = match bus.pop() {
                Some(msg) => msg,
                None => {
                    // No message available, sleep briefly and check again
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    continue;
                }
            };

            match msg.view() {
                gst::MessageView::DeviceAdded(device_added) => {
                    let device = device_added.device();
                    let display_name = device.display_name().to_string();
                    let device_class = device.device_class().to_string();
                    let category = DeviceCategory::from_device_class(&device_class);

                    let provider = Self::get_provider_name(&device, &device_class);

                    debug!(
                        "Device added event: {} (class: {}, provider: {})",
                        display_name, device_class, provider
                    );

                    // Extract properties
                    let mut properties = HashMap::new();
                    if let Some(props) = device.properties() {
                        let prop_names = [
                            "device.api",
                            "device.name",
                            "device.description",
                            "node.name",
                            "ip",
                            "url-address",
                            "ndi-name",
                        ];
                        for prop_name in prop_names {
                            if let Ok(value) = props.get::<String>(prop_name) {
                                properties.insert(prop_name.to_string(), value);
                            }
                        }
                    }

                    let id = DiscoveredDevice::generate_id(&device_class, &provider, &display_name);
                    let now = Instant::now();

                    let discovered = DiscoveredDevice {
                        id: id.clone(),
                        name: display_name.clone(),
                        device_class,
                        category,
                        provider: provider.clone(),
                        properties,
                        first_seen: now,
                        last_seen: now,
                    };

                    let mut devices_guard = devices.write().await;
                    let is_new = !devices_guard.contains_key(&id);

                    if is_new {
                        info!(
                            "Discovered new device: {} (provider: {})",
                            display_name, provider
                        );
                    }

                    devices_guard
                        .entry(id.clone())
                        .and_modify(|d| d.last_seen = now)
                        .or_insert(discovered);
                    drop(devices_guard);

                    if let Ok(mut gst_devices_guard) = gst_devices.lock() {
                        gst_devices_guard.insert(id, device);
                    }
                }
                gst::MessageView::DeviceRemoved(device_removed) => {
                    let device = device_removed.device();
                    Self::handle_device_removed(&devices, &gst_devices, &device).await;
                }
                _ => {}
            }
        }
    }

    /// Refresh devices by re-querying the device monitor.
    pub async fn refresh(&self) {
        if let Some(monitor) = &self.monitor {
            let devices = monitor.devices();
            debug!("Refreshing devices, found {} devices", devices.len());

            for device in devices {
                self.handle_device_added(&device).await;
            }
        }
    }
}

impl Default for DeviceDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DeviceDiscovery {
    fn drop(&mut self) {
        // Signal the event loop to stop first
        self.shutdown.store(true, Ordering::SeqCst);

        // Stop the device monitor
        if let Some(monitor) = self.monitor.take() {
            debug!("Stopping device monitor on drop");
            monitor.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_category_from_class() {
        assert_eq!(
            DeviceCategory::from_device_class("Audio/Source"),
            DeviceCategory::AudioSource
        );
        assert_eq!(
            DeviceCategory::from_device_class("Audio/Sink"),
            DeviceCategory::AudioSink
        );
        assert_eq!(
            DeviceCategory::from_device_class("Video/Source"),
            DeviceCategory::VideoSource
        );
        assert_eq!(
            DeviceCategory::from_device_class("Source/Network"),
            DeviceCategory::NetworkSource
        );
        assert_eq!(
            DeviceCategory::from_device_class("Unknown/Class"),
            DeviceCategory::Other
        );
    }

    #[test]
    fn test_device_generate_id() {
        let id1 = DiscoveredDevice::generate_id("Audio/Source", "pulsedeviceprovider", "Mic 1");
        let id2 = DiscoveredDevice::generate_id("Audio/Source", "pulsedeviceprovider", "Mic 1");
        let id3 = DiscoveredDevice::generate_id("Audio/Source", "pulsedeviceprovider", "Mic 2");

        // Same inputs should produce same ID
        assert_eq!(id1, id2);
        // Different name should produce different ID
        assert_ne!(id1, id3);
        // ID should start with "dev-"
        assert!(id1.starts_with("dev-"));
    }
}
