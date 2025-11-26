//! Configuration management.

use crate::paths::{DataPaths, PathConfig};
use std::env;
use std::path::PathBuf;

/// Application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Port to listen on
    pub port: u16,
    /// Path to flows storage file (used if database_url is None)
    pub flows_path: PathBuf,
    /// Path to blocks storage file
    pub blocks_path: PathBuf,
    /// PostgreSQL database URL (if set, PostgreSQL is used instead of JSON files)
    /// Format: postgresql://user:password@host/database_name
    pub database_url: Option<String>,
}

impl Config {
    /// Create configuration from explicit values.
    ///
    /// This is the primary way to construct Config, typically from CLI arguments.
    pub fn new(
        port: u16,
        data_dir: Option<PathBuf>,
        flows_path: Option<PathBuf>,
        blocks_path: Option<PathBuf>,
        database_url: Option<String>,
    ) -> anyhow::Result<Self> {
        // Resolve data paths
        let path_config = PathConfig {
            data_dir,
            flows_path,
            blocks_path,
        };
        let data_paths = DataPaths::resolve(path_config)?;

        Ok(Self {
            port,
            flows_path: data_paths.flows_path,
            blocks_path: data_paths.blocks_path,
            database_url,
        })
    }

    /// Load configuration from environment variables only (legacy support).
    ///
    /// This method is primarily for backward compatibility and tests.
    /// CLI applications should use `Config::new()` with parsed arguments.
    pub fn from_env() -> anyhow::Result<Self> {
        let port = env::var("STROM_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(strom_types::DEFAULT_PORT);

        let data_dir = env::var("STROM_DATA_DIR").ok().map(PathBuf::from);
        let flows_path = env::var("STROM_FLOWS_PATH").ok().map(PathBuf::from);
        let blocks_path = env::var("STROM_BLOCKS_PATH").ok().map(PathBuf::from);
        let database_url = env::var("STROM_DATABASE_URL").ok();

        Self::new(port, data_dir, flows_path, blocks_path, database_url)
    }
}

impl Default for Config {
    fn default() -> Self {
        // For default, use the path resolution logic
        Self::from_env().unwrap_or_else(|_| {
            // Ultimate fallback (should rarely happen)
            Self {
                port: strom_types::DEFAULT_PORT,
                flows_path: PathBuf::from("flows.json"),
                blocks_path: PathBuf::from("blocks.json"),
                database_url: None,
            }
        })
    }
}
