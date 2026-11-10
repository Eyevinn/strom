//! Block registry combining built-in and user-defined blocks.

use crate::blocks::{builtin, storage};
use std::path::PathBuf;
use std::sync::Arc;
use strom_types::BlockDefinition;
use tokio::sync::RwLock;
use tracing::{error, info};

/// Thread-safe block registry.
#[derive(Clone)]
pub struct BlockRegistry {
    builtin_blocks: Vec<BlockDefinition>,
    user_blocks: Arc<RwLock<Vec<BlockDefinition>>>,
    storage_path: PathBuf,
}

impl BlockRegistry {
    /// Create a new block registry.
    pub fn new(storage_path: impl Into<PathBuf>) -> Self {
        Self {
            builtin_blocks: builtin::get_all_builtin_blocks(),
            user_blocks: Arc::new(RwLock::new(Vec::new())),
            storage_path: storage_path.into(),
        }
    }

    /// Load user-defined blocks from storage.
    pub async fn load_user_blocks(&self) -> Result<(), storage::StorageError> {
        let blocks = storage::load_user_blocks(&self.storage_path).await?;
        let mut user_blocks = self.user_blocks.write().await;
        *user_blocks = blocks;
        Ok(())
    }

    /// Save user-defined blocks to storage.
    async fn save_user_blocks(&self) -> Result<(), storage::StorageError> {
        let user_blocks = self.user_blocks.read().await;
        storage::save_user_blocks(&self.storage_path, &user_blocks).await
    }

    /// Get all blocks (built-in + user-defined).
    pub async fn get_all(&self) -> Vec<BlockDefinition> {
        let user_blocks = self.user_blocks.read().await;
        let mut all = self.builtin_blocks.clone();
        all.extend(user_blocks.clone());
        all
    }

    /// Get a block by ID.
    pub async fn get_by_id(&self, id: &str) -> Option<BlockDefinition> {
        // Check built-in blocks first
        if let Some(block) = self.builtin_blocks.iter().find(|b| b.id == id) {
            return Some(block.clone());
        }

        // Then check user blocks
        let user_blocks = self.user_blocks.read().await;
        user_blocks.iter().find(|b| b.id == id).cloned()
    }

    /// Get all unique categories.
    pub async fn get_categories(&self) -> Vec<String> {
        let all_blocks = self.get_all().await;
        let mut categories: Vec<String> = all_blocks
            .iter()
            .map(|b| b.category.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        categories.sort();
        categories
    }

    /// Add a new user-defined block.
    pub async fn add_user_block(&self, mut block: BlockDefinition) -> Result<(), String> {
        // Ensure it's marked as user-defined
        block.built_in = false;

        // Generate ID if not provided or ensure it has user prefix
        if block.id.is_empty() || block.id.starts_with("builtin.") {
            block.id = format!("user.{}", uuid::Uuid::new_v4());
        } else if !block.id.starts_with("user.") {
            block.id = format!("user.{}", block.id);
        }

        // Check if ID already exists
        if self.get_by_id(&block.id).await.is_some() {
            return Err(format!("Block with ID {} already exists", block.id));
        }

        // Add to user blocks
        let mut user_blocks = self.user_blocks.write().await;
        user_blocks.push(block);
        drop(user_blocks);

        // Save to storage
        if let Err(e) = self.save_user_blocks().await {
            error!("Failed to save user blocks: {}", e);
            return Err(format!("Failed to save blocks: {}", e));
        }

        info!("Added new user block");
        Ok(())
    }

    /// Update an existing user-defined block.
    pub async fn update_user_block(&self, block: BlockDefinition) -> Result<(), String> {
        // Cannot update built-in blocks
        if block.built_in || block.id.starts_with("builtin.") {
            return Err("Cannot update built-in blocks".to_string());
        }

        let mut user_blocks = self.user_blocks.write().await;

        // Find and update
        let block_id = block.id.clone();
        if let Some(existing) = user_blocks.iter_mut().find(|b| b.id == block.id) {
            *existing = block;
            drop(user_blocks);

            // Save to storage
            if let Err(e) = self.save_user_blocks().await {
                error!("Failed to save user blocks: {}", e);
                return Err(format!("Failed to save blocks: {}", e));
            }

            info!("Updated user block: {}", block_id);
            Ok(())
        } else {
            Err(format!("Block {} not found", block.id))
        }
    }

    /// Delete a user-defined block.
    pub async fn delete_user_block(&self, id: &str) -> Result<bool, String> {
        // Cannot delete built-in blocks
        if id.starts_with("builtin.") {
            return Err("Cannot delete built-in blocks".to_string());
        }

        let mut user_blocks = self.user_blocks.write().await;

        let original_len = user_blocks.len();
        user_blocks.retain(|b| b.id != id);

        if user_blocks.len() < original_len {
            drop(user_blocks);

            // Save to storage
            if let Err(e) = self.save_user_blocks().await {
                error!("Failed to save user blocks: {}", e);
                return Err(format!("Failed to save blocks: {}", e));
            }

            info!("Deleted user block: {}", id);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_builtin_blocks() {
        // Builds GStreamer objects: a test must not count on another test in
        // the same process having called gst::init() first.
        gstreamer::init().unwrap();
        let temp_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(temp_file.path());

        let all = registry.get_all().await;
        assert!(!all.is_empty());

        // Check that AES67 input exists
        let aes67 = registry.get_by_id("builtin.aes67_input").await;
        assert!(aes67.is_some());
        assert_eq!(aes67.unwrap().name, "AES67 Input");
    }

    #[tokio::test]
    async fn test_add_user_block() {
        // Builds GStreamer objects: a test must not count on another test in
        // the same process having called gst::init() first.
        gstreamer::init().unwrap();
        let temp_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(temp_file.path());

        let test_block = BlockDefinition {
            id: "my_block".to_string(),
            name: "My Block".to_string(),
            description: "Test".to_string(),
            category: "Test".to_string(),
            exposed_properties: vec![],
            external_pads: ExternalPads {
                inputs: vec![],
                outputs: vec![],
            },
            built_in: false,
            ui_metadata: None,
        };

        let result = registry.add_user_block(test_block).await;
        assert!(result.is_ok());

        // Should be retrievable with user. prefix
        let retrieved = registry.get_by_id("user.my_block").await;
        assert!(retrieved.is_some());
    }

    #[tokio::test]
    async fn test_cannot_delete_builtin() {
        // Builds GStreamer objects: a test must not count on another test in
        // the same process having called gst::init() first.
        gstreamer::init().unwrap();
        let temp_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(temp_file.path());

        let result = registry.delete_user_block("builtin.aes67_input").await;
        assert!(result.is_err());
    }
}
