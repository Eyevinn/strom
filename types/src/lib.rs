//! Shared types for the Strom GStreamer flow engine.
//!
//! This crate contains domain models and API types shared between
//! the backend and frontend components.

/// Default port for the Strom backend server.
pub const DEFAULT_PORT: u16 = 8080;

/// Buffer age threshold for latency warnings (milliseconds).
/// Used by automatic buffer age probes and UI color indicators.
pub const BUFFER_AGE_WARNING_THRESHOLD_MS: u64 = 500;

pub mod api;
pub mod auth;
pub mod block;
pub mod discovery;
pub mod element;
pub mod events;
pub mod flow;
pub mod mediaplayer;
pub mod mixer;
pub mod network;
pub mod state;
pub mod stats;
pub mod system_monitor;
pub mod thread_stats;
pub mod vision_mixer;
pub mod whep;
pub mod whip;

// Re-export commonly used types
pub use block::{
    common_video_resolution_enum_values, parse_resolution_string, BlockDefinition, BlockInstance,
    BlockListResponse, BlockResponse, CreateBlockRequest, EnumValue, ExposedProperty, ExternalPad,
    ExternalPads, PropertyMapping, PropertyType, COMMON_VIDEO_RESOLUTIONS,
    DEFAULT_AES67_INPUT_BUFFER_DURATION_MS, DEFAULT_EFP_BUCKET_TIMEOUT, DEFAULT_EFP_HOL_TIMEOUT,
    DEFAULT_EFP_MTU, DEFAULT_OPUS_BITRATE, DEFAULT_OPUS_COMPLEXITY, DEFAULT_SRT_INPUT_URI,
    DEFAULT_SRT_LATENCY_MS, DEFAULT_SRT_OUTPUT_URI,
};
pub use element::{Element, ElementId, Link, MediaType, PropertyValue};
pub use events::StromEvent;
pub use flow::{CpuAffinity, Flow, FlowId, ThreadPriority, ThreadPriorityStatus};
pub use network::{
    Ipv4AddressInfo, Ipv6AddressInfo, NetworkInterfaceInfo, NetworkInterfacesResponse,
};
pub use state::PipelineState;
pub use stats::{
    BlockStats, BlockStatsResponse, FlowStats, FlowStatsAvailability, RtpJitterbufferStats,
    RtpSessionStats, StatMetadata, StatValue, Statistic,
};
pub use system_monitor::{GlRendererInfo, GpuStats, SystemStats};
pub use thread_stats::{ThreadCpuStats, ThreadStats};
