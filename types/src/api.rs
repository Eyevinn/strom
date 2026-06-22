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
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct UpdatePropertyRequest {
    /// The name of the property to update
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 255)))]
    pub property_name: String,
    /// The new value for the property
    #[cfg_attr(feature = "validation", garde(skip))]
    pub value: PropertyValue,
    /// Optional ramp duration in milliseconds. Currently honored for audio
    /// `volume`-element `volume` and `mute` updates — when set, `volume` is
    /// interpolated per-sample over the given duration (anti-zipper / fade)
    /// and `mute=true` is preceded by a fade-out of the same length while
    /// `mute=false` is followed by a 0→pre_mute fade-in. Useful for
    /// broadcast-style on-air / off-air route transitions (e.g. 500 ms).
    /// When omitted, a short default ramp is used for `volume`/`mute`; other
    /// properties are set immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(range(max = 60000)))]
    pub ramp_ms: Option<u32>,
}

/// Request to trigger a transition on a compositor block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct TriggerTransitionRequest {
    /// Index of the currently active input (0-based)
    #[cfg_attr(feature = "validation", garde(skip))]
    pub from_input: usize,
    /// Index of the input to transition to (0-based)
    #[cfg_attr(feature = "validation", garde(skip))]
    pub to_input: usize,
    /// Type of transition: "cut", "fade", "slide_left", "slide_right", "slide_up", "slide_down"
    #[serde(default = "default_transition_type")]
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 50)))]
    pub transition_type: String,
    /// Duration of the transition in milliseconds (ignored for "cut")
    #[serde(default = "default_transition_duration")]
    #[cfg_attr(feature = "validation", garde(range(max = 60000)))]
    pub duration_ms: u64,
}

fn default_transition_type() -> String {
    "fade".to_string()
}

fn default_transition_duration() -> u64 {
    300
}

/// Response after triggering a transition.
///
/// Reports *what was done*, not the resulting bus state — for the latter,
/// listen to the `VisionMixerStateChanged` WebSocket event, which is
/// broadcast immediately after the take completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct TransitionResponse {
    /// Success message
    pub message: String,
    /// The transition type requested by the client.
    pub transition_type: String,
    /// The transition type that was actually executed. Differs from
    /// `transition_type` when the engine downgraded the request — e.g.
    /// Slide/Push across heterogeneous PiP/input sources downgrades to "fade".
    pub actual_transition_type: String,
    /// Duration of the transition in milliseconds
    pub duration_ms: u64,
}

/// Request to animate a single input's position/size.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct AnimateInputRequest {
    /// Input index (0-based)
    #[cfg_attr(feature = "validation", garde(skip))]
    pub input: usize,
    /// Target X position (optional, keeps current if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub xpos: Option<i32>,
    /// Target Y position (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub ypos: Option<i32>,
    /// Target width (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub width: Option<i32>,
    /// Target height (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub height: Option<i32>,
    /// Animation duration in milliseconds
    #[serde(default = "default_transition_duration")]
    #[cfg_attr(feature = "validation", garde(range(max = 60000)))]
    pub duration_ms: u64,
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
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct UpdatePadPropertyRequest {
    /// The name of the property to update
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 255)))]
    pub property_name: String,
    /// The new value for the property
    #[cfg_attr(feature = "validation", garde(skip))]
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

/// Request to update one or more exposed properties on a block instance live.
///
/// Values are expressed in the block-level (user-facing) units defined by the block
/// — e.g. `ch1_pfl: true` (Bool), `fader_db: -3.0` (dB). The backend resolves each
/// property to its underlying GStreamer element via the block's PropertyMapping and
/// applies the declared transform (`bool_to_volume`, `db_to_linear`, …) before
/// writing.
///
/// Only properties marked `live: true` in the block definition can be patched via
/// this endpoint. Non-live properties must go through the regular flow update.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct UpdateBlockPropertiesRequest {
    /// Map of exposed property name → new value (in block-level units).
    #[cfg_attr(feature = "validation", garde(skip))]
    pub properties: HashMap<String, PropertyValue>,
    /// Optional default ramp duration in ms applied to every property in this
    /// batch (honored for volume-element writes — produces anti-click fades for
    /// bool/dB toggles that map to a `volume` property). Acts as the fallback
    /// when no per-property override is set in `ramp_ms_overrides`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(range(max = 60000)))]
    pub ramp_ms: Option<u32>,
    /// Optional per-property ramp duration overrides, keyed by exposed
    /// property name. When a property in `properties` has an entry here, that
    /// duration is used instead of the batch-level `ramp_ms`. Entries for
    /// names not present in `properties` are silently ignored. Only effective
    /// for properties whose underlying write goes through the ramp path
    /// (currently audio `volume`-element `volume` and `mute`); for other
    /// properties the override is accepted but has no effect, matching the
    /// behavior of the batch-level `ramp_ms`. Enables crossfades where
    /// individual faders ramp at different rates within a single PATCH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "validation", garde(skip))]
    pub ramp_ms_overrides: Option<HashMap<String, u32>>,
}

