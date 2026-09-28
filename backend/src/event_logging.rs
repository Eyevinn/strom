//! Structured logging of internal `StromEvent` broadcasts.
//!
//! Called inline from `EventBroadcaster::broadcast()` so events cannot be silently
//! dropped under broadcast-channel lag. Gating is done by the caller.

use strom_types::StromEvent;
use tracing::{error, info, warn};

/// Emit a `StromEvent` as a structured tracing log record.
///
/// Field values (`event.name`, `strom.flow.id`) come from `StromEvent` accessors,
/// not a separate vocabulary.
pub(crate) fn log_strom_event(event: &StromEvent) {
    let name = event.event_type();
    let description = event.description();

    if event.is_high_frequency() {
        // Logged at info: the operator already opted in via two config flags, so gating
        // behind debug on top of that would silently contradict the config.
        match event.flow_id() {
            Some(flow_id) => info!(
                event.domain = "strom",
                event.name = %name,
                strom.flow.id = %flow_id,
                "{}",
                description
            ),
            None => info!(
                event.domain = "strom",
                event.name = %name,
                "{}",
                description
            ),
        }
        return;
    }

    match event {
        StromEvent::PipelineError {
            flow_id,
            error: err,
            source,
        } => error!(
            event.domain = "strom",
            event.name = %name,
            strom.flow.id = %flow_id,
            error.message = %err,
            error.source = source.as_deref().unwrap_or_default(),
            "{}",
            description
        ),
        StromEvent::PipelineWarning {
            flow_id,
            warning,
            source,
        } => warn!(
            event.domain = "strom",
            event.name = %name,
            strom.flow.id = %flow_id,
            error.message = %warning,
            error.source = source.as_deref().unwrap_or_default(),
            "{}",
            description
        ),
        StromEvent::TamsError {
            flow_id,
            error: err,
            ..
        } => error!(
            event.domain = "strom",
            event.name = %name,
            strom.flow.id = %flow_id,
            error.message = %err,
            "{}",
            description
        ),
        other => match other.flow_id() {
            Some(flow_id) => info!(
                event.domain = "strom",
                event.name = %name,
                strom.flow.id = %flow_id,
                "{}",
                description
            ),
            None => info!(
                event.domain = "strom",
                event.name = %name,
                "{}",
                description
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strom_types::FlowId;

    // These are smoke tests confirming `log_strom_event` doesn't panic across the
    // high-frequency / error / warning / generic branches. The actual name/flow-id/
    // frequency logic is covered by the exhaustive, drift-proof accessor tests in
    // `strom_types::events`.

    #[test]
    fn logs_high_frequency_event_without_panicking() {
        log_strom_event(&StromEvent::MeterData {
            flow_id: FlowId::nil(),
            element_id: "level0".to_string(),
            rms: vec![],
            peak: vec![],
            decay: vec![],
        });
    }

    #[test]
    fn logs_lifecycle_event_without_panicking() {
        log_strom_event(&StromEvent::FlowCreated {
            flow_id: FlowId::nil(),
        });
    }

    #[test]
    fn logs_pipeline_error_without_panicking() {
        log_strom_event(&StromEvent::PipelineError {
            flow_id: FlowId::nil(),
            error: "boom".to_string(),
            source: None,
        });
    }

    #[test]
    fn logs_event_without_flow_id_without_panicking() {
        log_strom_event(&StromEvent::Ping);
    }
}
