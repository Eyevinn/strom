//! GPU capability detection and video conversion mode selection.
//!
//! This module detects at startup whether GPU-accelerated video conversion
//! with CUDA-GL interop is supported. On systems where it works (native Linux
//! with X11 and NVIDIA drivers), we use `autovideoconvert` for better performance.
//! On systems where it fails (WSL, headless/GBM, broken interop), we fall back
//! to software `videoconvert`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

/// Video conversion mode based on detected GPU capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoConvertMode {
    /// Use `autovideoconvert` - GPU-accelerated when available (GL/CUDA interop works)
    GpuAccelerated,
    /// Use `videoconvert` - safe software fallback (interop broken or no GPU)
    Software,
}

impl VideoConvertMode {
    /// Returns the GStreamer element name to use for video conversion.
    pub fn element_name(&self) -> &'static str {
        match self {
            VideoConvertMode::GpuAccelerated => "autovideoconvert",
            VideoConvertMode::Software => "videoconvert",
        }
    }
}

/// Global detected video convert mode, set once at startup.
static VIDEO_CONVERT_MODE: OnceLock<VideoConvertMode> = OnceLock::new();

/// Get the detected video conversion mode.
/// Panics if called before `detect_gpu_capabilities()`.
pub fn video_convert_mode() -> VideoConvertMode {
    *VIDEO_CONVERT_MODE
        .get()
        .expect("GPU capabilities not detected yet - call detect_gpu_capabilities() first")
}

/// Check if running inside WSL (Windows Subsystem for Linux).
fn is_wsl() -> bool {
    // Check WSL environment variable
    if std::env::var("WSL_DISTRO_NAME").is_ok() || std::env::var("WSL_INTEROP").is_ok() {
        return true;
    }

    // Check /proc/version for WSL signature
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        if version.to_lowercase().contains("microsoft") || version.to_lowercase().contains("wsl") {
            return true;
        }
    }

    false
}

// /// Deprioritize NVIDIA hardware decoders so decodebin3 prefers software decoders.
// /// On WSL, nvh264dec/nvh265dec can cause QoS issues since CUDA-GL interop is broken.
// fn deprioritize_nv_decoders() {
//     let registry = gst::Registry::get();
//     for name in &[
//         "nvh264dec",
//         "nvh265dec",
//         "nvh264sldec",
//         "nvh265sldec",
//         "nvav1dec",
//     ] {
//         if let Some(feature) = registry.find_feature(name, gst::ElementFactory::static_type()) {
//             feature.set_rank(gst::Rank::MARGINAL);
//             info!("Deprioritized {} (set rank to MARGINAL) for WSL", name);
//         }
//     }
// }

/// Detect GPU capabilities and set the global video conversion mode.
/// This should be called once at startup after GStreamer is initialized.
///
/// Detection strategy:
/// 1. If running on WSL, skip GPU test (CUDA-GL interop is known broken)
/// 2. If cudadownload element is unavailable, use software mode
/// 3. Test GL→CUDA interop with a fast pipeline (no nvenc initialization)
pub fn detect_gpu_capabilities() -> VideoConvertMode {
    // Fast path: WSL has broken CUDA-GL interop, skip expensive test
    if is_wsl() {
        info!("WSL detected - using software video conversion (CUDA-GL interop unsupported)");
        // deprioritize_nv_decoders();
        let mode = VideoConvertMode::Software;
        let _ = VIDEO_CONVERT_MODE.set(mode);
        return mode;
    }

    // Check if nvh264enc is available (required for GPU-accelerated encoding)
    let registry = gst::Registry::get();
    let has_nvenc = registry
        .find_feature("nvh264enc", gst::ElementFactory::static_type())
        .is_some();

    if !has_nvenc {
        info!("NVENC not available - using software video conversion");
        let mode = VideoConvertMode::Software;
        let _ = VIDEO_CONVERT_MODE.set(mode);
        return mode;
    }

    debug!("Testing CUDA-GL interop (this may take a moment on first run)...");

    let mode = match test_cuda_gl_interop() {
        Ok(()) => {
            info!("CUDA-GL interop works - using GPU-accelerated video conversion");
            VideoConvertMode::GpuAccelerated
        }
        Err(e) => {
            warn!(
                "CUDA-GL interop failed: {} - using software video conversion",
                e
            );
            VideoConvertMode::Software
        }
    };

    let _ = VIDEO_CONVERT_MODE.set(mode);
    mode
}

/// Test if true zero-copy GL-CUDA interop works with nvh264enc.
/// Runs gst-launch-1.0 with GST_DEBUG to capture interop warnings.
/// Returns Ok if zero-copy works, Err if fallback copy is used.
fn test_cuda_gl_interop() -> Result<(), String> {
    use std::process::Command;

    // Get GL window/platform from environment (for headless Docker with egl-device)
    let gl_window = std::env::var("GST_GL_WINDOW").unwrap_or_default();
    let gl_platform = std::env::var("GST_GL_PLATFORM").unwrap_or_default();

    debug!(
        "Testing CUDA-GL interop with GST_GL_WINDOW={:?}, GST_GL_PLATFORM={:?}",
        gl_window, gl_platform
    );

    // Run gst-launch-1.0 with GST_DEBUG to capture warnings
    // Pipeline: videotestsrc ! glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format=NV12 ! nvh264enc ! fakesink
    let gst_launch = if cfg!(windows) {
        "gst-launch-1.0.exe"
    } else {
        "gst-launch-1.0"
    };
    let mut cmd = Command::new(gst_launch);
    cmd.env("GST_DEBUG", "nvenc:3,nvencoder:3,cudautils:3");

    // Pass through GL environment variables for headless support
    if !gl_window.is_empty() {
        cmd.env("GST_GL_WINDOW", &gl_window);
    }
    if !gl_platform.is_empty() {
        cmd.env("GST_GL_PLATFORM", &gl_platform);
    }

    let output = cmd
        .arg("videotestsrc")
        .arg("num-buffers=1")
        .arg("!")
        .arg("video/x-raw,width=160,height=64")
        .arg("!")
        .arg("glupload")
        .arg("!")
        .arg("glcolorconvert")
        .arg("!")
        .arg("video/x-raw(memory:GLMemory),format=NV12")
        .arg("!")
        .arg("nvh264enc")
        .arg("!")
        .arg("fakesink")
        .output()
        .map_err(|e| format!("Failed to run gst-launch-1.0: {}", e))?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    // Check for interop failure indicators
    if stderr.contains("CUDA_ERROR_OPERATING_SYSTEM")
        || stderr.contains("failed to register")
        || stderr.contains("Couldn't get GL context")
        || stderr.contains("could not register resource")
    {
        return Err("CUDA-GL interop failed (fallback copy detected)".to_string());
    }

    // Check if pipeline succeeded
    if !output.status.success() {
        return Err(format!(
            "Pipeline failed with exit code: {:?}",
            output.status.code()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_convert_mode_element_name() {
        assert_eq!(
            VideoConvertMode::GpuAccelerated.element_name(),
            "autovideoconvert"
        );
        assert_eq!(VideoConvertMode::Software.element_name(), "videoconvert");
    }
}
