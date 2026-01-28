//! Thread CPU statistics for GStreamer streaming threads.

use crate::FlowId;
use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// CPU statistics for a single GStreamer streaming thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ThreadCpuStats {
    /// Native thread ID (OS-specific)
    pub thread_id: u64,
    /// CPU usage percentage (0-100%)
    pub cpu_usage: f32,
    /// Name of the GStreamer element that owns this thread
    pub element_name: String,
    /// Flow ID this thread belongs to
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    /// Block ID if the element is inside a block
    pub block_id: Option<String>,
}

/// Aggregated thread statistics for all GStreamer streaming threads.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ThreadStats {
    /// Statistics for all active streaming threads
    pub threads: Vec<ThreadCpuStats>,
    /// Timestamp when this snapshot was taken (Unix timestamp in milliseconds)
    pub timestamp: i64,
}
