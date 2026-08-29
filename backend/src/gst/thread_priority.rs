//! Thread priority management for GStreamer streaming threads.
//!
//! This module provides functionality to set thread priorities on GStreamer's
//! internal streaming threads using the bus sync handler mechanism.
//!
//! On macOS it sets a QoS class on those threads instead of a pthread
//! priority. On Apple Silicon the QoS class — not the priority — is what
//! decides whether a thread is eligible for a performance (P) core, so without
//! it a video streaming thread can be scheduled onto an efficiency core with
//! nothing to signal that it happened. The two settings are mutually exclusive
//! on macOS; see [`set_current_thread_priority`] for why.

use crate::thread_handle::ThreadHandle;
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

/// Configure the calling thread's scheduling for the requested level.
///
/// Linux and Windows set a pthread/Win32 priority. macOS instead sets a QoS
/// class, because on Apple Silicon that — not the priority — is what decides
/// whether the thread is eligible for a performance (P) core.
///
/// The two are mutually exclusive on macOS, in both orders, so this is a
/// replacement rather than an addition:
///
/// * `pthread_setschedparam` first, then `pthread_set_qos_class_self_np` ->
///   the QoS call is refused with `EPERM` and the thread stays `UNSPECIFIED`.
/// * QoS class first, then `pthread_setschedparam` -> the priority call
///   silently resets the class back to `UNSPECIFIED`.
///
/// (Both verified directly on macOS 15 / M2.) Since only the QoS class governs
/// core placement, that is the one worth having, and macOS does not call the
/// priority API at all.
///
/// Returns Ok(()) if the thread was configured, Err with description otherwise.
pub fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        set_current_thread_qos(priority)
    }

    #[cfg(not(target_os = "macos"))]
    {
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
}

/// macOS quality-of-service classes, as defined in `<sys/qos.h>`.
///
/// Kept as our own enum rather than `libc::qos_class_t` because libc only
/// exposes that type through a `pub(crate)` module tree, and because we need
/// `PartialEq` and a total conversion from the raw value for the readback test.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum QosClass {
    UserInteractive = 0x21,
    UserInitiated = 0x19,
    Default = 0x15,
    Utility = 0x11,
    Background = 0x09,
    Unspecified = 0x00,
}

#[cfg(target_os = "macos")]
impl QosClass {
    /// Convert a raw `qos_class_t`. Unknown values map to `Unspecified` — the
    /// kernel only ever returns one of the six documented classes, so this is
    /// a total conversion rather than a lossy one.
    fn from_raw(raw: u32) -> Self {
        match raw {
            0x21 => QosClass::UserInteractive,
            0x19 => QosClass::UserInitiated,
            0x15 => QosClass::Default,
            0x11 => QosClass::Utility,
            0x09 => QosClass::Background,
            _ => QosClass::Unspecified,
        }
    }
}

// `pthread_set_qos_class_self_np` / `pthread_get_qos_class_np` from
// <pthread/qos.h>. The libc crate carries these but only inside its
// `pub(crate)` `new::apple` module tree, so they are not reachable as
// `libc::*`; declaring them here avoids depending on that internal layout.
#[cfg(target_os = "macos")]
extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int)
        -> libc::c_int;
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        qos_class: *mut u32,
        relative_priority: *mut libc::c_int,
    ) -> libc::c_int;
}

/// The QoS class a given [`ThreadPriority`] maps to on macOS.
///
/// `High` maps to `USER_INITIATED` rather than `USER_INTERACTIVE`: both are
/// scheduled on performance cores (only `BACKGROUND` is confined to the
/// efficiency cluster), but `USER_INTERACTIVE` is the band Apple reserves for
/// main-thread UI work, and Strom's own native GUI runs there. Encoder threads
/// saturate every core they are given, so putting them in the same band as the
/// GUI makes the GUI compete with them at equal priority. `USER_INITIATED` —
/// "the user started this and is waiting for the result" — is both the correct
/// description of a live media pipeline and one band below the UI, which keeps
/// the P cores while leaving the interface responsive. `Realtime` is an
/// explicit request for the maximum, so it does take `USER_INTERACTIVE`.
#[cfg(target_os = "macos")]
fn qos_class_for(priority: ThreadPriority) -> Option<QosClass> {
    match priority {
        // Normal means "do not touch this thread's scheduling"; leaving the
        // class alone lets it keep whatever it inherited.
        ThreadPriority::Normal => None,
        ThreadPriority::High => Some(QosClass::UserInitiated),
        ThreadPriority::Realtime => Some(QosClass::UserInteractive),
    }
}

