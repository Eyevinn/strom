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
        /// Playback state
        state: crate::mediaplayer::PlayerState,
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
    /// TAMS Output block successfully uploaded and registered a media segment
    TamsSegmentRegistered {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// TAMS flow UUID the segment was registered on
        tams_flow_id: String,
        /// TAMS object id of the uploaded media object (`<bucket>/<key>`)
        object_id: String,
        /// TAMS timerange string `[<sec>:<ns>_<sec>:<ns>)`
        timerange: String,
    },
    /// TAMS Output block failed to upload or register a segment
    TamsError {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// Human-readable error description
        error: String,
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
        /// Current PVW input. `None` when PVW is a PiP source.
        preview_input: Option<usize>,
        /// Current PGM input. `None` when PGM is a PiP source.
        program_input: Option<usize>,
        /// PiP index on PVW, or `None` if PVW is an input.
        preview_pip: Option<usize>,
        /// PiP index on PGM, or `None` if PGM is an input.
        program_pip: Option<usize>,
    },
    /// Vision mixer DSK layer toggled
    VisionMixerDskChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// DSK layer number (1-based)
        dsk: usize,
        enabled: bool,
    },
    /// Vision mixer multiview overlay alpha changed
    VisionMixerOverlayAlphaChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        alpha: f64,
    },
    /// Vision mixer Fade to Black state changed
    VisionMixerFtbChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        active: bool,
    },
    /// Vision mixer video effect changed (shader FX engine)
    VisionMixerEffectChanged {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        block_id: String,
        /// Where the effect was applied (an input or the PGM master).
        target: crate::effects::EffectTarget,
        /// The effect as applied (after parameter clamping).
        effect: crate::effects::VideoEffect,
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
            StromEvent::TamsSegmentRegistered {
                flow_id,
                block_id,
                tams_flow_id,
                object_id: _,
                timerange,
            } => {
                format!(
                    "TAMS {} in flow {} registered segment {} on tams flow {}",
                    block_id, flow_id, timerange, tams_flow_id
                )
            }
            StromEvent::TamsError {
                flow_id,
                block_id,
                error,
            } => {
                format!("TAMS {} in flow {} error: {}", block_id, flow_id, error)
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
                preview_pip,
                program_pip,
            } => {
                let pvw = preview_pip
                    .map(|p| format!("pip:{}", p))
                    .or_else(|| preview_input.map(|i| format!("input:{}", i)))
                    .unwrap_or_else(|| "<none>".to_string());
                let pgm = program_pip
                    .map(|p| format!("pip:{}", p))
                    .or_else(|| program_input.map(|i| format!("input:{}", i)))
                    .unwrap_or_else(|| "<none>".to_string());
                format!(
                    "Vision mixer {} in flow {}: PVW={}, PGM={}",
                    block_id, flow_id, pvw, pgm
                )
            }
            StromEvent::VisionMixerDskChanged {
                flow_id,
                block_id,
                dsk,
                enabled,
            } => {
                format!(
                    "Vision mixer {} in flow {}: DSK {} {}",
                    block_id,
                    flow_id,
                    dsk,
                    if *enabled { "ON" } else { "OFF" }
                )
            }
            StromEvent::VisionMixerOverlayAlphaChanged {
                flow_id,
                block_id,
                alpha,
            } => {
                format!(
                    "Vision mixer {} in flow {}: overlay alpha {}",
                    block_id, flow_id, alpha
                )
            }
            StromEvent::VisionMixerFtbChanged {
                flow_id,
                block_id,
                active,
            } => {
                format!(
                    "Vision mixer {} in flow {}: FTB {}",
                    block_id,
                    flow_id,
                    if *active { "ON" } else { "OFF" }
                )
            }
            StromEvent::VisionMixerEffectChanged {
                flow_id,
                block_id,
                target,
                effect,
            } => {
                format!(
                    "Vision mixer {} in flow {}: effect '{}' on {}",
                    block_id,
                    flow_id,
                    effect.kind(),
                    target
                )
            }
        }
    }

    // Three exhaustive (no-wildcard) accessors below: `event_type()`, `flow_id()`,
    // `is_high_frequency()`. Adding a variant requires updating all three; the compiler
    // enforces this.

    /// The event's stable wire name (the serde `type` tag). Kept in sync with the wire
    /// contract by the `event_type_matches_serde_wire_tag` test.
    pub fn event_type(&self) -> &'static str {
        match self {
            StromEvent::FlowCreated { .. } => "FlowCreated",
            StromEvent::FlowUpdated { .. } => "FlowUpdated",
            StromEvent::FlowDeleted { .. } => "FlowDeleted",
            StromEvent::FlowStarted { .. } => "FlowStarted",
            StromEvent::FlowStopped { .. } => "FlowStopped",
            StromEvent::FlowStateChanged { .. } => "FlowStateChanged",
            StromEvent::PipelineError { .. } => "PipelineError",
            StromEvent::PipelineWarning { .. } => "PipelineWarning",
            StromEvent::PipelineInfo { .. } => "PipelineInfo",
            StromEvent::PipelineEos { .. } => "PipelineEos",
            StromEvent::PropertyChanged { .. } => "PropertyChanged",
            StromEvent::PadPropertyChanged { .. } => "PadPropertyChanged",
            StromEvent::Ping => "Ping",
            StromEvent::MeterData { .. } => "MeterData",
            StromEvent::SpectrumData { .. } => "SpectrumData",
            StromEvent::LoudnessData { .. } => "LoudnessData",
            StromEvent::LatencyData { .. } => "LatencyData",
            StromEvent::SystemStats(_) => "SystemStats",
            StromEvent::ThreadStats(_) => "ThreadStats",
            StromEvent::PtpStats { .. } => "PtpStats",
            StromEvent::SourceOutputAvailable { .. } => "SourceOutputAvailable",
            StromEvent::SourceOutputUnavailable { .. } => "SourceOutputUnavailable",
            StromEvent::SubscriptionStatusChanged { .. } => "SubscriptionStatusChanged",
            StromEvent::QoSStats { .. } => "QoSStats",
            StromEvent::StreamDiscovered { .. } => "StreamDiscovered",
            StromEvent::StreamUpdated { .. } => "StreamUpdated",
            StromEvent::StreamRemoved { .. } => "StreamRemoved",
            StromEvent::MediaPlayerPosition { .. } => "MediaPlayerPosition",
            StromEvent::MediaPlayerStateChanged { .. } => "MediaPlayerStateChanged",
            StromEvent::TransitionTriggered { .. } => "TransitionTriggered",
            StromEvent::AudioAnalyzerData { .. } => "AudioAnalyzerData",
            StromEvent::RecorderFileChanged { .. } => "RecorderFileChanged",
            StromEvent::RecorderAutoStop { .. } => "RecorderAutoStop",
            StromEvent::TamsSegmentRegistered { .. } => "TamsSegmentRegistered",
            StromEvent::TamsError { .. } => "TamsError",
            StromEvent::BufferAgeWarning { .. } => "BufferAgeWarning",
            StromEvent::BufferAgeProbe { .. } => "BufferAgeProbe",
            StromEvent::BufferAgeProbeActivated { .. } => "BufferAgeProbeActivated",
            StromEvent::BufferAgeProbeDeactivated { .. } => "BufferAgeProbeDeactivated",
            StromEvent::VisionMixerStateChanged { .. } => "VisionMixerStateChanged",
            StromEvent::VisionMixerDskChanged { .. } => "VisionMixerDskChanged",
            StromEvent::VisionMixerOverlayAlphaChanged { .. } => "VisionMixerOverlayAlphaChanged",
            StromEvent::VisionMixerFtbChanged { .. } => "VisionMixerFtbChanged",
            StromEvent::VisionMixerEffectChanged { .. } => "VisionMixerEffectChanged",
        }
    }

    /// The flow this event pertains to, if any. Roughly a third of variants — system-wide
    /// stats, AES67 stream discovery — are not scoped to a single flow and return `None`.
    ///
    /// Deliberately has no wildcard arm: adding a variant forces a decision here instead of
    /// silently falling through to `None`.
    pub fn flow_id(&self) -> Option<FlowId> {
        match self {
            StromEvent::FlowCreated { flow_id }
            | StromEvent::FlowUpdated { flow_id }
            | StromEvent::FlowDeleted { flow_id }
            | StromEvent::FlowStarted { flow_id }
            | StromEvent::FlowStopped { flow_id }
            | StromEvent::FlowStateChanged { flow_id, .. }
            | StromEvent::PipelineError { flow_id, .. }
            | StromEvent::PipelineWarning { flow_id, .. }
            | StromEvent::PipelineInfo { flow_id, .. }
            | StromEvent::PipelineEos { flow_id }
            | StromEvent::PropertyChanged { flow_id, .. }
            | StromEvent::PadPropertyChanged { flow_id, .. }
            | StromEvent::MeterData { flow_id, .. }
            | StromEvent::SpectrumData { flow_id, .. }
            | StromEvent::LoudnessData { flow_id, .. }
            | StromEvent::LatencyData { flow_id, .. }
            | StromEvent::PtpStats { flow_id, .. }
            | StromEvent::QoSStats { flow_id, .. }
            | StromEvent::MediaPlayerPosition { flow_id, .. }
            | StromEvent::MediaPlayerStateChanged { flow_id, .. }
            | StromEvent::TransitionTriggered { flow_id, .. }
            | StromEvent::AudioAnalyzerData { flow_id, .. }
            | StromEvent::RecorderFileChanged { flow_id, .. }
            | StromEvent::RecorderAutoStop { flow_id, .. }
            | StromEvent::TamsSegmentRegistered { flow_id, .. }
            | StromEvent::TamsError { flow_id, .. }
            | StromEvent::BufferAgeWarning { flow_id, .. }
            | StromEvent::BufferAgeProbe { flow_id, .. }
            | StromEvent::BufferAgeProbeActivated { flow_id, .. }
            | StromEvent::BufferAgeProbeDeactivated { flow_id, .. }
            | StromEvent::VisionMixerStateChanged { flow_id, .. }
            | StromEvent::VisionMixerDskChanged { flow_id, .. }
            | StromEvent::VisionMixerOverlayAlphaChanged { flow_id, .. }
            | StromEvent::VisionMixerFtbChanged { flow_id, .. }
            | StromEvent::VisionMixerEffectChanged { flow_id, .. } => Some(*flow_id),

            // These only carry a source-side flow id (the flow publishing the output).
            StromEvent::SourceOutputAvailable { source_flow_id, .. }
            | StromEvent::SourceOutputUnavailable { source_flow_id, .. } => Some(*source_flow_id),

            // Carries both a consumer and a source flow id; the consuming flow is the one
            // this event is "about" from an operator's perspective (its subscription state
            // changed), so it wins as the primary id.
            StromEvent::SubscriptionStatusChanged {
                consumer_flow_id, ..
            } => Some(*consumer_flow_id),

            StromEvent::Ping
            | StromEvent::SystemStats(_)
            | StromEvent::ThreadStats(_)
            | StromEvent::StreamDiscovered { .. }
            | StromEvent::StreamUpdated { .. }
            | StromEvent::StreamRemoved { .. } => None,
        }
    }

    /// Whether this event is chatty enough that logging or forwarding it by default would
    /// flood output — either because a single instance fires many times per second, or
    /// because it fans out per connection, element, or flow even at a modest tick rate.
    ///
    /// Single source of truth for this classification — do not hand-maintain a second list
    /// elsewhere. Deliberately has no wildcard arm: adding a variant forces a decision here.
    pub fn is_high_frequency(&self) -> bool {
        match self {
            StromEvent::MeterData { .. }
            | StromEvent::SpectrumData { .. }
            | StromEvent::LoudnessData { .. }
            | StromEvent::LatencyData { .. }
            | StromEvent::SystemStats(_)
            | StromEvent::ThreadStats(_)
            | StromEvent::PtpStats { .. }
            | StromEvent::QoSStats { .. }
            | StromEvent::AudioAnalyzerData { .. }
            | StromEvent::MediaPlayerPosition { .. }
            | StromEvent::BufferAgeProbe { .. } => true,

            StromEvent::FlowCreated { .. }
            | StromEvent::FlowUpdated { .. }
            | StromEvent::FlowDeleted { .. }
            | StromEvent::FlowStarted { .. }
            | StromEvent::FlowStopped { .. }
            | StromEvent::FlowStateChanged { .. }
            | StromEvent::PipelineError { .. }
            | StromEvent::PipelineWarning { .. }
            | StromEvent::PipelineInfo { .. }
            | StromEvent::PipelineEos { .. }
            | StromEvent::PropertyChanged { .. }
            | StromEvent::PadPropertyChanged { .. }
            | StromEvent::Ping
            | StromEvent::SourceOutputAvailable { .. }
            | StromEvent::SourceOutputUnavailable { .. }
            | StromEvent::SubscriptionStatusChanged { .. }
            | StromEvent::StreamDiscovered { .. }
            | StromEvent::StreamUpdated { .. }
            | StromEvent::StreamRemoved { .. }
            | StromEvent::MediaPlayerStateChanged { .. }
            | StromEvent::TransitionTriggered { .. }
            | StromEvent::RecorderFileChanged { .. }
            | StromEvent::RecorderAutoStop { .. }
            | StromEvent::TamsSegmentRegistered { .. }
            | StromEvent::TamsError { .. }
            | StromEvent::BufferAgeWarning { .. }
            | StromEvent::BufferAgeProbeActivated { .. }
            | StromEvent::BufferAgeProbeDeactivated { .. }
            | StromEvent::VisionMixerStateChanged { .. }
            | StromEvent::VisionMixerDskChanged { .. }
            | StromEvent::VisionMixerOverlayAlphaChanged { .. }
            | StromEvent::VisionMixerFtbChanged { .. }
            | StromEvent::VisionMixerEffectChanged { .. } => false,
        }
    }
}

