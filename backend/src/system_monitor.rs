//! System monitoring for CPU and GPU statistics.
//!
//! Stats are collected in a background thread to avoid blocking the async runtime.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::thread_registry::ThreadRegistry;
use strom_types::{GpuStats, SystemStats, ThreadCpuStats, ThreadStats};

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
    /// Signal to stop the background collector thread
    shutdown: Arc<AtomicBool>,
    /// Handle to background thread, joined on drop
    collector_handle: Option<thread::JoinHandle<()>>,
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

        let shutdown = Arc::new(AtomicBool::new(false));
        let stats_clone = cached_stats.clone();
        let shutdown_clone = shutdown.clone();

        // Spawn background thread for stats collection
        let collector_handle = thread::spawn(move || {
            Self::collector_loop(stats_clone, shutdown_clone);
        });

        Self {
            cached_stats,
            shutdown,
            collector_handle: Some(collector_handle),
        }
    }

    /// Background loop that collects stats periodically.
    fn collector_loop(cached_stats: Arc<RwLock<SystemStats>>, shutdown: Arc<AtomicBool>) {
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );

        #[cfg(feature = "nvidia")]
        let (nvml, use_nvidia_smi_fallback) = match nvml_wrapper::Nvml::init() {
            Ok(nvml) => {
                let count = nvml.device_count().unwrap_or(0);
                tracing::info!("NVML initialized successfully - found {} GPU(s)", count);
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
                        tracing::info!("nvidia-smi fallback enabled - found {} GPU(s)", count);
                        (None, true)
                    }
                    _ => {
                        tracing::warn!("nvidia-smi also unavailable. GPU monitoring disabled.");
                        (None, false)
                    }
                }
            }
        };

        #[cfg(not(feature = "nvidia"))]
        let (nvml, use_nvidia_smi_fallback): (Option<()>, bool) = (None, false);

        while !shutdown.load(Ordering::Relaxed) {
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

impl Drop for SystemMonitor {
    fn drop(&mut self) {
        // Signal the background thread to stop
        self.shutdown.store(true, Ordering::Relaxed);

        // Wait for the thread to finish
        if let Some(handle) = self.collector_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Thread CPU sampler for measuring per-thread CPU usage.
///
/// This uses Linux-specific /proc filesystem to read thread CPU times.
/// On other platforms, CPU usage is not available and returns None.
pub struct ThreadCpuSampler {
    /// Previous CPU times for each thread (for delta calculation)
    #[cfg(target_os = "linux")]
    previous_times: HashMap<u64, ThreadCpuTime>,
    /// Previous total CPU time (for delta calculation)
    #[cfg(target_os = "linux")]
    previous_total_time: u64,
    /// Number of CPU cores (for scaling)
    #[cfg(target_os = "linux")]
    num_cpus: usize,
}

/// Get the number of CPUs on this system.
#[cfg(target_os = "linux")]
fn get_num_cpus() -> usize {
    // Use sysinfo to get CPU count (already a dependency)
    let system =
        System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()));
    system.cpus().len().max(1)
}

/// CPU time for a single thread.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct ThreadCpuTime {
    /// User mode CPU time in clock ticks
    utime: u64,
    /// System mode CPU time in clock ticks
    stime: u64,
}

impl ThreadCpuSampler {
    /// Create a new thread CPU sampler.
    #[cfg(target_os = "linux")]
    pub fn new() -> Self {
        Self {
            previous_times: HashMap::new(),
            previous_total_time: 0,
            num_cpus: get_num_cpus(),
        }
    }

    /// Create a new thread CPU sampler (non-Linux stub).
    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Self {
        Self {}
    }

    /// Sample CPU usage for all threads in the registry.
    ///
    /// Returns ThreadStats with CPU usage percentages for each thread.
    /// On non-Linux platforms, returns stats with 0% CPU usage.
    pub fn sample(&mut self, registry: &ThreadRegistry) -> ThreadStats {
        let threads = registry.get_all();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        #[cfg(target_os = "linux")]
        let thread_stats = self.sample_linux(&threads);

        #[cfg(not(target_os = "linux"))]
        let thread_stats = self.sample_stub(&threads);

        ThreadStats {
            threads: thread_stats,
            timestamp,
        }
    }