/// Set the macOS QoS class of the calling thread.
///
/// This has to run *on* the streaming thread, which is why it is called from
/// the bus `StreamStatus::Enter` handler and the session pad probe rather than
/// from wherever the pipeline is built: a thread that never sets a class of
/// its own gets `QOS_CLASS_DEFAULT`, so a GStreamer streaming thread spawned
/// from a tokio worker would otherwise never be placed deliberately at all.
///
/// Calling it there also reaches the element-internal worker threads that never
/// post a `StreamStatus` message of their own — libx264's frame threads, for
/// example. Those are created from the streaming thread during caps
/// negotiation, which happens after `Enter`, and a thread created by a thread
/// that *has* an explicit class inherits it.
#[cfg(target_os = "macos")]
fn set_current_thread_qos(priority: ThreadPriority) -> Result<(), String> {
    let Some(preferred) = qos_class_for(priority) else {
        debug!("Thread priority set to Normal (QoS class left as inherited)");
        return Ok(());
    };

    // A task can be running under a QoS clamp — launchd jobs get one, and so
    // does anything spawned by an already-clamped parent. A class above the
    // clamp is refused with EPERM rather than quietly lowered, and
    // USER_INTERACTIVE is the one that gets refused in practice.
    // USER_INITIATED is still a performance-core class, so fall back to it
    // instead of leaving the thread with no class at all.
    let class = match try_set_qos(preferred) {
        Ok(()) => preferred,
        Err(rc) if preferred == QosClass::UserInteractive && rc == libc::EPERM => {
            try_set_qos(QosClass::UserInitiated).map_err(|rc2| {
                format!(
                    "pthread_set_qos_class_self_np({:?}) failed: errno {}, \
                     and the {:?} fallback failed: errno {}",
                    preferred,
                    rc,
                    QosClass::UserInitiated,
                    rc2
                )
            })?;
            debug!(
                "QoS class {:?} refused (EPERM, task is clamped); fell back to {:?}",
                preferred,
                QosClass::UserInitiated
            );
            QosClass::UserInitiated
        }
        Err(rc) => {
            return Err(format!(
                "pthread_set_qos_class_self_np({:?}) failed: errno {}",
                preferred, rc
            ))
        }
    };

    // Read it back rather than trusting the return code. A thread that has had
    // its scheduling parameters set directly can end up UNSPECIFIED with the
    // call still reporting success, and a silently unplaced thread is exactly
    // the failure this module exists to prevent.
    match current_thread_qos() {
        Ok((actual, _)) if actual == class => {
            debug!("Thread QoS class set to {:?}", class);
            Ok(())
        }
        Ok((actual, _)) => Err(format!(
            "QoS class requested {:?} but thread reads back as {:?}",
            class, actual
        )),
        Err(e) => Err(e),
    }
}

/// Set the calling thread's QoS class, returning the raw errno on failure.
///
/// Relative priority 0 = the top of the band; the argument must be <= 0.
#[cfg(target_os = "macos")]
fn try_set_qos(class: QosClass) -> Result<(), libc::c_int> {
    let rc = unsafe { pthread_set_qos_class_self_np(class as u32, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(rc)
    }
}

/// Read back the calling thread's QoS class and relative priority.
#[cfg(target_os = "macos")]
fn current_thread_qos() -> Result<(QosClass, i32), String> {
    let mut raw: u32 = 0;
    let mut relative: libc::c_int = 0;
    let rc = unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut raw, &mut relative) };
    if rc == 0 {
        Ok((QosClass::from_raw(raw), relative as i32))
    } else {
        Err(format!("pthread_get_qos_class_np failed: errno {}", rc))
    }
}

