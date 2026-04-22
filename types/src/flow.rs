//! Flow (pipeline) definitions.

use crate::block::BlockInstance;
use crate::element::{Element, Link};
use crate::state::PipelineState;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Unique identifier for a flow.
pub type FlowId = Uuid;

/// CPU affinity strategy for GStreamer streaming threads.
///
/// Controls whether pipeline threads are pinned to a single CPU core
/// for better cache locality and reduced context switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CpuAffinity {
    /// No pinning, OS scheduler decides thread placement
    #[default]
    Off,
    /// Pin all pipeline threads to a single core for maximum cache locality.
    /// Core is assigned by the AffinityManager using least-loaded strategy.
    SingleCore,
}

impl<'de> Deserialize<'de> for CpuAffinity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "off" => Ok(CpuAffinity::Off),
            "single_core" => Ok(CpuAffinity::SingleCore),
            _ => Ok(CpuAffinity::default()),
        }
    }
}

/// Thread priority level for GStreamer streaming threads.
///
/// Controls the scheduling priority of GStreamer's internal streaming threads.
/// Higher priorities help ensure smooth media processing under system load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ThreadPriority {
    /// Normal priority (default OS scheduling, no elevation)
    Normal,
    /// Elevated priority (nice -10 equivalent, good for most use cases)
    #[default]
    High,
    /// Real-time priority (SCHED_FIFO, requires privileges)
    /// Warning: May require root or CAP_SYS_NICE capability
    Realtime,
}

impl ThreadPriority {
    /// Get the human-readable description of this priority level.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Normal => "Normal - default OS scheduling",
            Self::High => "High - elevated priority (recommended)",
            Self::Realtime => "Realtime - SCHED_FIFO (requires privileges)",
        }
    }

    /// Get all available priority levels.
    pub fn all() -> &'static [ThreadPriority] {
        &[Self::Normal, Self::High, Self::Realtime]
    }
}

/// Status of thread priority configuration for a running pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ThreadPriorityStatus {
    /// The requested thread priority level
    pub requested: ThreadPriority,
    /// Whether the requested priority was successfully applied
    pub achieved: bool,
    /// Error message if priority could not be set (empty if achieved)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Number of threads that had priority set
    pub threads_configured: u32,
}

/// GStreamer clock type selection.
///
/// Maps to GStreamer's clock implementations:
/// - `Monotonic`: SystemClock with GST_CLOCK_TYPE_MONOTONIC (default)
/// - `Realtime`: SystemClock with GST_CLOCK_TYPE_REALTIME (UTC wall clock)
/// - `Tai`: SystemClock with GST_CLOCK_TYPE_TAI (linear atomic time, no leap seconds)
/// - `Ptp`: PtpClock for IEEE 1588 PTP synchronization
/// - `Ntp`: NtpClock for NTP synchronization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum GStreamerClockType {
    /// System monotonic clock (default, recommended for most use cases)
    #[default]
    Monotonic,
    /// System realtime clock (UTC wall clock time, subject to leap seconds)
    Realtime,
    /// System TAI clock (atomic time, linear — no leap seconds). Requires
    /// the OS clock sync daemon to have set the kernel TAI-UTC offset.
    Tai,
    /// Precision Time Protocol clock (IEEE 1588, for synchronized multi-device scenarios)
    Ptp,
    /// Network Time Protocol clock (GStreamer polls an NTP server directly)
    Ntp,
}

/// Clock synchronization status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ClockSyncStatus {
    /// Clock is synchronized
    Synced,
    /// Clock is not synchronized
    NotSynced,
    /// Synchronization status unknown or not applicable
    Unknown,
}

/// PTP clock information (IEEE 1588).
///
/// Contains detailed information about the PTP clock state including
/// grandmaster and master clock identities, and synchronization statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PtpInfo {
    /// PTP domain currently in use by the running pipeline (0-255)
    pub domain: u8,
    /// Whether the clock is synchronized with a PTP master
    pub synced: bool,
    /// Grandmaster clock ID (EUI-64 format as hex string, e.g., "00:11:22:FF:FE:33:44:55")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grandmaster_clock_id: Option<String>,
    /// Master clock ID (EUI-64 format as hex string)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_clock_id: Option<String>,
    /// True if configured domain differs from running domain (restart needed)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart_needed: bool,
    /// PTP synchronization statistics (updated periodically)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<PtpStats>,
}