/// Response containing current block-level exposed property values and any rejections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct BlockPropertiesResponse {
    /// The block instance ID
    pub block_id: String,
    /// Current values (in block-level units, inverse-transformed from the live elements)
    pub properties: HashMap<String, PropertyValue>,
    /// Names of properties that could not be applied, mapped to a short reason.
    /// Empty on full success. Only populated on PATCH.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub rejected: HashMap<String, String>,
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
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Pipeline state has changed
    StateChange {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
        state: PipelineState,
    },
    /// An error occurred
    Error {
        #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
        flow_id: Option<FlowId>,
        message: String,
    },
    /// A warning message
    Warning {
        #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
        flow_id: Option<FlowId>,
        message: String,
    },
    /// Informational message
    Info { message: String },
}

/// Messages sent from client to server via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to updates for a specific flow
    Subscribe {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
    /// Unsubscribe from updates for a specific flow
    Unsubscribe {
        #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
        flow_id: FlowId,
    },
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
// SRT Stats Types
// ============================================================================

/// Direction in which an SRT element transports data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SrtRole {
    /// Outgoing SRT (`srtsink` — this element sends data).
    Sink,
    /// Incoming SRT (`srtsrc` — this element receives data).
    Source,
}

impl SrtRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SrtRole::Sink => "sink",
            SrtRole::Source => "source",
        }
    }
}

impl std::fmt::Display for SrtRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SRT connection mode (matches the `mode` enum nicks exposed by the SRT plugin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SrtMode {
    Caller,
    Listener,
    Rendezvous,
}

impl SrtMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SrtMode::Caller => "caller",
            SrtMode::Listener => "listener",
            SrtMode::Rendezvous => "rendezvous",
        }
    }

    /// Parse from the enum nick the gst-plugins-bad SRT plugin exposes.
    pub fn from_nick(nick: &str) -> Option<Self> {
        match nick {
            "caller" => Some(SrtMode::Caller),
            "listener" => Some(SrtMode::Listener),
            "rendezvous" => Some(SrtMode::Rendezvous),
            _ => None,
        }
    }
}

impl std::fmt::Display for SrtMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SRT statistics for a flow. Covers both srtsink (outputs) and srtsrc (inputs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtStats {
    /// Stats for each SRT element, keyed by element name (`block_id:srtsrc`/`block_id:srtsink`).
    pub connections: HashMap<String, SrtConnectionStats>,
}

/// Stats for a single SRT element (srtsrc or srtsink).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtConnectionStats {
    /// Element role — determines which side of `SrtCallerStats` is populated.
    pub role: SrtRole,
    /// Connection mode (`caller`/`listener`/`rendezvous`).
    pub mode: Option<SrtMode>,
    /// True when at least one caller is exchanging data.
    pub connected: bool,
    /// Per-caller stats. Always contains one entry for caller/rendezvous mode and the
    /// single peer of a caller-mode srtsrc; may contain zero or many entries for
    /// listener mode (one per connected caller).
    pub callers: Vec<SrtCallerStats>,
}

impl Default for SrtConnectionStats {
    fn default() -> Self {
        Self {
            role: SrtRole::Sink,
            mode: None,
            connected: false,
            callers: Vec::new(),
        }
    }
}

