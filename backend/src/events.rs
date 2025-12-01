//! Event broadcasting system for real-time updates via WebSocket.

use std::sync::Arc;
use strom_types::StromEvent;
use tokio::sync::broadcast;
use tracing::{debug, trace};

/// Event broadcaster for WebSocket connections.
#[derive(Clone)]
pub struct EventBroadcaster {
    /// Broadcast channel for events
    sender: Arc<broadcast::Sender<StromEvent>>,
}

impl EventBroadcaster {
    /// Create a new event broadcaster with a buffer size.
    pub fn new(buffer_size: usize) -> Self {
        let (sender, _) = broadcast::channel(buffer_size);
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Broadcast an event to all connected WebSocket clients.
    pub fn broadcast(&self, event: StromEvent) {
        // Use trace for high-frequency events, debug for others
        match &event {
            StromEvent::MeterData { .. } => {
                trace!("Broadcasting event: {}", event.description());
            }
            _ => {
                debug!("Broadcasting event: {}", event.description());
            }
        }
        // broadcast::send returns the number of receivers
        // We don't care about the result since clients may or may not be connected
        let _ = self.sender.send(event);
    }

    /// Get the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Subscribe to events via a raw broadcast receiver (used by WebSocket handler).
    pub fn subscribe(&self) -> broadcast::Receiver<StromEvent> {
        self.sender.subscribe()
    }
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new(100) // Default buffer of 100 events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::FlowId;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_broadcaster_creation() {
        let broadcaster = EventBroadcaster::new(10);
        assert_eq!(broadcaster.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn test_broadcast_event() {
        let broadcaster = EventBroadcaster::new(10);
        let flow_id = FlowId::from(Uuid::new_v4());

        // Subscribe before broadcasting
        let mut _rx = broadcaster.subscribe();
        assert_eq!(broadcaster.subscriber_count(), 1);

        // Broadcast an event
        broadcaster.broadcast(StromEvent::FlowCreated { flow_id });

        // Verify we can receive the event
        let received = _rx.recv().await.unwrap();
        match received {
            StromEvent::FlowCreated { flow_id: id } => assert_eq!(id, flow_id),
            _ => panic!("Unexpected event type"),
        }
    }
}
