//! Events for real-time updates across clients.

use crate::element::PropertyValue;
use crate::system_monitor::SystemStats;
use crate::thread_stats::ThreadStats;
use crate::FlowId;
use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Event types that can be broadcast to all connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", content = "data")]
pub enum StromEvent {
    /// A flow was created
    FlowCreated {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// A flow was updated
    FlowUpdated {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// A flow was deleted
    FlowDeleted {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// A flow was started
    FlowStarted {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// A flow was stopped
    FlowStopped {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// A flow's state changed
    FlowStateChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        state: String,
    },
    /// Pipeline error occurred
    PipelineError {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        error: String,
        source: Option<String>,
    },
    /// Pipeline warning message
    PipelineWarning {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        warning: String,
        source: Option<String>,
    },
    /// Pipeline info message
    PipelineInfo {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        message: String,
        source: Option<String>,
    },
    /// Pipeline reached end of stream
    PipelineEos {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// Element property was changed on a running pipeline
    PropertyChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        property_name: String,
        value: PropertyValue,
    },
    /// Pad property was changed on a running pipeline
    PadPropertyChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
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
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        /// RMS values in dB for each channel
        rms: Vec<f64>,
        /// Peak values in dB for each channel
        peak: Vec<f64>,
        /// Decay values in dB for each channel
        decay: Vec<f64>,
    },
    /// Audio spectrum analyzer data from GStreamer spectrum element
    SpectrumData {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        /// Magnitude values in dB per channel, each inner Vec is one channel's frequency bands
        magnitudes: Vec<Vec<f32>>,
    },
    /// EBU R128 loudness measurement data from GStreamer ebur128level element
    LoudnessData {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        /// Momentary loudness in LUFS (400ms window)
        momentary: f64,
        /// Short-term loudness in LUFS (3s window)
        shortterm: Option<f64>,
        /// Integrated (global) loudness in LUFS (from start)
        integrated: Option<f64>,
        /// Loudness range in LU
        loudness_range: Option<f64>,
        /// True peak per channel in dBTP
        true_peak: Vec<f64>,
    },
    /// Audio latency measurement data from GStreamer audiolatency element
    LatencyData {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        /// Last measured latency in microseconds
        last_latency_us: i64,
        /// Running average latency in microseconds (last 5 measurements)
        average_latency_us: i64,
    },
    /// System monitoring statistics (CPU and GPU)
    SystemStats(SystemStats),
    /// Thread CPU statistics for GStreamer streaming threads
    ThreadStats(ThreadStats),
    /// PTP clock statistics for a flow
    PtpStats {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
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
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        source_flow_id: FlowId,
        output_name: String,
        channel_name: String,
    },
    /// A flow's published output became unavailable (flow stopped)
    SourceOutputUnavailable {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        source_flow_id: FlowId,
        output_name: String,
    },
    /// Subscription connection status changed
    SubscriptionStatusChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        consumer_flow_id: FlowId,
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        source_flow_id: FlowId,
        output_name: String,
        connected: bool,
    },
    /// Quality of Service statistics (aggregated buffer drop info)
    QoSStats {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
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
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
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
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// Playback state: "playing", "paused", "stopped", "buffering"
        state: String,
        /// Current file path (if any)
        current_file: Option<String>,
    },
    /// A transition was triggered on a compositor block
    TransitionTriggered {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_instance_id: String,
        from_input: usize,
        to_input: usize,
        transition_type: String,
        duration_ms: u64,
    },
    /// Audio analyzer waveform and vectorscope data from appsink
    AudioAnalyzerData {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        /// Waveform min values per column for L channel (base64-encoded i8 samples)
        waveform_l_min: String,
        /// Waveform max values per column for L channel (base64-encoded i8 samples)
        waveform_l_max: String,
        /// Waveform min values per column for R channel (base64-encoded i8 samples)
        waveform_r_min: String,
        /// Waveform max values per column for R channel (base64-encoded i8 samples)
        waveform_r_max: String,
        /// Vectorscope L channel samples (base64-encoded i8 samples)
        vectorscope_l: String,
        /// Vectorscope R channel samples (base64-encoded i8 samples)
        vectorscope_r: String,
    },
    /// Recorder block started writing a new file
    RecorderFileChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// Full path to the file currently being written
        filename: String,
    },
    /// Recorder block reached its configured max duration and requests the flow to stop
    RecorderAutoStop {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
    },
    /// Buffer age warning (buffer is older than threshold)
    BufferAgeWarning {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        element_id: String,
        pad_name: String,
        /// Buffer age in milliseconds
        age_ms: u64,
        /// Threshold that was exceeded, in milliseconds
        threshold_ms: u64,
    },
    /// Manual buffer age probe measurement
    BufferAgeProbe {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        probe_id: String,
        element_id: String,
        pad_name: String,
        /// Buffer age in milliseconds
        age_ms: u64,
        /// Sequential sample number
        sample_number: u64,
    },
    /// A manual buffer age probe was activated
    BufferAgeProbeActivated {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        probe_id: String,
        element_id: String,
        pad_name: String,
    },
    /// A manual buffer age probe was deactivated
    BufferAgeProbeDeactivated {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        probe_id: String,
        /// Reason: "manual", "timeout", "flow_stopped"
        reason: String,
    },
    /// Vision mixer PVW/PGM state changed
    VisionMixerStateChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        preview_input: usize,
        program_input: usize,
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
            StromEvent::SpectrumData {
                flow_id,
                element_id,
                magnitudes,
                ..
            } => {
                let bands = magnitudes.first().map_or(0, |ch| ch.len());
                format!(
                    "Spectrum data from {} in flow {} ({} ch, {} bands)",
                    element_id,
                    flow_id,
                    magnitudes.len(),
                    bands
                )
            }
            StromEvent::LoudnessData {
                flow_id,
                element_id,
                momentary,
                integrated,
                ..
            } => {
                let i_str = integrated
                    .map(|v| format!("{:.1}", v))
                    .unwrap_or_else(|| "---".to_string());
                format!(
                    "Loudness data from {} in flow {}: M={:.1} LUFS, I={} LUFS",
                    element_id, flow_id, momentary, i_str
                )
            }
            StromEvent::LatencyData {
                flow_id,
                element_id,
                last_latency_us,
                average_latency_us,
            } => {
                format!(
                    "Latency data from {} in flow {}: last={:.2}ms, avg={:.2}ms",
                    element_id,
                    flow_id,
                    *last_latency_us as f64 / 1000.0,
                    *average_latency_us as f64 / 1000.0
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
            StromEvent::ThreadStats(stats) => {
                format!("Thread stats: {} active threads", stats.threads.len())
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
            StromEvent::AudioAnalyzerData {
                flow_id,
                element_id,
                waveform_l_min,
                vectorscope_l,
                ..
            } => {
                format!(
                    "Audio analyzer data from {} in flow {} ({} columns, {} vector pairs)",
                    element_id,
                    flow_id,
                    waveform_l_min.len() * 3 / 4,
                    vectorscope_l.len() * 3 / 4
                )
            }
            StromEvent::RecorderFileChanged {
                flow_id,
                block_id,
                filename,
            } => {
                format!(
                    "Recorder {} in flow {} writing: {}",
                    block_id, flow_id, filename
                )
            }
            StromEvent::RecorderAutoStop { flow_id, block_id } => {
                format!(
                    "Recorder {} in flow {} reached max duration, stopping flow",
                    block_id, flow_id
                )
            }
            StromEvent::BufferAgeWarning {
                flow_id,
                element_id,
                pad_name,
                age_ms,
                threshold_ms,
            } => {
                format!(
                    "Buffer age warning on {}:{} in flow {}: {}ms (threshold {}ms)",
                    element_id, pad_name, flow_id, age_ms, threshold_ms
                )
            }
            StromEvent::BufferAgeProbe {
                flow_id,
                probe_id,
                element_id,
                pad_name,
                age_ms,
                sample_number,
            } => {
                format!(
                    "Buffer age probe {} on {}:{} in flow {}: {}ms (sample #{})",
                    probe_id, element_id, pad_name, flow_id, age_ms, sample_number
                )
            }
            StromEvent::BufferAgeProbeActivated {
                flow_id,
                probe_id,
                element_id,
                pad_name,
            } => {
                format!(
                    "Buffer age probe {} activated on {}:{} in flow {}",
                    probe_id, element_id, pad_name, flow_id
                )
            }
            StromEvent::BufferAgeProbeDeactivated {
                flow_id,
                probe_id,
                reason,
            } => {
                format!(
                    "Buffer age probe {} deactivated in flow {}: {}",
                    probe_id, flow_id, reason
                )
            }
            StromEvent::VisionMixerStateChanged {
                flow_id,
                block_id,
                preview_input,
                program_input,
            } => {
                format!(
                    "Vision mixer {} in flow {}: PVW={}, PGM={}",
                    block_id, flow_id, preview_input, program_input
                )
            }
        }
    }
}
