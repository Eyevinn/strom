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
    /// PTP clock statistics for a flow
    PtpStats {
        flow_id: FlowId,
        /// PTP domain
        domain: u8,
        /// Whether clock is synchronized
        synced: bool,
        /// Mean path delay to master in nanoseconds
        mean_path_delay_ns: Option<u64>,
        /// Clock offset/correction in nanoseconds
        clock_offset_ns: Option<i64>,
        /// R-squared (clock estimation quality, 0.0-1.0)
        r_squared: Option<f64>,
        /// Clock rate ratio (local vs master)
        clock_rate: Option<f64>,
        /// Grandmaster clock ID (EUI-64 identifier)
        grandmaster_id: Option<u64>,
        /// Master clock ID (EUI-64 identifier)
        master_id: Option<u64>,
    },
    /// A flow's published output became available (flow started)
    SourceOutputAvailable {
        source_flow_id: FlowId,
        output_name: String,
        channel_name: String,
    },
    /// A flow's published output became unavailable (flow stopped)
    SourceOutputUnavailable {
        source_flow_id: FlowId,
        output_name: String,
    },
    /// Subscription connection status changed
    SubscriptionStatusChanged {
        consumer_flow_id: FlowId,
        source_flow_id: FlowId,
        output_name: String,
        connected: bool,
    },
    /// Quality of Service statistics (aggregated buffer drop info)
    QoSStats {
        flow_id: FlowId,
        /// Block ID if element is inside a block, None if standalone element
        block_id: Option<String>,
        /// Element ID (standalone element ID or block ID if element is in block)
        element_id: String,
        /// Full GStreamer element name (e.g., "block_id:element_type" or "element_id")
        element_name: String,
        /// Internal element type if part of a block (e.g., "videoconvert")
        internal_element_type: Option<String>,
        /// Number of QoS events in aggregation period
        event_count: u64,
        /// Average proportion (< 1.0 = falling behind)
        avg_proportion: f64,
        /// Minimum proportion seen
        min_proportion: f64,
        /// Maximum proportion seen
        max_proportion: f64,
        /// Average jitter in nanoseconds
        avg_jitter: i64,
        /// Total buffers processed
        total_processed: u64,
        /// Whether pipeline is falling behind (avg_proportion < 1.0)
        is_falling_behind: bool,
    },
    /// A new AES67 stream was discovered via SAP or mDNS
    StreamDiscovered {
        stream_id: String,
        name: String,
        /// Discovery source: "sap" or "mdns"
        source: String,
    },
    /// A discovered stream was updated (re-announced)
    StreamUpdated { stream_id: String },
    /// A discovered stream expired or was deleted
    StreamRemoved { stream_id: String },
    /// Media player position update (periodic)
    MediaPlayerPosition {
        flow_id: FlowId,
        block_id: String,
        /// Current position in nanoseconds
        position_ns: u64,
        /// Total duration in nanoseconds
        duration_ns: u64,
        /// Current file index (0-based)
        current_file_index: usize,
        /// Total number of files in playlist
        total_files: usize,
    },
    /// Media player state changed
    MediaPlayerStateChanged {
        flow_id: FlowId,
        block_id: String,
        /// Playback state: "playing", "paused", "stopped", "buffering"
        state: String,
        /// Current file path (if any)
        current_file: Option<String>,
    },
    /// A transition was triggered on a compositor block
    TransitionTriggered {
        flow_id: FlowId,
        block_instance_id: String,
        from_input: usize,
        to_input: usize,
        transition_type: String,
        duration_ms: u64,
    },
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
            StromEvent::QoSStats {
                flow_id,
                block_id,
                element_id,
                internal_element_type,
                event_count,
                avg_proportion,
                is_falling_behind,
                ..
            } => {
                let target = if let Some(block_id) = block_id {
                    if let Some(elem_type) = internal_element_type {
                        format!("block {} ({})", block_id, elem_type)
                    } else {
                        format!("block {}", block_id)
                    }
                } else {
                    format!("element {}", element_id)
                };

                if *is_falling_behind {
                    format!(
                        "QoS: {} in flow {} falling behind ({} events, avg proportion {:.3})",
                        target, flow_id, event_count, avg_proportion
                    )
                } else {
                    format!(
                        "QoS: {} in flow {} OK ({} events, avg proportion {:.3})",
                        target, flow_id, event_count, avg_proportion
                    )
                }
            }
            StromEvent::PtpStats {
                flow_id,
                synced,
                mean_path_delay_ns,
                clock_offset_ns,
                ..
            } => {
                let status = if *synced { "synced" } else { "not synced" };
                let delay = mean_path_delay_ns
                    .map(|ns| format!("{:.1}µs delay", ns as f64 / 1000.0))
                    .unwrap_or_default();
                let offset = clock_offset_ns
                    .map(|ns| format!("{:.1}µs offset", ns as f64 / 1000.0))
                    .unwrap_or_default();
                format!(
                    "PTP stats for flow {}: {} {} {}",
                    flow_id, status, delay, offset
                )
            }
            StromEvent::SourceOutputAvailable {
                source_flow_id,
                output_name,
                channel_name,
            } => {
                format!(
                    "Source output '{}' from flow {} available (channel: {})",
                    output_name, source_flow_id, channel_name
                )
            }
            StromEvent::SourceOutputUnavailable {
                source_flow_id,
                output_name,
            } => {
                format!(
                    "Source output '{}' from flow {} unavailable",
                    output_name, source_flow_id
                )
            }
            StromEvent::SubscriptionStatusChanged {
                consumer_flow_id,
                source_flow_id,
                output_name,
                connected,
            } => {
                let status = if *connected {
                    "connected"
                } else {
                    "disconnected"
                };
                format!(
                    "Subscription to '{}' from flow {} in flow {}: {}",
                    output_name, source_flow_id, consumer_flow_id, status
                )
            }
            StromEvent::StreamDiscovered {
                stream_id,
                name,
                source,
            } => {
                format!(
                    "Discovered AES67 stream '{}' ({}) via {}",
                    name, stream_id, source
                )
            }
            StromEvent::StreamUpdated { stream_id } => {
                format!("Updated AES67 stream {}", stream_id)
            }
            StromEvent::StreamRemoved { stream_id } => {
                format!("Removed AES67 stream {}", stream_id)
            }
            StromEvent::MediaPlayerPosition {
                flow_id,
                block_id,
                position_ns,
                duration_ns,
                current_file_index,
                total_files,
            } => {
                let pos_secs = *position_ns as f64 / 1_000_000_000.0;
                let dur_secs = *duration_ns as f64 / 1_000_000_000.0;
                format!(
                    "Media player {} in flow {}: {:.1}s / {:.1}s (file {}/{})",
                    block_id,
                    flow_id,
                    pos_secs,
                    dur_secs,
                    current_file_index + 1,
                    total_files
                )
            }
            StromEvent::MediaPlayerStateChanged {
                flow_id,
                block_id,
                state,
                current_file,
            } => {
                if let Some(file) = current_file {
                    format!(
                        "Media player {} in flow {} state: {} ({})",
                        block_id, flow_id, state, file
                    )
                } else {
                    format!(
                        "Media player {} in flow {} state: {}",
                        block_id, flow_id, state
                    )
                }
            }
            StromEvent::TransitionTriggered {
                flow_id,
                block_instance_id,
                from_input,
                to_input,
                transition_type,
                duration_ms,
            } => {
                format!(
                    "Transition {} triggered on {} in flow {}: {} -> {} ({}ms)",
                    transition_type, block_instance_id, flow_id, from_input, to_input, duration_ms
                )
            }
        }
    }
}