/// NTP clock information (GStreamer GstNtpClock / NetClientClock).
///
/// Populated at read-time from the running NtpClock instance. Values are snapshots
/// from the clock's internal state at the moment the flow was queried.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NtpInfo {
    /// Server hostname/IP the NtpClock is polling
    pub server: String,
    /// Server port
    pub port: u16,
    /// Whether the clock has collected enough observations to be considered stable
    pub synced: bool,
    /// Minimum update interval between polls, in nanoseconds (from clock property)
    pub minimum_update_interval_ns: u64,
    /// Maximum RTT for accepted samples, in nanoseconds (from clock property)
    pub round_trip_limit_ns: u64,
    /// Current calibration rate (external clock speed / local speed).
    /// 1.0 = same speed, <1.0 = server slower than local, >1.0 = server faster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<f64>,
    /// Current offset between external (server) and internal (local) clocks at
    /// the calibration reference point, in nanoseconds. Positive = server ahead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ns: Option<i64>,
    /// Timestamp of last read (Unix seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<u64>,
}

/// PTP clock synchronization statistics.
///
/// Contains measurements from PTP clock synchronization including
/// path delay, clock offset, and estimation quality.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PtpStats {
    /// Mean path delay to master clock in nanoseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_path_delay_ns: Option<u64>,
    /// Clock offset/discontinuity in nanoseconds (positive = local clock ahead)
    /// This is the correction being applied to keep clocks synchronized
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_offset_ns: Option<i64>,
    /// R-squared value of clock estimation regression (0.0-1.0, higher is better)
    /// Values close to 1.0 indicate stable, accurate synchronization
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r_squared: Option<f64>,
    /// Clock rate ratio (local clock speed relative to PTP master)
    /// 1.0 means clocks run at same speed, <1.0 means local is slower
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_rate: Option<f64>,
    /// Timestamp of last statistics update (Unix timestamp in seconds)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update: Option<u64>,
}

impl PtpInfo {
    /// Format a u64 clock ID as EUI-64 hex string (XX:XX:XX:FF:FE:XX:XX:XX format)
    pub fn format_clock_id(id: u64) -> String {
        let bytes = id.to_be_bytes();
        format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        )
    }
}

impl GStreamerClockType {
    /// Get the human-readable label for this clock type (for UI dropdowns).
    /// Acronyms are capitalized (PTP, NTP, TAI).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Monotonic => "Monotonic",
            Self::Realtime => "Realtime (UTC)",
            Self::Tai => "TAI",
            Self::Ptp => "PTP",
            Self::Ntp => "NTP",
        }
    }

    /// Get the human-readable description of this clock type.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Monotonic => "Stable system clock, not affected by time changes (recommended)",
            Self::Realtime => "System UTC wall clock. Jumps on leap seconds. Accuracy depends on however the OS clock is synchronized.",
            Self::Tai => "System TAI wall clock (linear, no leap seconds). Accuracy depends on however the OS clock is synchronized. Recommended for broadcast timing.",
            Self::Ptp => "Precision Time Protocol for synchronized multi-device setups",
            Self::Ntp => "Polls a remote NTP server directly (independent of the system clock)",
        }
    }

    /// Whether this clock type delivers a wall-clock pipeline-clock value that
    /// can be meaningfully used with direct media timing (`base_time=0`).
    ///
    /// - `Ptp`, `Tai`, `Realtime`, `Ntp`: yes — they expose wall-clock time.
    /// - `Monotonic`: no — the clock value is seconds-since-boot with no
    ///   external meaning, so wall-clock PTS is useless.
    pub fn supports_wall_clock_pts(&self) -> bool {
        matches!(self, Self::Ptp | Self::Tai | Self::Realtime | Self::Ntp)
    }

    /// Get all available clock types.
    pub fn all() -> &'static [GStreamerClockType] {
        &[
            Self::Monotonic,
            Self::Realtime,
            Self::Tai,
            Self::Ptp,
            Self::Ntp,
        ]
    }
}

