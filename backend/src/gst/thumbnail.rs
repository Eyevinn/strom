//! Thumbnail capture error types.
//!
//! The actual thumbnail capture logic has moved to `thumbnail_tap.rs`, which
//! uses GStreamer-native processing (videoconvertscale) instead of
//! CPU-based pad probes. This module retains the shared error type.

use thiserror::Error;

/// Errors that can occur during thumbnail capture.
#[derive(Debug, Error)]
pub enum ThumbnailError {
    #[error("Pad not found: {0}")]
    PadNotFound(String),

    #[error("Frame capture timed out")]
    Timeout,

    #[error("Failed to map video frame: {0}")]
    FrameMapping(String),

    #[error("JPEG encoding failed: {0}")]
    JpegEncoding(String),

    #[error("Pipeline not running")]
    PipelineNotRunning,
}