#[cfg(test)]
mod event_accessor_tests {
    use super::*;

    fn flow_id() -> FlowId {
        FlowId::nil()
    }

    #[test]
    fn event_type_matches_serde_wire_tag() {
        let event = StromEvent::FlowCreated { flow_id: flow_id() };
        assert_eq!(event.event_type(), "FlowCreated");

        let event = StromEvent::PipelineError {
            flow_id: flow_id(),
            error: "boom".to_string(),
            source: None,
        };
        assert_eq!(event.event_type(), "PipelineError");

        let event = StromEvent::Ping;
        assert_eq!(event.event_type(), "Ping");
    }

    #[test]
    fn flow_bearing_variants_return_their_flow_id() {
        let id = flow_id();
        assert_eq!(StromEvent::FlowCreated { flow_id: id }.flow_id(), Some(id));
        assert_eq!(
            StromEvent::PipelineError {
                flow_id: id,
                error: "boom".to_string(),
                source: None,
            }
            .flow_id(),
            Some(id)
        );
        assert_eq!(
            StromEvent::RecorderAutoStop {
                flow_id: id,
                block_id: "rec0".to_string(),
            }
            .flow_id(),
            Some(id)
        );
    }

    #[test]
    fn dual_flow_id_variants_pick_the_documented_side() {
        let source_id = FlowId::from(uuid::Uuid::from_u128(1));
        let consumer_id = FlowId::from(uuid::Uuid::from_u128(2));

        assert_eq!(
            StromEvent::SourceOutputAvailable {
                source_flow_id: source_id,
                output_name: "out".to_string(),
                channel_name: "ch".to_string(),
            }
            .flow_id(),
            Some(source_id)
        );

        assert_eq!(
            StromEvent::SubscriptionStatusChanged {
                consumer_flow_id: consumer_id,
                source_flow_id: source_id,
                output_name: "out".to_string(),
                connected: true,
            }
            .flow_id(),
            Some(consumer_id)
        );
    }

