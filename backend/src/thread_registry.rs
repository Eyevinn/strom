//! Thread registry for tracking GStreamer streaming threads.
//!
//! This module maintains a mapping between native thread IDs and the
//! GStreamer elements that own them, enabling CPU usage correlation.

use crate::thread_handle::ThreadHandle;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use strom_types::FlowId;

/// Information about a registered GStreamer streaming thread.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    /// Owned handle to the thread, safe to sample for as long as it is held
    /// (see [`ThreadHandle`]).
    pub handle: ThreadHandle,
    /// Name of the GStreamer element that owns this thread
    pub element_name: String,
    /// Flow ID this thread belongs to
    pub flow_id: FlowId,
    /// Block ID if the element is inside a block
    pub block_id: Option<String>,
    /// Logical CPUs this thread is pinned to (None if affinity is off)
    pub pinned_cpus: Option<Vec<usize>>,
}

impl ThreadInfo {
    /// Native thread ID (OS-specific), used as the registry key and shown in
    /// the UI.
    pub fn thread_id(&self) -> u64 {
        self.handle.id()
    }
}

/// Registry for tracking active GStreamer streaming threads.
///
/// This registry is updated by the thread priority handler when threads
/// enter or leave their streaming loops (via StreamStatus messages).
#[derive(Debug, Clone)]
pub struct ThreadRegistry {
    threads: Arc<RwLock<HashMap<u64, ThreadInfo>>>,
}

impl ThreadRegistry {
    /// Create a new empty thread registry.
    pub fn new() -> Self {
        Self {
            threads: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a thread that has entered its streaming loop.
    ///
    /// `handle` must have been captured on that thread; the registry owns it
    /// from here, which is what keeps it valid to sample.
    pub fn register(
        &self,
        handle: ThreadHandle,
        element_name: String,
        flow_id: FlowId,
        block_id: Option<String>,
        pinned_cpus: Option<Vec<usize>>,
    ) {
        let thread_id = handle.id();
        tracing::debug!(
            "Registered thread {} for element '{}' in flow {}",
            thread_id,
            element_name,
            flow_id
        );
        let mut threads = self.threads.write();
        threads.insert(
            thread_id,
            ThreadInfo {
                handle,
                element_name,
                flow_id,
                block_id,
                pinned_cpus,
            },
        );
    }

    /// Unregister a thread that has left its streaming loop.
    pub fn unregister(&self, thread_id: u64) {
        let mut threads = self.threads.write();
        if let Some(info) = threads.remove(&thread_id) {
            tracing::debug!(
                "Unregistered thread {} (element '{}', flow {})",
                thread_id,
                info.element_name,
                info.flow_id
            );
        }
    }

    /// Unregister all threads belonging to a specific flow.
    ///
    /// This should be called when a flow is stopped to clean up any
    /// threads that didn't properly send Leave messages.
    pub fn unregister_flow(&self, flow_id: &FlowId) {
        let mut threads = self.threads.write();
        let before_count = threads.len();
        threads.retain(|_, info| &info.flow_id != flow_id);
        let removed = before_count - threads.len();
        if removed > 0 {
            tracing::debug!(
                "Unregistered {} threads for flow {} (cleanup)",
                removed,
                flow_id
            );
        }
    }

    /// Get all registered threads.
    pub fn get_all(&self) -> Vec<ThreadInfo> {
        let threads = self.threads.read();
        threads.values().cloned().collect()
    }

    /// Get the number of registered threads.
    pub fn len(&self) -> usize {
        self.threads.read().len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.threads.read().is_empty()
    }
}

impl Default for ThreadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// A handle to a thread that has already exited, which is the interesting
    /// case: on macOS the registry entry is the only thing keeping its mach
    /// port name from being freed and recycled.
    fn handle_from_finished_thread() -> ThreadHandle {
        std::thread::spawn(ThreadHandle::current).join().unwrap()
    }

    #[test]
    fn test_register_unregister() {
        let registry = ThreadRegistry::new();
        let flow_id = Uuid::new_v4();
        let handle = handle_from_finished_thread();
        let thread_id = handle.id();

        registry.register(handle, "element0".to_string(), flow_id, None, None);
        assert_eq!(registry.len(), 1);

        let threads = registry.get_all();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id(), thread_id);
        assert_eq!(threads[0].element_name, "element0");
        assert_eq!(threads[0].flow_id, flow_id);

        registry.unregister(thread_id);
        assert!(registry.is_empty());
    }