/// Lowercase wire name used by the MCP tool arguments and similar string APIs.
/// `serde` handles these via `#[serde(rename_all = "snake_case")]` implicitly
/// on the enum, but MCP consumers receive raw strings.
impl std::str::FromStr for GStreamerClockType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "monotonic" => Ok(Self::Monotonic),
            "realtime" => Ok(Self::Realtime),
            "tai" => Ok(Self::Tai),
            "ptp" => Ok(Self::Ptp),
            "ntp" => Ok(Self::Ntp),
            _ => Err(format!("Invalid clock_type: {}", s)),
        }
    }
}

/// Flow configuration properties.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowProperties {
    /// Human-readable description (multiline text)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// GStreamer clock type to use for this flow
    #[serde(default)]
    pub clock_type: GStreamerClockType,
    /// PTP domain (0-255, only used when clock_type is PTP)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_domain: Option<u8>,
    /// NTP server address (hostname or IP, only used when clock_type is NTP)
    /// If not set but clock_type is NTP, will signal as "ntp=/traceable/"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_server: Option<String>,
    /// NTP server port (only used when clock_type is NTP). Defaults to 123.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_port: Option<u16>,
    /// Force direct media timing (`base_time=0`, `start_time=NONE`) so buffer
    /// PTS corresponds to absolute pipeline-clock time.
    ///
    /// - `Some(true)`: explicitly on.
    /// - `Some(false)`: explicitly off.
    /// - `None` (default): follow the clock-type default — `true` for `Ptp`
    ///   (AES67 contract, RFC 7273 `mediaclk:direct=0`), `false` otherwise.
    ///
    /// Use this for narrow pipelines where every element cooperates with
    /// wall-clock timing (PTP+AES67, or TAI disciplined by `ptp4l`/`phc2sys`
    /// feeding AES67 with TAI timestamps). Avoid for general pipelines
    /// (MPEG-TS demuxers, RTP jitter buffers, WHEP session sinks) that assume
    /// `running_time` starts near 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_media_timing: Option<bool>,
    /// Clock synchronization status (updated by backend for running pipelines)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_sync_status: Option<ClockSyncStatus>,

    /// PTP clock information (updated by backend when clock_type is PTP)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_info: Option<PtpInfo>,

    /// NTP clock information (updated by backend when clock_type is NTP)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_info: Option<NtpInfo>,

    /// Thread priority for GStreamer streaming threads
    /// Default is High (elevated but not realtime)
    #[serde(default)]
    pub thread_priority: ThreadPriority,

    /// CPU affinity for GStreamer streaming threads
    /// SingleCore pins all threads to one core for better cache locality
    #[serde(default)]
    pub cpu_affinity: CpuAffinity,

    /// Status of thread priority configuration (updated by backend when pipeline starts)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_priority_status: Option<ThreadPriorityStatus>,

    /// Whether this flow should be automatically restarted when the backend starts
    /// (set to true when starting a flow, false when manually stopping it)
    #[serde(default)]
    pub auto_restart: bool,

    /// When true, the flow is not persisted to storage. Useful for temporary or
    /// API-created flows that should not survive a server restart.
    #[serde(default)]
    pub ephemeral: bool,

    /// Timestamp when the flow was started (entered Playing state)
    /// ISO 8601 format with timezone (e.g., "2024-01-15T14:30:00+01:00")
    /// None if the flow has never been started or is currently stopped
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    /// Timestamp when the flow was last modified (any change to flow config)
    /// ISO 8601 format with timezone
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,

    /// Timestamp when the flow was created
    /// ISO 8601 format with timezone
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

impl FlowProperties {
    /// Resolve the effective direct media timing setting for this flow.
    ///
    /// Uses `direct_media_timing` when explicitly set; otherwise falls back to
    /// the clock-type default (`Ptp` → `true`, others → `false`).
    pub fn effective_direct_media_timing(&self) -> bool {
        self.direct_media_timing
            .unwrap_or(matches!(self.clock_type, GStreamerClockType::Ptp))
    }
}

