//! Video frame conversion utilities.
//!
//! Provides functions to convert GStreamer video frames between various
//! pixel formats (YUV, RGB, etc.) and to/from the `image` crate types.
//! These utilities are shared between thumbnail capture, video preview blocks,
//! and other video processing code.

use gstreamer as gst;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use image::{Rgb, RgbImage};
use thiserror::Error;

/// Errors that can occur during video frame conversion.
#[derive(Debug, Error)]
pub enum VideoFrameError {
    #[error("Failed to map video frame: {0}")]
    FrameMapping(String),

    #[error("Unsupported video format: {0}")]
    UnsupportedFormat(String),
}

/// Convert a GStreamer video frame to an RGB image.
///
/// Supports common video formats:
/// - RGB, BGR
/// - RGBA, RGBX, BGRA, BGRX
/// - I420, YV12 (YUV 4:2:0 planar)
/// - NV12 (Y + interleaved UV)
/// - YUYV, UYVY (packed YUV 4:2:2)
pub fn convert_frame_to_rgb(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
) -> Result<RgbImage, VideoFrameError> {
    let width = frame.width();
    let height = frame.height();
    let format = frame.format();

    match format {
        gst_video::VideoFormat::Rgb => convert_rgb(frame, width, height),
        gst_video::VideoFormat::Rgba | gst_video::VideoFormat::Rgbx => {
            convert_rgba(frame, width, height)
        }
        gst_video::VideoFormat::Bgra | gst_video::VideoFormat::Bgrx => {
            convert_bgra(frame, width, height)
        }
        gst_video::VideoFormat::Bgr => convert_bgr(frame, width, height),
        gst_video::VideoFormat::I420 | gst_video::VideoFormat::Yv12 => convert_yuv420_to_rgb(frame),
        gst_video::VideoFormat::Nv12 => convert_nv12_to_rgb(frame),
        gst_video::VideoFormat::Uyvy | gst_video::VideoFormat::Yuy2 => {
            convert_yuy2_to_rgb(frame, format)
        }
        _ => Err(VideoFrameError::UnsupportedFormat(format!("{:?}", format))),
    }
}

/// Extract an RGB image from a GStreamer buffer using pre-determined video info.
pub fn extract_rgb_image(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
) -> Result<RgbImage, VideoFrameError> {
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, info)
        .map_err(|_| VideoFrameError::FrameMapping("Failed to map video frame".to_string()))?;

    convert_frame_to_rgb(&frame)
}

// ============================================================================
// RGB/RGBA conversion helpers
// ============================================================================

/// Convert RGB format (direct copy).
fn convert_rgb(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    width: u32,
    height: u32,
) -> Result<RgbImage, VideoFrameError> {
    let data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    let mut img = RgbImage::new(width, height);
    for y in 0..height {
        let row_start = y as usize * stride;
        for x in 0..width {
            let offset = row_start + (x as usize * 3);
            if offset + 2 < data.len() {
                img.put_pixel(
                    x,
                    y,
                    Rgb([data[offset], data[offset + 1], data[offset + 2]]),
                );
            }
        }
    }
    Ok(img)
}

/// Convert RGBA/RGBx format (drop alpha).
fn convert_rgba(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    width: u32,
    height: u32,
) -> Result<RgbImage, VideoFrameError> {
    let data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    let mut img = RgbImage::new(width, height);
    for y in 0..height {
        let row_start = y as usize * stride;
        for x in 0..width {
            let offset = row_start + (x as usize * 4);
            if offset + 2 < data.len() {
                img.put_pixel(
                    x,
                    y,
                    Rgb([data[offset], data[offset + 1], data[offset + 2]]),
                );
            }
        }
    }
    Ok(img)
}

/// Convert BGRA/BGRx format (swap channels, drop alpha).
fn convert_bgra(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    width: u32,
    height: u32,
) -> Result<RgbImage, VideoFrameError> {
    let data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    let mut img = RgbImage::new(width, height);
    for y in 0..height {
        let row_start = y as usize * stride;
        for x in 0..width {
            let offset = row_start + (x as usize * 4);
            if offset + 2 < data.len() {
                img.put_pixel(
                    x,
                    y,
                    Rgb([data[offset + 2], data[offset + 1], data[offset]]),
                );
            }
        }
    }
    Ok(img)
}

/// Convert BGR format (swap channels).
fn convert_bgr(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    width: u32,
    height: u32,
) -> Result<RgbImage, VideoFrameError> {
    let data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    let mut img = RgbImage::new(width, height);
    for y in 0..height {
        let row_start = y as usize * stride;
        for x in 0..width {
            let offset = row_start + (x as usize * 3);
            if offset + 2 < data.len() {
                img.put_pixel(
                    x,
                    y,
                    Rgb([data[offset + 2], data[offset + 1], data[offset]]),
                );
            }
        }
    }
    Ok(img)
}

// ============================================================================
// YUV conversion helpers
// ============================================================================

/// Convert YUV to RGB using BT.601 coefficients.
///
/// This is the standard conversion for SD video content.
/// For HD content (BT.709) the coefficients would be slightly different.
#[inline]
pub fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let r = (y + ((359 * v) >> 8)).clamp(0, 255) as u8;
    let g = (y - ((88 * u + 183 * v) >> 8)).clamp(0, 255) as u8;
    let b = (y + ((454 * u) >> 8)).clamp(0, 255) as u8;
    (r, g, b)
}