    #[test]
    fn test_unregister_flow() {
        let registry = ThreadRegistry::new();
        let flow1 = Uuid::new_v4();
        let flow2 = Uuid::new_v4();

        registry.register(
            handle_from_finished_thread(),
            "elem1".to_string(),
            flow1,
            None,
            None,
        );
        registry.register(
            handle_from_finished_thread(),
            "elem2".to_string(),
            flow1,
            None,
            None,
        );
        registry.register(
            handle_from_finished_thread(),
            "elem3".to_string(),
            flow2,
            None,
            None,
        );

        assert_eq!(registry.len(), 3);

        registry.unregister_flow(&flow1);
        assert_eq!(registry.len(), 1);

        let threads = registry.get_all();
        assert_eq!(threads[0].flow_id, flow2);
    }

    /// A registered thread's mach port name must stay allocated for as long as
    /// the registry holds it, so a sampler that reaches the entry after the
    /// thread has exited cannot hit a name the kernel has handed to another —
    /// possibly guarded — port.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_registered_thread_keeps_its_port_name_after_exiting() {
        use crate::thread_handle::mach_port_name_is_allocated;

        let registry = ThreadRegistry::new();
        let handle = handle_from_finished_thread();
        let name = handle.mach_port();
        let thread_id = handle.id();

        registry.register(handle, "elem".to_string(), Uuid::new_v4(), None, None);

        assert!(
            mach_port_name_is_allocated(name),
            "registry entry did not keep port name {:#x} allocated",
            name
        );

        registry.unregister(thread_id);

        assert!(
            !mach_port_name_is_allocated(name),
            "unregister leaked the port reference for name {:#x}",
            name
        );
    }

    /// `get_all` hands out clones that the sampler holds across its mach calls,
    /// so those clones must keep the port alive even if the thread unregisters
    /// in between.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_snapshot_outlives_a_concurrent_unregister() {
        use crate::thread_handle::mach_port_name_is_allocated;

        let registry = ThreadRegistry::new();
        let handle = handle_from_finished_thread();
        let name = handle.mach_port();
        let thread_id = handle.id();
        registry.register(handle, "elem".to_string(), Uuid::new_v4(), None, None);

        let snapshot = registry.get_all();
        registry.unregister(thread_id);

        assert!(
            mach_port_name_is_allocated(name),
            "a snapshot taken before unregister did not keep port name {:#x} alive",
            name
        );

        drop(snapshot);
        assert!(!mach_port_name_is_allocated(name));
    }

    /// Every unregister path releases the reference, including dropping the
    /// registry outright and overwriting an entry with the same key.
    #[test]
    #[cfg(target_os = "macos")]
    fn every_removal_path_releases_the_port_reference() {
        use crate::thread_handle::mach_port_name_is_allocated;

        let flow_id = Uuid::new_v4();

        // unregister_flow
        let registry = ThreadRegistry::new();
        let handle = handle_from_finished_thread();
        let name = handle.mach_port();
        registry.register(handle, "elem".to_string(), flow_id, None, None);
        registry.unregister_flow(&flow_id);
        assert!(
            !mach_port_name_is_allocated(name),
            "unregister_flow leaked the reference for name {:#x}",
            name
        );

        // Dropping the registry.
        let registry = ThreadRegistry::new();
        let handle = handle_from_finished_thread();
        let name = handle.mach_port();
        registry.register(handle, "elem".to_string(), flow_id, None, None);
        drop(registry);
        assert!(
            !mach_port_name_is_allocated(name),
            "dropping the registry leaked the reference for name {:#x}",
            name
        );

        // Re-registering the same thread must not leak the displaced entry.
        let registry = ThreadRegistry::new();
        let handle = handle_from_finished_thread();
        let name = handle.mach_port();
        let thread_id = handle.id();
        registry.register(handle.clone(), "first".to_string(), flow_id, None, None);
        registry.register(handle, "second".to_string(), flow_id, None, None);
        assert_eq!(registry.len(), 1);
        registry.unregister(thread_id);
        assert!(
            !mach_port_name_is_allocated(name),
            "overwriting an entry leaked the reference for name {:#x}",
            name
        );
    }
}
