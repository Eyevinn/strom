//! Storage for user-defined blocks.

use serde::{Deserialize, Serialize};
use std::path::Path;
use strom_types::BlockDefinition;
use thiserror::Error;
use tokio::fs;
use tracing::{debug, info};

#[derive(Error, Debug)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Storage format for blocks.json
#[derive(Debug, Serialize, Deserialize)]
struct BlocksFile {
    blocks: Vec<BlockDefinition>,
}

/// Load user-defined blocks from JSON file.
pub async fn load_user_blocks(path: &Path) -> Result<Vec<BlockDefinition>, StorageError> {
    if !path.exists() {
        info!("Blocks file does not exist, starting with empty user blocks");
        return Ok(Vec::new());
    }

    debug!("Loading user blocks from: {}", path.display());
    let contents = fs::read_to_string(path).await?;
    let blocks_file: BlocksFile = serde_json::from_str(&contents)?;

    info!("Loaded {} user-defined blocks", blocks_file.blocks.len());
    Ok(blocks_file.blocks)
}

/// Save user-defined blocks to JSON file.
pub async fn save_user_blocks(path: &Path, blocks: &[BlockDefinition]) -> Result<(), StorageError> {
    debug!("Saving {} user blocks to: {}", blocks.len(), path.display());

    let blocks_file = BlocksFile {
        blocks: blocks.to_vec(),
    };

    let contents = serde_json::to_string_pretty(&blocks_file)?;
    fs::write(path, contents).await?;

    info!("Saved user blocks to: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_load_nonexistent_file() {
        let path = Path::new("/nonexistent/blocks.json");
        let result = load_user_blocks(path).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_save_and_load_blocks() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        let test_block = BlockDefinition {
            id: "user.test_block".to_string(),
            name: "Test Block".to_string(),
            description: "A test block".to_string(),
            category: "Test".to_string(),
            exposed_properties: vec![],
            external_pads: ExternalPads {
                inputs: vec![],
                outputs: vec![],
            },
            built_in: false,
            ui_metadata: None,
        };

        // Save
        let save_result = save_user_blocks(path, std::slice::from_ref(&test_block)).await;
        assert!(save_result.is_ok());

        // Load
        let loaded = load_user_blocks(path).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "user.test_block");
        assert_eq!(loaded[0].name, "Test Block");
    }
}
