//! WHIP endpoint registry.
//!
//! Maps endpoint IDs to the stream mode of their WHIP Input blocks.

use std::collections::HashMap;
use std::sync::Arc;
use strom_types::block::StreamMode;
use tokio::sync::RwLock;

/// Information about a registered WHIP endpoint.
#[derive(Debug, Clone)]
pub struct WhipEndpointEntry {
    /// Stream mode (audio, video, or both)
    pub mode: StreamMode,
}

/// Registry mapping endpoint IDs to their endpoint info.
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

    /// Register an endpoint with its stream mode.
    ///
    /// Returns an error if an endpoint with the same ID is already registered.
    pub async fn register(&self, endpoint_id: String, mode: StreamMode) -> Result<(), String> {
        let mut map = self.inner.write().await;
        if map.contains_key(&endpoint_id) {
            return Err(format!(
                "WHIP endpoint '{}' is already registered by another flow",
                endpoint_id
            ));
        }
        map.insert(endpoint_id, WhipEndpointEntry { mode });
        Ok(())
    }

    /// Unregister an endpoint.
    pub async fn unregister(&self, endpoint_id: &str) {
        let mut map = self.inner.write().await;
        map.remove(endpoint_id);
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
}
