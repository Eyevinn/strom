//! System monitoring for CPU and GPU statistics.
//!
//! Stats are collected in a background thread to avoid blocking the async runtime.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use strom_types::{GpuStats, SystemStats};

#[cfg(feature = "nvidia")]
use std::process::Command;

/// System monitor that collects CPU and GPU statistics.
///
/// Stats collection runs in a dedicated background thread to avoid
/// blocking the async runtime, which would cause delays in WebSocket
/// event delivery (meter data, etc).
pub struct SystemMonitor {
    /// Cached stats updated by background thread
    cached_stats: Arc<RwLock<SystemStats>>,
    /// Handle to background thread (dropped when monitor is dropped)
    _collector_handle: Option<thread::JoinHandle<()>>,
}

impl SystemMonitor {
    /// Create a new system monitor with background stats collection.
    pub fn new() -> Self {
        let cached_stats = Arc::new(RwLock::new(SystemStats {
            cpu_usage: 0.0,
            total_memory: 0,
            used_memory: 0,
            gpu_stats: Vec::new(),
            timestamp: 0,
        }));

        let stats_clone = cached_stats.clone();

        // Spawn background thread for stats collection
        let collector_handle = thread::spawn(move || {
            Self::collector_loop(stats_clone);
        });

        Self {
            cached_stats,
            _collector_handle: Some(collector_handle),
        }
    }

    /// Background loop that collects stats periodically.
    fn collector_loop(cached_stats: Arc<RwLock<SystemStats>>) {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );

        #[cfg(feature = "nvidia")]
        let (nvml, use_nvidia_smi_fallback) = match nvml_wrapper::Nvml::init() {
            Ok(nvml) => {
                let count = nvml.device_count().unwrap_or(0);
                tracing::info!("✓ NVML initialized successfully - found {} GPU(s)", count);
                (Some(nvml), false)
            }
            Err(e) => {
                tracing::warn!(
                    "✗ NVML initialization failed: {}. Trying nvidia-smi fallback...",
                    e
                );
                match Command::new("nvidia-smi").arg("-L").output() {
                    Ok(output) if output.status.success() => {
                        let gpu_list = String::from_utf8_lossy(&output.stdout);
                        let count = gpu_list.lines().filter(|l| l.contains("GPU")).count();
                        tracing::info!("✓ nvidia-smi fallback enabled - found {} GPU(s)", count);
                        (None, true)
                    }
                    _ => {
                        tracing::warn!("✗ nvidia-smi also unavailable. GPU monitoring disabled.");
                        (None, false)
                    }
                }
            }
        };

        #[cfg(not(feature = "nvidia"))]
        let (nvml, use_nvidia_smi_fallback): (Option<()>, bool) = (None, false);

        loop {
            // Refresh system information
            system.refresh_cpu_all();
            system.refresh_memory();

            let cpu_usage = system.global_cpu_usage();
            let total_memory = system.total_memory();
            let used_memory = system.used_memory();

            // Collect GPU stats
            #[allow(unused_mut)]
            let mut gpu_stats = Vec::new();

            #[cfg(feature = "nvidia")]
            {
                if let Some(ref nvml) = nvml {
                    gpu_stats = Self::collect_gpu_stats_nvml(nvml);
                } else if use_nvidia_smi_fallback {
                    gpu_stats = Self::collect_gpu_stats_via_nvidia_smi();
                }
            }

            let _ = (nvml.is_none(), use_nvidia_smi_fallback); // Suppress unused warning

            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;

            // Update cached stats
            {
                let mut stats = cached_stats.write();
                *stats = SystemStats {
                    cpu_usage,
                    total_memory,
                    used_memory,
                    gpu_stats,
                    timestamp,
                };
            }

            // Sleep before next collection (900ms to allow some slack before 1s WebSocket interval)
            thread::sleep(Duration::from_millis(900));
        }
    }

    /// Get current system statistics (returns cached values, non-blocking).
    pub async fn collect_stats(&self) -> SystemStats {
        self.cached_stats.read().clone()
    }

    /// Collect GPU statistics from NVML.
    #[cfg(feature = "nvidia")]
    fn collect_gpu_stats_nvml(nvml: &nvml_wrapper::Nvml) -> Vec<GpuStats> {
        let mut gpu_stats = Vec::new();

        match nvml.device_count() {
            Ok(count) => {
                for i in 0..count {
                    match nvml.device_by_index(i) {
                        Ok(device) => {
                            let name = device.name().unwrap_or_else(|_| "Unknown".to_string());

                            let utilization = device
                                .utilization_rates()
                                .map(|u| u.gpu as f32)
                                .unwrap_or(0.0);

                            let memory_info = device.memory_info().ok();
                            let total_memory = memory_info.as_ref().map(|m| m.total).unwrap_or(0);
                            let used_memory = memory_info.as_ref().map(|m| m.used).unwrap_or(0);
                            let memory_utilization = if total_memory > 0 {
                                (used_memory as f32 / total_memory as f32) * 100.0
                            } else {
                                0.0
                            };

                            let temperature = device
                                .temperature(
                                    nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu,
                                )
                                .ok()
                                .map(|t| t as f32);

                            let power_usage = device.power_usage().ok().map(|p| p as f32 / 1000.0);

                            gpu_stats.push(GpuStats {
                                index: i,
                                name,
                                utilization,
                                memory_utilization,
                                total_memory,
                                used_memory,
                                temperature,
                                power_usage,
                            });
                        }
                        Err(e) => {
                            tracing::warn!("Failed to get GPU device {}: {}", i, e);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to get GPU device count: {}", e);
            }
        }

        gpu_stats
    }

    #[cfg(feature = "nvidia")]
    fn collect_gpu_stats_via_nvidia_smi() -> Vec<GpuStats> {
        let mut gpu_stats = Vec::new();

        let output = match Command::new("nvidia-smi")
            .args([
                "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw",
                "--format=csv,noheader,nounits"
            ])
            .env("LD_LIBRARY_PATH", "/usr/lib/wsl/lib")
            .output() {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                tracing::warn!("nvidia-smi failed with status: {}", output.status);
                return gpu_stats;
            }
            Err(e) => {
                tracing::warn!("Failed to execute nvidia-smi: {}", e);
                return gpu_stats;
            }
        };

        let output_str = String::from_utf8_lossy(&output.stdout);

        for line in output_str.lines() {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if parts.len() >= 7 {
                let index = parts[0].parse::<u32>().unwrap_or(0);
                let name = parts[1].to_string();
                let utilization = parts[2].parse::<f32>().unwrap_or(0.0);
                let used_memory = parts[3].parse::<u64>().unwrap_or(0) * 1_048_576;
                let total_memory = parts[4].parse::<u64>().unwrap_or(0) * 1_048_576;
                let memory_utilization = if total_memory > 0 {
                    (used_memory as f32 / total_memory as f32) * 100.0
                } else {
                    0.0
                };
                let temperature = if parts[5] != "[N/A]" {
                    parts[5].parse::<f32>().ok()
                } else {
                    None
                };
                let power_usage = if parts[6] != "[N/A]" {
                    parts[6].parse::<f32>().ok()
                } else {
                    None
                };

                gpu_stats.push(GpuStats {
                    index,
                    name,
                    utilization,
                    memory_utilization,
                    total_memory,
                    used_memory,
                    temperature,
                    power_usage,
                });
            }
        }

        gpu_stats
    }
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self::new()
    }
}
