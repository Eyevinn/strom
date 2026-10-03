//! GStreamer integration.

pub(crate) mod aggregator_start;
mod block_expansion;
pub mod buffer_age_probe;
pub(crate) mod control_bindings;
pub(crate) mod crop;
pub mod discovery;
pub mod gl_input_front;
pub mod gl_link;
pub mod ice_preflight;
pub mod keyframe_request;
pub mod orphan_guard;
pub mod pipeline;
pub mod pipeline_bridge;
pub mod pipeline_monitor;
pub mod rtp_hdrext;
pub mod shaders;
pub mod stinger;
pub mod thread_priority;
pub mod thumbnail;
pub mod thumbnail_tap;
pub mod transitions;
pub(crate) mod underlay;
pub mod video_adapt;
pub mod video_frame;
pub mod video_input_bridge;
pub mod volume_ramp;
pub mod whep_probe;

pub use discovery::ElementDiscovery;
pub use pipeline::{PipelineError, PipelineManager};
pub use thread_priority::{
    setup_thread_priority_handler, SessionThreadConfig, ThreadPriorityState,
};
pub use thumbnail::ThumbnailError;
pub use thumbnail_tap::{new_tap_store, ThumbnailTap, ThumbnailTapConfig, ThumbnailTapStore};
pub use transitions::{TransitionController, TransitionError, TransitionType};
