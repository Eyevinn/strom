/// System information embedded at compile time
use chrono::{DateTime, Local};
use std::sync::OnceLock;
use sysinfo::System;

pub use strom_types::api::SystemInfo;

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

/// Get the current system information, collecting runtime data.
pub fn get() -> SystemInfo {
    // Get GStreamer version at runtime
    let (major, minor, micro, nano) = gstreamer::version();
    let gstreamer_version = if nano > 0 {
        format!("{}.{}.{}.{}", major, minor, micro, nano)
    } else {
        format!("{}.{}.{}", major, minor, micro)
    };

    // Get OS info
    let os_info = get_os_info();

    // Check if running in Docker
    let in_docker = is_in_docker();

    // Calculate system boot time from uptime (cross-platform via sysinfo)
    let uptime_seconds = System::uptime() as i64;
    let boot_time = Local::now() - chrono::Duration::seconds(uptime_seconds);
    let system_boot_time = boot_time.to_rfc3339();

    // Get system hostname
    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();

    SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_hash: env!("GIT_HASH").to_string(),
        git_tag: env!("GIT_TAG").to_string(),
        git_branch: env!("GIT_BRANCH").to_string(),
        git_dirty: env!("GIT_DIRTY") == "true",
        build_timestamp: env!("BUILD_TIMESTAMP").to_string(),
        build_id: env!("BUILD_ID").to_string(),
        gstreamer_version,
        os_info,
        in_docker,
        process_started_at: get_process_startup_time().to_rfc3339(),
        system_boot_time,
        hostname,
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
        if content.contains("docker") || content.contains("kubepods") || content.contains("lxc") {
            return true;
        }
    }

    // Method 3: Check for container environment variable
    if std::env::var("container").is_ok() {
        return true;
    }

    // Method 4: Kubernetes injects this into every pod whatever the runtime, so
    // it is the only one of the four that fires under containerd on cgroup v2.
    if strom_types::env::var_opt("KUBERNETES_SERVICE_HOST").is_some() {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn kubernetes_service_host_is_a_container_signal() {
        // Vacuous on a host that is already detected as containerised by one of
        // the other methods, so the baseline goes into the failure message.
        let baseline = is_in_docker();

        std::env::set_var("KUBERNETES_SERVICE_HOST", "10.96.0.1");
        let detected = is_in_docker();
        std::env::remove_var("KUBERNETES_SERVICE_HOST");

        assert!(
            detected,
            "a pod with KUBERNETES_SERVICE_HOST set must be reported as containerised \
             (is_in_docker() was {baseline} before the variable was set)"
        );
    }

    #[test]
    fn test_system_info() {
        let info = get();

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
