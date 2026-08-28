//! System monitoring for CPU and GPU statistics.
//!
//! Stats are collected in a background thread to avoid blocking the async runtime.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
#[cfg(target_os = "macos")]
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::thread_registry::ThreadRegistry;
use strom_types::{GlRendererInfo, GpuStats, SystemStats, ThreadCpuStats, ThreadStats};

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
    pub fn new(num_cores: usize) -> Self {
        let cached_stats = Arc::new(RwLock::new(SystemStats {
            cpu_usage: 0.0,
            num_cores,
            total_memory: 0,
            used_memory: 0,
            gpu_stats: Vec::new(),
            gl_renderer: None,
            timestamp: 0,
        }));

        let shutdown = Arc::new(AtomicBool::new(false));
        let stats_clone = cached_stats.clone();
        let shutdown_clone = shutdown.clone();

        // Spawn background thread for stats collection
        let collector_handle = thread::spawn(move || {
            Self::collector_loop(stats_clone, shutdown_clone, num_cores);
        });

        Self {
            cached_stats,
            shutdown,
            collector_handle: Some(collector_handle),
        }
    }

    /// Background loop that collects stats periodically.
    fn collector_loop(
        cached_stats: Arc<RwLock<SystemStats>>,
        shutdown: Arc<AtomicBool>,
        num_cores: usize,
    ) {
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
                    "NVML initialization failed: {}. Trying nvidia-smi fallback...",
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

        // Fetch GL renderer info once (already probed at startup)
        let gl_renderer: Option<GlRendererInfo> = crate::gpu::gl_renderer_info();

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
                    num_cores,
                    total_memory,
                    used_memory,
                    gpu_stats,
                    gl_renderer: gl_renderer.clone(),
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
        let num_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::new(num_cores)
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
/// On Linux this reads thread CPU times from the /proc filesystem; on macOS it
/// reads them from mach's `thread_info(THREAD_BASIC_INFO)`. Both report the
/// same quantity in the same units (see [`cpu_usage_percent`]).
/// On other platforms, CPU usage is not available and every thread reports 0%.
pub struct ThreadCpuSampler {
    /// Previous CPU times for each thread (for delta calculation)
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    previous_times: HashMap<u64, ThreadCpuTime>,
    /// Previous total CPU time (for delta calculation)
    #[cfg(target_os = "linux")]
    previous_total_time: u64,
    /// When the previous sample was taken (macOS denominator, see `sample_macos`)
    #[cfg(target_os = "macos")]
    previous_sample_at: Option<Instant>,
    /// Number of CPU cores (for scaling)
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    num_cpus: usize,
}

/// Get the number of CPUs on this system.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn get_num_cpus() -> usize {
    // Use sysinfo to get CPU count (already a dependency)
    let system =
        System::new_with_specifics(RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()));
    system.cpus().len().max(1)
}

/// Convert a CPU-time delta into the percentage reported to the UI.
///
/// `delta_thread` is the CPU time the thread consumed over the interval and
/// `delta_total` is the CPU time available across *all* cores over the same
/// interval; both must be in the same unit (clock ticks on Linux, microseconds
/// on macOS). Scaling the ratio by `num_cpus` normalises the result to
/// per-core percentage: 100.0 is one fully saturated core, and the maximum on
/// an N-core machine is N * 100.0.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cpu_usage_percent(delta_thread: u64, delta_total: u64, num_cpus: usize) -> f32 {
    if delta_total == 0 {
        return 0.0;
    }
    (delta_thread as f32 / delta_total as f32) * 100.0 * num_cpus as f32
}

