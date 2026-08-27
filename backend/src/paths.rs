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
    /// Resolved data directory the individual paths default to
    pub data_dir: PathBuf,
    /// Path to flows storage file
    pub flows_path: PathBuf,
    /// Path to blocks storage file
    pub blocks_path: PathBuf,
    /// Path to media files directory
    pub media_path: PathBuf,
    /// Directory for the CEF/Chromium profile used by `cefsrc`
    pub cef_cache_path: PathBuf,
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
    /// Explicit path to the CEF cache directory
    pub cef_cache_path: Option<PathBuf>,
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

        // Resolve media path (individual path overrides base_dir)
        let media_path = if let Some(path) = config.media_path {
            Self::log_path_override("media", &path, &base_dir);
            path
        } else {
            base_dir.join("media")
        };

        // Ensure media directory exists
        if !media_path.exists() {
            std::fs::create_dir_all(&media_path)?;
            info!("Created media directory: {}", media_path.display());
        }

        // Resolve the CEF cache directory. Unlike the paths above it defaults
        // outside the data directory: it holds a Chromium profile (cookies,
        // local storage, HTTP cache), which is disposable state rather than
        // something to back up alongside flows and media.
        let cef_cache_path = config
            .cef_cache_path
            .unwrap_or_else(|| Self::default_cef_cache_dir(&base_dir));

        // Check for legacy files in current directory
        Self::check_legacy_files(&flows_path, &blocks_path);

        info!("Data paths resolved:");
        info!("  Flows:  {}", flows_path.display());
        info!("  Blocks: {}", blocks_path.display());
        info!("  Media:  {}", media_path.display());
        info!("  CEF:    {}", cef_cache_path.display());

        Ok(Self {
            data_dir: base_dir,
            flows_path,
            blocks_path,
            media_path,
            cef_cache_path,
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

    /// Default directory for the CEF/Chromium profile used by `cefsrc`.
    ///
    /// Chromium keys its process singleton on this directory, so two Strom
    /// instances sharing one leave the second unable to start any `cefsrc`
    /// element. The directory therefore has to be per-instance, and the data
    /// directory is what makes an instance distinct (two instances sharing a
    /// data directory are already broken — both write flows.json).
    ///
    /// The profile lives in the OS cache directory rather than the data
    /// directory: it is disposable state, and clearing an OS cache directory
    /// is a move operators already know. The name is a hash of the data
    /// directory so the profile stays both warm across restarts and isolated
    /// between instances.
    fn default_cef_cache_dir(base_dir: &Path) -> PathBuf {
        // Canonicalize so ./data and /abs/path/data resolve to one profile.
        // base_dir exists by this point, but fall back to the path as given.
        let key = base_dir
            .canonicalize()
            .unwrap_or_else(|_| base_dir.to_path_buf());
        let dir_name = format!("cef-{:016x}", Self::path_hash(&key));

        match ProjectDirs::from("com", "eyevinn", "strom") {
            Some(proj_dirs) => proj_dirs.cache_dir().join(dir_name),
            None => {
                warn!("Could not determine user cache directory, keeping the CEF profile in the data directory");
                base_dir.join(dir_name)
            }
        }
    }

    /// FNV-1a hash of a path, used to name the CEF cache directory.
    ///
    /// Hand-rolled rather than `DefaultHasher` because the value ends up in a
    /// directory name that has to stay stable across Rust versions — a changed
    /// hash would silently orphan the existing profile.
    fn path_hash(path: &Path) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in path.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
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
            cef_cache_path: None,
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
            cef_cache_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.flows_path, temp_dir.path().join("flows.json"));
        assert_eq!(paths.blocks_path, temp_dir.path().join("blocks.json"));
        assert_eq!(paths.media_path, temp_dir.path().join("media"));
    }

    #[test]
    fn test_individual_paths_override_data_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            flows_path: Some(PathBuf::from("/override/flows.json")),
            blocks_path: None,
            media_path: None,
            cef_cache_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.flows_path, PathBuf::from("/override/flows.json"));
        assert_eq!(paths.blocks_path, temp_dir.path().join("blocks.json"));
    }

    #[test]
    fn test_explicit_media_path_override() {
        let temp_dir = tempfile::tempdir().unwrap();
        let media_dir = temp_dir.path().join("custom_media");
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            flows_path: None,
            blocks_path: None,
            media_path: Some(media_dir.clone()),
            cef_cache_path: None,
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.media_path, media_dir);
    }

    #[test]
    fn test_resolve_exposes_data_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            ..Default::default()
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.data_dir, temp_dir.path());
    }

    #[test]
    fn test_cef_cache_differs_per_data_dir() {
        // Chromium's process singleton is keyed on this directory, so two
        // instances with distinct data directories must not share it.
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let first_paths = DataPaths::resolve(PathConfig {
            data_dir: Some(first.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let second_paths = DataPaths::resolve(PathConfig {
            data_dir: Some(second.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_ne!(first_paths.cef_cache_path, second_paths.cef_cache_path);
    }

    #[test]
    fn test_cef_cache_is_stable_for_one_data_dir() {
        // The profile is only warm across restarts if the derivation is
        // deterministic for a given data directory.
        let temp_dir = tempfile::tempdir().unwrap();
        let resolve = || {
            DataPaths::resolve(PathConfig {
                data_dir: Some(temp_dir.path().to_path_buf()),
                ..Default::default()
            })
            .unwrap()
            .cef_cache_path
        };

        assert_eq!(resolve(), resolve());
    }

    #[test]
    fn test_cef_cache_ignores_flows_path_override() {
        // flows_path is independently overridable, so deriving the cache from
        // its parent would put two instances back on one directory whenever
        // they share a flows directory (or use a bare relative filename).
        let temp_dir = tempfile::tempdir().unwrap();
        let shared_config_dir = tempfile::tempdir().unwrap();

        let baseline = DataPaths::resolve(PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        let overridden = DataPaths::resolve(PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            flows_path: Some(shared_config_dir.path().join("flows.json")),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(baseline.cef_cache_path, overridden.cef_cache_path);
        assert!(!overridden
            .cef_cache_path
            .starts_with(shared_config_dir.path()));
    }

    #[test]
    fn test_explicit_cef_cache_path_override() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cef_dir = temp_dir.path().join("custom_cef");
        let config = PathConfig {
            data_dir: Some(temp_dir.path().to_path_buf()),
            cef_cache_path: Some(cef_dir.clone()),
            ..Default::default()
        };

        let paths = DataPaths::resolve(config).unwrap();
        assert_eq!(paths.cef_cache_path, cef_dir);
    }
}
