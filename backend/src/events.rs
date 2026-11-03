//! Event broadcasting system for real-time updates via WebSocket.

use std::sync::Arc;
use strom_types::StromEvent;
use tokio::sync::broadcast;
use tracing::{debug, trace};

use crate::event_logging::log_strom_event;

/// Event broadcaster for WebSocket connections.
#[derive(Clone)]
pub struct EventBroadcaster {
    /// Broadcast channel for events
    sender: Arc<broadcast::Sender<StromEvent>>,
    /// Emit selected events as structured tracing log records (see `LoggingConfig`).
    structured_events: bool,
    /// Include high-frequency events in structured event logs (only if `structured_events`).
    include_high_frequency_events: bool,
}

impl EventBroadcaster {
    /// Create a new event broadcaster with a buffer size and structured-logging config.
    pub fn new(
        buffer_size: usize,
        structured_events: bool,
        include_high_frequency_events: bool,
    ) -> Self {
        let (sender, _) = broadcast::channel(buffer_size);
        Self {
            sender: Arc::new(sender),
            structured_events,
            include_high_frequency_events,
        }
    }

    /// Create a broadcaster with the given buffer size and structured logging disabled.
    pub fn with_capacity(buffer_size: usize) -> Self {
        Self::new(buffer_size, false, false)
    }

    /// Broadcast an event to all connected WebSocket clients.
    pub fn broadcast(&self, event: StromEvent) {
        // Use trace for high-frequency events, debug for others
        if event.is_high_frequency() {
            trace!("Broadcasting event: {}", event.description());
        } else {
            debug!("Broadcasting event: {}", event.description());
        }

        // Emitted inline (not from a subscribe() task) so a lagging/dropped broadcast
        // receiver can never silently drop a structured log record.
        if self.logs_structured(&event) {
            log_strom_event(&event);
        }

        // broadcast::send returns the number of receivers
        // We don't care about the result since clients may or may not be connected
        let _ = self.sender.send(event);
    }

    /// Whether `broadcast` emits `event` as a structured log record.
    fn logs_structured(&self, event: &StromEvent) -> bool {
        self.structured_events && (self.include_high_frequency_events || !event.is_high_frequency())
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
        Self::new(100, false, false) // Default buffer of 100 events, structured logging off
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use strom_types::FlowId;
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    /// Counts tracing events emitted from `event_logging`, without the unrelated
    /// "Broadcasting event" trace/debug line. Only a smoke test of `broadcast()`'s
    /// path uses it: a count above 0 is not reliable (see below), the gate itself
    /// is tested through `logs_structured`.
    struct StructuredLogCounter {
        count: Arc<AtomicUsize>,
    }

    impl<S: tracing::Subscriber> Layer<S> for StructuredLogCounter {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            if event.metadata().target().ends_with("event_logging") {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn structured_events_disabled_emits_no_log_record() {
        let count = Arc::new(AtomicUsize::new(0));
        let layer = StructuredLogCounter {
            count: count.clone(),
        };
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));

        let broadcaster = EventBroadcaster::new(10, false, false);
        broadcaster.broadcast(StromEvent::FlowCreated {
            flow_id: FlowId::nil(),
        });

        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    // The gate is checked directly, not through a counting subscriber. tracing
    // stores per callsite, globally, whether anyone listens; with a single
    // scoped dispatcher alive it asks the dispatcher of the thread that hits
    // the callsite first. The smoke tests in `event_logging` hit the same
    // `info!` callsites on threads without a subscriber, and when one comes
    // first the callsite reads "never": the counted record went missing
    // (1 run in 48 under load).
    #[test]
    fn high_frequency_events_dropped_unless_included() {
        let meter = StromEvent::MeterData {
            flow_id: FlowId::nil(),
            element_id: "level0".to_string(),
            rms: vec![],
            peak: vec![],
            decay: vec![],
        };
        let created = StromEvent::FlowCreated {
            flow_id: FlowId::nil(),
        };

        // High-frequency: dropped even though structured_events is on; others
        // are still logged.
        let broadcaster = EventBroadcaster::new(10, true, false);
        assert!(!broadcaster.logs_structured(&meter));
        assert!(broadcaster.logs_structured(&created));

        // With include_high_frequency_events the same event is logged.
        let broadcaster = EventBroadcaster::new(10, true, true);
        assert!(broadcaster.logs_structured(&meter));

        // Without structured_events nothing is.
        let broadcaster = EventBroadcaster::new(10, false, true);
        assert!(!broadcaster.logs_structured(&created));
    }
}