/// A complete GStreamer pipeline definition.
///
/// A flow represents a named, configured GStreamer pipeline that can be
/// started, stopped, and persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Flow {
    /// Unique identifier for this flow
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub id: FlowId,
    /// Human-readable name
    pub name: String,
    /// Elements in this flow
    #[serde(default)]
    pub elements: Vec<Element>,
    /// Block instances in this flow
    #[serde(default)]
    pub blocks: Vec<BlockInstance>,
    /// Links between element pads and/or block external pads
    #[serde(default)]
    pub links: Vec<Link>,
    /// Whether the pipeline is actively running (data flowing).
    ///
    /// This is the field most callers should use. It is `true` when the
    /// GStreamer pipeline is in `Playing` *or* in `Paused` due to an
    /// async element that has not yet completed its transition.
    #[serde(default)]
    pub running: bool,
    /// Raw GStreamer pipeline state, exposed for diagnostics.
    ///
    /// Prefer [`running`](Self::running) for logic and display.
    #[serde(default)]
    pub gst_state: Option<PipelineState>,
    /// Flow configuration properties
    #[serde(default)]
    pub properties: FlowProperties,
}

impl Flow {
    /// Create a new empty flow with a generated ID.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            elements: Vec::new(),
            blocks: Vec::new(),
            links: Vec::new(),
            running: false,
            gst_state: Some(PipelineState::Null),
            properties: FlowProperties::default(),
        }
    }

    /// Create a new flow with a specific ID (useful for loading from storage).
    pub fn with_id(id: FlowId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            elements: Vec::new(),
            blocks: Vec::new(),
            links: Vec::new(),
            running: false,
            gst_state: Some(PipelineState::Null),
            properties: FlowProperties::default(),
        }
    }

    /// Set the GStreamer state and update `running` to match.
    pub fn set_gst_state(&mut self, state: Option<PipelineState>) {
        self.running = state.is_some_and(|s| s.is_active());
        self.gst_state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_wall_clock_pts_matches_clock_types() {
        assert!(GStreamerClockType::Ptp.supports_wall_clock_pts());
        assert!(GStreamerClockType::Tai.supports_wall_clock_pts());
        assert!(GStreamerClockType::Realtime.supports_wall_clock_pts());
        assert!(GStreamerClockType::Ntp.supports_wall_clock_pts());
        assert!(!GStreamerClockType::Monotonic.supports_wall_clock_pts());
    }

    #[test]
    fn effective_direct_media_timing_defaults_to_ptp_only() {
        let ptp = FlowProperties {
            clock_type: GStreamerClockType::Ptp,
            ..Default::default()
        };
        assert!(ptp.effective_direct_media_timing());

        for ct in [
            GStreamerClockType::Monotonic,
            GStreamerClockType::Realtime,
            GStreamerClockType::Tai,
            GStreamerClockType::Ntp,
        ] {
            let props = FlowProperties {
                clock_type: ct,
                ..Default::default()
            };
            assert!(
                !props.effective_direct_media_timing(),
                "{:?} should default to false",
                ct
            );
        }
    }

    #[test]
    fn from_str_accepts_all_variants() {
        use std::str::FromStr;
        // MCP wire form matches serde's snake_case rename on this enum.
        // Keep this table in sync with GStreamerClockType::all().
        let cases: &[(&str, GStreamerClockType)] = &[
            ("monotonic", GStreamerClockType::Monotonic),
            ("realtime", GStreamerClockType::Realtime),
            ("tai", GStreamerClockType::Tai),
            ("ptp", GStreamerClockType::Ptp),
            ("ntp", GStreamerClockType::Ntp),
        ];
        for (wire, expected) in cases {
            assert_eq!(GStreamerClockType::from_str(wire).unwrap(), *expected);
        }
        // Every variant in ::all() must appear in the table above, otherwise
        // a new variant lacks a FromStr mapping.
        assert_eq!(cases.len(), GStreamerClockType::all().len());

        assert!(GStreamerClockType::from_str("bogus").is_err());
        assert!(GStreamerClockType::from_str("PTP").is_err());
    }

    #[test]
    fn effective_direct_media_timing_respects_explicit_override() {
        let ptp_off = FlowProperties {
            clock_type: GStreamerClockType::Ptp,
            direct_media_timing: Some(false),
            ..Default::default()
        };
        assert!(!ptp_off.effective_direct_media_timing());

        let tai_on = FlowProperties {
            clock_type: GStreamerClockType::Tai,
            direct_media_timing: Some(true),
            ..Default::default()
        };
        assert!(tai_on.effective_direct_media_timing());
    }
}
