//! WHIP endpoint registry.
//!
//! Maps endpoint IDs to internal localhost ports for WHIP Input blocks.
//! The axum proxy uses this to route requests to the correct whipserversrc instance.

use crate::blocks::WhepStreamMode;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Information about a registered WHIP endpoint.
#[derive(Debug, Clone)]
pub struct WhipEndpointEntry {
    /// Internal localhost port where whipserversrc is listening
    pub port: u16,
    /// Stream mode (audio, video, or both)
    pub mode: WhepStreamMode,
}

/// Registry mapping endpoint IDs to internal ports.
#[derive(Debug, Clone, Default)]
pub struct WhipRegistry {
    inner: Arc<RwLock<HashMap<String, WhipEndpointEntry>>>,
}

impl WhipRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register an endpoint with its internal port and stream mode.
    pub async fn register(&self, endpoint_id: String, port: u16, mode: WhepStreamMode) {
        let mut map = self.inner.write().await;
        map.insert(endpoint_id, WhipEndpointEntry { port, mode });
    }

    /// Unregister an endpoint.
    pub async fn unregister(&self, endpoint_id: &str) {
        let mut map = self.inner.write().await;
        map.remove(endpoint_id);
    }

    /// Look up the internal port for an endpoint ID.
    pub async fn get_port(&self, endpoint_id: &str) -> Option<u16> {
        let map = self.inner.read().await;
        map.get(endpoint_id).map(|e| e.port)
    }

    /// Look up endpoint info (port and mode) for an endpoint ID.
    pub async fn get(&self, endpoint_id: &str) -> Option<WhipEndpointEntry> {
        let map = self.inner.read().await;
        map.get(endpoint_id).cloned()
    }

    /// Check if an endpoint ID is already registered.
    pub async fn contains(&self, endpoint_id: &str) -> bool {
        let map = self.inner.read().await;
        map.contains_key(endpoint_id)
    }

    /// Get all registered endpoints with their info.
    pub async fn list_all(&self) -> Vec<(String, WhipEndpointEntry)> {
        let map = self.inner.read().await;
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Update the port for an existing endpoint (sync version for use from
    /// non-async contexts like `spawn_blocking` or `std::thread::spawn`).
    ///
    /// Used when whipserversrc is recreated on a new port after session end.
    pub fn update_port_sync(&self, endpoint_id: &str, new_port: u16) {
        let mut map = self.inner.blocking_write();
        if let Some(entry) = map.get_mut(endpoint_id) {
            entry.port = new_port;
        }
    }
}