    /// Linux implementation: read /proc/{pid}/task/{tid}/stat for CPU times.
    #[cfg(target_os = "linux")]
    fn sample_linux(
        &mut self,
        threads: &[crate::thread_registry::ThreadInfo],
    ) -> Vec<ThreadCpuStats> {
        let pid = std::process::id();
        let current_total_time = Self::read_total_cpu_time();

        let mut results = Vec::with_capacity(threads.len());

        for thread in threads {
            let cpu_usage = if let Some(current) = Self::read_thread_cpu_time(pid, thread.thread_id)
            {
                // Calculate delta
                let prev = self.previous_times.get(&thread.thread_id);
                let cpu_usage = if let Some(prev) = prev {
                    let delta_thread =
                        (current.utime + current.stime).saturating_sub(prev.utime + prev.stime);
                    let delta_total = current_total_time.saturating_sub(self.previous_total_time);

                    if delta_total > 0 {
                        // CPU usage formula: (thread_cpu_ticks / total_cpu_ticks) * 100% * num_cpus
                        // - delta_thread: CPU ticks used by this thread (user + system time)
                        // - delta_total: Total CPU ticks across all cores from /proc/stat
                        // - Multiplying by num_cpus normalizes to per-core percentage
                        //   (100% = one full core, 800% max on 8-core system)
                        (delta_thread as f32 / delta_total as f32) * 100.0 * self.num_cpus as f32
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };

                // Store current values for next sample
                self.previous_times.insert(thread.thread_id, current);

                cpu_usage
            } else {
                0.0
            };

            results.push(ThreadCpuStats {
                thread_id: thread.thread_id,
                cpu_usage,
                element_name: thread.element_name.clone(),
                flow_id: thread.flow_id,
                block_id: thread.block_id.clone(),
            });
        }

        // Update total time for next sample
        self.previous_total_time = current_total_time;

        // Clean up old entries for threads that no longer exist
        let active_thread_ids: std::collections::HashSet<u64> =
            threads.iter().map(|t| t.thread_id).collect();
        self.previous_times
            .retain(|id, _| active_thread_ids.contains(id));

        results
    }

    /// Read CPU time for a specific thread from /proc/{pid}/task/{tid}/stat.
    #[cfg(target_os = "linux")]
    fn read_thread_cpu_time(pid: u32, tid: u64) -> Option<ThreadCpuTime> {
        let path = format!("/proc/{}/task/{}/stat", pid, tid);
        let content = std::fs::read_to_string(&path).ok()?;

        // /proc/[pid]/task/[tid]/stat format:
        // pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt
        // cmajflt utime stime cutime cstime ...
        //
        // We need fields 14 (utime) and 15 (stime), which are 0-indexed as 13 and 14
        // But the command name (field 2) can contain spaces and parentheses, so we need
        // to find the closing paren first.
        let close_paren = content.rfind(')')?;
        let fields: Vec<&str> = content[close_paren + 2..].split_whitespace().collect();

        // After (comm), fields are: state(0) ppid(1) pgrp(2) session(3) tty_nr(4)
        // tpgid(5) flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12)
        let utime = fields.get(11)?.parse::<u64>().ok()?;
        let stime = fields.get(12)?.parse::<u64>().ok()?;

        Some(ThreadCpuTime { utime, stime })
    }

    /// Read total CPU time from /proc/stat.
    #[cfg(target_os = "linux")]
    fn read_total_cpu_time() -> u64 {
        if let Ok(content) = std::fs::read_to_string("/proc/stat") {
            // First line is total CPU: cpu user nice system idle iowait irq softirq steal guest guest_nice
            if let Some(cpu_line) = content.lines().next() {
                let parts: Vec<&str> = cpu_line.split_whitespace().collect();
                if parts.len() >= 5 && parts[0] == "cpu" {
                    // Sum all CPU times
                    return parts[1..]
                        .iter()
                        .filter_map(|s| s.parse::<u64>().ok())
                        .sum();
                }
            }
        }
        0
    }

    /// Stub implementation for non-Linux platforms.
    #[cfg(not(target_os = "linux"))]
    fn sample_stub(
        &mut self,
        threads: &[crate::thread_registry::ThreadInfo],
    ) -> Vec<ThreadCpuStats> {
        // On non-Linux platforms, return threads with 0% CPU usage
        threads
            .iter()
            .map(|thread| ThreadCpuStats {
                thread_id: thread.thread_id,
                cpu_usage: 0.0, // Not available on this platform
                element_name: thread.element_name.clone(),
                flow_id: thread.flow_id,
                block_id: thread.block_id.clone(),
            })
            .collect()
    }
}

impl Default for ThreadCpuSampler {
    fn default() -> Self {
        Self::new()
    }
}