/// Set high priority (elevated but not realtime).
/// Uses nice value or increased thread priority.
///
/// Not built on macOS: there the QoS class replaces this entirely, and calling
/// it would make the QoS class unsettable. See [`set_current_thread_priority`].
#[cfg(not(target_os = "macos"))]
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

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        warn!("High thread priority not supported on this platform");
        Ok(())
    }
}

/// Set realtime priority (SCHED_FIFO on Linux).
///
/// Not built on macOS, for the same reason as [`set_high_priority`].
#[cfg(not(target_os = "macos"))]
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

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
                    // Captured here, on the streaming thread itself, so the
                    // handle owns whatever reference makes it safe to sample
                    // after this thread exits.
                    let handle = ThreadHandle::current();
                    let thread_id = handle.id();

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
                        registry.register(handle, owner.clone(), flow_id, block_id, actual_pinned_cpus);
                    }
                }
                gst::StreamStatusType::Leave => {
                    let thread_id = ThreadHandle::current().id();

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
                // Captured on the streaming thread itself. These threads never
                // send a Leave message, so the handle lives until the flow's
                // entries are dropped by unregister_flow.
                let handle = ThreadHandle::current();
                let thread_id = handle.id();

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
                        handle,
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

    /// Each case runs on its own thread: a QoS class sticks to the thread that
    /// set it, and libtest reuses its worker threads between tests.
    #[cfg(target_os = "macos")]
    fn on_fresh_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().expect("test thread panicked")
    }

    /// `High` must leave the thread in `USER_INITIATED`.
    ///
    /// This is the guard for the whole change: dropping the QoS call, or
    /// reinstating a `pthread_setschedparam` call on the same thread, leaves
    /// the thread `UNSPECIFIED` and this assertion fails.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_high_priority_thread_is_user_initiated() {
        on_fresh_thread(|| {
            set_current_thread_priority(ThreadPriority::High)
                .expect("High priority should be settable without privileges");

            let (class, relative) = current_thread_qos().expect("QoS class should read back");
            assert_eq!(
                class,
                QosClass::UserInitiated,
                "pipeline threads must carry an explicit QoS class; \
                 UNSPECIFIED means they are not pinned to the performance cluster"
            );
            assert_eq!(
                relative, 0,
                "relative priority should be the top of the band"
            );
        });
    }

    /// `Realtime` asks for the maximum band, but `USER_INTERACTIVE` is refused
    /// with EPERM when the task runs under a QoS clamp (a launchd job, or a
    /// child of an already-clamped process), so the thread may legitimately
    /// land on the `USER_INITIATED` fallback. Either way it must end up in a
    /// performance-core band and never `UNSPECIFIED`.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_realtime_thread_lands_in_a_performance_band() {
        on_fresh_thread(|| {
            set_current_thread_priority(ThreadPriority::Realtime)
                .expect("Realtime should fall back rather than fail");

            let (class, _) = current_thread_qos().expect("QoS class should read back");
            assert!(
                matches!(class, QosClass::UserInteractive | QosClass::UserInitiated),
                "Realtime landed in {:?}, which is not a performance-core band",
                class
            );
        });
    }

    /// `Normal` means "do not touch this thread's scheduling" — including its
    /// QoS class, which it should keep inheriting from its creator.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_normal_priority_leaves_qos_class_alone() {
        on_fresh_thread(|| {
            let (before, _) = current_thread_qos().expect("QoS class should read back");
            set_current_thread_priority(ThreadPriority::Normal).expect("Normal always succeeds");
            let (after, _) = current_thread_qos().expect("QoS class should read back");
            assert_eq!(before, after);
        });
    }

    /// Every documented `qos_class_t` value round-trips, so the readback above
    /// cannot pass by accident through the unknown-value fallback.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_qos_class_round_trips_from_raw() {
        for class in [
            QosClass::UserInteractive,
            QosClass::UserInitiated,
            QosClass::Default,
            QosClass::Utility,
            QosClass::Background,
            QosClass::Unspecified,
        ] {
            assert_eq!(QosClass::from_raw(class as u32), class);
        }
    }
}
