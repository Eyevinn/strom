//! API request and response types.

use crate::element::{ElementInfo, MediaType, PropertyValue};
use crate::flow::{Flow, FlowId, FlowProperties};
use crate::state::PipelineState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

// ============================================================================
// Flow API Types
// ============================================================================

/// Request to create a new flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateFlowRequest {
    pub name: String,
    /// Optional description for the flow
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Request to update an existing flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdateFlowRequest {
    pub flow: Flow,
}

/// Response containing a single flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowResponse {
    pub flow: Flow,
}

/// Response containing a list of flows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowListResponse {
    pub flows: Vec<Flow>,
}

/// Response for flow state query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowStateResponse {
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub id: FlowId,
    pub state: PipelineState,
}

/// Request to update flow properties.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdateFlowPropertiesRequest {
    pub properties: FlowProperties,
}

// ============================================================================
// Element API Types
// ============================================================================

/// Response containing information about available elements.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ElementListResponse {
    pub elements: Vec<ElementInfo>,
}

/// Response containing detailed information about a specific element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ElementInfoResponse {
    pub element: ElementInfo,
}

// ============================================================================
// Property API Types (for live updates)
// ============================================================================

/// Request to update a property on a running pipeline element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdatePropertyRequest {
    /// The name of the property to update
    pub property_name: String,
    /// The new value for the property
    pub value: PropertyValue,
}

/// Response containing current property values from a running element.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ElementPropertiesResponse {
    /// The element ID
    pub element_id: String,
    /// Current property values
    pub properties: HashMap<String, PropertyValue>,
}

/// Request to update a property on a pad in a running pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdatePadPropertyRequest {
    /// The name of the property to update
    pub property_name: String,
    /// The new value for the property
    pub value: PropertyValue,
}

/// Response containing current property values from a pad.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PadPropertiesResponse {
    /// The element ID
    pub element_id: String,
    /// The pad name
    pub pad_name: String,
    /// Current property values
    pub properties: HashMap<String, PropertyValue>,
}

// ============================================================================
// Latency API Types
// ============================================================================

/// Response containing pipeline latency information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct LatencyResponse {
    /// Minimum latency in nanoseconds
    pub min_latency_ns: u64,
    /// Maximum latency in nanoseconds
    pub max_latency_ns: u64,
    /// Whether the pipeline is a live pipeline
    pub live: bool,
    /// Minimum latency formatted as human-readable string (e.g., "10.5 ms")
    pub min_latency_formatted: String,
    /// Maximum latency formatted as human-readable string
    pub max_latency_formatted: String,
}

impl LatencyResponse {
    /// Create a new latency response from raw values.
    pub fn new(min_ns: u64, max_ns: u64, live: bool) -> Self {
        Self {
            min_latency_ns: min_ns,
            max_latency_ns: max_ns,
            live,
            min_latency_formatted: Self::format_ns(min_ns),
            max_latency_formatted: Self::format_ns(max_ns),
        }
    }

    /// Format nanoseconds as a human-readable string.
    fn format_ns(ns: u64) -> String {
        if ns == 0 {
            "0 ns".to_string()
        } else if ns < 1_000 {
            format!("{} ns", ns)
        } else if ns < 1_000_000 {
            format!("{:.2} µs", ns as f64 / 1_000.0)
        } else if ns < 1_000_000_000 {
            format!("{:.2} ms", ns as f64 / 1_000_000.0)
        } else {
            format!("{:.2} s", ns as f64 / 1_000_000_000.0)
        }
    }
}

// ============================================================================
// WebSocket Message Types
// ============================================================================

/// Messages sent from server to client via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Pipeline state has changed
    StateChange {
        flow_id: FlowId,
        state: PipelineState,
    },
    /// An error occurred
    Error {
        flow_id: Option<FlowId>,
        message: String,
    },
    /// A warning message
    Warning {
        flow_id: Option<FlowId>,
        message: String,
    },
    /// Informational message
    Info { message: String },
}

/// Messages sent from client to server via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to updates for a specific flow
    Subscribe { flow_id: FlowId },
    /// Unsubscribe from updates for a specific flow
    Unsubscribe { flow_id: FlowId },
    /// Ping to keep connection alive
    Ping,
}

// ============================================================================
// WebRTC Stats Types
// ============================================================================

