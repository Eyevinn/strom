//! Shared types for the Strom GStreamer flow engine.
//!
//! This crate contains domain models and API types shared between
//! the backend and frontend components.

/// Default port for the Strom backend server.
pub const DEFAULT_PORT: u16 = 8080;

pub mod api;
pub mod block;
pub mod element;
pub mod events;
pub mod flow;
pub mod network;
pub mod state;
pub mod stats;
pub mod system_monitor;
pub mod thread_stats;

// Re-export commonly used types
pub use block::{
    common_video_resolution_enum_values, parse_resolution_string, BlockDefinition, BlockInstance,
    BlockListResponse, BlockResponse, CreateBlockRequest, EnumValue, ExposedProperty, ExternalPad,
    ExternalPads, PropertyMapping, PropertyType, COMMON_VIDEO_RESOLUTIONS,
};
pub use element::{Element, ElementId, Link, MediaType, PropertyValue};
pub use events::StromEvent;
pub use flow::{Flow, FlowId, ThreadPriority, ThreadPriorityStatus};
pub use network::{
    Ipv4AddressInfo, Ipv6AddressInfo, NetworkInterfaceInfo, NetworkInterfacesResponse,
};
pub use state::PipelineState;
pub use stats::{
    BlockStats, BlockStatsResponse, FlowStats, FlowStatsResponse, RtpJitterbufferStats,
    RtpSessionStats, StatMetadata, StatValue, Statistic,
};
pub use system_monitor::{GpuStats, SystemStats};
pub use thread_stats::{ThreadCpuStats, ThreadStats};