/// Stats for a single SRT caller/peer.
///
/// Fields are grouped by direction so that consumers don't have to guess which
/// counter applies to which role. A `srtsink` populates the sender-side fields
/// and leaves the receiver-side ones empty; a `srtsrc` does the opposite. Link
/// metrics (RTT, bandwidth, negotiated latency) apply to both directions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtCallerStats {
    /// Peer address (`ip:port`), when known.
    pub address: Option<String>,

    // ---- Link metrics (apply to both sender and receiver) ----
    /// Smoothed round-trip time (ms).
    pub rtt_ms: Option<f64>,
    /// Estimated link bandwidth between the two endpoints (Mbps).
    pub bandwidth_mbps: Option<f64>,
    /// Negotiated SRT latency (ms).
    pub negotiated_latency_ms: Option<u32>,

    // ---- Sender metrics (populated by srtsink) ----
    /// Total packets sent.
    pub packets_sent: Option<u64>,
    /// Packets lost on the wire and reported by NAK from the peer.
    pub packets_sent_lost: Option<u64>,
    /// Packets dropped locally before transmission (TLPKTDROP).
    pub packets_sent_dropped: Option<u64>,
    /// Packets the sender retransmitted in response to NAKs.
    pub packets_retransmitted: Option<u64>,
    /// Total bytes sent.
    pub bytes_sent: Option<u64>,
    /// Instantaneous send rate (Mbps).
    pub send_rate_mbps: Option<f64>,
    /// Sender buffer fill level (ms).
    pub snd_buf_level_ms: Option<u32>,

    // ---- Receiver metrics (populated by srtsrc) ----
    /// Total packets received.
    pub packets_received: Option<u64>,
    /// Packets the receiver detected as missing.
    pub packets_received_lost: Option<u64>,
    /// Packets skipped by the receiver due to TSBPD timeout.
    pub packets_received_dropped: Option<u64>,
    /// Packets received that were retransmissions of earlier lost packets.
    pub packets_received_retransmitted: Option<u64>,
    /// Total bytes received.
    pub bytes_received: Option<u64>,
    /// Instantaneous receive rate (Mbps).
    pub recv_rate_mbps: Option<f64>,
    /// Receiver buffer fill level (ms).
    pub recv_buf_level_ms: Option<u32>,
}

/// Response containing SRT statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtStatsResponse {
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    pub stats: SrtStats,
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
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct ParseGstLaunchRequest {
    /// The gst-launch-1.0 pipeline string to parse
    /// Example: "videotestsrc pattern=ball ! videoconvert ! autovideosink"
    #[cfg_attr(feature = "validation", garde(length(min = 1)))]
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
// System Information and Auth Response Types
// ============================================================================

/// Server system information: version, build, runtime environment, and host details.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SystemInfo {
    /// Package version from Cargo.toml
    pub version: String,
    /// Git commit hash (short)
    pub git_hash: String,
    /// Git tag (if on a tagged commit)
    pub git_tag: String,
    /// Git branch name
    pub git_branch: String,
    /// Whether the working directory had uncommitted changes
    pub git_dirty: bool,
    /// Build timestamp (ISO 8601 format)
    pub build_timestamp: String,
    /// Unique build ID (UUID) generated at compile time
    #[serde(default)]
    pub build_id: String,
    /// GStreamer runtime version
    #[serde(default)]
    pub gstreamer_version: String,
    /// Operating system name and version
    #[serde(default)]
    pub os_info: String,
    /// Whether running inside a Docker container
    #[serde(default)]
    pub in_docker: bool,
    /// When the Strom server process was started (ISO 8601 format with timezone)
    #[serde(default)]
    pub process_started_at: String,
    /// When the system was booted (ISO 8601 format with timezone)
    #[serde(default)]
    pub system_boot_time: String,
    /// Server hostname (for generating external URLs)
    #[serde(default)]
    pub hostname: String,
}

impl SystemInfo {
    /// Get a human-readable version string.
    ///
    /// Returns:
    /// - "v0.1.0" if on a tagged release
    /// - "v0.1.0-dev+abc12345" if on main/master without tag
    /// - "v0.1.0-dev+abc12345-dirty" if there are uncommitted changes
    pub fn version_string(&self) -> String {
        if !self.git_tag.is_empty() {
            self.git_tag.clone()
        } else {
            let mut version = format!("v{}-dev+{}", self.version, self.git_hash);
            if self.git_dirty {
                version.push_str("-dirty");
            }
            version
        }
    }