/// WebRTC statistics for a flow.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WebRtcStats {
    /// Stats for each WebRTC connection (keyed by element name)
    pub connections: HashMap<String, WebRtcConnectionStats>,
}

/// Stats for a single WebRTC connection (webrtcbin element).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WebRtcConnectionStats {
    /// Inbound RTP stream statistics
    pub inbound_rtp: Vec<RtpStreamStats>,
    /// Outbound RTP stream statistics
    pub outbound_rtp: Vec<RtpStreamStats>,
    /// ICE candidate pair statistics
    pub ice_candidates: Option<IceCandidateStats>,
    /// Transport statistics
    pub transport: Option<TransportStats>,
    /// Codec statistics (keyed by codec ID)
    pub codecs: Vec<CodecStats>,
    /// Raw stats as key-value pairs (for debugging/extensibility)
    pub raw: HashMap<String, String>,
}

/// RTP stream statistics (inbound or outbound).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RtpStreamStats {
    /// Stream identifier
    pub ssrc: Option<u32>,
    /// Media type (audio or video)
    pub media_type: Option<String>,
    /// Codec being used
    pub codec: Option<String>,
    /// Total bytes sent/received
    pub bytes: Option<u64>,
    /// Total packets sent/received
    pub packets: Option<u64>,
    /// Packets lost (inbound only)
    pub packets_lost: Option<i64>,
    /// Fraction of packets lost in last interval (0.0-1.0, inbound only)
    pub fraction_lost: Option<f64>,
    /// Jitter in seconds (inbound only)
    pub jitter: Option<f64>,
    /// Round-trip time in seconds
    pub round_trip_time: Option<f64>,
    /// Bitrate in bits per second (calculated)
    pub bitrate: Option<u64>,
}

/// ICE candidate statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct IceCandidateStats {
    /// Local candidate type (host, srflx, relay)
    pub local_candidate_type: Option<String>,
    /// Remote candidate type
    pub remote_candidate_type: Option<String>,
    /// Connection state
    pub state: Option<String>,
    /// Local candidate address
    pub local_address: Option<String>,
    /// Local candidate port
    pub local_port: Option<u32>,
    /// Local candidate protocol (UDP/TCP)
    pub local_protocol: Option<String>,
    /// Remote candidate address
    pub remote_address: Option<String>,
    /// Remote candidate port
    pub remote_port: Option<u32>,
    /// Remote candidate protocol
    pub remote_protocol: Option<String>,
}

/// Transport statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct TransportStats {
    /// Total bytes sent
    pub bytes_sent: Option<u64>,
    /// Total bytes received
    pub bytes_received: Option<u64>,
    /// Total packets sent
    pub packets_sent: Option<u64>,
    /// Total packets received
    pub packets_received: Option<u64>,
}

/// Codec statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CodecStats {
    /// Codec MIME type (e.g., "audio/opus", "video/VP8")
    pub mime_type: Option<String>,
    /// Clock rate in Hz
    pub clock_rate: Option<u32>,
    /// Payload type number
    pub payload_type: Option<u32>,
    /// Number of channels (for audio)
    pub channels: Option<u32>,
}

/// Response containing WebRTC statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct WebRtcStatsResponse {
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    pub stats: WebRtcStats,
}

// ============================================================================
// Statistics API Types
// ============================================================================

/// Response containing statistics for a running flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowStatsResponse {
    /// The flow ID
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    /// The flow name
    pub flow_name: String,
    /// Statistics for each block in the flow
    pub blocks: Vec<crate::stats::BlockStats>,
    /// Timestamp when stats were collected (nanoseconds since UNIX epoch)
    pub collected_at: u64,
}

// ============================================================================
// Debug Info API Types
// ============================================================================

