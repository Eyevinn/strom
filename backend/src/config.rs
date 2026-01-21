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
    #[serde(default)]
    discovery: DiscoveryConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerConfig {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_ice_servers")]
    ice_servers: Vec<String>,
}

fn default_ice_servers() -> Vec<String> {
    vec!["stun:stun.l.google.com:19302".to_string()]
}

/// Normalize an ICE server URL to RFC 7064/7065 format.
/// Converts GStreamer-style URLs (stun://, turn://) to standard format (stun:, turn:).
fn normalize_ice_server_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("stun://") {
        format!("stun:{}", rest)
    } else if let Some(rest) = url.strip_prefix("turn://") {
        format!("turn:{}", rest)
    } else if let Some(rest) = url.strip_prefix("turns://") {
        format!("turns:{}", rest)
    } else {
        url.to_string()
    }
}

/// Normalize a list of ICE server URLs to RFC format.
fn normalize_ice_servers(servers: Vec<String>) -> Vec<String> {
    servers
        .into_iter()
        .map(|s| normalize_ice_server_url(&s))
        .collect()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryConfig {
    /// SAP multicast addresses to listen on and announce to.
    /// Default: ["239.255.255.255", "224.2.127.254"] (AES67 + global scope)
    #[serde(default = "default_sap_multicast_addresses")]
    sap_multicast_addresses: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            sap_multicast_addresses: default_sap_multicast_addresses(),
        }
    }
}

fn default_sap_multicast_addresses() -> Vec<String> {
    vec![
        "239.255.255.255".to_string(), // AES67/Dante (admin-scoped)
        "224.2.127.254".to_string(),   // Global scope (broadcast)
    ]
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
    /// ICE servers for WebRTC NAT traversal (STUN/TURN)
    /// Format: stun:host:port or turn:user:pass@host:port
    pub ice_servers: Vec<String>,
    /// SAP multicast addresses to listen on and announce to.
    /// Default: ["239.255.255.255", "224.2.127.254"] (AES67 + global scope)
    pub sap_multicast_addresses: Vec<String>,
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
                ice_servers: default_ice_servers(),
            },
            storage: StorageConfig::default(),
            logging: LoggingConfig::default(),
            discovery: DiscoveryConfig::default(),
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
        // Note: Single underscore splits nested keys (e.g., STROM_SERVER_PORT -> server.port)
        // This means field names with underscores can't be set via env vars using this method
        figment = figment.merge(
            Env::prefixed("STROM_")
                .map(|key| key.as_str().replace("__", ".").into())
                .split("_"),
        );

        // 4b. Handle STROM_SERVER_ICE_SERVERS specially (comma-separated array)
        // This needs special handling because:
        // - The split("_") above would turn ICE_SERVERS into ice.servers (wrong)
        // - Figment doesn't parse comma-separated values into arrays
        if let Ok(ice_servers_str) = env::var("STROM_SERVER_ICE_SERVERS") {
            let ice_servers: Vec<String> = ice_servers_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !ice_servers.is_empty() {
                figment = figment.merge(Serialized::default("server.ice_servers", ice_servers));
            }
        }

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
            ice_servers: normalize_ice_servers(config_file.server.ice_servers),
            sap_multicast_addresses: config_file.discovery.sap_multicast_addresses,
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
            ice_servers: default_ice_servers(),
            sap_multicast_addresses: default_sap_multicast_addresses(),
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
                ice_servers: default_ice_servers(),
                sap_multicast_addresses: default_sap_multicast_addresses(),
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

        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join(".strom.toml");

        // Create a test config file with port 7777
        fs::write(&config_file, "[server]\nport = 7777").unwrap();

        // Set environment variable to override (STROM_SERVER_PORT -> server.port)
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

        // Env var should override config file
        assert_eq!(config.port, 8888);
    }

    #[test]
    #[serial]
    fn test_from_figment_cli_overrides_env_and_config() {
        // Save any existing env vars
        let original_server_port = std::env::var("STROM_SERVER_PORT").ok();

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
    #[serial]
    fn test_ice_servers_env_var() {
        // Save any existing env vars
        let original_ice_servers = std::env::var("STROM_SERVER_ICE_SERVERS").ok();

        let temp_dir = TempDir::new().unwrap();
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&temp_dir).unwrap();

        // Set ICE servers env var (comma-separated)
        std::env::set_var(
            "STROM_SERVER_ICE_SERVERS",
            "stun:stun.example.com:3478,turn:user:pass@turn.example.com:3478",
        );

        let config = Config::from_figment(None, None, None, None, None, None).unwrap();

        // Restore
        let _ = std::env::set_current_dir(&original_dir);
        if let Some(ice) = original_ice_servers {
            std::env::set_var("STROM_SERVER_ICE_SERVERS", ice);
        } else {
            std::env::remove_var("STROM_SERVER_ICE_SERVERS");
        }

        assert_eq!(config.ice_servers.len(), 2);
        assert_eq!(config.ice_servers[0], "stun:stun.example.com:3478");
        assert_eq!(
            config.ice_servers[1],
            "turn:user:pass@turn.example.com:3478"
        );
    }

    #[test]
    fn test_ice_servers_normalization() {
        // Test that URLs with :// are normalized to RFC format (without //)
        assert_eq!(
            normalize_ice_server_url("stun://stun.example.com:3478"),
            "stun:stun.example.com:3478"
        );
        assert_eq!(
            normalize_ice_server_url("turn://user:pass@turn.example.com:3478"),
            "turn:user:pass@turn.example.com:3478"
        );
        assert_eq!(
            normalize_ice_server_url("turns://user:pass@turn.example.com:5349"),
            "turns:user:pass@turn.example.com:5349"
        );
        // Already RFC format should remain unchanged
        assert_eq!(
            normalize_ice_server_url("stun:stun.example.com:3478"),
            "stun:stun.example.com:3478"
        );
        assert_eq!(
            normalize_ice_server_url("turn:user:pass@turn.example.com:3478"),
            "turn:user:pass@turn.example.com:3478"
        );
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
