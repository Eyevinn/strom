//! Events for real-time updates across clients.

use crate::element::PropertyValue;
use crate::system_monitor::SystemStats;
use crate::FlowId;
use serde::{Deserialize, Serialize};

/// Event types that can be broadcast to all connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum StromEvent {
    /// A flow was created
    FlowCreated { flow_id: FlowId },
    /// A flow was updated
    FlowUpdated { flow_id: FlowId },
    /// A flow was deleted
    FlowDeleted { flow_id: FlowId },
    /// A flow was started
    FlowStarted { flow_id: FlowId },
    /// A flow was stopped
    FlowStopped { flow_id: FlowId },
    /// A flow's state changed
    FlowStateChanged { flow_id: FlowId, state: String },
    /// Pipeline error occurred
    PipelineError {
        flow_id: FlowId,
        error: String,
        source: Option<String>,
    },
    /// Pipeline warning message
    PipelineWarning {
        flow_id: FlowId,
        warning: String,
        source: Option<String>,
    },
    /// Pipeline info message
    PipelineInfo {
        flow_id: FlowId,
        message: String,
        source: Option<String>,
    },
    /// Pipeline reached end of stream
    PipelineEos { flow_id: FlowId },
    /// Element property was changed on a running pipeline
    PropertyChanged {
        flow_id: FlowId,
        element_id: String,
        property_name: String,
        value: PropertyValue,
    },
    /// Pad property was changed on a running pipeline
    PadPropertyChanged {
        flow_id: FlowId,
        element_id: String,
        pad_name: String,
        property_name: String,
        value: PropertyValue,
    },
    /// Ping event to keep connection alive
    Ping,
    /// Audio level meter data from GStreamer level element
    MeterData {
        flow_id: FlowId,
        element_id: String,
        /// RMS values in dB for each channel
        rms: Vec<f64>,
        /// Peak values in dB for each channel
        peak: Vec<f64>,
        /// Decay values in dB for each channel
        decay: Vec<f64>,
    },
    /// System monitoring statistics (CPU and GPU)
    SystemStats(SystemStats),
}

impl StromEvent {
    /// Get a human-readable description of the event
    pub fn description(&self) -> String {
        match self {
            StromEvent::FlowCreated { flow_id } => format!("Flow {} created", flow_id),
            StromEvent::FlowUpdated { flow_id } => format!("Flow {} updated", flow_id),
            StromEvent::FlowDeleted { flow_id } => format!("Flow {} deleted", flow_id),
            StromEvent::FlowStarted { flow_id } => format!("Flow {} started", flow_id),
            StromEvent::FlowStopped { flow_id } => format!("Flow {} stopped", flow_id),
            StromEvent::FlowStateChanged { flow_id, state } => {
                format!("Flow {} state changed to {}", flow_id, state)
            }
            StromEvent::PipelineError {
                flow_id,
                error,
                source,
            } => {
                if let Some(src) = source {
                    format!("Pipeline error in flow {} from {}: {}", flow_id, src, error)
                } else {
                    format!("Pipeline error in flow {}: {}", flow_id, error)
                }
            }
            StromEvent::PipelineWarning {
                flow_id,
                warning,
                source,
            } => {
                if let Some(src) = source {
                    format!(
                        "Pipeline warning in flow {} from {}: {}",
                        flow_id, src, warning
                    )
                } else {
                    format!("Pipeline warning in flow {}: {}", flow_id, warning)
                }
            }
            StromEvent::PipelineInfo {
                flow_id,
                message,
                source,
            } => {
                if let Some(src) = source {
                    format!(
                        "Pipeline info in flow {} from {}: {}",
                        flow_id, src, message
                    )
                } else {
                    format!("Pipeline info in flow {}: {}", flow_id, message)
                }
            }
            StromEvent::PipelineEos { flow_id } => {
                format!("Pipeline {} reached end of stream", flow_id)
            }
            StromEvent::PropertyChanged {
                flow_id,
                element_id,
                property_name,
                value,
            } => {
                format!(
                    "Property {}.{} changed to {:?} in flow {}",
                    element_id, property_name, value, flow_id
                )
            }
            StromEvent::PadPropertyChanged {
                flow_id,
                element_id,
                pad_name,
                property_name,
                value,
            } => {
                format!(
                    "Pad property {}:{}:{} changed to {:?} in flow {}",
                    element_id, pad_name, property_name, value, flow_id
                )
            }
            StromEvent::Ping => "Ping".to_string(),
            StromEvent::MeterData {
                flow_id,
                element_id,
                rms,
                ..
            } => {
                format!(
                    "Meter data from {} in flow {} ({} channels)",
                    element_id,
                    flow_id,
                    rms.len()
                )
            }
            StromEvent::SystemStats(stats) => {
                format!(
                    "System stats: CPU {:.1}%, Memory {:.1}%, {} GPU(s)",
                    stats.cpu_usage,
                    (stats.used_memory as f64 / stats.total_memory as f64) * 100.0,
                    stats.gpu_stats.len()
                )
            }
        }
    }
}
