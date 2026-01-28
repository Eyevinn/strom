//! Thread registry for tracking GStreamer streaming threads.
//!
//! This module maintains a mapping between native thread IDs and the
//! GStreamer elements that own them, enabling CPU usage correlation.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use strom_types::FlowId;

/// Information about a registered GStreamer streaming thread.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    /// Native thread ID (OS-specific)
    pub thread_id: u64,
    /// Name of the GStreamer element that owns this thread
    pub element_name: String,
    /// Flow ID this thread belongs to
    pub flow_id: FlowId,
    /// Block ID if the element is inside a block
    pub block_id: Option<String>,
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
    pub fn register(
        &self,
        thread_id: u64,
        element_name: String,
        flow_id: FlowId,
        block_id: Option<String>,
    ) {
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
                thread_id,
                element_name,
                flow_id,
                block_id,
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

    #[test]
    fn test_register_unregister() {
        let registry = ThreadRegistry::new();
        let flow_id = Uuid::new_v4();

        registry.register(12345, "element0".to_string(), flow_id, None);
        assert_eq!(registry.len(), 1);

        let threads = registry.get_all();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, 12345);
        assert_eq!(threads[0].element_name, "element0");
        assert_eq!(threads[0].flow_id, flow_id);

        registry.unregister(12345);
        assert!(registry.is_empty());
    }

    #[test]
    fn test_unregister_flow() {
        let registry = ThreadRegistry::new();
        let flow1 = Uuid::new_v4();
        let flow2 = Uuid::new_v4();

        registry.register(1, "elem1".to_string(), flow1, None);
        registry.register(2, "elem2".to_string(), flow1, None);
        registry.register(3, "elem3".to_string(), flow2, None);

        assert_eq!(registry.len(), 3);

        registry.unregister_flow(&flow1);
        assert_eq!(registry.len(), 1);

        let threads = registry.get_all();
        assert_eq!(threads[0].flow_id, flow2);
    }
}