    /// Get a short version string for display.
    ///
    /// Returns:
    /// - "v0.1.0" if on a tagged release
    /// - "v0.1.0-dev" if not on a tag
    pub fn short_version(&self) -> String {
        if !self.git_tag.is_empty() {
            self.git_tag.clone()
        } else {
            format!("v{}-dev", self.version)
        }
    }
}

/// System clock synchronization information.
///
/// Reads the kernel's `ntp_adjtime()` state (same information used by chrony,
/// ntpd, systemd-timesyncd, etc.) and reports how the system clock is being
/// disciplined. Relevant for flows using Realtime or TAI pipeline clocks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SystemClockInfo {
    /// TAI - UTC offset in seconds (typically 37 as of 2026).
    /// Set by the clock sync daemon so that `CLOCK_TAI` is correct.
    pub tai_offset_sec: i32,
    /// High-level state from `ntp_adjtime()` return value.
    /// One of: `ok`, `ins` (leap insert pending), `del`, `oop`, `wait`, `error` (unsynced).
    pub state: String,
    /// Whether the kernel considers the clock synchronized (STA_UNSYNC not set).
    pub synchronized: bool,
    /// Whether PLL discipline is active (STA_PLL).
    pub pll_active: bool,
    /// Current time offset being applied to the clock, in nanoseconds.
    pub offset_ns: i64,
    /// Current frequency adjustment in parts-per-million.
    pub frequency_ppm: f64,
    /// Maximum error estimate, in microseconds.
    pub max_error_us: i64,
    /// Estimated error, in microseconds.
    pub est_error_us: i64,
    /// Timestamp when this info was read (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<u64>,
}

/// Authentication status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AuthStatusResponse {
    /// Whether the current session is authenticated
    pub authenticated: bool,
    /// Whether authentication is required for this server
    pub auth_required: bool,
    /// Available authentication methods (e.g., "session", "api_key")
    pub methods: Vec<String>,
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
// OSC Authentication API Types
// ============================================================================

/// Request to set the OSC Personal Access Token (PAT) at runtime.
///
/// The PAT is used to mint short-lived Service Access Tokens for OSC-hosted
/// services (e.g. a TAMS gateway). It is held in memory only — not persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetOscPatRequest {
    /// The OSC Personal Access Token.
    pub pat: String,
}

/// Status of the OSC Personal Access Token configuration. Token values are never
/// returned.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OscAuthStatusResponse {
    /// Whether the default (fallback) PAT is configured (via env var or the
    /// keyless API). Used for single-tenant deployments.
    pub configured: bool,
    /// Credential keys (flow ids) that have a per-flow PAT registered. Used to
    /// isolate OSC tenants on a shared Strom instance.
    pub keys: Vec<String>,
}

// ============================================================================
// Logging API Types
// ============================================================================

/// Response for log level queries and updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct LogLevelResponse {
    /// The currently active log filter string (e.g. "info,strom::api=debug")
    pub current: String,
    /// The default filter the server started with
    pub default: String,
}

/// Request to change the log level at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetLogLevelRequest {
    /// The new log filter string (e.g. "info,strom::api=debug")
    pub filter: String,
}

/// Response for GStreamer debug level queries and updates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GstLogLevelResponse {
    /// The currently active GST_DEBUG filter string (e.g. "*:2,webrtcbin:5")
    pub current: String,
    /// The default GST_DEBUG filter the server started with
    pub default: String,
}

/// Request to change the GStreamer debug level at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SetGstLogLevelRequest {
    /// The new GST_DEBUG filter string (e.g. "*:2,webrtcbin:5")
    pub filter: String,
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
// Dynamic Pads API Types
// ============================================================================

/// Response containing runtime dynamic pads information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DynamicPadsResponse {
    /// Map of element_id -> {pad_name -> tee_element_name}
    /// These are pads that appeared at runtime without defined links.
    pub pads: HashMap<String, HashMap<String, String>>,
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
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct RenameMediaRequest {
    /// Current path (relative to media root)
    #[cfg_attr(feature = "validation", garde(length(min = 1)))]
    pub old_path: String,
    /// New name (just the filename, not full path)
    #[cfg_attr(feature = "validation", garde(length(min = 1, max = 255)))]
    pub new_name: String,
}

