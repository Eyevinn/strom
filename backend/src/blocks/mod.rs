//! Block management system for reusable element groupings.

pub mod builder;
pub mod builtin;
pub mod registry;
pub mod sdp;
pub mod storage;

pub use builder::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, BusMessageConnectFn,
    DynamicWebrtcbinStore, ElementSetupFn, WhepEndpointInfo, WhepStreamMode, WhipEndpointInfo,
};
pub use registry::BlockRegistry;

use gstreamer as gst;

/// Maximum bytes allowed in a video appsrc queue before dropping old buffers.
pub const APPSRC_MAX_BYTES_VIDEO: u64 = 20 * 1024 * 1024;
/// Maximum bytes allowed in an audio appsrc queue before dropping old buffers.
pub const APPSRC_MAX_BYTES_AUDIO: u64 = 2 * 1024 * 1024;
/// Maximum time buffered in an appsrc queue before dropping old buffers.
pub const APPSRC_MAX_TIME: gst::ClockTime = gst::ClockTime::from_seconds(10);
