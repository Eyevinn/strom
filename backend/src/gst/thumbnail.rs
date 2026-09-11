//! Thumbnail capture error types.
//!
//! The actual thumbnail capture logic has moved to `thumbnail_tap.rs`, which
//! uses GStreamer-native processing (videoconvertscale) instead of
//! CPU-based pad probes. This module retains the shared error type.

use thiserror::Error;

/// Errors that can occur during thumbnail capture.
#[derive(Debug, Error)]
pub enum ThumbnailError {
    #[error("Element not found: {0}")]
    ElementNotFound(String),

    #[error("Pad not found: {0}")]
    PadNotFound(String),

    #[error("Frame capture timed out")]
    Timeout,

    #[error("Failed to map video frame: {0}")]
    FrameMapping(String),

    #[error("Unsupported video format: {0}")]
    UnsupportedFormat(String),

    #[error("JPEG encoding failed: {0}")]
    JpegEncoding(String),

    #[error("Pipeline not running")]
    PipelineNotRunning,

    #[error("Channel error: {0}")]
    Channel(String),
}
