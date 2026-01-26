//! Thumbnail capture for compositor inputs.
//!
//! Captures single frames from GStreamer elements using pad probes,
//! scales them, and encodes as JPEG. This module provides poll-based
//! (on-demand) thumbnail capture, suitable for compositor input previews.
//!
//! For continuous streaming thumbnails, see the `builtin.thumbnail` block
//! which uses appsink and WebSocket events.

use crate::gst::video_frame::{self, VideoFrameError};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use image::RgbImage;
use std::io::Cursor;
use std::sync::mpsc;
use std::time::Duration;
use thiserror::Error;

/// Default thumbnail dimensions.
pub const DEFAULT_THUMBNAIL_WIDTH: u32 = 320;
pub const DEFAULT_THUMBNAIL_HEIGHT: u32 = 180;

/// Default JPEG quality (0-100).
pub const DEFAULT_JPEG_QUALITY: u8 = 75;

/// Timeout for frame capture.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

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

impl From<VideoFrameError> for ThumbnailError {
    fn from(err: VideoFrameError) -> Self {
        match err {
            VideoFrameError::FrameMapping(msg) => ThumbnailError::FrameMapping(msg),
            VideoFrameError::UnsupportedFormat(msg) => ThumbnailError::UnsupportedFormat(msg),
        }
    }
}

/// Configuration for thumbnail capture.
#[derive(Debug, Clone)]
pub struct ThumbnailConfig {
    /// Target width in pixels.
    pub width: u32,
    /// Target height in pixels.
    pub height: u32,
    /// JPEG quality (0-100).
    pub quality: u8,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            width: DEFAULT_THUMBNAIL_WIDTH,
            height: DEFAULT_THUMBNAIL_HEIGHT,
            quality: DEFAULT_JPEG_QUALITY,
        }
    }
}

/// Capture a single frame from an element's src pad and encode as JPEG.
///
/// This uses a blocking pad probe to capture the next buffer that passes through,
/// converts it to RGB, scales it, and encodes it as JPEG.
///
/// # Arguments
/// * `pipeline` - The GStreamer pipeline containing the element
/// * `element_name` - Name of the element to capture from
/// * `pad_name` - Name of the pad to probe (typically "src")
/// * `config` - Thumbnail configuration (size, quality)
///
/// # Returns
/// JPEG-encoded image bytes on success
pub fn capture_frame_as_jpeg(
    pipeline: &gst::Pipeline,
    element_name: &str,
    pad_name: &str,
    config: &ThumbnailConfig,
) -> Result<Vec<u8>, ThumbnailError> {
    tracing::debug!(
        "Capturing thumbnail from element={} pad={}",
        element_name,
        pad_name
    );

    // Check pipeline state
    let (_, state, _) = pipeline.state(gst::ClockTime::from_mseconds(100));
    if state != gst::State::Playing && state != gst::State::Paused {
        tracing::debug!("Pipeline not running, state={:?}", state);
        return Err(ThumbnailError::PipelineNotRunning);
    }

    // Find the element
    let element = pipeline.by_name(element_name).ok_or_else(|| {
        tracing::debug!("Element not found: {}", element_name);
        ThumbnailError::ElementNotFound(element_name.to_string())
    })?;

    // Get the pad
    let pad = element.static_pad(pad_name).ok_or_else(|| {
        tracing::debug!("Pad not found: {}", pad_name);
        ThumbnailError::PadNotFound(pad_name.to_string())
    })?;

    // Get caps from the pad to determine video format
    let caps = pad.current_caps().ok_or_else(|| {
        tracing::debug!("No caps on pad {}", pad_name);
        ThumbnailError::FrameMapping("No caps on pad".to_string())
    })?;

    let video_info = gst_video::VideoInfo::from_caps(&caps).map_err(|e| {
        tracing::debug!("Failed to parse video info from caps: {}", e);
        ThumbnailError::FrameMapping(format!("Invalid video caps: {}", e))
    })?;

    tracing::debug!(
        "Video info: {}x{} format={:?}",
        video_info.width(),
        video_info.height(),
        video_info.format()
    );

    // Channel to receive the captured frame
    let (tx, rx) = mpsc::channel::<Result<RgbImage, ThumbnailError>>();

    // Add a blocking pad probe to capture a single buffer
    let probe_id = pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let result = match &info.data {
            Some(gst::PadProbeData::Buffer(buffer)) => {
                video_frame::extract_rgb_image(buffer, &video_info).map_err(ThumbnailError::from)
            }
            _ => Err(ThumbnailError::FrameMapping(
                "No buffer in probe data".to_string(),
            )),
        };
        let _ = tx.send(result);
        gst::PadProbeReturn::Remove // Remove probe after first buffer
    });

    // Handle probe_id being None (shouldn't happen but be safe)
    let probe_id =
        probe_id.ok_or_else(|| ThumbnailError::Channel("Failed to add pad probe".to_string()))?;

    // Wait for frame with timeout
    let rgb_image = match rx.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Remove the probe if we timeout
            pad.remove_probe(probe_id);
            tracing::debug!("Thumbnail capture timed out");
            return Err(ThumbnailError::Timeout);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(ThumbnailError::Channel("Channel disconnected".to_string()));
        }
    };

    tracing::debug!(
        "Captured frame {}x{}, scaling to {}x{}",
        rgb_image.width(),
        rgb_image.height(),
        config.width,
        config.height
    );

    // Scale the image
    let scaled = video_frame::scale_image(&rgb_image, config.width, config.height);

    // Encode as JPEG
    encode_jpeg(&scaled, config.quality)
}

/// Encode an RGB image as JPEG.
fn encode_jpeg(img: &RgbImage, quality: u8) -> Result<Vec<u8>, ThumbnailError> {
    let mut buffer = Cursor::new(Vec::new());

    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, quality);
    encoder
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| ThumbnailError::JpegEncoding(e.to_string()))?;

    let jpeg_bytes = buffer.into_inner();

    // Debug: check JPEG header (should start with FF D8 FF)
    if jpeg_bytes.len() >= 3 {
        tracing::debug!(
            "JPEG header bytes: {:02X} {:02X} {:02X} (expected FF D8 FF)",
            jpeg_bytes[0],
            jpeg_bytes[1],
            jpeg_bytes[2]
        );
    }

    Ok(jpeg_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thumbnail_config_default() {
        let config = ThumbnailConfig::default();
        assert_eq!(config.width, 320);
        assert_eq!(config.height, 180);
        assert_eq!(config.quality, 75);
    }
}
