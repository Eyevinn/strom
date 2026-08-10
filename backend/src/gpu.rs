//! GPU capability detection and video conversion mode selection.
//!
//! This module detects at startup whether GPU-accelerated video conversion
//! is supported, and picks between `autovideoconvert` (which auto-inserts
//! bridge elements such as `gldownload`/`glcolorconvert` when GL-memory
//! frames need to reach a non-GL element, e.g. an encoder) and plain
//! `videoconvert` (software-only; cannot accept `memory:GLMemory` caps).
//!
//! - On macOS there is no CUDA to worry about, so `autovideoconvert` is
//!   always used (see `detect_gpu_capabilities`).
//! - On Linux, GPU accel additionally requires CUDA-GL interop with NVENC.
//!   On systems where it works (native Linux with X11 and NVIDIA drivers),
//!   we use `autovideoconvert` for better performance. On systems where it
//!   fails (WSL, headless/GBM, broken interop), we fall back to software
//!   `videoconvert`.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::OnceLock;
use strom_types::GlRendererInfo;
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

/// Global GL renderer info, probed once at startup.
static GL_RENDERER_INFO: OnceLock<Option<GlRendererInfo>> = OnceLock::new();

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

/// Get the detected GL renderer info (None if detection failed or not yet run).
pub fn gl_renderer_info() -> Option<GlRendererInfo> {
    GL_RENDERER_INFO.get().cloned().flatten()
}

/// Returns true if a hardware-accelerated GL renderer was detected at startup.
/// Returns false if GL probe failed, hasn't run, or detected a software renderer
/// (Mesa llvmpipe/softpipe/swrast). Used by compositor auto-selection to avoid
/// choosing glvideomixerelement when it would run slower than CPU compositor.
pub fn has_hardware_gl() -> bool {
    match gl_renderer_info() {
        Some(info) => {
            let r = info.renderer.to_lowercase();
            !r.contains("llvmpipe") && !r.contains("softpipe") && !r.contains("swrast")
        }
        None => false,
    }
}

/// Probe the OpenGL renderer via GStreamer's GL context API.
///
/// Creates a minimal in-process pipeline (`videotestsrc ! glupload ! appsink`),
/// runs it to produce one buffer, extracts the `GstGLContext` from the GL memory,
/// then queries `glGetString` on the GL thread via `thread_add`.
fn detect_gl_renderer() -> Option<GlRendererInfo> {
    use gstreamer_gl::prelude::*;
    use std::ffi::CStr;

    // GL constants
    const GL_VENDOR: u32 = 0x1F00;
    const GL_RENDERER: u32 = 0x1F01;
    const GL_VERSION: u32 = 0x1F02;
    const GL_SHADING_LANGUAGE_VERSION: u32 = 0x8B8C;

    let pipeline = gst::parse::launch(
        "videotestsrc num-buffers=1 ! video/x-raw,width=64,height=64 ! glupload ! appsink name=sink",
    )
    .map_err(|e| warn!("GL probe: failed to create pipeline: {}", e))
    .ok()?;

    let pipeline = pipeline.downcast::<gst::Pipeline>().ok()?;

    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gstreamer_app::AppSink>().ok())?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| warn!("GL probe: failed to start pipeline: {}", e))
        .ok()?;

    // Pull one buffer to ensure the GL context has been created (timeout avoids
    // hanging on headless systems without a display server)
    let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(5));
    if sample.is_none() {
        debug!("GL probe: no sample received within timeout (GL context may not be available)");
    }
    let gl_context = sample.as_ref().and_then(|s| {
        let buffer = s.buffer()?;
        let mem = (buffer.n_memory() > 0).then(|| buffer.peek_memory(0))?;
        let gl_mem = mem.downcast_memory_ref::<gstreamer_gl::GLBaseMemory>()?;
        Some(gl_mem.context().clone())
    });

    // Tear down pipeline regardless of result
    let _ = pipeline.set_state(gst::State::Null);

    let gl_context = match gl_context {
        Some(ctx) => ctx,
        None => {
            debug!("GL probe: could not extract GL context from buffer");
            return None;
        }
    };

    // Query GL strings on the GL thread
    let (renderer, version, vendor, glsl_version) = {
        let result =
            std::sync::Mutex::new((String::new(), String::new(), String::new(), String::new()));

        gl_context.thread_add(|ctx| {
            // glGetString function pointer from the GL context
            type GlGetString = unsafe extern "system" fn(u32) -> *const u8;
            let get_string_ptr = ctx.proc_address("glGetString");
            if get_string_ptr == 0 {
                return;
            }
            let get_string: GlGetString = unsafe { std::mem::transmute(get_string_ptr) };

            let read_str = |name: u32| -> String {
                let ptr = unsafe { get_string(name) };
                if ptr.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(ptr as *const _) }
                        .to_string_lossy()
                        .into_owned()
                }
            };

            if let Ok(mut r) = result.lock() {
                *r = (
                    read_str(GL_RENDERER),
                    read_str(GL_VERSION),
                    read_str(GL_VENDOR),
                    read_str(GL_SHADING_LANGUAGE_VERSION),
                );
            }
        });

        result.into_inner().unwrap_or_default()
    };

    if renderer.is_empty() {
        debug!("GL probe: glGetString returned empty strings");
        return None;
    }

    let gl_info = GlRendererInfo {
        renderer,
        version,
        vendor,
        glsl_version,
    };

    info!(
        "GL renderer: {} ({}), GL {}, GLSL {}",
        gl_info.renderer, gl_info.vendor, gl_info.version, gl_info.glsl_version
    );

    Some(gl_info)
}

/// Detect GPU capabilities and set the global video conversion mode.
/// This should be called once at startup after GStreamer is initialized.
///
/// Detection strategy:
/// 1. If running on WSL, skip GPU test (CUDA-GL interop is known broken)
/// 2. If cudadownload element is unavailable, use software mode
/// 3. Test GL→CUDA interop with a fast pipeline (no nvenc initialization)
pub fn detect_gpu_capabilities() -> VideoConvertMode {
    // Probe GL renderer info early (best-effort, independent of CUDA-GL interop)
    let gl_info = detect_gl_renderer();
    let _ = GL_RENDERER_INFO.set(gl_info);

    // macOS has no CUDA/NVENC, so the interop test below (which is entirely
    // CUDA-specific) never applies here and would always force the software
    // fallback. But GL-memory frames still occur on macOS (e.g. from the GL
    // video mixer) and need bridging to non-GL encoders like vtenc_h264_hw.
    // autovideoconvert auto-inserts the needed gldownload/glcolorconvert
    // bridge; plain videoconvert cannot accept memory:GLMemory caps at all.
    // There's no known autovideoconvert breakage on macOS to guard against
    // (unlike the WSL/CUDA-GL cases below), so always use it here.
    if cfg!(target_os = "macos") {
        info!(
            "macOS detected - using GPU-accelerated video conversion (autovideoconvert bridges GL memory)"
        );
        let mode = VideoConvertMode::GpuAccelerated;
        let _ = VIDEO_CONVERT_MODE.set(mode);
        return mode;
    }

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
