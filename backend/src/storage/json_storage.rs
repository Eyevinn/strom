//! JSON file-based storage implementation.

use super::{migrate_flow, Result, Storage, StorageError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom_types::{Flow, FlowId};
use tokio::fs;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// JSON file storage format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageFormat {
    version: u32,
    flows: Vec<Flow>,
}

impl Default for StorageFormat {
    fn default() -> Self {
        Self {
            version: 1,
            flows: Vec::new(),
        }
    }
}

/// Storage backend that persists flows to a JSON file.
pub struct JsonFileStorage {
    path: PathBuf,
    cache: RwLock<Option<HashMap<FlowId, Flow>>>,
}

impl JsonFileStorage {
    /// Create a new JSON file storage.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            cache: RwLock::new(None),
        }
    }

    /// Load flows from file, using cache if available.
    async fn load_from_file(&self) -> Result<HashMap<FlowId, Flow>> {
        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(flows) = cache.as_ref() {
                debug!("Returning cached flows");
                return Ok(flows.clone());
            }
        }

        // Load from file
        debug!("Loading flows from {:?}", self.path);

        if !self.path.exists() {
            info!("Storage file does not exist, starting with empty flows");
            return Ok(HashMap::new());
        }

        let contents = fs::read_to_string(&self.path).await?;

        if contents.trim().is_empty() {
            info!("Storage file is empty, starting with empty flows");
            return Ok(HashMap::new());
        }

        let storage: StorageFormat = serde_json::from_str(&contents)?;

        let flows: HashMap<FlowId, Flow> = storage
            .flows
            .into_iter()
            .map(migrate_flow)
            .map(|flow| (flow.id, flow))
            .collect();

        info!("Loaded {} flows from storage", flows.len());

        // Update cache
        {
            let mut cache = self.cache.write().await;
            *cache = Some(flows.clone());
        }

        Ok(flows)
    }

    /// Write flows to file and update cache.
    async fn write_to_file(&self, flows: &HashMap<FlowId, Flow>) -> Result<()> {
        debug!("Writing {} flows to {:?}", flows.len(), self.path);

        let storage = StorageFormat {
            version: 1,
            flows: flows.values().cloned().collect(),
        };

        let json = serde_json::to_string_pretty(&storage)?;

        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent).await?;
            }
        }

        // Write to temporary file first, then rename (atomic operation)
        let temp_path = self.path.with_extension("tmp");
        fs::write(&temp_path, json).await?;
        fs::rename(&temp_path, &self.path).await?;

        info!("Successfully wrote {} flows to storage", flows.len());

        // Update cache
        {
            let mut cache = self.cache.write().await;
            *cache = Some(flows.clone());
        }

        Ok(())
    }

    /// Invalidate the cache.
    pub async fn invalidate_cache(&self) {
        let mut cache = self.cache.write().await;
        *cache = None;
        debug!("Cache invalidated");
    }
}

#[async_trait]
impl Storage for JsonFileStorage {
    async fn load_all(&self) -> Result<HashMap<FlowId, Flow>> {
        self.load_from_file().await
    }

    async fn save_all(&self, flows: &HashMap<FlowId, Flow>) -> Result<()> {
        self.write_to_file(flows).await
    }

    async fn save_flow(&self, flow: &Flow) -> Result<()> {
        let mut flows = self.load_all().await?;
        flows.insert(flow.id, flow.clone());
        self.save_all(&flows).await
    }

    async fn delete_flow(&self, id: &FlowId) -> Result<()> {
        let mut flows = self.load_all().await?;
        if flows.remove(id).is_none() {
            warn!("Attempted to delete non-existent flow: {}", id);
            return Err(StorageError::NotFound(*id));
        }
        self.save_all(&flows).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_empty_storage() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("flows.json");
        let storage = JsonFileStorage::new(&path);

        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 0);
    }

    #[tokio::test]
    async fn test_save_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("flows.json");
        let storage = JsonFileStorage::new(&path);

        // Create a flow
        let flow = Flow::new("Test Flow");

        // Save it
        storage.save_flow(&flow).await.unwrap();

        // Load all flows
        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows.get(&flow.id).unwrap().name, "Test Flow");
    }

    #[tokio::test]
    async fn test_delete_flow() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("flows.json");
        let storage = JsonFileStorage::new(&path);

        // Create and save a flow
        let flow = Flow::new("Test Flow");
        storage.save_flow(&flow).await.unwrap();

        // Delete it
        storage.delete_flow(&flow.id).await.unwrap();

        // Verify it's gone
        let flows = storage.load_all().await.unwrap();
        assert_eq!(flows.len(), 0);
    }
}
