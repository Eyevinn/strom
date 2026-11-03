//! JSON file-based storage implementation.

use super::{migrate_flow, Result, Storage, StorageError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom_types::{Flow, FlowId};
use tokio::fs;
use tokio::sync::Mutex;
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
    /// The flows as last read or written, `None` until first used. The lock
    /// is held across each read-modify-write and its file write, so the cache
    /// and the file change together. Saves of different flows run
    /// concurrently; without it they share the temp file (one rename fails
    /// with ENOENT) and drop each other's flows.
    cache: Mutex<Option<HashMap<FlowId, Flow>>>,
}

impl JsonFileStorage {
    /// Create a new JSON file storage.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            cache: Mutex::new(None),
        }
    }

    /// The cached flows, read from the file on first use.
    async fn cached<'a>(
        &self,
        cache: &'a mut Option<HashMap<FlowId, Flow>>,
    ) -> Result<&'a HashMap<FlowId, Flow>> {
        if cache.is_none() {
            *cache = Some(self.read_file().await?);
        } else {
            debug!("Returning cached flows");
        }
        Ok(cache.get_or_insert_with(HashMap::new))
    }

    /// Read flows from the file.
    async fn read_file(&self) -> Result<HashMap<FlowId, Flow>> {
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

        Ok(flows)
    }

    /// Write flows to the file.
    async fn write_file(&self, flows: &HashMap<FlowId, Flow>) -> Result<()> {
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

        Ok(())
    }
}

#[async_trait]
impl Storage for JsonFileStorage {
    async fn load_all(&self) -> Result<HashMap<FlowId, Flow>> {
        let mut cache = self.cache.lock().await;
        Ok(self.cached(&mut cache).await?.clone())
    }

    async fn save_all(&self, flows: &HashMap<FlowId, Flow>) -> Result<()> {
        let mut cache = self.cache.lock().await;
        self.write_file(flows).await?;
        *cache = Some(flows.clone());
        Ok(())
    }

    async fn save_flow(&self, flow: &Flow) -> Result<()> {
        let mut cache = self.cache.lock().await;
        let mut flows = self.cached(&mut cache).await?.clone();
        flows.insert(flow.id, flow.clone());
        self.write_file(&flows).await?;
        *cache = Some(flows);
        Ok(())
    }

    async fn delete_flow(&self, id: &FlowId) -> Result<()> {
        let mut cache = self.cache.lock().await;
        let mut flows = self.cached(&mut cache).await?.clone();
        if flows.remove(id).is_none() {
            warn!("Attempted to delete non-existent flow: {}", id);
            return Err(StorageError::NotFound(*id));
        }
        self.write_file(&flows).await?;
        *cache = Some(flows);
        Ok(())
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

    /// Saves of different flows at the same time (nothing above the storage
    /// orders them) must all succeed and all reach the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_saves_keep_every_flow() {
        const FLOWS: usize = 32;

        let dir = tempdir().unwrap();
        let path = dir.path().join("flows.json");
        let storage = std::sync::Arc::new(JsonFileStorage::new(&path));

        let saves: Vec<_> = (0..FLOWS)
            .map(|i| {
                let storage = storage.clone();
                let flow = Flow::new(format!("Flow {i}"));
                tokio::spawn(async move { storage.save_flow(&flow).await })
            })
            .collect();
        for save in saves {
            save.await.unwrap().unwrap();
        }

        // A fresh storage reads the file, not the cache.
        let on_disk = JsonFileStorage::new(&path).load_all().await.unwrap();
        assert_eq!(on_disk.len(), FLOWS);
    }

    /// Reads, saves and deletes at the same time, starting before anything
    /// is cached, must leave exactly the expected flows in the file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_reads_saves_and_deletes_keep_the_file_right() {
        const EXISTING: usize = 8;
        const NEW: usize = 8;

        for round in 0..20 {
            let dir = tempdir().unwrap();
            let path = dir.path().join("flows.json");
            let existing: Vec<Flow> = (0..EXISTING)
                .map(|i| Flow::new(format!("Existing {i}")))
                .collect();
            let seed = JsonFileStorage::new(&path);
            for flow in &existing {
                seed.save_flow(flow).await.unwrap();
            }

            // Nothing cached yet: the first reads go to the file.
            let storage = std::sync::Arc::new(JsonFileStorage::new(&path));
            let new: Vec<Flow> = (0..NEW).map(|i| Flow::new(format!("New {i}"))).collect();
            let mut tasks = Vec::new();
            for flow in &new {
                let reader = storage.clone();
                tasks.push(tokio::spawn(
                    async move { reader.load_all().await.map(|_| ()) },
                ));
                let saver = storage.clone();
                let flow = flow.clone();
                tasks.push(tokio::spawn(async move { saver.save_flow(&flow).await }));
            }
            // Delete every other existing flow.
            for flow in existing.iter().step_by(2) {
                let deleter = storage.clone();
                let id = flow.id;
                tasks.push(tokio::spawn(async move { deleter.delete_flow(&id).await }));
            }
            for task in tasks {
                task.await.unwrap().unwrap();
            }

            let mut expected: Vec<FlowId> = existing
                .iter()
                .skip(1)
                .step_by(2)
                .chain(&new)
                .map(|f| f.id)
                .collect();
            expected.sort();
            let mut on_disk: Vec<FlowId> = JsonFileStorage::new(&path)
                .load_all()
                .await
                .unwrap()
                .into_keys()
                .collect();
            on_disk.sort();
            assert_eq!(
                on_disk, expected,
                "round {round}: the file lost or kept the wrong flows"
            );
        }
    }
}
