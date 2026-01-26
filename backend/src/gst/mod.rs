//! GStreamer integration.

mod block_expansion;
pub mod discovery;
pub mod pipeline;
pub mod thread_priority;
pub mod thumbnail;
pub mod transitions;
pub mod video_frame;

pub use discovery::ElementDiscovery;
pub use pipeline::{PipelineError, PipelineManager};
pub use thread_priority::{setup_thread_priority_handler, ThreadPriorityState};
pub use thumbnail::{capture_frame_as_jpeg, ThumbnailConfig, ThumbnailError};
pub use transitions::{TransitionController, TransitionError, TransitionType};
pub use video_frame::{convert_frame_to_rgb, extract_rgb_image, scale_image, VideoFrameError};
