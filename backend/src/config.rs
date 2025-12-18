//! Configuration management.

use crate::paths::{DataPaths, PathConfig};
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::PathBuf;

/// Configuration structure that matches the TOML file format.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    storage: StorageConfig,
    #[serde(default)]
    logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerConfig {
    #[serde(default = "default_port")]
    port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StorageConfig {
    database_url: Option<String>,
    data_dir: Option<PathBuf>,
    flows_path: Option<PathBuf>,
    blocks_path: Option<PathBuf>,
    media_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LoggingConfig {
    /// Path to log file (if set, logs will be written to file in addition to stdout)
    log_file: Option<PathBuf>,
    /// Log level (trace, debug, info, warn, error)
    /// If not set, uses RUST_LOG environment variable or defaults to "info"
    log_level: Option<String>,
}

fn default_port() -> u16 {
    strom_types::DEFAULT_PORT
}

/// Application configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Port to listen on
    pub port: u16,
    /// Path to flows storage file (used if database_url is None)
    pub flows_path: PathBuf,
    /// Path to blocks storage file
    pub blocks_path: PathBuf,
    /// Path to media files directory
    pub media_path: PathBuf,
    /// PostgreSQL database URL (if set, PostgreSQL is used instead of JSON files)
    /// Format: postgresql://user:password@host/database_name
    pub database_url: Option<String>,
    /// Path to log file (if set, logs will be written to file in addition to stdout)
    pub log_file: Option<PathBuf>,
    /// Log level (if set, overrides RUST_LOG environment variable)
    pub log_level: Option<String>,
}

impl Config {
    /// Load configuration with full priority chain: CLI args > env vars > config files > defaults.
    ///
    /// This is the recommended way to load configuration, as it supports config files.
    /// Config files are searched in this order:
    /// 1. `.strom.toml` in current directory
    /// 2. `config.toml` in user config directory (~/.config/strom/ on Linux)
    pub fn from_figment(
        port: Option<u16>,
        data_dir: Option<PathBuf>,
        flows_path: Option<PathBuf>,
        blocks_path: Option<PathBuf>,
        media_path: Option<PathBuf>,
        database_url: Option<String>,
    ) -> anyhow::Result<Self> {
        // Find config file paths
        let local_config = std::env::current_dir().ok().map(|d| d.join(".strom.toml"));
        let user_config = directories::ProjectDirs::from("", "", "strom")
            .map(|dirs| dirs.config_dir().join("config.toml"));

        // Build figment with priority: defaults < user config < local config < env vars < CLI args
        let mut figment = Figment::new();

        // 1. Start with defaults
        figment = figment.merge(Serialized::defaults(ConfigFile {
            server: ServerConfig {
                port: strom_types::DEFAULT_PORT,
            },
            storage: StorageConfig::default(),
            logging: LoggingConfig::default(),
        }));

        // 2. Merge user config file if it exists
        if let Some(ref path) = user_config {
            if path.exists() {
                figment = figment.merge(Toml::file(path));
            }
        }

        // 3. Merge local config file if it exists
        if let Some(ref path) = local_config {
            if path.exists() {
                figment = figment.merge(Toml::file(path));
            }
        }

        // 4. Merge environment variables (STROM_* prefix)
        figment = figment.merge(
            Env::prefixed("STROM_")
                .map(|key| key.as_str().replace("__", ".").into())
                .split("_"),
        );

        // 5. Merge CLI arguments (highest priority)
        if let Some(p) = port {
            figment = figment.merge(Serialized::default("server.port", p));
        }
        if let Some(ref dd) = data_dir {
            figment = figment.merge(Serialized::default("storage.data_dir", dd));
        }
        if let Some(ref fp) = flows_path {
            figment = figment.merge(Serialized::default("storage.flows_path", fp));
        }
        if let Some(ref bp) = blocks_path {
            figment = figment.merge(Serialized::default("storage.blocks_path", bp));
        }
        if let Some(ref mp) = media_path {
            figment = figment.merge(Serialized::default("storage.media_path", mp));
        }
        if let Some(ref db) = database_url {
            figment = figment.merge(Serialized::default("storage.database_url", db));
        }

        // Extract the configuration
        let config_file: ConfigFile = figment.extract()?;

        // Resolve data paths
        let path_config = PathConfig {
            data_dir: config_file.storage.data_dir,
            flows_path: config_file.storage.flows_path,
            blocks_path: config_file.storage.blocks_path,
            media_path: config_file.storage.media_path,
        };
        let data_paths = DataPaths::resolve(path_config)?;

        Ok(Self {
            port: config_file.server.port,
            flows_path: data_paths.flows_path,
            blocks_path: data_paths.blocks_path,
            media_path: data_paths.media_path,
            database_url: config_file.storage.database_url,
            log_file: config_file.logging.log_file,
            log_level: config_file.logging.log_level,
        })
    }

    /// Create configuration from explicit values.
    ///
    /// This is the legacy way to construct Config. Use `from_figment()` for config file support.
    pub fn new(
        port: u16,
        data_dir: Option<PathBuf>,
        flows_path: Option<PathBuf>,
        blocks_path: Option<PathBuf>,
        media_path: Option<PathBuf>,
        database_url: Option<String>,
    ) -> anyhow::Result<Self> {
        // Resolve data paths
        let path_config = PathConfig {
            data_dir,
            flows_path,
            blocks_path,
            media_path,
        };
        let data_paths = DataPaths::resolve(path_config)?;

        Ok(Self {
            port,
            flows_path: data_paths.flows_path,
            blocks_path: data_paths.blocks_path,
            media_path: data_paths.media_path,
            database_url,
            log_file: None,
            log_level: None,
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
        let media_path = env::var("STROM_MEDIA_PATH").ok().map(PathBuf::from);
        let database_url = env::var("STROM_DATABASE_URL").ok();

        Self::new(
            port,
            data_dir,
            flows_path,
            blocks_path,
            media_path,
            database_url,
        )
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
                media_path: PathBuf::from("media"),
                database_url: None,
                log_file: None,
                log_level: None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    #[serial]
    fn test_from_figment_defaults() {
        // Clear any env vars that might have been set by other tests
        std::env::remove_var("STROM_SERVER_PORT");
        std::env::remove_var("STROM_PORT");
        std::env::remove_var("STROM_STORAGE_DATABASE_URL");
        std::env::remove_var("STROM_STORAGE_DATA_DIR");

        // Run in a temp directory to avoid picking up project .strom.toml
        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let config = Config::from_figment(None, None, None, None, None, None).unwrap();

        // Restore (ignore errors)
        let _ = std::env::set_current_dir(original_dir);

        assert_eq!(config.port, strom_types::DEFAULT_PORT);
        assert!(config.database_url.is_none());
    }

    #[test]
    fn test_from_figment_cli_args_override() {
        let temp_dir = TempDir::new().unwrap();
        let flows = temp_dir.path().join("flows.json");
        let blocks = temp_dir.path().join("blocks.json");

        let config = Config::from_figment(
            Some(9000),
            None,
            Some(flows.clone()),
            Some(blocks.clone()),
            None,
            Some("postgresql://test".to_string()),
        )
        .unwrap();

        assert_eq!(config.port, 9000);
        assert_eq!(config.flows_path, flows);
        assert_eq!(config.blocks_path, blocks);
        assert_eq!(config.database_url, Some("postgresql://test".to_string()));
    }

    #[test]
    #[serial]
    fn test_from_figment_config_file() {
        // Clear any env vars that might interfere
        std::env::remove_var("STROM_SERVER_PORT");
        std::env::remove_var("STROM_STORAGE_DATABASE_URL");

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join(".strom.toml");

        // Create a test config file
        let config_content = r#"
[server]
port = 7777

[storage]
database_url = "postgresql://localhost/test"
"#;
        fs::write(&config_file, config_content).unwrap();

        // Change to temp directory to make config file discoverable
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let config = Config::from_figment(None, None, None, None, None, None).unwrap();

        // Restore original directory (ignore errors if it fails)
        let _ = std::env::set_current_dir(original_dir);

        assert_eq!(config.port, 7777);
        assert_eq!(
            config.database_url,
            Some("postgresql://localhost/test".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_from_figment_env_vars_override_config_file() {
        // Save and clear any existing env vars
        let original_server_port = std::env::var("STROM_SERVER_PORT").ok();
        let original_port = std::env::var("STROM_PORT").ok();

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join(".strom.toml");

        // Create a test config file with port 7777
        fs::write(&config_file, "[server]\nport = 7777").unwrap();

        // Set environment variable to override (use STROM_SERVER_PORT to match figment's split logic)
        std::env::set_var("STROM_SERVER_PORT", "8888");

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let config = Config::from_figment(None, None, None, None, None, None).unwrap();

        // Restore (restore dir before temp_dir is dropped, ignore errors)
        let _ = std::env::set_current_dir(&original_dir);

        // Restore env vars
        if let Some(port) = original_server_port {
            std::env::set_var("STROM_SERVER_PORT", port);
        } else {
            std::env::remove_var("STROM_SERVER_PORT");
        }
        if let Some(port) = original_port {
            std::env::set_var("STROM_PORT", port);
        } else {
            std::env::remove_var("STROM_PORT");
        }

        // Env var should override config file
        assert_eq!(config.port, 8888);
    }

    #[test]
    #[serial]
    fn test_from_figment_cli_overrides_env_and_config() {
        // Save any existing env vars
        let original_server_port = std::env::var("STROM_SERVER_PORT").ok();
        let original_port = std::env::var("STROM_PORT").ok();

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join(".strom.toml");

        // Create config file with port 7777
        fs::write(&config_file, "[server]\nport = 7777").unwrap();

        // Set env var to 8888
        std::env::set_var("STROM_SERVER_PORT", "8888");

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        // Pass CLI arg 9999
        let config = Config::from_figment(Some(9999), None, None, None, None, None).unwrap();

        // Restore (restore dir before temp_dir is dropped, ignore errors)
        let _ = std::env::set_current_dir(&original_dir);

        // Restore env vars
        if let Some(port) = original_server_port {
            std::env::set_var("STROM_SERVER_PORT", port);
        } else {
            std::env::remove_var("STROM_SERVER_PORT");
        }
        if let Some(port) = original_port {
            std::env::set_var("STROM_PORT", port);
        } else {
            std::env::remove_var("STROM_PORT");
        }

        // CLI should have highest priority
        assert_eq!(config.port, 9999);
    }

    #[test]
    #[serial]
    fn test_config_file_with_data_dir() {
        // Clear any env vars that might interfere
        std::env::remove_var("STROM_SERVER_PORT");
        std::env::remove_var("STROM_STORAGE_DATA_DIR");

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join(".strom.toml");
        let data_dir = temp_dir.path().join("custom_data");

        let config_content = format!(
            r#"
[server]
port = 8080

[storage]
data_dir = "{}"
"#,
            data_dir.display()
        );
        fs::write(&config_file, config_content).unwrap();

        // Change to temp directory
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        let config = Config::from_figment(None, None, None, None, None, None).unwrap();

        // Restore (ignore errors)
        let _ = std::env::set_current_dir(original_dir);

        assert!(config.flows_path.starts_with(&data_dir));
        assert!(config.blocks_path.starts_with(&data_dir));
    }

    #[test]
    fn test_legacy_config_new() {
        let temp_dir = TempDir::new().unwrap();
        let flows = temp_dir.path().join("flows.json");
        let blocks = temp_dir.path().join("blocks.json");

        let config = Config::new(
            8080,
            None,
            Some(flows.clone()),
            Some(blocks.clone()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(config.port, 8080);
        assert_eq!(config.flows_path, flows);
        assert_eq!(config.blocks_path, blocks);
    }
}
