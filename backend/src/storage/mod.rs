//! Storage layer for persisting flows.

mod json_storage;
mod postgres_storage;

pub use json_storage::JsonFileStorage;
pub use postgres_storage::PostgresStorage;

use async_trait::async_trait;
use std::collections::HashMap;
use strom_types::{Flow, FlowId};

/// Error type for storage operations.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Flow not found: {0}")]
    NotFound(FlowId),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Trait for flow storage backends.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Load all flows from storage.
    async fn load_all(&self) -> Result<HashMap<FlowId, Flow>>;

    /// Save all flows to storage.
    async fn save_all(&self, flows: &HashMap<FlowId, Flow>) -> Result<()>;

    /// Save a single flow.
    async fn save_flow(&self, flow: &Flow) -> Result<()> {
        let mut flows = self.load_all().await?;
        flows.insert(flow.id, flow.clone());
        self.save_all(&flows).await
    }

    /// Delete a flow.
    async fn delete_flow(&self, id: &FlowId) -> Result<()> {
        let mut flows = self.load_all().await?;
        flows.remove(id).ok_or(StorageError::NotFound(*id))?;
        self.save_all(&flows).await
    }
}