/// Convert a mach `time_value_t` to microseconds.
///
/// The fields are signed `integer_t`; mach never reports negative CPU time, so
/// clamping at zero simply keeps the conversion total.
#[cfg(target_os = "macos")]
fn time_value_to_micros(t: libc::time_value_t) -> u64 {
    (t.seconds.max(0) as u64) * 1_000_000 + (t.microseconds.max(0) as u64)
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

/// Cumulative user+system CPU time for a single thread, in microseconds.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct ThreadCpuTime {
    total_us: u64,
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

    /// Create a new thread CPU sampler.
    #[cfg(target_os = "macos")]
    pub fn new() -> Self {
        Self {
            previous_times: HashMap::new(),
            previous_sample_at: None,
            num_cpus: get_num_cpus(),
        }
    }

    /// Create a new thread CPU sampler (stub for platforms without sampling).
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn new() -> Self {
        Self {}
    }

    /// Sample CPU usage for all threads in the registry.
    ///
    /// Returns ThreadStats with CPU usage percentages for each thread.
    /// On platforms without a sampling backend, returns stats with 0% CPU usage.
    pub fn sample(&mut self, registry: &ThreadRegistry) -> ThreadStats {
        let threads = registry.get_all();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        #[cfg(target_os = "linux")]
        let thread_stats = self.sample_linux(&threads);

        #[cfg(target_os = "macos")]
        let thread_stats = self.sample_macos(&threads);

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

                    // delta_thread: CPU ticks used by this thread (user + system time)
                    // delta_total: total CPU ticks across all cores from /proc/stat
                    cpu_usage_percent(delta_thread, delta_total, self.num_cpus)
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
                pinned_cpus: thread.pinned_cpus.clone(),
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

    /// macOS implementation: read cumulative thread CPU time via mach's
    /// `thread_info(THREAD_BASIC_INFO)`.
    ///
    /// Thread IDs in the registry are mach thread ports, captured on the
    /// streaming thread itself (see `get_current_thread_native_id`).
    #[cfg(target_os = "macos")]
    fn sample_macos(
        &mut self,
        threads: &[crate::thread_registry::ThreadInfo],
    ) -> Vec<ThreadCpuStats> {
        let now = Instant::now();

        // The Linux path divides the thread's tick delta by the /proc/stat
        // delta, i.e. by the CPU time accumulated across all cores over the
        // interval, and then scales by num_cpus. macOS has no /proc/stat, but
        // that denominator is by construction elapsed wall time times the core
        // count, so computing it directly yields the same percentage with the
        // same meaning: 100.0 is one fully saturated core.
        let delta_total = self
            .previous_sample_at
            .map(|prev| now.duration_since(prev).as_micros() as u64)
            .unwrap_or(0)
            .saturating_mul(self.num_cpus as u64);

        let mut results = Vec::with_capacity(threads.len());

        for thread in threads {
            let cpu_usage = match Self::read_thread_cpu_time(thread.thread_id) {
                Some(current) => {
                    let cpu_usage = match self.previous_times.get(&thread.thread_id) {
                        Some(prev) => {
                            let delta_thread = current.total_us.saturating_sub(prev.total_us);
                            cpu_usage_percent(delta_thread, delta_total, self.num_cpus)
                        }
                        None => 0.0,
                    };

                    // Store current values for next sample
                    self.previous_times.insert(thread.thread_id, current);

                    cpu_usage
                }
                None => {
                    // The thread is gone (or was never a live mach port). Drop
                    // any stored baseline: mach recycles port names, so keeping
                    // it would produce a bogus spike if this name is handed to a
                    // new thread before the registry entry is cleaned up.
                    self.previous_times.remove(&thread.thread_id);
                    0.0
                }
            };

            results.push(ThreadCpuStats {
                thread_id: thread.thread_id,
                cpu_usage,
                element_name: thread.element_name.clone(),
                flow_id: thread.flow_id,
                block_id: thread.block_id.clone(),
                pinned_cpus: thread.pinned_cpus.clone(),
            });
        }

        self.previous_sample_at = Some(now);

        // Clean up old entries for threads that no longer exist
        let active_thread_ids: std::collections::HashSet<u64> =
            threads.iter().map(|t| t.thread_id).collect();
        self.previous_times
            .retain(|id, _| active_thread_ids.contains(id));

        results
    }

    /// Read cumulative user+system CPU time for a mach thread port.
    ///
    /// Returns `None` for a thread that has exited or a port name that was
    /// never valid. mach reports this as `MACH_SEND_INVALID_DEST` rather than
    /// `KERN_INVALID_ARGUMENT`, so any non-success return is treated the same
    /// way: the thread is skipped for this sample. This is a normal race
    /// against thread teardown and happens on every sampling tick until the
    /// registry entry is removed, so it is deliberately not logged.
    #[cfg(target_os = "macos")]
    fn read_thread_cpu_time(mach_port: u64) -> Option<ThreadCpuTime> {
        let port = libc::mach_port_t::try_from(mach_port).ok()?;

        let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::uninit();
        let mut count = libc::THREAD_BASIC_INFO_COUNT;

        // SAFETY: `info` is sized for thread_basic_info and `count` is its
        // length in integer_t units, as thread_info() requires. The struct is
        // only read after a KERN_SUCCESS return, which guarantees mach filled
        // it in.
        let result = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                info.as_mut_ptr() as libc::thread_info_t,
                &mut count,
            )
        };

        if result != libc::KERN_SUCCESS {
            return None;
        }

        let info = unsafe { info.assume_init() };

        Some(ThreadCpuTime {
            total_us: time_value_to_micros(info.user_time) + time_value_to_micros(info.system_time),
        })
    }

    /// Stub implementation for platforms without a sampling backend.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn sample_stub(
        &mut self,
        threads: &[crate::thread_registry::ThreadInfo],
    ) -> Vec<ThreadCpuStats> {
        // Without a sampling backend, return threads with 0% CPU usage
        threads
            .iter()
            .map(|thread| ThreadCpuStats {
                thread_id: thread.thread_id,
                cpu_usage: 0.0, // Not available on this platform
                element_name: thread.element_name.clone(),
                flow_id: thread.flow_id,
                block_id: thread.block_id.clone(),
                pinned_cpus: thread.pinned_cpus.clone(),
            })
            .collect()
    }
}