/// Request to create a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct CreateDirectoryRequest {
    /// Path for new directory (relative to media root)
    #[cfg_attr(feature = "validation", garde(length(min = 1)))]
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

// ============================================================================
// Buffer Age Probe API Types
// ============================================================================

/// Request to activate a buffer age probe on a pad.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ActivateProbeRequest {
    /// Element ID to probe (standalone element or block ID)
    pub element_id: String,
    /// Measure every Nth buffer (default 1)
    #[serde(default = "default_sample_interval")]
    pub sample_interval: Option<u32>,
    /// Auto-remove after this many seconds (default 60)
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: Option<u32>,
}

fn default_sample_interval() -> Option<u32> {
    Some(1)
}

fn default_timeout_secs() -> Option<u32> {
    Some(60)
}

/// Response after activating a probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ProbeResponse {
    /// Unique probe ID
    pub probe_id: String,
}

/// Information about an active probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ProbeInfo {
    /// Unique probe ID
    pub probe_id: String,
    /// Element ID being probed
    pub element_id: String,
    /// Pad name being probed
    pub pad_name: String,
    /// Number of samples collected so far
    pub sample_count: u64,
}

/// Response listing active probes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ActiveProbesResponse {
    /// List of active probes
    pub probes: Vec<ProbeInfo>,
}

impl MediaOperationResponse {
    /// Create a success response.
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            success: true,
            message: message.into(),
        }
    }
}

// ============================================================================
// Vision Mixer API Types
// ============================================================================

/// Request to select a preview source on a vision mixer block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SelectPreviewRequest {
    /// Source to place on the PVW bus.
    pub source: crate::vision_mixer::Source,
}

/// Request to update a PiP composition (background source + zones).
///
/// A PiP is a background source plus zero or more zones. Each zone is a
/// sub-region with its own current sources (FIFO, oldest first) that
/// auto-tile inside the zone's rect. See [`crate::vision_mixer::Zone`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdatePipConfigRequest {
    /// Background input index. Omit/null for no bg.
    #[serde(default)]
    pub bg: Option<usize>,
    /// Zones in z-order (zone 0 lowest, last zone on top). Sources within a
    /// zone are also in z-order (oldest first).
    #[serde(default)]
    pub zones: Vec<crate::vision_mixer::Zone>,
    /// Per-source crop ("zoom"/"punch-in"), keyed by input index. Applies to
    /// the source wherever it renders inside this PiP (bg or zone). Missing
    /// key = no crop. Entries for sources currently *outside* the PiP are
    /// retained and re-apply when the source returns (swap-zone workflow);
    /// removing a crop is explicit — omit/delete its entry while the source
    /// is present. See [`crate::vision_mixer::SourceCrop`].
    #[serde(default)]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = std::collections::HashMap<String, crate::vision_mixer::SourceCrop>)
    )]
    pub transforms: crate::vision_mixer::PipTransforms,
}

/// Response after updating a PiP composition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct UpdatePipConfigResponse {
    pub message: String,
    pub pip_idx: usize,
    pub bg: Option<usize>,
    /// Authoritative zone state. Identical to the request except that
    /// `NormRect`s are clamped to `[0,1]`. Duplicate sources or out-of-range
    /// indices are rejected with 400 rather than silently sanitized.
    pub zones: Vec<crate::vision_mixer::Zone>,
    /// Authoritative per-source crop state. Identical to the request except
    /// that crop fractions are clamped (see `SourceCrop::clamped`).
    #[serde(default)]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = std::collections::HashMap<String, crate::vision_mixer::SourceCrop>)
    )]
    pub transforms: crate::vision_mixer::PipTransforms,
}

/// Response after selecting a preview source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SelectPreviewResponse {
    pub message: String,
    /// Current PVW input. `None` when PVW is a PiP source.
    #[serde(default)]
    pub preview_input: Option<usize>,
    /// Current PGM input. `None` when PGM is a PiP source.
    #[serde(default)]
    pub program_input: Option<usize>,
    /// PiP index currently displayed on PVW, or `None` if PVW is an input.
    #[serde(default)]
    pub preview_pip: Option<usize>,
    /// PiP index currently displayed on PGM, or `None` if PGM is an input.
    #[serde(default)]
    pub program_pip: Option<usize>,
}

