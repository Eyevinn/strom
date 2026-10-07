//! Block management system for reusable element groupings.

pub mod builder;
pub mod builtin;
pub mod registry;
pub mod sdp;
pub mod storage;
pub mod transforms;

pub use builder::{
    set_ice_transport_policy, BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder,
    BlockLiveness, BusMessageConnectFn, DynamicWebrtcbinStore, ElementSetupFn, FlowLinkTargets,
    LivenessFailure, WhepEndpointInfo, WhipEndpointInfo,
};
pub use registry::BlockRegistry;

use gstreamer as gst;

/// Maximum bytes allowed in a video appsrc queue before dropping old buffers.
pub const APPSRC_MAX_BYTES_VIDEO: u64 = 20 * 1024 * 1024;
/// Maximum bytes allowed in an audio appsrc queue before dropping old buffers.
pub const APPSRC_MAX_BYTES_AUDIO: u64 = 2 * 1024 * 1024;
/// Maximum time buffered in an appsrc queue before dropping old buffers.
pub const APPSRC_MAX_TIME: gst::ClockTime = gst::ClockTime::from_seconds(10);

/// Builder for a live appsrc that hands buffers from a pipeline of its own
/// into a flow (WHIP Input, Media Player). It queues up to `max_bytes` or
/// [`APPSRC_MAX_TIME`], whichever is reached first; the caller picks which end
/// leaks.
///
/// It reports that queue as its maximum latency. A live appsrc left at its
/// defaults reports a maximum of 0, as if it could not hold a single buffer.
/// An aggregator downstream takes the smallest maximum among its inputs, so
/// one such input caps the whole mixer at whatever queue sits after it (1 s
/// for a default queue). An input with more latency than that, such as an SRT
/// input at 2000 ms, then fails the mixer's latency configuration. The mixer
/// retries on every aggregation cycle and posts a warning each time,
/// thousands per second. The minimum stays 0, as before.
pub fn live_bridge_appsrc<'a>(max_bytes: u64) -> gstreamer_app::app_src::AppSrcBuilder<'a> {
    gstreamer_app::AppSrc::builder()
        .format(gst::Format::Time)
        .is_live(true)
        .automatic_eos(false)
        .max_bytes(max_bytes)
        .max_time(APPSRC_MAX_TIME)
        .min_latency(0)
        .max_latency(APPSRC_MAX_TIME.nseconds() as i64)
}
