//! System monitoring types for CPU and GPU statistics.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// System monitoring statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SystemStats {
    /// CPU usage percentage (0-100)
    pub cpu_usage: f32,
    /// Total system memory in bytes
    pub total_memory: u64,
    /// Used system memory in bytes
    pub used_memory: u64,
    /// GPU statistics (if available)
    pub gpu_stats: Vec<GpuStats>,
    /// Timestamp of the measurement
    pub timestamp: i64,
}

/// GPU statistics for a single GPU device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GpuStats {
    /// GPU device index
    pub index: u32,
    /// GPU device name
    pub name: String,
    /// GPU utilization percentage (0-100)
    pub utilization: f32,
    /// Memory utilization percentage (0-100)
    pub memory_utilization: f32,
    /// Total memory in bytes
    pub total_memory: u64,
    /// Used memory in bytes
    pub used_memory: u64,
    /// Temperature in Celsius (if available)
    pub temperature: Option<f32>,
    /// Power usage in watts (if available)
    pub power_usage: Option<f32>,
}