/// Convert I420/YV12 to RGB.
fn convert_yuv420_to_rgb(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
) -> Result<RgbImage, VideoFrameError> {
    let width = frame.width();
    let height = frame.height();
    let format = frame.format();

    let y_data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get Y plane: {}", e)))?;
    let y_stride = frame.plane_stride()[0] as usize;

    // I420: U then V, YV12: V then U
    let (u_plane, v_plane): (usize, usize) = if format == gst_video::VideoFormat::I420 {
        (1, 2)
    } else {
        (2, 1)
    };

    let u_data = frame
        .plane_data(u_plane as u32)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get U plane: {}", e)))?;
    let u_stride = frame.plane_stride()[u_plane] as usize;

    let v_data = frame
        .plane_data(v_plane as u32)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get V plane: {}", e)))?;
    let v_stride = frame.plane_stride()[v_plane] as usize;

    let mut img = RgbImage::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let y_val = y_data[y as usize * y_stride + x as usize] as i32;
            let u_val = u_data[(y as usize / 2) * u_stride + (x as usize / 2)] as i32 - 128;
            let v_val = v_data[(y as usize / 2) * v_stride + (x as usize / 2)] as i32 - 128;

            let (r, g, b) = yuv_to_rgb(y_val, u_val, v_val);
            img.put_pixel(x, y, Rgb([r, g, b]));
        }
    }

    Ok(img)
}

/// Convert NV12 to RGB.
fn convert_nv12_to_rgb(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
) -> Result<RgbImage, VideoFrameError> {
    let width = frame.width();
    let height = frame.height();

    let y_data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get Y plane: {}", e)))?;
    let y_stride = frame.plane_stride()[0] as usize;

    let uv_data = frame
        .plane_data(1)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get UV plane: {}", e)))?;
    let uv_stride = frame.plane_stride()[1] as usize;

    let mut img = RgbImage::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let y_val = y_data[y as usize * y_stride + x as usize] as i32;
            let uv_offset = (y as usize / 2) * uv_stride + (x as usize / 2) * 2;
            let u_val = uv_data[uv_offset] as i32 - 128;
            let v_val = uv_data[uv_offset + 1] as i32 - 128;

            let (r, g, b) = yuv_to_rgb(y_val, u_val, v_val);
            img.put_pixel(x, y, Rgb([r, g, b]));
        }
    }

    Ok(img)
}

/// Convert YUYV/UYVY to RGB.
fn convert_yuy2_to_rgb(
    frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
    format: gst_video::VideoFormat,
) -> Result<RgbImage, VideoFrameError> {
    let width = frame.width();
    let height = frame.height();

    let data = frame
        .plane_data(0)
        .map_err(|e| VideoFrameError::FrameMapping(format!("Failed to get plane data: {}", e)))?;
    let stride = frame.plane_stride()[0] as usize;

    let mut img = RgbImage::new(width, height);

    for y in 0..height {
        for x in (0..width).step_by(2) {
            let offset = y as usize * stride + x as usize * 2;

            let (y0, u, y1, v) = if format == gst_video::VideoFormat::Yuy2 {
                // YUYV
                (
                    data[offset] as i32,
                    data[offset + 1] as i32 - 128,
                    data[offset + 2] as i32,
                    data[offset + 3] as i32 - 128,
                )
            } else {
                // UYVY
                (
                    data[offset + 1] as i32,
                    data[offset] as i32 - 128,
                    data[offset + 3] as i32,
                    data[offset + 2] as i32 - 128,
                )
            };

            let (r0, g0, b0) = yuv_to_rgb(y0, u, v);
            img.put_pixel(x, y, Rgb([r0, g0, b0]));

            if x + 1 < width {
                let (r1, g1, b1) = yuv_to_rgb(y1, u, v);
                img.put_pixel(x + 1, y, Rgb([r1, g1, b1]));
            }
        }
    }

    Ok(img)
}

// ============================================================================
// Image scaling utilities
// ============================================================================

/// Scale an image to the target dimensions.
///
/// Uses triangle (bilinear) filtering for a good balance of quality and speed.
pub fn scale_image(img: &RgbImage, target_width: u32, target_height: u32) -> RgbImage {
    if img.width() == target_width && img.height() == target_height {
        return img.clone();
    }

    let dynamic = image::DynamicImage::ImageRgb8(img.clone());
    let resized = dynamic.resize_exact(
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    resized.to_rgb8()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yuv_to_rgb_white() {
        // White (Y=255, U=0, V=0 after offset)
        let (r, g, b) = yuv_to_rgb(255, 0, 0);
        assert_eq!((r, g, b), (255, 255, 255));
    }

    #[test]
    fn test_yuv_to_rgb_black() {
        // Black (Y=0)
        let (r, g, b) = yuv_to_rgb(0, 0, 0);
        assert_eq!((r, g, b), (0, 0, 0));
    }

    #[test]
    fn test_scale_image_noop() {
        let img = RgbImage::new(320, 180);
        let scaled = scale_image(&img, 320, 180);
        assert_eq!(scaled.width(), 320);
        assert_eq!(scaled.height(), 180);
    }

    #[test]
    fn test_scale_image_downscale() {
        let img = RgbImage::new(1920, 1080);
        let scaled = scale_image(&img, 320, 180);
        assert_eq!(scaled.width(), 320);
        assert_eq!(scaled.height(), 180);
    }
}