/// One PiP composition's runtime state (background + zones).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PipState {
    /// Background input index, or `None` if no bg is set.
    pub bg: Option<usize>,
    /// Overlay zones (FIFO order, oldest first inside each zone).
    pub zones: Vec<crate::vision_mixer::Zone>,
    /// Per-source crop transforms, keyed by input index.
    #[serde(default)]
    #[cfg_attr(
        feature = "openapi",
        schema(value_type = std::collections::HashMap<String, crate::vision_mixer::SourceCrop>)
    )]
    pub transforms: crate::vision_mixer::PipTransforms,
}

/// Current runtime state of a vision mixer block.
///
/// This is the snapshot a client uses to reconcile on (re)connect when WS
/// events have not yet arrived. Static config (input count, labels, DSK
/// count) is *not* included — that lives on the block resource itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct VisionMixerState {
    /// Current PGM input. `None` when PGM is a PiP source.
    pub program_input: Option<usize>,
    /// Current PVW input. `None` when PVW is a PiP source.
    pub preview_input: Option<usize>,
    /// PiP index currently displayed on PGM, or `None` if PGM is an input.
    pub program_pip: Option<usize>,
    /// PiP index currently displayed on PVW, or `None` if PVW is an input.
    pub preview_pip: Option<usize>,
    /// Whether Fade to Black is currently active.
    pub ftb_active: bool,
    /// DSK on/off state, one entry per configured DSK input.
    pub dsk_enabled: Vec<bool>,
    /// Multiview overlay alpha (0.0–1.0).
    pub overlay_alpha: f64,
    /// Per-PiP runtime state (length = configured `num_pips`).
    pub pips: Vec<PipState>,
    /// Negotiated resolution per input (length = configured `num_inputs`).
    /// `None` for inputs whose caps are not negotiated yet. Inputs can have
    /// arbitrary resolutions/aspects — clients must not assume the PGM aspect
    /// (the crop editor needs the real source aspect for its window math).
    #[serde(default)]
    pub input_resolutions: Vec<Option<crate::vision_mixer::InputResolution>>,
    /// Whether the shader FX engine is built into this pipeline (GPU backend
    /// with Shader FX enabled). When `false`, effect endpoints reject and
    /// shader transitions downgrade to Fade.
    #[serde(default)]
    pub fx_available: bool,
    /// Current per-input video effects (length = configured `num_inputs`).
    /// Empty when the FX engine is unavailable.
    #[serde(default)]
    pub input_effects: Vec<crate::effects::VideoEffect>,
    /// Current master (PGM) video effect.
    #[serde(default)]
    pub master_effect: crate::effects::VideoEffect,
}

/// Request to set the multiview overlay alpha on a vision mixer block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OverlayAlphaRequest {
    /// Alpha value (0.0 = fully transparent, 1.0 = fully opaque)
    pub alpha: f64,
}

/// Response after setting the multiview overlay alpha.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct OverlayAlphaResponse {
    pub message: String,
    pub alpha: f64,
}

/// Request to toggle a DSK (Downstream Keyer) layer on a vision mixer block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DskToggleRequest {
    /// DSK layer number (1 or 2, 1-based)
    pub dsk: usize,
    /// Enable or disable the DSK layer
    pub enabled: bool,
}

/// Response after toggling a DSK layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct DskToggleResponse {
    pub message: String,
    /// DSK layer number (1-based)
    pub dsk: usize,
    pub enabled: bool,
}

/// Request to toggle Fade to Black on a vision mixer block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FadeToBlackRequest {
    /// Duration in milliseconds (0 = instant)
    pub duration_ms: u64,
}

/// Response after toggling Fade to Black.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FadeToBlackResponse {
    pub message: String,
    pub active: bool,
}

/// Response for a vision mixer multiview endpoint query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct MultiviewEndpointResponse {
    /// WHEP endpoint path (e.g. "/whep/my-endpoint"), empty if not connected.
    pub endpoint: String,
}