/// Debug information for a running flow's pipeline.
/// Provides detailed timing, clock, and state information for troubleshooting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowDebugInfo {
    /// The flow ID
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    /// The flow name
    pub flow_name: String,
    /// Pipeline state (Playing, Paused, etc.)
    pub pipeline_state: Option<String>,
    /// Whether this is a live pipeline
    pub is_live: Option<bool>,

    // -- Timing information --
    /// Pipeline base_time in nanoseconds (reference point for running_time calculation)
    pub base_time_ns: Option<u64>,
    /// Current clock time in nanoseconds
    pub clock_time_ns: Option<u64>,
    /// Current running time in nanoseconds (clock_time - base_time)
    pub running_time_ns: Option<u64>,
    /// Human-readable running_time (how long the pipeline has been playing)
    pub running_time_formatted: Option<String>,

    // -- Clock information --
    /// Clock type being used (e.g., "PTP", "Monotonic", "Realtime")
    pub clock_type: Option<String>,
    /// PTP grandmaster clock ID (only if using PTP clock)
    pub ptp_grandmaster: Option<String>,

    // -- Latency information --
    /// Minimum pipeline latency in nanoseconds
    pub latency_min_ns: Option<u64>,
    /// Maximum pipeline latency in nanoseconds
    pub latency_max_ns: Option<u64>,
    /// Human-readable latency
    pub latency_formatted: Option<String>,

    // -- Pipeline structure --
    /// Number of elements in the pipeline
    pub element_count: Option<u32>,
}

// ============================================================================
// gst-launch API Types
// ============================================================================

/// Request to parse a gst-launch-1.0 pipeline string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ParseGstLaunchRequest {
    /// The gst-launch-1.0 pipeline string to parse
    /// Example: "videotestsrc pattern=ball ! videoconvert ! autovideosink"
    pub pipeline: String,
}

/// Response containing parsed pipeline elements and links.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ParseGstLaunchResponse {
    /// Elements extracted from the parsed pipeline
    pub elements: Vec<crate::element::Element>,
    /// Links between elements
    pub links: Vec<crate::element::Link>,
}

/// Request to convert flow elements/links to gst-launch-1.0 syntax.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ExportGstLaunchRequest {
    /// Elements to export
    pub elements: Vec<crate::element::Element>,
    /// Links between elements
    pub links: Vec<crate::element::Link>,
}

/// Response containing the gst-launch-1.0 pipeline string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ExportGstLaunchResponse {
    /// The generated gst-launch-1.0 pipeline string
    pub pipeline: String,
}

// ============================================================================
// Error Response
// ============================================================================

/// Standard error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ErrorResponse {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ErrorResponse {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            details: None,
        }
    }

    pub fn with_details(error: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            details: Some(details.into()),
        }
    }
}

// ============================================================================
// Sources API Types (for inter-pipeline sharing)
// ============================================================================

/// Information about an available published output from a source flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AvailableOutput {
    /// Name of the published output (block ID)
    pub name: String,
    /// Channel name for inter-pipeline communication (what InterInput blocks use)
    pub channel_name: String,
    /// Name of the flow that publishes this output
    pub flow_name: String,
    /// Description of the output
    pub description: Option<String>,
    /// Media type (Audio, Video, Generic)
    pub media_type: MediaType,
    /// Whether the source flow is currently running (output is active)
    pub is_active: bool,
}

/// Information about a flow that has published outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SourceFlowInfo {
    /// The flow ID
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    /// The flow name
    pub flow_name: String,
    /// Available outputs from this flow
    pub outputs: Vec<AvailableOutput>,
}

/// Response containing available source flows for subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AvailableSourcesResponse {
    /// List of flows that have published outputs
    pub sources: Vec<SourceFlowInfo>,
}

// ============================================================================
// Media File API Types
// ============================================================================

/// A file or directory entry in a media directory listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MediaFileEntry {
    /// File or directory name
    pub name: String,
    /// Full path relative to media root
    pub path: String,
    /// Whether this is a directory
    pub is_directory: bool,
    /// File size in bytes (0 for directories)
    pub size: u64,
    /// Last modified timestamp (UNIX epoch seconds)
    pub modified: u64,
    /// MIME type (None for directories)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Response containing a directory listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ListMediaResponse {
    /// Current directory path (relative to media root)
    pub current_path: String,
    /// Parent directory path (None if at root)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_path: Option<String>,
    /// Directory contents
    pub entries: Vec<MediaFileEntry>,
}

/// Request to rename a file or directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RenameMediaRequest {
    /// Current path (relative to media root)
    pub old_path: String,
    /// New name (just the filename, not full path)
    pub new_name: String,
}

/// Request to create a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateDirectoryRequest {
    /// Path for new directory (relative to media root)
    pub path: String,
}

/// Response for media operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MediaOperationResponse {
    /// Whether the operation succeeded
    pub success: bool,
    /// Human-readable message
    pub message: String,
}

impl MediaOperationResponse {
    /// Create a success response.
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }

    /// Create an error response.
    #[allow(dead_code)]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            success: false,
            message: message.into(),
        }
    }
}
