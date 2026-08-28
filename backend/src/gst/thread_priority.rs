//! Thread priority management for GStreamer streaming threads.
//!
//! This module provides functionality to set thread priorities on GStreamer's
//! internal streaming threads using the bus sync handler mechanism.

use crate::thread_registry::ThreadRegistry;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use strom_types::flow::{ThreadPriority, ThreadPriorityStatus};
use strom_types::FlowId;
use tracing::{debug, error, info, warn};

/// Shared state for tracking thread priority configuration across threads.
#[derive(Debug, Clone)]
pub struct ThreadPriorityState {
    /// The requested priority level
    requested: ThreadPriority,
    /// Whether at least one priority setting succeeded
    achieved: Arc<AtomicBool>,
    /// First error encountered (if any)
    error: Arc<std::sync::Mutex<Option<String>>>,
    /// Number of threads configured
    threads_configured: Arc<AtomicU32>,
}

impl ThreadPriorityState {
    /// Create a new thread priority state tracker.
    pub fn new(requested: ThreadPriority) -> Self {
        Self {
            requested,
            achieved: Arc::new(AtomicBool::new(false)),
            error: Arc::new(std::sync::Mutex::new(None)),
            threads_configured: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Record a successful priority configuration.
    fn record_success(&self) {
        self.achieved.store(true, Ordering::SeqCst);
        self.threads_configured.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a failed priority configuration. Returns `true` if this was the
    /// first failure for this flow, so the caller can log loudly once and stay
    /// quiet on the identical failures that follow (e.g. a new mux thread every
    /// segment rotation, all failing the same way).
    fn record_failure(&self, error_msg: String) -> bool {
        let mut error = self.error.lock().unwrap();
        let is_first = error.is_none();
        if is_first {
            *error = Some(error_msg);
        }
        is_first
    }

    /// Get the current status.
    pub fn get_status(&self) -> ThreadPriorityStatus {
        ThreadPriorityStatus {
            requested: self.requested,
            achieved: self.achieved.load(Ordering::SeqCst),
            error: self.error.lock().unwrap().clone(),
            threads_configured: self.threads_configured.load(Ordering::SeqCst),
        }
    }
}

/// Set thread priority for the current thread.
///
/// Returns Ok(()) if priority was set successfully, Err with description otherwise.
pub fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), String> {
    match priority {
        ThreadPriority::Normal => {
            // Normal priority - nothing to do
            debug!("Thread priority set to Normal (no change)");
            Ok(())
        }
        ThreadPriority::High => set_high_priority(),
        ThreadPriority::Realtime => set_realtime_priority(),
    }
}

/// Set high priority (elevated but not realtime).
/// Uses nice value or increased thread priority.
fn set_high_priority() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority as TpThreadPriority};

