//! mDNS service discovery for RAVENNA, NDI, and other network protocols.
//!
//! This module uses the mdns-sd crate to discover services advertised via
//! mDNS/Bonjour/Zeroconf on the local network.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// mDNS service discovery manager.
pub struct MdnsDiscovery {
    /// The mdns-sd service daemon.
    daemon: ServiceDaemon,
    /// Currently registered announcements (service_name -> ServiceInfo).
    announcements: Arc<RwLock<HashMap<String, ServiceInfo>>>,
}

impl MdnsDiscovery {
    /// Create a new mDNS discovery instance.
    pub fn new() -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        Ok(Self {
            daemon,
            announcements: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Start browsing for a specific service type.
    ///
    /// Returns a receiver channel that emits discovery events.
    ///
    /// # Arguments
    /// * `service_type` - The service type to browse (e.g., "_rtsp._tcp.local.")
    pub fn browse(&self, service_type: &str) -> Result<flume::Receiver<ServiceEvent>> {
        info!("Starting mDNS browse for {}", service_type);
        let receiver = self.daemon.browse(service_type)?;
        Ok(receiver)
    }

    /// Register a service for announcement via mDNS.
    ///
    /// # Arguments
    /// * `service_info` - The service information to announce
    pub fn register(&self, service_info: ServiceInfo) -> Result<()> {
        let fullname = service_info.get_fullname().to_string();
        info!("Registering mDNS service: {}", fullname);

        self.daemon.register(service_info.clone())?;

        // Store for later unregistration
        tokio::spawn({
            let announcements = self.announcements.clone();
            async move {
                let mut map = announcements.write().await;
                map.insert(fullname, service_info);
            }
        });

        Ok(())
    }

    /// Unregister a previously announced service.
    ///
    /// # Arguments
    /// * `fullname` - The full service name to unregister
    pub async fn unregister(&self, fullname: &str) -> Result<()> {
        info!("Unregistering mDNS service: {}", fullname);

        let mut map = self.announcements.write().await;
        if let Some(service_info) = map.remove(fullname) {
            self.daemon.unregister(service_info.get_fullname())?;
        }

        Ok(())
    }

    /// Shutdown the mDNS daemon and unregister all services.
    pub async fn shutdown(&self) -> Result<()> {
        info!("Shutting down mDNS discovery");

        // Unregister all services
        let services: Vec<_> = {
            let map = self.announcements.read().await;
            map.values().cloned().collect()
        };

        for service in services {
            if let Err(e) = self.daemon.unregister(service.get_fullname()) {
                warn!("Failed to unregister {}: {}", service.get_fullname(), e);
            }
        }

        self.daemon.shutdown()?;
        Ok(())
    }
}

/// Extract hostname and port from mDNS service info.
pub fn extract_service_address(info: &ServiceInfo) -> Option<(String, u16)> {
    let hostname = info.get_hostname().to_string();
    let port = info.get_port();
    Some((hostname, port))
}

/// Extract TXT record value by key.
pub fn get_txt_property(info: &ServiceInfo, key: &str) -> Option<String> {
    info.get_property_val_str(key).map(|s| s.to_string())
}
