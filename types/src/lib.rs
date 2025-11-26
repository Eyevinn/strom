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
pub mod state;
pub mod stats;

// Re-export commonly used types
pub use block::{
    BlockDefinition, BlockInstance, BlockListResponse, BlockResponse, CreateBlockRequest,
    ExposedProperty, ExternalPad, ExternalPads, PropertyMapping, PropertyType,
};
pub use element::{Element, ElementId, Link, MediaType, PropertyValue};
pub use events::StromEvent;
pub use flow::{Flow, FlowId, ThreadPriority, ThreadPriorityStatus};
pub use state::PipelineState;
pub use stats::{
    BlockStats, BlockStatsResponse, FlowStats, FlowStatsResponse, RtpJitterbufferStats,
    RtpSessionStats, StatMetadata, StatValue, Statistic,
};
