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
/// - `Realtime`: SystemClock with GST_CLOCK_TYPE_REALTIME
/// - `Ptp`: PtpClock for IEEE 1588 PTP synchronization
/// - `Ntp`: NtpClock for NTP synchronization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum GStreamerClockType {
    /// System monotonic clock (default, recommended for most use cases)
    #[default]
    Monotonic,
    /// System realtime clock (wall clock time)
    Realtime,
    /// Precision Time Protocol clock (IEEE 1588, for synchronized multi-device scenarios)
    Ptp,
    /// Network Time Protocol clock
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
    /// Acronyms are capitalized (PTP, NTP).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Monotonic => "Monotonic",
            Self::Realtime => "Realtime",
            Self::Ptp => "PTP",
            Self::Ntp => "NTP",
        }
    }

    /// Get the human-readable description of this clock type.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Monotonic => "Stable system clock, not affected by time changes (recommended)",
            Self::Realtime => "Wall clock time, may jump if system time changes",
            Self::Ptp => "Precision Time Protocol for synchronized multi-device setups",
            Self::Ntp => "Network Time Protocol",
        }
    }

    /// Get all available clock types.
    pub fn all() -> &'static [GStreamerClockType] {
        &[Self::Monotonic, Self::Realtime, Self::Ptp, Self::Ntp]
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
    /// Clock synchronization status (updated by backend for running pipelines)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_sync_status: Option<ClockSyncStatus>,

    /// PTP clock information (updated by backend when clock_type is PTP)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_info: Option<PtpInfo>,

    /// Thread priority for GStreamer streaming threads
    /// Default is High (elevated but not realtime)
    #[serde(default)]
    pub thread_priority: ThreadPriority,

    /// Status of thread priority configuration (updated by backend when pipeline starts)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_priority_status: Option<ThreadPriorityStatus>,

    /// Whether this flow should be automatically restarted when the backend starts
    /// (set to true when starting a flow, false when manually stopping it)
    #[serde(default)]
    pub auto_restart: bool,

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
    /// Current runtime state (persisted to storage for automatic restart)
    #[serde(default)]
    pub state: Option<PipelineState>,
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
            state: Some(PipelineState::Null),
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
            state: Some(PipelineState::Null),
            properties: FlowProperties::default(),
        }
    }
}