impl Default for ThreadCpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// The percentage is normalised per core: a thread that consumed a full
    /// core's worth of the interval reads 100%, regardless of core count.
    #[test]
    fn one_saturated_core_reads_100_percent() {
        // 8 cores, so the interval offers 8 units of CPU time in total; a
        // thread using 1 of them has saturated exactly one core.
        assert_eq!(cpu_usage_percent(1_000, 8_000, 8), 100.0);
        // Same thread share on a 4-core machine: 1 of 4 units is still one core.
        assert_eq!(cpu_usage_percent(1_000, 4_000, 4), 100.0);
    }

    #[test]
    fn partial_and_multi_core_usage_scale_linearly() {
        // Half a core.
        assert_eq!(cpu_usage_percent(500, 8_000, 8), 50.0);
        // Three cores' worth.
        assert_eq!(cpu_usage_percent(3_000, 8_000, 8), 300.0);
        // A thread pegging every core reads num_cpus * 100.
        assert_eq!(cpu_usage_percent(8_000, 8_000, 8), 800.0);
    }

    #[test]
    fn idle_thread_reads_zero() {
        assert_eq!(cpu_usage_percent(0, 8_000, 8), 0.0);
    }

    /// The first sample has no previous timestamp, so the denominator is zero.
    /// It must yield 0%, not a division by zero.
    #[test]
    fn zero_interval_reads_zero() {
        assert_eq!(cpu_usage_percent(1_000, 0, 8), 0.0);
    }

    /// A thread that has exited, or an id that was never a live mach port,
    /// must produce no sample rather than a bogus reading. mach reports this as
    /// MACH_SEND_INVALID_DEST; MACH_PORT_NULL reproduces it deterministically.
    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_mach_port_yields_no_sample() {
        // MACH_PORT_NULL
        assert!(ThreadCpuSampler::read_thread_cpu_time(0).is_none());
        // A port name that cannot exist in this task's IPC space.
        assert!(ThreadCpuSampler::read_thread_cpu_time(0x7fff_ffff).is_none());
        // Wider than mach_port_t, so it cannot name a port at all.
        assert!(ThreadCpuSampler::read_thread_cpu_time(u64::MAX).is_none());
    }

    /// A live thread's port must yield a cumulative time that advances as the
    /// thread burns CPU. This is what actually breaks if the mach call, the
    /// flavor, or the time_value conversion is wrong.
    #[cfg(target_os = "macos")]
    #[test]
    fn live_mach_port_reports_advancing_cpu_time() {
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) } as u64;

        let before = ThreadCpuSampler::read_thread_cpu_time(port)
            .expect("the calling thread's own mach port must be readable");

        // Burn a measurable amount of CPU on this thread.
        let mut sink = 0u64;
        let spin_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < spin_until {
            sink = sink.wrapping_add(1);
        }
        assert!(sink > 0);

        let after = ThreadCpuSampler::read_thread_cpu_time(port)
            .expect("the calling thread's own mach port must be readable");

        assert!(
            after.total_us > before.total_us,
            "cumulative CPU time did not advance: {} -> {}",
            before.total_us,
            after.total_us
        );

        // Spinning for 200ms cannot have consumed more than ~200ms of CPU on
        // one thread; a much larger delta means the unit conversion is wrong.
        let delta_us = after.total_us - before.total_us;
        assert!(
            delta_us < 1_000_000,
            "implausible CPU time delta for a 200ms spin: {}us",
            delta_us
        );
    }

    /// The registry stores mach ports on macOS, so a sampler fed a live thread
    /// must report a plausible non-zero percentage on the second sample.
    #[cfg(target_os = "macos")]
    #[test]
    fn sampler_reports_nonzero_for_a_busy_thread() {
        use crate::thread_registry::ThreadRegistry;

        let registry = ThreadRegistry::new();
        let flow_id = uuid::Uuid::new_v4();
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) } as u64;
        registry.register(port, "test-thread".to_string(), flow_id, None, None);

        let mut sampler = ThreadCpuSampler::new();

        // First sample establishes the baseline and reads 0% by construction.
        let first = sampler.sample(&registry);
        assert_eq!(first.threads.len(), 1);
        assert_eq!(first.threads[0].cpu_usage, 0.0);

        let mut sink = 0u64;
        let spin_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < spin_until {
            sink = sink.wrapping_add(1);
        }
        assert!(sink > 0);

        let second = sampler.sample(&registry);
        let cpu = second.threads[0].cpu_usage;
        assert!(
            cpu > 10.0,
            "a thread spinning for the whole interval should read well above 10%, got {}",
            cpu
        );
        // One thread cannot exceed one core.
        assert!(
            cpu < 150.0,
            "single thread reported above one core: {}",
            cpu
        );
    }
}
