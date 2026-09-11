//! Statistics types for Strom blocks and pipelines.
//!
//! This module provides generic statistics types that can be used by any block
//! to expose operational metrics. The design allows for:
//! - Generic statistic types (counters, gauges, strings, etc.)
//! - Block-specific statistics (e.g., RTP jitterbuffer stats)
//! - Consistent API across different block types

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// A single statistic value with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(tag = "type", content = "value")]
pub enum StatValue {
    /// Counter - monotonically increasing value (e.g., packets received)
    Counter(u64),
    /// Gauge - value that can go up or down (e.g., buffer level)
    Gauge(i64),
    /// Float value (e.g., average jitter)
    Float(f64),
    /// Boolean flag (e.g., is_synced)
    Bool(bool),
    /// String value (e.g., current SSRC)
    String(String),
    /// Duration in nanoseconds
    DurationNs(u64),
    /// Timestamp in nanoseconds since epoch
    TimestampNs(u64),
}

impl StatValue {
    /// Format the value for display.
    pub fn format(&self) -> String {
        match self {
            StatValue::Counter(v) => format!("{}", v),
            StatValue::Gauge(v) => format!("{}", v),
            StatValue::Float(v) => format!("{:.2}", v),
            StatValue::Bool(v) => if *v { "Yes" } else { "No" }.to_string(),
            StatValue::String(v) => v.clone(),
            StatValue::DurationNs(v) => {
                if *v < 1_000 {
                    format!("{} ns", v)
                } else if *v < 1_000_000 {
                    format!("{:.2} us", *v as f64 / 1_000.0)
                } else if *v < 1_000_000_000 {
                    format!("{:.2} ms", *v as f64 / 1_000_000.0)
                } else {
                    format!("{:.2} s", *v as f64 / 1_000_000_000.0)
                }
            }
            StatValue::TimestampNs(v) => format!("{}", v),
        }
    }
}

/// Metadata about a statistic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StatMetadata {
    /// Human-readable name for display
    pub display_name: String,
    /// Description of what this statistic measures
    pub description: String,
    /// Unit of measurement (e.g., "packets", "ms", "bytes")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Category for grouping in UI (e.g., "RTP", "Buffer", "Network")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// A statistic with its current value and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Statistic {
    /// Unique identifier for this statistic within the block
    pub id: String,
    /// Current value
    pub value: StatValue,
    /// Metadata about this statistic
    pub metadata: StatMetadata,
}

/// Statistics for a single block instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct BlockStats {
    /// The block instance ID
    pub block_instance_id: String,
    /// The block definition ID (e.g., "builtin.aes67_input")
    pub block_definition_id: String,
    /// Human-readable block name
    pub block_name: String,
    /// Collection of statistics for this block
    pub stats: Vec<Statistic>,
    /// Timestamp when these stats were collected (nanoseconds since epoch)
    pub collected_at: u64,
}

/// Statistics for an entire flow (pipeline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowStats {
    /// Flow ID
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: uuid::Uuid,
    /// Flow name
    pub flow_name: String,
    /// Statistics for each block in the flow
    pub block_stats: Vec<BlockStats>,
    /// Timestamp when these stats were collected
    pub collected_at: u64,
}

/// RTP-specific statistics from jitterbuffer.
/// These are the most commonly needed stats for AES67/RTP streams.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RtpJitterbufferStats {
    /// Number of packets pushed out
    pub num_pushed: u64,
    /// Number of packets lost
    pub num_lost: u64,
    /// Number of packets that arrived late
    pub num_late: u64,
    /// Number of duplicate packets received
    pub num_duplicates: u64,
    /// Average jitter in nanoseconds
    pub avg_jitter_ns: u64,
    /// Number of retransmission requests sent
    pub rtx_count: u64,
    /// Number of successful retransmissions
    pub rtx_success_count: u64,
    /// Average retransmissions per packet
    pub rtx_per_packet: f64,
    /// Retransmission round-trip time in nanoseconds
    pub rtx_rtt_ns: u64,
    /// The buffer's configured size in milliseconds.
    ///
    /// Reported alongside the measured jitter so the two can be compared: a
    /// buffer is sized correctly when it comfortably exceeds the jitter the
    /// stream actually shows, and `num_late` stays at zero.
    pub latency_ms: u64,
}

impl RtpJitterbufferStats {
    /// Convert to generic statistics.
    pub fn to_statistics(&self) -> Vec<Statistic> {
        vec![
            Statistic {
                id: "num_pushed".to_string(),
                value: StatValue::Counter(self.num_pushed),
                metadata: StatMetadata {
                    display_name: "Packets Pushed".to_string(),
                    description: "Total packets pushed out of jitterbuffer".to_string(),
                    unit: Some("packets".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "num_lost".to_string(),
                value: StatValue::Counter(self.num_lost),
                metadata: StatMetadata {
                    display_name: "Packets Lost".to_string(),
                    description: "Total packets lost".to_string(),
                    unit: Some("packets".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "num_late".to_string(),
                value: StatValue::Counter(self.num_late),
                metadata: StatMetadata {
                    display_name: "Packets Late".to_string(),
                    description: "Packets that arrived after their playout time".to_string(),
                    unit: Some("packets".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "num_duplicates".to_string(),
                value: StatValue::Counter(self.num_duplicates),
                metadata: StatMetadata {
                    display_name: "Duplicate Packets".to_string(),
                    description: "Duplicate packets received".to_string(),
                    unit: Some("packets".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "avg_jitter_ns".to_string(),
                value: StatValue::DurationNs(self.avg_jitter_ns),
                metadata: StatMetadata {
                    display_name: "Average Jitter".to_string(),
                    description: "Average network jitter".to_string(),
                    unit: Some("ns".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "latency_ms".to_string(),
                value: StatValue::Gauge(self.latency_ms as i64),
                metadata: StatMetadata {
                    display_name: "Configured Buffer".to_string(),
                    description: "How much audio or video this jitterbuffer is configured to hold"
                        .to_string(),
                    unit: Some("ms".to_string()),
                    category: Some("RTP".to_string()),
                },
            },
            Statistic {
                id: "rtx_count".to_string(),
                value: StatValue::Counter(self.rtx_count),
                metadata: StatMetadata {
                    display_name: "Retransmission Requests".to_string(),
                    description: "Number of retransmission requests sent".to_string(),
                    unit: Some("requests".to_string()),
                    category: Some("Retransmission".to_string()),
                },
            },
            Statistic {
                id: "rtx_success_count".to_string(),
                value: StatValue::Counter(self.rtx_success_count),
                metadata: StatMetadata {
                    display_name: "Successful Retransmissions".to_string(),
                    description: "Retransmitted packets received successfully".to_string(),
                    unit: Some("packets".to_string()),
                    category: Some("Retransmission".to_string()),
                },
            },
        ]
    }
}

/// RTP session statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RtpSessionStats {
    /// Current SSRC (Synchronization Source)
    pub ssrc: Option<u32>,
    /// Payload type
    pub payload_type: Option<u8>,
    /// Clock rate in Hz
    pub clock_rate: Option<u32>,
    /// Jitterbuffer statistics
    pub jitterbuffer: RtpJitterbufferStats,
}

/// API response for block statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct BlockStatsResponse {
    /// Whether statistics are available
    pub available: bool,
    /// Statistics if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<BlockStats>,
    /// Error message if stats are not available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// API response wrapping flow statistics with availability info.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct FlowStatsAvailability {
    /// Whether the flow is running (stats only available for running flows)
    pub running: bool,
    /// Statistics if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<FlowStats>,
    /// Error message if stats are not available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
