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
/// GStreamer's streaming threads come from a shared pool and are reused, so an
/// elevation must not outlive the flow that asked for it. Before the first
/// elevation the thread's own scheduling is saved, and
/// [`restore_current_thread_priority`] puts it back. `Normal` means "do not
/// touch this thread's scheduling": it undoes an elevation this module left on
/// the thread, and leaves any other thread as it is.
///
/// Returns Ok(()) if the thread was configured, Err with description otherwise.
pub fn set_current_thread_priority(priority: ThreadPriority) -> Result<(), String> {
    if matches!(priority, ThreadPriority::Normal) {
        return restore_current_thread_priority();
    }

    // Refuse to elevate a thread whose scheduling we could not save: it would
    // go back to the pool elevated with nothing to restore it from.
    BASELINE.with(|baseline| {
        let mut baseline = baseline.borrow_mut();
        if baseline.is_none() {
            *baseline = Some(current_scheduling()?);
        }
        Ok::<(), String>(())
    })?;

    elevate_current_thread(priority)
}

/// Undo the elevation [`set_current_thread_priority`] applied to the calling
/// thread, restoring the scheduling it had before. A thread this module never
/// elevated is left alone.
pub fn restore_current_thread_priority() -> Result<(), String> {
    let Some(saved) = BASELINE.with(|baseline| baseline.borrow_mut().take()) else {
        return Ok(());
    };
    match apply_scheduling(&saved) {
        Ok(()) => {
            debug!("Thread scheduling restored to {:?}", saved);
            Ok(())
        }
        Err(e) => {
            // Keep it, so the next Leave or Normal flow on this thread retries.
            BASELINE.with(|baseline| *baseline.borrow_mut() = Some(saved));
            Err(e)
        }
    }
}

thread_local! {
    /// The calling thread's scheduling from before this module first elevated
    /// it. `None` means this module has not changed the thread.
    static BASELINE: std::cell::RefCell<Option<Scheduling>> = const { std::cell::RefCell::new(None) };
}

/// A thread's scheduling, as far as [`set_current_thread_priority`] changes it.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduling {
    class: QosClass,
    relative: i32,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduling {
    policy: libc::c_int,
    sched_priority: libc::c_int,
    nice: libc::c_int,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduling(thread_priority::ThreadPriority);

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scheduling;

#[cfg(target_os = "macos")]
fn current_scheduling() -> Result<Scheduling, String> {
    current_thread_qos().map(|(class, relative)| Scheduling { class, relative })
}

#[cfg(target_os = "macos")]
fn apply_scheduling(saved: &Scheduling) -> Result<(), String> {
    let rc = unsafe { pthread_set_qos_class_self_np(saved.class as u32, saved.relative) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "pthread_set_qos_class_self_np({:?}, {}) failed: errno {}",
            saved.class, saved.relative, rc
        ))
    }
}

#[cfg(target_os = "linux")]
fn current_scheduling() -> Result<Scheduling, String> {
    unsafe {
        let mut policy: libc::c_int = 0;
        let mut param: libc::sched_param = std::mem::zeroed();
        let rc = libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param);
        if rc != 0 {
            return Err(format!("pthread_getschedparam failed: errno {}", rc));
        }
        // getpriority can legitimately return -1, so errno is the only signal.
        *libc::__errno_location() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, 0);
        let errno = *libc::__errno_location();
        if nice == -1 && errno != 0 {
            return Err(format!("getpriority failed: errno {}", errno));
        }
        Ok(Scheduling {
            policy,
            sched_priority: param.sched_priority,
            nice,
        })
    }
}

