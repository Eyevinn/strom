//! WHEP endpoint registry.
//!
//! Maps endpoint IDs to internal localhost ports for WHEP Output blocks.
//! The axum proxy uses this to route requests to the correct whepserversink instance.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Information about a registered WHEP endpoint.
#[derive(Debug, Clone)]
pub struct WhepEndpointEntry {
    /// Internal localhost port where whepserversink is listening
    pub port: u16,
    /// Number of audio tracks exposed by this endpoint (0 = no audio)
    pub num_audio_tracks: usize,
    /// Number of video tracks exposed by this endpoint (0 = no video)
    pub num_video_tracks: usize,
}

/// Registry mapping endpoint IDs to internal ports.
#[derive(Debug, Clone, Default)]
pub struct WhepRegistry {
    inner: Arc<RwLock<HashMap<String, WhepEndpointEntry>>>,
}

impl WhepRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register an endpoint with its internal port and track counts.
    ///
    /// Returns an error if an endpoint with the same ID is already registered.
    pub async fn register(
        &self,
        endpoint_id: String,
        port: u16,
        num_audio_tracks: usize,
        num_video_tracks: usize,
    ) -> Result<(), String> {
        let mut map = self.inner.write().await;
        if map.contains_key(&endpoint_id) {
            return Err(format!(
                "WHEP endpoint '{}' is already registered by another flow",
                endpoint_id
            ));
        }
        map.insert(
            endpoint_id,
            WhepEndpointEntry {
                port,
                num_audio_tracks,
                num_video_tracks,
            },
        );
        Ok(())
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
    pub async fn get(&self, endpoint_id: &str) -> Option<WhepEndpointEntry> {
        let map = self.inner.read().await;
        map.get(endpoint_id).cloned()
    }

    /// Check if an endpoint ID is already registered.
    pub async fn contains(&self, endpoint_id: &str) -> bool {
        let map = self.inner.read().await;
        map.contains_key(endpoint_id)
    }

    /// Get all registered endpoints with their info.
    pub async fn list_all(&self) -> Vec<(String, WhepEndpointEntry)> {
        let map = self.inner.read().await;
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}
