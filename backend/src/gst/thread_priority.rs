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

    /// Record a failed priority configuration.
    fn record_failure(&self, error_msg: String) {
        // Only store the first error
        let mut error = self.error.lock().unwrap();
        if error.is_none() {
            *error = Some(error_msg);
        }
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
                // Fall back to trying nice value
                warn!("Could not set crossplatform priority, trying nice: {}", e);
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
    flow_id: FlowId,
    thread_registry: Option<ThreadRegistry>,
) -> ThreadPriorityState {
    let state = ThreadPriorityState::new(priority);

    // Check if we have a thread registry before moving it
    let has_registry = thread_registry.is_some();

    // Always set up handler if we have a thread registry (for CPU monitoring),
    // even if priority is Normal
    let need_handler = !matches!(priority, ThreadPriority::Normal) || has_registry;

    if !need_handler {
        info!("Thread priority set to Normal and no thread registry - no sync handler needed");
        state.achieved.store(true, Ordering::SeqCst);
        return state;
    }

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

                    // Register thread with the registry
                    if let Some(ref registry) = thread_registry {
                        // Try to extract block ID from element name (format: "block_id:element_type")
                        let block_id = if owner.contains(':') {
                            owner.split(':').next().map(|s| s.to_string())
                        } else {
                            None
                        };
                        registry.register(thread_id, owner.clone(), flow_id, block_id);
                    }

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
                                warn!(
                                    "Failed to set {:?} priority for streaming thread {} (element: {}, pipeline: {}): {}",
                                    state_clone.requested, thread_id, owner, flow_name, e
                                );
                                state_clone.record_failure(e);
                            }
                        }
                    } else {
                        // For Normal priority, still count as success for status reporting
                        state_clone.record_success();
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
        "Thread priority sync handler installed for pipeline '{}' (requested: {:?}, registry: {})",
        pipeline.name(),
        priority,
        has_registry
    );

    state
}

/// Get the native thread ID of the current thread.
///
/// On Linux, this returns the TID from gettid() syscall, which is needed
/// for /proc/{pid}/task/{tid}/stat access.
fn get_current_thread_native_id() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // Use gettid() syscall to get the actual Linux TID
        // This is different from pthread_t which is what thread_native_id() returns
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }

    #[cfg(target_os = "macos")]
    {
        use thread_priority::thread_native_id;
        thread_native_id() as u64
    }

    #[cfg(target_os = "windows")]
    {
        use thread_priority::thread_native_id;
        thread_native_id() as u64
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        0
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