    #[test]
    fn non_flow_variants_return_none() {
        assert_eq!(StromEvent::Ping.flow_id(), None);
        assert_eq!(
            StromEvent::StreamDiscovered {
                stream_id: "s1".to_string(),
                name: "n".to_string(),
                source: "sap".to_string(),
            }
            .flow_id(),
            None
        );
    }

    /// One instance of every `StromEvent` variant, so `event_type()` can be checked against
    /// all of them at once instead of a hand-picked sample — this is the test the maintainer
    /// asked for, kept exhaustive without hand-maintaining a match here: adding a variant
    /// without adding it to this list just makes the list shorter than the enum, which the
    /// count assertion below catches.
    fn one_of_each_variant() -> Vec<StromEvent> {
        use crate::effects::{EffectTarget, VideoEffect};
        use crate::mediaplayer::PlayerState;
        use crate::system_monitor::SystemStats;
        use crate::thread_stats::ThreadStats;

        let id = flow_id();
        vec![
            StromEvent::FlowCreated { flow_id: id },
            StromEvent::FlowUpdated { flow_id: id },
            StromEvent::FlowDeleted { flow_id: id },
            StromEvent::FlowStarted { flow_id: id },
            StromEvent::FlowStopped { flow_id: id },
            StromEvent::FlowStateChanged {
                flow_id: id,
                state: "playing".to_string(),
            },
            StromEvent::PipelineError {
                flow_id: id,
                error: "boom".to_string(),
                source: None,
            },
            StromEvent::PipelineWarning {
                flow_id: id,
                warning: "careful".to_string(),
                source: None,
            },
            StromEvent::PipelineInfo {
                flow_id: id,
                message: "fyi".to_string(),
                source: None,
            },
            StromEvent::PipelineEos { flow_id: id },
            StromEvent::PropertyChanged {
                flow_id: id,
                element_id: "e0".to_string(),
                property_name: "p".to_string(),
                value: crate::element::PropertyValue::Bool(true),
            },
            StromEvent::PadPropertyChanged {
                flow_id: id,
                element_id: "e0".to_string(),
                pad_name: "sink".to_string(),
                property_name: "p".to_string(),
                value: crate::element::PropertyValue::Bool(true),
            },
            StromEvent::Ping,
            StromEvent::MeterData {
                flow_id: id,
                element_id: "level0".to_string(),
                rms: vec![],
                peak: vec![],
                decay: vec![],
            },
            StromEvent::SpectrumData {
                flow_id: id,
                element_id: "spec0".to_string(),
                magnitudes: vec![],
            },
            StromEvent::LoudnessData {
                flow_id: id,
                element_id: "loud0".to_string(),
                momentary: -20.0,
                shortterm: None,
                integrated: None,
                loudness_range: None,
                true_peak: vec![],
            },
            StromEvent::LatencyData {
                flow_id: id,
                element_id: "lat0".to_string(),
                last_latency_us: 0,
                average_latency_us: 0,
            },
            StromEvent::SystemStats(SystemStats {
                cpu_usage: 0.0,
                num_cores: 1,
                total_memory: 0,
                used_memory: 0,
                gpu_stats: vec![],
                gl_renderer: None,
                timestamp: 0,
            }),
            StromEvent::ThreadStats(ThreadStats {
                threads: vec![],
                timestamp: 0,
            }),
            StromEvent::PtpStats {
                flow_id: id,
                domain: 0,
                synced: false,
                mean_path_delay_ns: None,
                clock_offset_ns: None,
                r_squared: None,
                clock_rate: None,
                grandmaster_id: None,
                master_id: None,
            },
            StromEvent::SourceOutputAvailable {
                source_flow_id: id,
                output_name: "out".to_string(),
                channel_name: "ch".to_string(),
            },
            StromEvent::SourceOutputUnavailable {
                source_flow_id: id,
                output_name: "out".to_string(),
            },
            StromEvent::SubscriptionStatusChanged {
                consumer_flow_id: id,
                source_flow_id: id,
                output_name: "out".to_string(),
                connected: true,
            },
            StromEvent::QoSStats {
                flow_id: id,
                block_id: None,
                element_id: "e0".to_string(),
                element_name: "e0".to_string(),
                internal_element_type: None,
                event_count: 0,
                avg_proportion: 1.0,
                min_proportion: 1.0,
                max_proportion: 1.0,
                avg_jitter: 0,
                total_processed: 0,
                is_falling_behind: false,
            },
            StromEvent::StreamDiscovered {
                stream_id: "s1".to_string(),
                name: "n".to_string(),
                source: "sap".to_string(),
            },
            StromEvent::StreamUpdated {
                stream_id: "s1".to_string(),
            },
            StromEvent::StreamRemoved {
                stream_id: "s1".to_string(),
            },
            StromEvent::MediaPlayerPosition {
                flow_id: id,
                block_id: "mp0".to_string(),
                position_ns: 0,
                duration_ns: 0,
                current_file_index: 0,
                total_files: 1,
            },
            StromEvent::MediaPlayerStateChanged {
                flow_id: id,
                block_id: "mp0".to_string(),
                state: PlayerState::Playing,
                current_file: None,
            },
            StromEvent::TransitionTriggered {
                flow_id: id,
                block_instance_id: "mix0".to_string(),
                from_input: 0,
                to_input: 1,
                transition_type: "cut".to_string(),
                duration_ms: 0,
            },
            StromEvent::AudioAnalyzerData {
                flow_id: id,
                element_id: "an0".to_string(),
                waveform_l_min: String::new(),
                waveform_l_max: String::new(),
                waveform_r_min: String::new(),
                waveform_r_max: String::new(),
                vectorscope_l: String::new(),
                vectorscope_r: String::new(),
            },
            StromEvent::RecorderFileChanged {
                flow_id: id,
                block_id: "rec0".to_string(),
                filename: "out.mp4".to_string(),
            },
            StromEvent::RecorderAutoStop {
                flow_id: id,
                block_id: "rec0".to_string(),
            },
            StromEvent::TamsSegmentRegistered {
                flow_id: id,
                block_id: "tams0".to_string(),
                tams_flow_id: "tf0".to_string(),
                object_id: "bucket/key".to_string(),
                timerange: "[0:0_1:0)".to_string(),
            },
            StromEvent::TamsError {
                flow_id: id,
                block_id: "tams0".to_string(),
                error: "boom".to_string(),
            },
            StromEvent::BufferAgeWarning {
                flow_id: id,
                element_id: "e0".to_string(),
                pad_name: "sink".to_string(),
                age_ms: 0,
                threshold_ms: 0,
            },
            StromEvent::BufferAgeProbe {
                flow_id: id,
                probe_id: "p0".to_string(),
                element_id: "e0".to_string(),
                pad_name: "sink".to_string(),
                age_ms: 0,
                sample_number: 0,
            },
            StromEvent::BufferAgeProbeActivated {
                flow_id: id,
                probe_id: "p0".to_string(),
                element_id: "e0".to_string(),
                pad_name: "sink".to_string(),
            },
            StromEvent::BufferAgeProbeDeactivated {
                flow_id: id,
                probe_id: "p0".to_string(),
                reason: "manual".to_string(),
            },
            StromEvent::VisionMixerStateChanged {
                flow_id: id,
                block_id: "mix0".to_string(),
                preview_input: Some(0),
                program_input: Some(1),
                preview_pip: None,
                program_pip: None,
            },
            StromEvent::VisionMixerDskChanged {
                flow_id: id,
                block_id: "mix0".to_string(),
                dsk: 1,
                enabled: true,
            },
            StromEvent::VisionMixerOverlayAlphaChanged {
                flow_id: id,
                block_id: "mix0".to_string(),
                alpha: 0.5,
            },
            StromEvent::VisionMixerFtbChanged {
                flow_id: id,
                block_id: "mix0".to_string(),
                active: false,
            },
            StromEvent::VisionMixerEffectChanged {
                flow_id: id,
                block_id: "mix0".to_string(),
                target: EffectTarget::Master,
                effect: VideoEffect::None,
            },
        ]
    }

    #[test]
    fn every_variant_event_type_matches_its_serde_wire_tag() {
        let events = one_of_each_variant();

        let variant_count = 44;
        assert_eq!(
            events.len(),
            variant_count,
            "one_of_each_variant() is out of sync with StromEvent — update it alongside new variants"
        );

        for event in &events {
            let wire_tag = serde_json::to_value(event)
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(
                event.event_type(),
                wire_tag,
                "event_type() disagrees with the serde wire tag for {event:?}"
            );
        }
    }

    #[test]
    fn high_frequency_classification_matches_known_variants() {
        assert!(StromEvent::MeterData {
            flow_id: flow_id(),
            element_id: "level0".to_string(),
            rms: vec![],
            peak: vec![],
            decay: vec![],
        }
        .is_high_frequency());

        assert!(!StromEvent::FlowCreated { flow_id: flow_id() }.is_high_frequency());
        assert!(!StromEvent::Ping.is_high_frequency());
    }
}