        // Try to set a high priority (but not realtime)
        // ThreadPriority values go from 0-99, we use a moderate high value
        match set_current_thread_priority(TpThreadPriority::Crossplatform(
            80u8.try_into()
                .map_err(|e| format!("Invalid priority value: {}", e))?,
        )) {
            Ok(()) => {
                debug!("Thread priority set to High (crossplatform 80)");
                Ok(())
            }
            Err(e) => {
                // Fall back to trying nice value. Debug, not warn: this is an
                // intermediate step that fires per thread (every segment rotation
                // for splitmuxsink). The final outcome is logged once per flow by
                // the bus handler's failure arm.
                debug!("Could not set crossplatform priority, trying nice: {}", e);
                set_nice_value(-10)
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use thread_priority::{
            set_current_thread_priority, ThreadPriority as TpThreadPriority, WinAPIThreadPriority,
        };

        match set_current_thread_priority(TpThreadPriority::Os(
            WinAPIThreadPriority::AboveNormal.into(),
        )) {
            Ok(()) => {
                debug!("Thread priority set to High (Windows AboveNormal)");
                Ok(())
            }
            Err(e) => Err(format!("Failed to set high priority on Windows: {}", e)),
        }
    }

    #[cfg(target_os = "macos")]
    {
        use thread_priority::{set_current_thread_priority, ThreadPriority as TpThreadPriority};

        match set_current_thread_priority(TpThreadPriority::Crossplatform(
            80u8.try_into()
                .map_err(|e| format!("Invalid priority value: {}", e))?,
        )) {
            Ok(()) => {
                debug!("Thread priority set to High (macOS crossplatform 80)");
                Ok(())
            }
            Err(e) => Err(format!("Failed to set high priority on macOS: {}", e)),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        warn!("High thread priority not supported on this platform");
        Ok(())
    }
}

/// Set realtime priority (SCHED_FIFO on Linux).
fn set_realtime_priority() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use thread_priority::{
            set_thread_priority_and_policy, thread_native_id, RealtimeThreadSchedulePolicy,
            ThreadPriority as TpThreadPriority, ThreadSchedulePolicy,
        };

        let thread_id = thread_native_id();

        // Use SCHED_FIFO with priority 50 (middle of 1-99 range)
        // This gives good realtime performance without being too aggressive
        match set_thread_priority_and_policy(
            thread_id,
            TpThreadPriority::Crossplatform(
                50u8.try_into()
                    .map_err(|e| format!("Invalid priority: {}", e))?,
            ),
            ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
        ) {
            Ok(()) => {
                info!("Thread priority set to Realtime (SCHED_FIFO priority 50)");
                Ok(())
            }
            Err(e) => {
                let err_msg = format!(
                    "Failed to set realtime priority: {}. \
                     This typically requires root privileges or CAP_SYS_NICE capability. \
                     You can grant this with: sudo setcap cap_sys_nice+ep <binary>",
                    e
                );
                error!("{}", err_msg);
                Err(err_msg)
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use thread_priority::{
            set_current_thread_priority, ThreadPriority as TpThreadPriority, WinAPIThreadPriority,
        };

        match set_current_thread_priority(TpThreadPriority::Os(
            WinAPIThreadPriority::TimeCritical.into(),
        )) {
            Ok(()) => {
                info!("Thread priority set to Realtime (Windows TimeCritical)");
                Ok(())
            }
            Err(e) => Err(format!("Failed to set realtime priority on Windows: {}", e)),
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macOS doesn't support SCHED_FIFO directly, use highest possible priority
        use thread_priority::{set_current_thread_priority, ThreadPriority as TpThreadPriority};

        match set_current_thread_priority(TpThreadPriority::Max) {
            Ok(()) => {
                info!("Thread priority set to Realtime (macOS Max)");
                Ok(())
            }
            Err(e) => Err(format!("Failed to set realtime priority on macOS: {}", e)),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Err("Realtime thread priority not supported on this platform".to_string())
    }
}

/// Set nice value on Linux (fallback for high priority).
#[cfg(target_os = "linux")]
fn set_nice_value(nice: i32) -> Result<(), String> {
    // Use libc to set nice value for current thread
    // Note: setpriority affects the calling thread when using PRIO_PROCESS with 0
    unsafe {
        let result = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
        if result == 0 {
            debug!("Set nice value to {}", nice);
            Ok(())
        } else {
            let errno = *libc::__errno_location();
            Err(format!(
                "Failed to set nice value to {}: errno {}",
                nice, errno
            ))
        }
    }
}

/// Set up a sync handler on the pipeline bus to configure thread priorities
/// and register threads with the thread registry.
///
/// The sync handler is called in the context of the thread that posts the message,
/// which allows us to set the priority of GStreamer's streaming threads as they
/// enter their processing loops, and to capture their native thread IDs.
pub fn setup_thread_priority_handler(
    pipeline: &gst::Pipeline,
    priority: ThreadPriority,
    assigned_cpus: Option<Vec<usize>>,
    flow_id: FlowId,
    thread_registry: Option<ThreadRegistry>,
) -> ThreadPriorityState {
    let state = ThreadPriorityState::new(priority);

    // Check if we have a thread registry before moving it
    let has_registry = thread_registry.is_some();

    // Always set up handler if we have a thread registry (for CPU monitoring),
    // even if priority is Normal
    let need_handler =
        !matches!(priority, ThreadPriority::Normal) || assigned_cpus.is_some() || has_registry;

    if !need_handler {
        info!("Thread priority set to Normal, no affinity, no registry - no sync handler needed");
        state.achieved.store(true, Ordering::SeqCst);
        return state;
    }

    // Pre-compute the CPU affinity mask
    if let Some(ref cpus) = assigned_cpus {
        info!(
            "CPU affinity: flow {} pinned to physical core (CPUs {:?})",
            flow_id, cpus
        );
    }
    let affinity_cpus = assigned_cpus;
    let affinity_cpus_debug = affinity_cpus.clone();

    let Some(bus) = pipeline.bus() else {
        error!("Pipeline has no bus - cannot set up thread priority handler");
        state.record_failure("Pipeline has no bus".to_string());
        return state;
    };

    let state_clone = state.clone();
    let flow_name = pipeline.name().to_string();

    bus.set_sync_handler(move |_bus, msg| {
        use gst::MessageView;

        if let MessageView::StreamStatus(status) = msg.view() {
            let (status_type, owner_element) = status.get();
            let owner = owner_element.name().to_string();

            match status_type {
                gst::StreamStatusType::Enter => {
                    // Get the native thread ID
                    let thread_id = get_current_thread_native_id();

                    debug!(
                        "Thread {} entering streaming loop for element '{}' in pipeline '{}'",
                        thread_id, owner, flow_name
                    );

                    // Set thread priority (if not Normal)
                    if !matches!(state_clone.requested, ThreadPriority::Normal) {
                        match set_current_thread_priority(state_clone.requested) {
                            Ok(()) => {
                                info!(
                                    "Set {:?} priority for streaming thread {} (element: {}, pipeline: {})",
                                    state_clone.requested, thread_id, owner, flow_name
                                );
                                state_clone.record_success();
                            }
                            Err(e) => {
                                if state_clone.record_failure(e.clone()) {
                                    warn!(
                                        "Failed to set {:?} priority for streaming thread {} (element: {}, pipeline: {}): {}. \
                                         Elevated priority needs CAP_SYS_NICE (grant with: sudo setcap cap_sys_nice+ep <binary>). \
                                         Continuing at normal priority; further failures for this flow are logged at debug.",
                                        state_clone.requested, thread_id, owner, flow_name, e
                                    );
                                } else {
                                    debug!(
                                        "Failed to set {:?} priority for thread {} (element: {}): {} (already warned for this flow)",
                                        state_clone.requested, thread_id, owner, e
                                    );
                                }
                            }
                        }
                    } else {
                        // For Normal priority, still count as success for status reporting
                        state_clone.record_success();
                    }

                    // Set CPU affinity (if configured) — track actual result
                    let actual_pinned_cpus = if let Some(ref cpus) = affinity_cpus {
                        match set_thread_cpu_affinity(thread_id, cpus) {
                            Ok(()) => {
                                info!(
                                    "Set CPU affinity {:?} for thread {} (element: {}, pipeline: {})",
                                    cpus, thread_id, owner, flow_name
                                );
                                Some(cpus.clone())
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to set CPU affinity for thread {} (element: {}, pipeline: {}): {}",
                                    thread_id, owner, flow_name, e
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Register thread with the registry (using actual pinned CPUs, not intended)
                    if let Some(ref registry) = thread_registry {
                        // Try to extract block ID from element name (format: "block_id:element_type")
                        let block_id = if owner.contains(':') {
                            owner.split(':').next().map(|s| s.to_string())
                        } else {
                            None
                        };
                        registry.register(thread_id, owner.clone(), flow_id, block_id, actual_pinned_cpus);
                    }
                }
                gst::StreamStatusType::Leave => {
                    let thread_id = get_current_thread_native_id();

                    debug!(
                        "Thread {} leaving streaming loop for element '{}' in pipeline '{}'",
                        thread_id, owner, flow_name
                    );

                    // Unregister thread from the registry
                    if let Some(ref registry) = thread_registry {
                        registry.unregister(thread_id);
                    }
                }
                _ => {}
            }
        }

        // Pass the message to the async handler
        gst::BusSyncReply::Pass
    });

    info!(
        "Thread priority sync handler installed for pipeline '{}' (priority: {:?}, assigned_cpus: {:?}, registry: {})",
        pipeline.name(),
        priority,
        affinity_cpus_debug,
        has_registry
    );

    state
}

/// Get the native thread ID of the current thread.
///
/// The value is used as the key in [`ThreadRegistry`] and as the handle the
/// system monitor samples CPU time with, so each platform returns whatever
/// identifier its sampling API needs.
///
/// On Linux, this returns the TID from gettid() syscall, which is needed
/// for /proc/{pid}/task/{tid}/stat access.
///
/// On macOS, this returns the mach thread port (`mach_port_t`), which is what
/// `thread_info(THREAD_BASIC_INFO)` takes. It is deliberately not the
/// `pthread_t`: the port is captured here, on the streaming thread itself, so
/// the sampler never has to dereference a `pthread_t` whose thread may already
/// have exited.
fn get_current_thread_native_id() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // Use gettid() syscall to get the actual Linux TID
        // This is different from pthread_t which is what thread_native_id() returns
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }

    #[cfg(target_os = "macos")]
    {
        // pthread_mach_thread_np() on the calling thread is documented as safe
        // from any thread and returns the task-local port name for it.
        unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) as u64 }
    }

    #[cfg(target_os = "windows")]
    {
        // Use Windows API directly - GetCurrentThreadId returns DWORD (u32)
        extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        unsafe { GetCurrentThreadId() as u64 }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        0
    }
}

/// Set CPU affinity for a specific thread (Linux only).
#[cfg(target_os = "linux")]
fn set_thread_cpu_affinity(thread_id: u64, cpus: &[usize]) -> Result<(), String> {
    use std::mem;

    unsafe {
        let mut cpuset: libc::cpu_set_t = mem::zeroed();
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut cpuset);
        }
        let ret = libc::sched_setaffinity(
            thread_id as libc::pid_t,
            mem::size_of::<libc::cpu_set_t>(),
            &cpuset,
        );
        if ret == 0 {
            Ok(())
        } else {
            let errno = *libc::__errno_location();
            Err(format!("sched_setaffinity failed: errno {}", errno))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn set_thread_cpu_affinity(_thread_id: u64, _cpus: &[usize]) -> Result<(), String> {
    debug!("CPU affinity not supported on this platform");
    Ok(())
}

/// Shared thread priority configuration for dynamically created session pipelines.
///
/// Created at block-build time (empty), populated in `PipelineManager::start()`,
/// and read by signal callbacks when WebRTC sessions are created.
///
/// webrtcsink's internal session pipelines set their own bus sync handler
/// (returning `BusSyncReply::Drop` and routing messages through an internal
/// channel). We cannot replace or chain that handler without breaking session
/// lifecycle. Instead, we use one-shot `EVENT_DOWNSTREAM` pad probes on each
/// element's sink pad — the probe callback runs on the streaming thread,
/// letting us set priority and register the thread in the same context a
/// sync handler would.
#[derive(Clone, Default)]
pub struct SessionThreadConfig(Arc<std::sync::OnceLock<SessionThreadConfigInner>>);

struct SessionThreadConfigInner {
    priority: ThreadPriority,
    assigned_cpus: Option<Vec<usize>>,
    flow_id: FlowId,
    thread_registry: Option<ThreadRegistry>,
}

impl SessionThreadConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Populate the config with thread priority settings.
    /// Must be called before the pipeline reaches PLAYING (i.e. before sessions are created).
    pub fn populate(
        &self,
        priority: ThreadPriority,
        assigned_cpus: Option<Vec<usize>>,
        flow_id: FlowId,
        thread_registry: Option<ThreadRegistry>,
    ) {
        let _ = self.0.set(SessionThreadConfigInner {
            priority,
            assigned_cpus,
            flow_id,
            thread_registry,
        });
    }

    /// Returns true if thread priority or registration is configured.
    pub fn is_active(&self) -> bool {
        let Some(config) = self.0.get() else {
            return false;
        };
        !matches!(config.priority, ThreadPriority::Normal)
            || config.assigned_cpus.is_some()
            || config.thread_registry.is_some()
    }

    /// Install pad-probe-based thread priority on a session pipeline.
    ///
    /// Call this from the `consumer-pipeline-created` signal handler, which
    /// fires before webrtcsink sets its own bus sync handler. We connect to
    /// `deep-element-added` on the pipeline so that every element that is
    /// added gets a one-shot `EVENT_DOWNSTREAM` probe on its sink pad. The
    /// probe fires on the streaming thread — we set priority, CPU affinity,
    /// and register the thread, then remove the probe.
    pub fn install_on_session_pipeline(&self, pipeline: &gst::Pipeline, session_id: &str) {
        let Some(config) = self.0.get() else {
            warn!(
                "SessionThreadConfig not yet populated when session {} was created",
                session_id
            );
            return;
        };

        if !self.is_active() {
            return;
        }

        info!(
            "Installing pad-probe thread priority on session pipeline '{}' for session {} \
             (priority: {:?}, cpus: {:?})",
            pipeline.name(),
            session_id,
            config.priority,
            config.assigned_cpus
        );

        let priority = config.priority;
        let assigned_cpus = config.assigned_cpus.clone();
        let flow_id = config.flow_id;
        let thread_registry = config.thread_registry.clone();
        let pipeline_name = pipeline.name().to_string();

        // Track which native thread IDs have already been configured so we
        // don't set priority twice when multiple pads share a thread.
        let configured_threads: Arc<std::sync::Mutex<std::collections::HashSet<u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        let if_bin = pipeline
            .clone()
            .upcast::<gst::Element>()
            .downcast::<gst::Bin>();
        let Ok(bin) = if_bin else {
            return;
        };

        bin.connect("deep-element-added", false, move |args| {
            let added: gst::Element = args[2].get().unwrap();

            let sink_pad = added.static_pad("sink")?;

            let priority = priority;
            let assigned_cpus = assigned_cpus.clone();
            let flow_id = flow_id;
            let thread_registry = thread_registry.clone();
            let pipeline_name = pipeline_name.clone();
            let configured = configured_threads.clone();
            let element_name = added.name().to_string();

            sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, _info| {
                let thread_id = get_current_thread_native_id();

                // Skip if this thread was already configured (multiple pads
                // can share the same streaming thread).
                {
                    let mut set = configured.lock().unwrap();
                    if !set.insert(thread_id) {
                        return gst::PadProbeReturn::Remove;
                    }
                }

                // Set thread priority
                if !matches!(priority, ThreadPriority::Normal) {
                    match set_current_thread_priority(priority) {
                        Ok(()) => {
                            info!(
                                "Set {:?} priority for session thread {} (element: {}, pipeline: {})",
                                priority, thread_id, element_name, pipeline_name
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Failed to set {:?} priority for session thread {} (element: {}, pipeline: {}): {}",
                                priority, thread_id, element_name, pipeline_name, e
                            );
                        }
                    }
                }

                // Set CPU affinity
                let actual_pinned_cpus = if let Some(ref cpus) = assigned_cpus {
                    match set_thread_cpu_affinity(thread_id, cpus) {
                        Ok(()) => {
                            info!(
                                "Set CPU affinity {:?} for session thread {} (element: {}, pipeline: {})",
                                cpus, thread_id, element_name, pipeline_name
                            );
                            Some(cpus.clone())
                        }
                        Err(e) => {
                            warn!(
                                "Failed to set CPU affinity for session thread {} (element: {}, pipeline: {}): {}",
                                thread_id, element_name, pipeline_name, e
                            );
                            None
                        }
                    }
                } else {
                    None
                };

                // Register in thread registry
                if let Some(ref registry) = thread_registry {
                    let block_id = if element_name.contains(':') {
                        element_name.split(':').next().map(|s| s.to_string())
                    } else {
                        None
                    };
                    registry.register(
                        thread_id,
                        element_name.clone(),
                        flow_id,
                        block_id,
                        actual_pinned_cpus,
                    );
                }

                gst::PadProbeReturn::Remove
            });

            None
        });
    }
}

/// Remove the sync handler from the pipeline bus.
pub fn remove_thread_priority_handler(pipeline: &gst::Pipeline) {
    if let Some(bus) = pipeline.bus() {
        bus.unset_sync_handler();
        debug!(
            "Thread priority sync handler removed from pipeline '{}'",
            pipeline.name()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thread_priority_state() {
        let state = ThreadPriorityState::new(ThreadPriority::High);

        // Initially not achieved
        let status = state.get_status();
        assert!(!status.achieved);
        assert_eq!(status.threads_configured, 0);
        assert!(status.error.is_none());

        // Record success
        state.record_success();
        let status = state.get_status();
        assert!(status.achieved);
        assert_eq!(status.threads_configured, 1);

        // Record another success
        state.record_success();
        let status = state.get_status();
        assert_eq!(status.threads_configured, 2);
    }

    #[test]
    fn test_thread_priority_state_failure() {
        let state = ThreadPriorityState::new(ThreadPriority::Realtime);

        // Record failure
        state.record_failure("Permission denied".to_string());
        let status = state.get_status();
        assert!(!status.achieved);
        assert_eq!(status.error, Some("Permission denied".to_string()));

        // Second failure doesn't overwrite first
        state.record_failure("Another error".to_string());
        let status = state.get_status();
        assert_eq!(status.error, Some("Permission denied".to_string()));
    }

    #[test]
    fn test_set_normal_priority() {
        // Normal priority should always succeed
        let result = set_current_thread_priority(ThreadPriority::Normal);
        assert!(result.is_ok());
    }
}