/// Lowering a thread's scheduling needs no privilege, so this succeeds
/// wherever the elevation it undoes did.
#[cfg(target_os = "linux")]
fn apply_scheduling(saved: &Scheduling) -> Result<(), String> {
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = saved.sched_priority;
        let rc = libc::pthread_setschedparam(libc::pthread_self(), saved.policy, &param);
        if rc != 0 {
            return Err(format!("pthread_setschedparam failed: errno {}", rc));
        }
        // On Linux PRIO_PROCESS with 0 is the calling thread, not the process.
        if libc::setpriority(libc::PRIO_PROCESS, 0, saved.nice) != 0 {
            return Err(format!(
                "setpriority({}) failed: errno {}",
                saved.nice,
                *libc::__errno_location()
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn current_scheduling() -> Result<Scheduling, String> {
    thread_priority::get_current_thread_priority()
        .map(Scheduling)
        .map_err(|e| format!("GetThreadPriority failed: {:?}", e))
}

#[cfg(target_os = "windows")]
fn apply_scheduling(saved: &Scheduling) -> Result<(), String> {
    thread_priority::set_current_thread_priority(saved.0)
        .map_err(|e| format!("SetThreadPriority failed: {:?}", e))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn current_scheduling() -> Result<Scheduling, String> {
    Ok(Scheduling)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn apply_scheduling(_saved: &Scheduling) -> Result<(), String> {
    Ok(())
}

/// Apply an elevated priority to the calling thread, without saving what it
/// had. Only [`set_current_thread_priority`] calls this.
fn elevate_current_thread(priority: ThreadPriority) -> Result<(), String> {
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
/// explicit request for the maximum, so it does take `USER_INTERACTIVE`. In a
/// process that is not a foreground app, such as the headless server, the
/// kernel schedules `USER_INTERACTIVE` at the same priority as
/// `USER_INITIATED`, so there `Realtime` and `High` behave the same.
#[cfg(target_os = "macos")]
fn qos_class_for(priority: ThreadPriority) -> Option<QosClass> {
    match priority {
        // Normal means "do not touch this thread's scheduling", so the
        // thread keeps whatever class it already has.
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
/// Only the calling thread is classified. Threads it creates do not inherit
/// the class: an encoder's own worker pool (libx264 with more than one thread,
/// SVT-AV1) is created from the streaming thread and still runs at
/// `QOS_CLASS_DEFAULT`, and macOS has no call that sets another thread's class.
#[cfg(target_os = "macos")]
fn set_current_thread_qos(priority: ThreadPriority) -> Result<(), String> {
    let Some(class) = qos_class_for(priority) else {
        debug!("Thread priority set to Normal (QoS class left unchanged)");
        return Ok(());
    };

    try_set_qos(class).map_err(|rc| {
        format!(
            "pthread_set_qos_class_self_np({:?}) failed: errno {}",
            class, rc
        )
    })?;

    // Read it back rather than trusting the return code: a thread that has had
    // its scheduling parameters set directly can end up UNSPECIFIED with the
    // call still reporting success. This confirms the class was recorded, not
    // where the thread runs — a task-level clamp (a background launch, App
    // Nap) overrides the class and it still reads back as set.
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

/// Remedy appended to the once-per-flow failure warning. Only Linux gates
/// elevated priority on a capability the operator can grant.
#[cfg(target_os = "linux")]
const ELEVATION_HINT: &str =
    " Elevated priority needs CAP_SYS_NICE (grant with: sudo setcap cap_sys_nice+ep <binary>).";
#[cfg(not(target_os = "linux"))]
const ELEVATION_HINT: &str = "";

/// What a flow asks of every streaming thread that does work for it: its
/// priority, its CPU affinity, and a place in the thread registry.
///
/// One value per flow, shared by every path that sees the flow's streaming
/// threads: the sync handler on the flow's own bus, the `sync-message` handler
/// on a block's internal pipeline, and the pad probes on a WebRTC session
/// pipeline. Each calls [`ThreadSetup::enter`] on the streaming thread itself.
#[derive(Clone)]
struct ThreadSetup {
    priority: ThreadPriority,
    assigned_cpus: Option<Vec<usize>>,
    flow_id: FlowId,
    thread_registry: Option<ThreadRegistry>,
    state: ThreadPriorityState,
}

impl ThreadSetup {
    /// Whether any thread needs touching at all.
    fn is_active(&self) -> bool {
        !matches!(self.priority, ThreadPriority::Normal)
            || self.assigned_cpus.is_some()
            || self.thread_registry.is_some()
    }

    /// Configure the calling thread, which has just entered its streaming
    /// loop for `owner` in `pipeline`, and register it.
    ///
    /// The registry files the thread under the block named by the owner's
    /// `block_id:` prefix, the way the flow's own elements are named, or under
    /// `block_id` for an element a block built without that prefix (anything
    /// inside an internal or session pipeline).
    fn enter(&self, owner: &str, block_id: Option<&str>, pipeline: &str) {
        // Captured here, on the streaming thread itself, so the handle owns
        // whatever reference makes it safe to sample after this thread exits.
        let handle = ThreadHandle::current();
        let thread_id = handle.id();

        debug!(
            "Thread {} entering streaming loop for element '{}' in pipeline '{}'",
            thread_id, owner, pipeline
        );

        if matches!(self.priority, ThreadPriority::Normal) {
            // A pooled thread can arrive still elevated by a WebRTC session,
            // whose threads never send a Leave.
            if let Err(e) = restore_current_thread_priority() {
                warn!(
                    "Failed to restore scheduling of streaming thread {} (element: {}, pipeline: {}): {}",
                    thread_id, owner, pipeline, e
                );
            }
            self.state.record_success();
        } else {
            match set_current_thread_priority(self.priority) {
                Ok(()) => {
                    info!(
                        "Set {:?} priority for streaming thread {} (element: {}, pipeline: {})",
                        self.priority, thread_id, owner, pipeline
                    );
                    self.state.record_success();
                }
                Err(e) => {
                    if self.state.record_failure(e.clone()) {
                        warn!(
                            "Failed to set {:?} priority for streaming thread {} (element: {}, pipeline: {}): {}.{} \
                             Continuing at normal priority; further failures for this flow are logged at debug.",
                            self.priority, thread_id, owner, pipeline, e, ELEVATION_HINT
                        );
                    } else {
                        debug!(
                            "Failed to set {:?} priority for thread {} (element: {}): {} (already warned for this flow)",
                            self.priority, thread_id, owner, e
                        );
                    }
                }
            }
        }

        // Track the actual result: the registry shows the CPUs the thread is
        // pinned to, not the ones it was meant to be.
        let pinned_cpus =
            self.assigned_cpus.as_ref().and_then(|cpus| {
                match set_thread_cpu_affinity(thread_id, cpus) {
                    Ok(()) => {
                        info!(
                            "Set CPU affinity {:?} for thread {} (element: {}, pipeline: {})",
                            cpus, thread_id, owner, pipeline
                        );
                        Some(cpus.clone())
                    }
                    Err(e) => {
                        warn!(
                        "Failed to set CPU affinity for thread {} (element: {}, pipeline: {}): {}",
                        thread_id, owner, pipeline, e
                    );
                        None
                    }
                }
            });

        if let Some(ref registry) = self.thread_registry {
            let block_id = owner
                .split_once(':')
                .map(|(prefix, _)| prefix)
                .or(block_id)
                .map(str::to_string);
            registry.register(
                handle,
                owner.to_string(),
                self.flow_id,
                block_id,
                pinned_cpus,
            );
        }
    }

    /// Undo [`ThreadSetup::enter`] on the calling thread, which is leaving its
    /// streaming loop for `owner`.
    fn leave(&self, owner: &str, pipeline: &str) {
        let thread_id = ThreadHandle::current().id();

        debug!(
            "Thread {} leaving streaming loop for element '{}' in pipeline '{}'",
            thread_id, owner, pipeline
        );

        // The thread goes back to GStreamer's shared pool next, and must not
        // take this flow's priority to whoever gets it.
        if let Err(e) = restore_current_thread_priority() {
            warn!(
                "Failed to restore scheduling of thread {} leaving element '{}' in pipeline '{}': {}",
                thread_id, owner, pipeline, e
            );
        }

        if let Some(ref registry) = self.thread_registry {
            registry.unregister(thread_id);
        }
    }

    /// Act on a `STREAM_STATUS` message, on the thread that posted it.
    fn on_stream_status(&self, msg: &gst::Message, block_id: Option<&str>, pipeline: &str) {
        let gst::MessageView::StreamStatus(status) = msg.view() else {
            return;
        };
        let (status_type, owner) = status.get();
        match status_type {
            gst::StreamStatusType::Enter => self.enter(&owner.name(), block_id, pipeline),
            gst::StreamStatusType::Leave => self.leave(&owner.name(), pipeline),
            _ => {}
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
    let setup = ThreadSetup {
        priority,
        assigned_cpus,
        flow_id,
        thread_registry,
        state: state.clone(),
    };

    // Always set up handler if we have a thread registry (for CPU monitoring),
    // even if priority is Normal
    if !setup.is_active() {
        info!("Thread priority set to Normal, no affinity, no registry - no sync handler needed");
        state.achieved.store(true, Ordering::SeqCst);
        return state;
    }

    if let Some(ref cpus) = setup.assigned_cpus {
        info!(
            "CPU affinity: flow {} pinned to physical core (CPUs {:?})",
            flow_id, cpus
        );
    }

    let Some(bus) = pipeline.bus() else {
        error!("Pipeline has no bus - cannot set up thread priority handler");
        state.record_failure("Pipeline has no bus".to_string());
        return state;
    };

    info!(
        "Thread priority sync handler installed for pipeline '{}' (priority: {:?}, assigned_cpus: {:?}, registry: {})",
        pipeline.name(),
        priority,
        setup.assigned_cpus,
        setup.thread_registry.is_some()
    );

    let flow_name = pipeline.name().to_string();
    bus.set_sync_handler(move |_bus, msg| {
        setup.on_stream_status(msg, None, &flow_name);
        // Pass the message to the async handler
        gst::BusSyncReply::Pass
    });

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

/// A flow's thread priority, CPU affinity and thread registry, for the
/// pipelines its blocks run of their own.
///
/// The flow's sync handler sees only the flow's own bus. A block that runs
/// part of its work in a pipeline of its own (an isolation pipeline behind an
/// appsink/appsrc pair, a WebRTC session) hands this config to every such
/// pipeline it creates, so the threads there run like the flow's own:
///
/// * [`SessionThreadConfig::install_on_internal_pipeline`] where the block
///   owns the pipeline and its bus (Media Player, WHIP Input).
/// * [`SessionThreadConfig::install_on_session_pipeline`] where webrtcsink
///   owns it (WHEP Output, WHIP Output).
///
/// Created empty at block-build time (`BlockBuildContext::session_thread_config`)
/// and populated in `PipelineManager::start()`, after the blocks are built.
/// Both install paths read it when a thread enters, not when they install, so
/// a pipeline built with the block picks up the flow's settings.
#[derive(Clone, Default)]
pub struct SessionThreadConfig(Arc<std::sync::OnceLock<ThreadSetup>>);

impl SessionThreadConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Populate the config with the flow's settings. `state` is the flow's
    /// own, from [`setup_thread_priority_handler`], so the threads configured
    /// here count toward the flow's status.
    /// Must be called before the pipeline reaches PLAYING (i.e. before sessions are created).
    pub fn populate(
        &self,
        state: &ThreadPriorityState,
        assigned_cpus: Option<Vec<usize>>,
        flow_id: FlowId,
        thread_registry: Option<ThreadRegistry>,
    ) {
        let _ = self.0.set(ThreadSetup {
            priority: state.requested,
            assigned_cpus,
            flow_id,
            thread_registry,
            state: state.clone(),
        });
    }

    /// Returns true if thread priority or registration is configured.
    pub fn is_active(&self) -> bool {
        self.0.get().is_some_and(ThreadSetup::is_active)
    }

    /// Give the streaming threads of `pipeline`, a pipeline block `block_id`
    /// runs of its own and whose bus it owns, the flow's settings.
    ///
    /// Subscribes to the bus's `sync-message::stream-status` signal rather
    /// than taking its sync handler: a bus has only one sync handler, and a
    /// block may need it for something else (`context_forward`). GStreamer
    /// emits `sync-message` beside the sync handler, on the posting thread,
    /// unless that handler drops the message.
    ///
    /// Call it once per pipeline, before the pipeline starts. A thread that
    /// enters before the flow has populated this config keeps its scheduling
    /// and stays out of the registry; one that leaves restores whatever
    /// [`ThreadSetup::enter`] changed.
    pub fn install_on_internal_pipeline(&self, pipeline: &gst::Pipeline, block_id: &str) {
        let Some(bus) = pipeline.bus() else {
            warn!(
                "Internal pipeline '{}' of block {} has no bus - its threads keep normal priority",
                pipeline.name(),
                block_id
            );
            return;
        };
        bus.enable_sync_message_emission();

        // The closure holds the config and two strings, nothing from
        // GStreamer: the bus belongs to the pipeline.
        let config = self.clone();
        let owner_block = block_id.to_string();
        let pipeline_name = pipeline.name().to_string();
        bus.connect_sync_message(Some("stream-status"), move |_bus, msg| {
            let Some(setup) = config.0.get() else {
                debug!(
                    "Flow thread settings not populated yet when a thread of '{}' changed state",
                    pipeline_name
                );
                return;
            };
            if setup.is_active() {
                setup.on_stream_status(msg, Some(&owner_block), &pipeline_name);
            }
        });
        debug!(
            "Flow thread settings installed on internal pipeline '{}' of block {}",
            pipeline.name(),
            block_id
        );
    }

    /// Give every session pipeline a webrtcsink-based element of block
    /// `block_id` creates (`whepserversink`, `whipclientsink`) the flow's
    /// settings, through [`SessionThreadConfig::install_on_session_pipeline`].
    ///
    /// `consumer-pipeline-created` fires before webrtcsink sets its own bus
    /// sync handler on the session pipeline. A webrtcsink without the signal
    /// keeps its session threads at normal priority, with a warning.
    pub fn install_on_webrtcsink(&self, sink: &gst::Element, block_id: &str) {
        const SIGNAL: &str = "consumer-pipeline-created";
        if gst::glib::subclass::signal::SignalId::lookup(SIGNAL, sink.type_()).is_none() {
            warn!(
                "{} ({}) has no {} signal - its session threads keep normal priority",
                sink.name(),
                sink.type_().name(),
                SIGNAL
            );
            return;
        }
        let config = self.clone();
        let block_id = block_id.to_string();
        sink.connect(SIGNAL, false, move |values| {
            if !config.is_active() {
                return None;
            }
            let session_id = values[1].get::<String>().unwrap_or_default();
            let pipeline = values[2].get::<gst::Pipeline>().ok()?;
            config.install_on_session_pipeline(&pipeline, &session_id, &block_id);
            None
        });
    }

    /// Install pad-probe-based thread priority on a session pipeline of block
    /// `block_id`.
    ///
    /// Call this from the `consumer-pipeline-created` signal handler, which
    /// fires before webrtcsink sets its own bus sync handler. That handler
    /// returns `BusSyncReply::Drop` and routes messages through an internal
    /// channel, so neither a sync handler of ours nor `sync-message` sees the
    /// session's `STREAM_STATUS`, and replacing the handler breaks session
    /// lifecycle. Instead we connect to `deep-element-added` on the pipeline
    /// so that every element that is added gets an `EVENT_DOWNSTREAM` probe on
    /// its sink pad. On `STREAM_START`, which arrives on the streaming thread,
    /// we set priority, CPU affinity, and register the thread, then remove the
    /// probe.
    pub fn install_on_session_pipeline(
        &self,
        pipeline: &gst::Pipeline,
        session_id: &str,
        block_id: &str,
    ) {
        let Some(setup) = self.0.get() else {
            warn!(
                "SessionThreadConfig not yet populated when session {} was created",
                session_id
            );
            return;
        };

        if !setup.is_active() {
            return;
        }

        info!(
            "Installing pad-probe thread priority on session pipeline '{}' for session {} \
             (priority: {:?}, cpus: {:?})",
            pipeline.name(),
            session_id,
            setup.priority,
            setup.assigned_cpus
        );

        let setup = setup.clone();
        let block_id = block_id.to_string();
        let pipeline_name = pipeline.name().to_string();

        // Track which native thread IDs have already been configured so we
        // don't set priority twice when multiple pads share a thread.
        let configured_threads: Arc<std::sync::Mutex<std::collections::HashSet<u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

        pipeline.connect("deep-element-added", false, move |args| {
            let added: gst::Element = args[2].get().unwrap();

            let sink_pad = added.static_pad("sink")?;

            let setup = setup.clone();
            let block_id = block_id.clone();
            let pipeline_name = pipeline_name.clone();
            let configured = configured_threads.clone();
            let element_name = added.name().to_string();

            sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                // Only a streaming thread pushes STREAM_START. Any other first
                // event can come from an application thread (an EOS sent on
                // teardown before any data), which must keep its scheduling.
                // Wait for STREAM_START instead.
                if !matches!(info.event(), Some(e) if e.type_() == gst::EventType::StreamStart) {
                    return gst::PadProbeReturn::Ok;
                }

                // Skip if this thread was already configured (multiple pads
                // can share the same streaming thread).
                {
                    let mut set = configured.lock().unwrap();
                    if !set.insert(ThreadHandle::current().id()) {
                        return gst::PadProbeReturn::Remove;
                    }
                }

                // These threads never send a Leave, so an elevation here goes
                // back to the pool with the thread; the next flow's Enter, or
                // Normal session, restores it. The registry entry lives until
                // the flow's entries are dropped by unregister_flow.
                setup.enter(&element_name, Some(&block_id), &pipeline_name);

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

    /// Each case runs on its own thread: scheduling sticks to the thread that
    /// set it, and libtest reuses its worker threads between tests.
    fn on_fresh_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().expect("test thread panicked")
    }

    /// Whether `High` takes effect without privileges. On Linux it needs
    /// CAP_SYS_NICE, so an unprivileged run cannot see an elevation to undo.
    const HIGH_NEEDS_NO_PRIVILEGE: bool = cfg!(any(target_os = "macos", target_os = "windows"));

    /// `Normal` undoes an elevation left on the thread, so a pooled thread
    /// does not carry one flow's priority into the next.
    #[test]
    fn normal_after_elevation_restores_scheduling() {
        let (baseline, elevated, after) = on_fresh_thread(|| {
            let baseline = current_scheduling().unwrap();
            let _ = set_current_thread_priority(ThreadPriority::High);
            let elevated = current_scheduling().unwrap();
            set_current_thread_priority(ThreadPriority::Normal).expect("restore should succeed");
            (baseline, elevated, current_scheduling().unwrap())
        });
        if HIGH_NEEDS_NO_PRIVILEGE {
            assert_ne!(elevated, baseline, "High should change the thread");
        }
        assert_eq!(after, baseline);
    }

    fn post_stream_status(bus: &gst::Bus, kind: gst::StreamStatusType, owner: &gst::Element) {
        bus.post(gst::message::StreamStatus::new(kind, owner))
            .expect("post stream status");
    }

    /// A pipeline with one element and the flow's bus handler installed.
    fn flow_pipeline(priority: ThreadPriority) -> (gst::Pipeline, gst::Element, gst::Bus) {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let owner = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&owner).unwrap();
        setup_thread_priority_handler(
            &pipeline,
            priority,
            None,
            FlowId::new_v4(),
            Some(ThreadRegistry::new()),
        );
        let bus = pipeline.bus().unwrap();
        (pipeline, owner, bus)
    }

    /// A streaming thread leaving a `High` flow goes back to the pool with
    /// the scheduling it had before the flow (#880).
    #[test]
    fn leave_restores_streaming_thread_scheduling() {
        let (_pipeline, owner, bus) = flow_pipeline(ThreadPriority::High);
        let (baseline, elevated, after) = on_fresh_thread(move || {
            let baseline = current_scheduling().unwrap();
            post_stream_status(&bus, gst::StreamStatusType::Enter, &owner);
            let elevated = current_scheduling().unwrap();
            post_stream_status(&bus, gst::StreamStatusType::Leave, &owner);
            (baseline, elevated, current_scheduling().unwrap())
        });
        if HIGH_NEEDS_NO_PRIVILEGE {
            assert_ne!(elevated, baseline, "Enter should elevate the thread");
        }
        assert_eq!(after, baseline, "Leave should restore the thread");
    }

    /// A thread elevated without a Leave (a WebRTC session thread) is reset
    /// when a `Normal` flow picks it up from the pool.
    #[test]
    fn normal_flow_resets_thread_left_elevated() {
        let (_high, high_owner, high_bus) = flow_pipeline(ThreadPriority::High);
        let (_normal, normal_owner, normal_bus) = flow_pipeline(ThreadPriority::Normal);
        let (baseline, elevated, after) = on_fresh_thread(move || {
            let baseline = current_scheduling().unwrap();
            post_stream_status(&high_bus, gst::StreamStatusType::Enter, &high_owner);
            let elevated = current_scheduling().unwrap();
            post_stream_status(&normal_bus, gst::StreamStatusType::Enter, &normal_owner);
            (baseline, elevated, current_scheduling().unwrap())
        });
        if HIGH_NEEDS_NO_PRIVILEGE {
            assert_ne!(elevated, baseline, "Enter should elevate the thread");
        }
        assert_eq!(after, baseline, "a Normal flow should reset the thread");
    }

    /// The session probe acts only on STREAM_START: an application thread
    /// that sends the first event (an EOS on teardown) keeps its scheduling
    /// and is not registered as a session thread (#880).
    #[test]
    fn session_probe_skips_application_thread() {
        gst::init().unwrap();
        let registry = ThreadRegistry::new();
        let config = SessionThreadConfig::new();
        config.populate(
            &ThreadPriorityState::new(ThreadPriority::High),
            None,
            FlowId::new_v4(),
            Some(registry.clone()),
        );

        let pipeline = gst::Pipeline::new();
        config.install_on_session_pipeline(&pipeline, "test-session", "test-block");
        let element = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&element).unwrap();
        pipeline.set_state(gst::State::Paused).unwrap();
        let sink_pad = element.static_pad("sink").unwrap();

        let pad = sink_pad.clone();
        let (baseline, after_eos) = on_fresh_thread(move || {
            let baseline = current_scheduling().unwrap();
            pad.send_event(gst::event::Eos::new());
            (baseline, current_scheduling().unwrap())
        });
        assert_eq!(
            after_eos, baseline,
            "EOS must not elevate the sending thread"
        );
        assert_eq!(registry.len(), 0, "EOS sender must not be registered");

        let pad = sink_pad.clone();
        let (baseline, after_start) = on_fresh_thread(move || {
            let baseline = current_scheduling().unwrap();
            pad.send_event(gst::event::StreamStart::new("test-stream"));
            (baseline, current_scheduling().unwrap())
        });
        if HIGH_NEEDS_NO_PRIVILEGE {
            assert_ne!(after_start, baseline, "STREAM_START should elevate");
        }
        assert_eq!(
            registry.len(),
            1,
            "STREAM_START thread should be registered"
        );

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// `High` must leave the thread in `USER_INITIATED`.
    ///
    /// Without the QoS call the thread reads back `DEFAULT`; with a
    /// `pthread_setschedparam` call on the same thread it reads back
    /// `UNSPECIFIED`. Either fails this assertion.
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
                "pipeline threads must carry an explicit performance-core QoS class"
            );
            assert_eq!(
                relative, 0,
                "relative priority should be the top of the band"
            );
        });
    }

    /// `Realtime` is an explicit request for the maximum band.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_realtime_thread_is_user_interactive() {
        on_fresh_thread(|| {
            set_current_thread_priority(ThreadPriority::Realtime)
                .expect("Realtime should be settable without privileges");

            let (class, _) = current_thread_qos().expect("QoS class should read back");
            assert_eq!(class, QosClass::UserInteractive);
        });
    }

    /// `Normal` means "do not touch this thread's scheduling", including its
    /// QoS class.
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
