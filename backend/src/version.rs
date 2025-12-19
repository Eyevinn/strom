/// Version and build information embedded at compile time
use chrono::{DateTime, Local};
use serde::Serialize;
use std::sync::OnceLock;
use sysinfo::System;
use utoipa::ToSchema;

/// Global process startup time - initialized once when the Strom process starts
static PROCESS_STARTUP_TIME: OnceLock<DateTime<Local>> = OnceLock::new();

/// Initialize the process startup time. Should be called once at process startup.
pub fn init_process_startup_time() {
    PROCESS_STARTUP_TIME.get_or_init(Local::now);
}

/// Get the process startup time (returns current time if not initialized)
pub fn get_process_startup_time() -> DateTime<Local> {
    *PROCESS_STARTUP_TIME.get_or_init(Local::now)
}

/// Build and version information
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VersionInfo {
    /// Package version from Cargo.toml
    pub version: &'static str,
    /// Git commit hash (short)
    pub git_hash: &'static str,
    /// Git tag (if on a tagged commit)
    pub git_tag: &'static str,
    /// Git branch name
    pub git_branch: &'static str,
    /// Whether the working directory had uncommitted changes
    pub git_dirty: bool,
    /// Build timestamp (ISO 8601 format)
    pub build_timestamp: &'static str,
    /// GStreamer runtime version
    pub gstreamer_version: String,
    /// Operating system name and version
    pub os_info: String,
    /// Whether running inside a Docker container
    pub in_docker: bool,
    /// When the Strom server process was started (ISO 8601 format with timezone)
    /// This is the process uptime, not the system uptime
    pub process_started_at: String,
    /// When the system was booted (ISO 8601 format with timezone)
    /// Cross-platform via sysinfo crate
    pub system_boot_time: String,
}

impl VersionInfo {
    /// Get the current version information
    pub fn get() -> Self {
        // Get GStreamer version at runtime
        let (major, minor, micro, nano) = gstreamer::version();
        let gstreamer_version = if nano > 0 {
            format!("{}.{}.{}.{}", major, minor, micro, nano)
        } else {
            format!("{}.{}.{}", major, minor, micro)
        };

        // Get OS info
        let os_info = Self::get_os_info();

        // Check if running in Docker
        let in_docker = Self::is_in_docker();

        // Calculate system boot time from uptime (cross-platform via sysinfo)
        let uptime_seconds = System::uptime() as i64;
        let boot_time = Local::now() - chrono::Duration::seconds(uptime_seconds);
        let system_boot_time = boot_time.to_rfc3339();

        Self {
            version: env!("CARGO_PKG_VERSION"),
            git_hash: env!("GIT_HASH"),
            git_tag: env!("GIT_TAG"),
            git_branch: env!("GIT_BRANCH"),
            git_dirty: env!("GIT_DIRTY") == "true",
            build_timestamp: env!("BUILD_TIMESTAMP"),
            gstreamer_version,
            os_info,
            in_docker,
            process_started_at: get_process_startup_time().to_rfc3339(),
            system_boot_time,
        }
    }

    /// Get OS name and version
    fn get_os_info() -> String {
        // Try to read /etc/os-release for Linux distributions
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            let mut name = None;
            let mut version = None;

            for line in content.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    // PRETTY_NAME is the best single field, use it directly
                    let value = value.trim_matches('"');
                    return value.to_string();
                }
                if let Some(value) = line.strip_prefix("NAME=") {
                    name = Some(value.trim_matches('"').to_string());
                }
                if let Some(value) = line.strip_prefix("VERSION=") {
                    version = Some(value.trim_matches('"').to_string());
                }
            }

            // Fall back to NAME + VERSION if PRETTY_NAME wasn't found
            match (name, version) {
                (Some(n), Some(v)) => format!("{} {}", n, v),
                (Some(n), None) => n,
                _ => format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            }
        } else {
            // Fall back to basic OS info from std::env::consts
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
        }
    }

    /// Check if running inside a Docker container
    fn is_in_docker() -> bool {
        // Method 1: Check for .dockerenv file
        if std::path::Path::new("/.dockerenv").exists() {
            return true;
        }

        // Method 2: Check cgroup for docker/container references
        if let Ok(content) = std::fs::read_to_string("/proc/1/cgroup") {
            if content.contains("docker") || content.contains("kubepods") || content.contains("lxc")
            {
                return true;
            }
        }

        // Method 3: Check for container environment variable
        if std::env::var("container").is_ok() {
            return true;
        }

        false
    }

    /// Get a human-readable version string
    ///
    /// Returns:
    /// - "v0.1.0" if on a tagged release
    /// - "v0.1.0-dev+abc12345" if on main/master without tag
    /// - "v0.1.0-dev+abc12345-dirty" if there are uncommitted changes
    pub fn version_string(&self) -> String {
        if !self.git_tag.is_empty() {
            // On a tagged release
            self.git_tag.to_string()
        } else {
            // Development version
            let mut version = format!("v{}-dev+{}", self.version, self.git_hash);
            if self.git_dirty {
                version.push_str("-dirty");
            }
            version
        }
    }

    /// Get a short version string for display
    ///
    /// Returns:
    /// - "v0.1.0" if on a tagged release
    /// - "v0.1.0-dev" if not on a tag
    pub fn short_version(&self) -> String {
        if !self.git_tag.is_empty() {
            self.git_tag.to_string()
        } else {
            format!("v{}-dev", self.version)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_info() {
        let info = VersionInfo::get();

        // These should always be set by build.rs
        assert!(!info.version.is_empty());
        assert!(!info.git_hash.is_empty());
        assert!(!info.build_timestamp.is_empty());

        // These might be empty depending on git state
        // but shouldn't panic
        let _ = info.version_string();
        let _ = info.short_version();
    }
}
