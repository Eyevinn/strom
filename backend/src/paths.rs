//! Cross-platform data path resolution.
//!
//! Provides utilities for determining where to store application data files
//! (flows.json, blocks.json) based on platform conventions and Docker detection.

use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Represents the resolved paths for application data storage.
#[derive(Debug, Clone)]
pub struct DataPaths {
    /// Path to flows storage file
    pub flows_path: PathBuf,
    /// Path to blocks storage file
    pub blocks_path: PathBuf,
    /// Path to media files directory
    pub media_path: PathBuf,
}

/// Configuration for path resolution.
#[derive(Debug, Default)]
pub struct PathConfig {
    /// Explicit data directory (flows.json and blocks.json will be inside)
    pub data_dir: Option<PathBuf>,
    /// Explicit path to flows file
    pub flows_path: Option<PathBuf>,
    /// Explicit path to blocks file
    pub blocks_path: Option<PathBuf>,
    /// Explicit path to media files directory
    pub media_path: Option<PathBuf>,
}

impl DataPaths {
    /// Resolve data paths based on configuration.
    ///
    /// Priority (highest to lowest):
    /// 1. Explicit flows_path/blocks_path if provided
    /// 2. Explicit data_dir if provided
    /// 3. Default directory (platform-specific or Docker-detected)
    pub fn resolve(config: PathConfig) -> anyhow::Result<Self> {
        // Determine base directory
        let base_dir = if config.data_dir.is_some() {
            config.data_dir.clone().unwrap()
        } else {
            Self::default_data_dir()?
        };

        // Ensure base directory exists
        if !base_dir.exists() {
            std::fs::create_dir_all(&base_dir)?;
            info!("Created data directory: {}", base_dir.display());
        }

        // Resolve flows path (individual path overrides base_dir)
        let flows_path = if let Some(path) = config.flows_path {
            Self::log_path_override("flows", &path, &base_dir);
            path
        } else {
            base_dir.join("flows.json")
        };

        // Resolve blocks path (individual path overrides base_dir)
        let blocks_path = if let Some(path) = config.blocks_path {
            Self::log_path_override("blocks", &path, &base_dir);
            path
        } else {
            base_dir.join("blocks.json")
        };

        // Resolve media path (individual path overrides default ./media)
        let media_path = if let Some(path) = config.media_path {
            Self::log_path_override("media", &path, &base_dir);
            path
        } else {
            // Default to ./media in current working directory
            PathBuf::from("./media")
        };

        // Ensure media directory exists
        if !media_path.exists() {
            std::fs::create_dir_all(&media_path)?;
            info!("Created media directory: {}", media_path.display());
        }

        // Check for legacy files in current directory
        Self::check_legacy_files(&flows_path, &blocks_path);

        info!("Data paths resolved:");
        info!("  Flows:  {}", flows_path.display());
        info!("  Blocks: {}", blocks_path.display());
        info!("  Media:  {}", media_path.display());

        Ok(Self {
            flows_path,
            blocks_path,
            media_path,
        })
    }

    /// Determine the default data directory based on platform and environment.
    fn default_data_dir() -> anyhow::Result<PathBuf> {
        // Check if running in Docker
        if Self::is_docker() {
            info!("Docker environment detected, using ./data/ for storage");
            return Ok(PathBuf::from("./data"));
        }

        // Use platform-specific user data directory
        if let Some(proj_dirs) = ProjectDirs::from("com", "eyevinn", "strom") {
            let data_dir = proj_dirs.data_dir().to_path_buf();
            info!(
                "Using platform-specific data directory: {}",
                data_dir.display()
            );
            Ok(data_dir)
        } else {
            // Fallback to current directory if ProjectDirs fails
            warn!("Could not determine user data directory, falling back to ./data/");
            Ok(PathBuf::from("./data"))
        }
    }

    /// Detect if running inside a Docker container.
    fn is_docker() -> bool {
        // Check for /.dockerenv file (standard Docker indicator)
        if Path::new("/.dockerenv").exists() {
            return true;
        }

        // Check for Docker-specific cgroup entries
        if let Ok(cgroup) = std::fs::read_to_string("/proc/self/cgroup") {
            if cgroup.contains("docker") || cgroup.contains("containerd") {
                return true;
            }
        }

        false
    }

    /// Log when an individual path overrides the base directory.
    fn log_path_override(file_type: &str, path: &Path, base_dir: &Path) {
        let base_path = base_dir.join(format!("{}.json", file_type));
        if path != base_path {
            info!(
                "Using custom {} path: {} (overriding default: {})",
                file_type,
                path.display(),
                base_path.display()
            );
        }
    }

    /// Check for legacy files in the current directory and warn if found.
    fn check_legacy_files(flows_path: &Path, blocks_path: &Path) {
        let cwd_flows = Path::new("./flows.json");
        let cwd_blocks = Path::new("./blocks.json");

        // Only warn if:
        // 1. Legacy file exists in current directory
        // 2. It's different from the resolved path
        if cwd_flows.exists() && cwd_flows.canonicalize().ok() != flows_path.canonicalize().ok() {
            warn!(
                "Found legacy flows.json in current directory, but using: {}",
                flows_path.display()
            );
            warn!("Consider moving your data or using --flows-path to specify the location");
        }

        if cwd_blocks.exists() && cwd_blocks.canonicalize().ok() != blocks_path.canonicalize().ok()
        {
            warn!(
                "Found legacy blocks.json in current directory, but using: {}",
                blocks_path.display()
            );
            warn!("Consider moving your data or using --blocks-path to specify the location");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_data_dir() {
        let data_dir = DataPaths::default_data_dir().unwrap();
        // Should return a valid path
        assert!(!data_dir.as_os_str().is_empty());
    }

    #[test]
    fn test_resolve_with_explicit_paths() {
        let config = PathConfig {
            data_dir: None,
            flows_path: Some(PathBuf::from("/custom/flows.json")),
            blocks_path: Some(PathBuf::from("/custom/blocks.json")),
            media_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.flows_path, PathBuf::from("/custom/flows.json"));
        assert_eq!(paths.blocks_path, PathBuf::from("/custom/blocks.json"));
    }

    #[test]
    fn test_resolve_with_data_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            flows_path: None,
            blocks_path: None,
            media_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.flows_path, temp_dir.path().join("flows.json"));
        assert_eq!(paths.blocks_path, temp_dir.path().join("blocks.json"));
    }

    #[test]
    fn test_individual_paths_override_data_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            flows_path: Some(PathBuf::from("/override/flows.json")),
            blocks_path: None,
            media_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.flows_path, PathBuf::from("/override/flows.json"));
        assert_eq!(paths.blocks_path, temp_dir.path().join("blocks.json"));
    }
}
