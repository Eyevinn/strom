//! GPU capability detection and video conversion mode selection.
//!
//! The conversion mode is decided once at startup, per platform:
//!
//! * On CUDA platforms (Linux/Windows with NVIDIA) `autovideoconvert` gives a
//!   real GPU path, but only where GL-CUDA interop actually works. NVENC's
//!   presence is the gate for attempting that test, and the interop test is
//!   the gate for using it. Where either fails we use software `videoconvert`.
//! * On macOS there is no CUDA question to ask, so we do not ask it. See
//!   `detect_convert_mode` for why macOS uses threaded software conversion.

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

    /// Returns the element name for a stage that converts *and* scales.
    ///
    /// Use this instead of [`Self::element_name`] for a stage that needs
    /// both: `videoconvertscale` does the two in a single walk of the frame.
    ///
    /// `autovideoconvert` covers both as well — it is
    /// `Bin/Colorspace/Scale/Video/Converter` and autoplugs a scaler when the
    /// caps ask for one.
    pub fn convert_scale_element_name(&self) -> &'static str {
        match self {
            VideoConvertMode::GpuAccelerated => "autovideoconvert",
            VideoConvertMode::Software => "videoconvertscale",
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
#[cfg(not(target_os = "macos"))]
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
pub fn detect_gpu_capabilities() -> VideoConvertMode {
    // Probe GL renderer info early (best-effort, independent of CUDA-GL interop)
    let gl_info = detect_gl_renderer();
    let _ = GL_RENDERER_INFO.set(gl_info);

    let mode = detect_convert_mode();
    let _ = VIDEO_CONVERT_MODE.set(mode);
    mode
}

/// Decide the conversion mode on macOS.
///
/// Do not add an NVENC check here: NVENC never exists on a Mac, so the probe
/// could only ever return one answer.
///
/// `Software` is right for two reasons. `autovideoconvert` offers no GPU path
/// for the frames these call sites see — given GL memory it picks
/// `glcolorconvert`, but our blocks feed it system memory (the GL chains in
/// this tree download at their boundary) and it then selects
/// `videoconvertscale` on the CPU. And being a bin it exposes no `n-threads`,
/// so it would forfeit the threading below, which is what moves the numbers.
#[cfg(target_os = "macos")]
fn detect_convert_mode() -> VideoConvertMode {
    info!(
        "macOS - using software video conversion with n-threads={} (autovideoconvert has no GPU path for system memory here)",
        video_convert_threads()
    );
    VideoConvertMode::Software
}

/// Decide the conversion mode on CUDA-capable platforms (Linux, Windows).
///
/// Detection strategy, unchanged:
/// 1. If running on WSL, skip GPU test (CUDA-GL interop is known broken)
/// 2. If NVENC is unavailable there is no CUDA stack to test, use software mode
/// 3. Test GL→CUDA interop with a fast pipeline (no nvenc initialization)
#[cfg(not(target_os = "macos"))]
fn detect_convert_mode() -> VideoConvertMode {
    // Fast path: WSL has broken CUDA-GL interop, skip expensive test
    if is_wsl() {
        info!("WSL detected - using software video conversion (CUDA-GL interop unsupported)");
        // deprioritize_nv_decoders();
        return VideoConvertMode::Software;
    }

    // Check if nvh264enc is available (required for GPU-accelerated encoding)
    let registry = gst::Registry::get();
    let has_nvenc = registry
        .find_feature("nvh264enc", gst::ElementFactory::static_type())
        .is_some();

    if !has_nvenc {
        info!("NVENC not available - using software video conversion");
        return VideoConvertMode::Software;
    }

    debug!("Testing CUDA-GL interop (this may take a moment on first run)...");

    match test_cuda_gl_interop() {
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
    }
}

/// Sanity bound on the detected core count, not a performance policy: it sits
/// above anything Apple silicon reports, and clamping is logged rather than
/// silent. Deliberately loose, because the errors are asymmetric — overshooting
/// the pool measured 1-2%, undershooting up to 24%.
#[cfg(target_os = "macos")]
const MAX_CONVERT_THREADS: u32 = 32;

/// Sanity bound for the `STROM_VIDEOCONVERT_THREADS` override, so a typo cannot
/// ask for thousands of threads.
#[cfg(target_os = "macos")]
const MAX_OVERRIDE_THREADS: u32 = 64;

/// The override is only an escape hatch if it can exceed the sanity bound.
#[cfg(target_os = "macos")]
const _: () = assert!(MAX_OVERRIDE_THREADS > MAX_CONVERT_THREADS);

/// Number of worker threads to give a software `videoconvert`.
///
/// The stock `n-threads=1` converts a whole frame on one core; a pool sized to
/// the machine measured 24% faster end to end at 1080p on a 4+4 M2.
///
/// The pool counts every performance tier macOS reports except the efficiency
/// one — 4 on an M2, 36 on an M5 Ultra. `videoconvert` cuts a frame into equal
/// stripes and cannot emit until the slowest finishes, so an efficiency core
/// becomes the straggler the whole frame waits on. Apple draws the same line,
/// describing performance cores as joining the super cores for demanding
/// multithreaded work while efficiency cores handle background tasks. Super and
/// performance cores do still differ, so a wide machine is worth measuring with
/// `STROM_VIDEOCONVERT_THREADS`.
///
/// Intel Macs report no tiers and fall back to the total logical CPU count,
/// which is correct there since all cores are equivalent.
#[cfg(target_os = "macos")]
pub fn video_convert_threads() -> u32 {
    static THREADS: OnceLock<u32> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let detected = performance_core_count().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1)
        });
        let raw = std::env::var("STROM_VIDEOCONVERT_THREADS").ok();
        resolve_thread_count(raw.as_deref(), detected)
    })
}

/// Apply the override and the ceiling to a detected core count.
///
/// Split out from [`video_convert_threads`] so the parsing and clamping can be
/// tested without the process-wide `OnceLock` or the environment, and so the
/// behaviour on machines wider than this one is pinned by a test rather than
/// left to inference.
#[cfg(target_os = "macos")]
fn resolve_thread_count(override_raw: Option<&str>, detected: u32) -> u32 {
    if let Some(raw) = override_raw {
        match raw.trim().parse::<u32>() {
            // The override deliberately bypasses MAX_CONVERT_THREADS so a
            // wider machine can be measured past the sanity bound without a
            // rebuild.
            Ok(n) if (1..=MAX_OVERRIDE_THREADS).contains(&n) => return n,
            _ => warn!(
                "ignoring STROM_VIDEOCONVERT_THREADS={:?} - expected an integer in 1..={}",
                raw, MAX_OVERRIDE_THREADS
            ),
        }
    }
    if detected > MAX_CONVERT_THREADS {
        warn!(
            "detected {} fast cores, capping the videoconvert pool at {} - set STROM_VIDEOCONVERT_THREADS to override",
            detected, MAX_CONVERT_THREADS
        );
    }
    detected.clamp(1, MAX_CONVERT_THREADS)
}

/// Count the logical CPUs in every performance tier that is not an efficiency
/// tier.
///
/// macOS numbers its core tiers fastest-first and names each. A part can have
/// more than one fast tier — an M5 Ultra is 12 super cores plus 24 performance
/// cores — so tier 0 alone would be 12 there and discard 24 fast cores.
///
/// Matching the tier to *exclude* is the durable form: "Efficiency" is stable
/// while the fast tiers gain names. A rename fails open — efficiency
/// cores get counted, costing the 1-2% that overshooting costs rather than most
/// of the machine.
///
/// Returns `None` on Intel Macs, where these keys are absent.
#[cfg(target_os = "macos")]
fn performance_core_count() -> Option<u32> {
    let levels = sysctl_u32("hw.nperflevels")?;
    let tiers: Vec<(String, u32)> = (0..levels)
        .map(|i| {
            (
                sysctl_string(&format!("hw.perflevel{}.name", i)).unwrap_or_default(),
                sysctl_u32(&format!("hw.perflevel{}.logicalcpu", i)).unwrap_or(0),
            )
        })
        .collect();

    debug!("CPU performance tiers: {:?}", tiers);
    let n = fast_cores_from_tiers(&tiers);
    (n > 0).then_some(n)
}

/// Sum the tiers that are not efficiency tiers.
///
/// Split from the syscalls so the tier policy can be tested against machines
/// that are not to hand.
#[cfg(target_os = "macos")]
fn fast_cores_from_tiers(tiers: &[(String, u32)]) -> u32 {
    tiers
        .iter()
        .filter(|(name, _)| !name.to_ascii_lowercase().contains("efficiency"))
        .map(|(_, count)| count)
        .sum()
}

/// Read an integer sysctl by name.
#[cfg(target_os = "macos")]
fn sysctl_u32(name: &str) -> Option<u32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut size = std::mem::size_of::<i32>();
    // SAFETY: `value` and `size` describe the same object, and `cname` is a
    // NUL-terminated C string that outlives the call.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut value as *mut i32 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then_some(value as u32)
}

/// Read a string sysctl by name.
#[cfg(target_os = "macos")]
fn sysctl_string(name: &str) -> Option<String> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut size: usize = 0;

    // SAFETY: a null data pointer asks sysctlbyname for the size only.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }

    let mut buf = vec![0u8; size];
    // SAFETY: `buf` has `size` bytes and `size` is updated with what was written.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }

    buf.truncate(size.min(buf.len()));
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Apply tuning properties to a freshly created video conversion element.
///
/// `videoconvert` ships with `n-threads=1`, so a 1080p colour conversion runs
/// on a single core however many the machine has. Every call site that builds a
/// conversion element from [`VideoConvertMode::element_name`] or
/// [`VideoConvertMode::convert_scale_element_name`] routes through here, so the
/// default is set in exactly one place.
///
/// This is macOS-only on purpose. The thread count above is an Apple-silicon
/// heuristic. On Linux in particular a container's visible CPU count routinely
/// overstates its cgroup quota, so picking a pool size there without measuring
/// risks oversubscribing shared hosts. Elsewhere this is a no-op.
pub fn configure_video_convert(element: &gst::Element) {
    #[cfg(target_os = "macos")]
    {
        // Only plain `videoconvert` carries the property; `autovideoconvert` is
        // a bin with nothing to forward it to.
        if element.has_property("n-threads") {
            element.set_property("n-threads", video_convert_threads());
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = element;
    }
}

/// Test if true zero-copy GL-CUDA interop works with nvh264enc.
/// Runs gst-launch-1.0 with GST_DEBUG to capture interop warnings.
/// Returns Ok if zero-copy works, Err if fallback copy is used.
#[cfg(not(target_os = "macos"))]
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

    /// The convert+scale variant must name elements that actually scale.
    /// `videoconvert` here would silently drop the scaling half.
    #[test]
    fn convert_scale_element_name_scales() {
        assert_eq!(
            VideoConvertMode::Software.convert_scale_element_name(),
            "videoconvertscale"
        );
        assert_eq!(
            VideoConvertMode::GpuAccelerated.convert_scale_element_name(),
            "autovideoconvert"
        );

        // autovideoconvert is only correct here because it autoplugs a scaler.
        gst::init().expect("gst init");
        let factory = gst::ElementFactory::find("autovideoconvert")
            .expect("autovideoconvert is in gst-plugins-bad");
        let klass = factory
            .metadata(gst::ELEMENT_METADATA_KLASS)
            .unwrap_or_default();
        assert!(
            klass.contains("Scale"),
            "autovideoconvert does not advertise scaling (klass '{}'); the convert+scale call sites need a separate scaler",
            klass
        );
    }

    /// `configure_video_convert` must raise the thread count off the stock
    /// default of 1. Dropping the property, or a call site's use of this
    /// helper, leaves `n-threads` at 1 and fails this test.
    ///
    /// macOS-only because the helper is deliberately a no-op elsewhere.
    #[cfg(target_os = "macos")]
    #[test]
    fn configure_video_convert_sets_thread_count() {
        gst::init().expect("gst init");

        let convert = gst::ElementFactory::make("videoconvert")
            .build()
            .expect("videoconvert is in gst-plugins-base and must be present");

        assert_eq!(
            convert.property::<u32>("n-threads"),
            1,
            "upstream default changed; the premise of this fix needs rechecking"
        );

        configure_video_convert(&convert);

        assert_eq!(
            convert.property::<u32>("n-threads"),
            video_convert_threads()
        );
        assert!(
            video_convert_threads() > 1,
            "every Mac this runs on is multi-core; a pool of 1 means detection failed"
        );
    }

    /// The macOS choice of `Software` rests on `autovideoconvert` being a bin
    /// that cannot forward a thread count to the converter it picks. If a
    /// future GStreamer grows such a property, that trade-off is worth
    /// re-measuring rather than silently keeping.
    #[cfg(target_os = "macos")]
    #[test]
    fn autovideoconvert_cannot_be_threaded() {
        gst::init().expect("gst init");

        let auto = gst::ElementFactory::make("autovideoconvert")
            .build()
            .expect("autovideoconvert is in gst-plugins-base and must be present");

        assert!(
            !auto.has_property("n-threads"),
            "autovideoconvert now exposes n-threads - re-measure Software vs GpuAccelerated on macOS"
        );
    }

    /// The pool size must stay inside the documented bounds whichever branch
    /// of `video_convert_threads` supplies it.
    #[cfg(target_os = "macos")]
    #[test]
    fn video_convert_threads_is_bounded() {
        let n = video_convert_threads();
        assert!(
            (1..=MAX_OVERRIDE_THREADS).contains(&n),
            "thread count out of range: {}",
            n
        );
    }

    /// Pins what happens on machines wider than this one, which cannot
    /// exercise these paths itself.
    #[cfg(target_os = "macos")]
    #[test]
    fn thread_count_scales_with_cores() {
        // The detected count is used as-is across the whole real Apple silicon
        // range: M2 4, M3 Pro 6, M2 Max 8, M4 Pro 10, M3/M4 Max 12, M3 Ultra 24.
        for cores in [4, 6, 8, 10, 12, 16, 24] {
            assert_eq!(
                resolve_thread_count(None, cores),
                cores,
                "a {}-P-core machine should use its cores, not a capped value",
                cores
            );
        }

        // The bound is a sanity check on the syscall, not a policy: it only
        // engages past anything Apple silicon reports. An old Intel Mac Pro
        // falling back to 56 logical CPUs is the realistic case that hits it.
        assert_eq!(resolve_thread_count(None, 56), MAX_CONVERT_THREADS);
        assert_eq!(resolve_thread_count(None, u32::MAX), MAX_CONVERT_THREADS);

        // A nonsense detection still yields a usable pool.
        assert_eq!(resolve_thread_count(None, 0), 1);
    }

    /// Pins the tier policy against real Apple silicon layouts, none of which
    /// beyond this machine are to hand.
    #[cfg(target_os = "macos")]
    #[test]
    fn fast_cores_counts_every_non_efficiency_tier() {
        let m2 = [("Performance".into(), 4), ("Efficiency".into(), 4)];
        assert_eq!(fast_cores_from_tiers(&m2), 4);

        // M4: fewer performance cores than efficiency cores.
        let m4 = [("Performance".into(), 4), ("Efficiency".into(), 6)];
        assert_eq!(fast_cores_from_tiers(&m4), 4);

        // M5 Ultra: 36 cores as 12 super + 24 performance, no efficiency tier.
        // Taking level 0 alone would yield 12 and throw away 24 fast cores.
        let m5_ultra = [("Super".into(), 12), ("Performance".into(), 24)];
        assert_eq!(fast_cores_from_tiers(&m5_ultra), 36);

        // M6: 2 super + 4 performance + 6 efficiency. All three tiers coexist,
        // so "performance" is a tier in its own right and not efficiency
        // renamed; only the efficiency tier is dropped.
        let m6 = [
            ("Super".into(), 2),
            ("Performance".into(), 4),
            ("Efficiency".into(), 6),
        ];
        assert_eq!(fast_cores_from_tiers(&m6), 6);
    }

    /// If Apple renames the efficiency tier the match fails open, including
    /// those cores rather than discarding most of the machine. Overshooting the
    /// pool costs ~1-2%; undershooting costs far more.
    #[cfg(target_os = "macos")]
    #[test]
    fn fast_cores_fails_open_on_an_unrecognised_tier_name() {
        let renamed = [("Performance".into(), 4), ("E-Core".into(), 4)];
        assert_eq!(fast_cores_from_tiers(&renamed), 8);
    }

    /// The tier names really are readable on this machine, so the policy above
    /// is driven by data rather than by an assumption about the sysctl.
    #[cfg(target_os = "macos")]
    #[test]
    fn perflevel_names_are_readable() {
        let levels = sysctl_u32("hw.nperflevels").expect("macOS reports hw.nperflevels");
        assert!(levels >= 1);
        let name = sysctl_string("hw.perflevel0.name").expect("perflevel0 has a name");
        assert!(!name.is_empty(), "perflevel0.name was empty");
    }

    /// The override is the escape hatch for a machine wider than this one, so
    /// it must beat the sanity bound — and must not be trusted blindly.
    #[cfg(target_os = "macos")]
    #[test]
    fn thread_count_override_bypasses_the_bound() {
        assert_eq!(resolve_thread_count(Some("16"), 4), 16);
        assert_eq!(resolve_thread_count(Some(" 24 "), 4), 24);

        // Past MAX_CONVERT_THREADS, which detection alone can never reach.
        assert_eq!(
            resolve_thread_count(Some(&MAX_OVERRIDE_THREADS.to_string()), 4),
            MAX_OVERRIDE_THREADS
        );

        // Garbage, zero and absurd values fall back to detection rather than
        // taking the process down or spawning thousands of threads.
        for bad in ["", "  ", "no", "-1", "0", "65", "999999", "4.5"] {
            assert_eq!(
                resolve_thread_count(Some(bad), 4),
                4,
                "STROM_VIDEOCONVERT_THREADS={:?} should have been rejected",
                bad
            );
        }
    }

    /// Tripwire for a VideoToolbox converter arriving in a future GStreamer.
    ///
    /// The macOS choice of `Software` rests on there being no hardware
    /// converter for `autovideoconvert` to offer. `autovideoconvert` discovers
    /// candidates by klass (`Filter/Converter/Video...`), and today no
    /// applemedia element carries one — the plugin ships codecs, a source and a
    /// sink, nothing that transforms raw video.
    ///
    /// That is a gap in GStreamer, not in the platform: Apple exposes
    /// `VTPixelTransferSession`, which converts colour and scales, and nothing
    /// upstream wraps it yet. If that changes, `autovideoconvert` will pick the
    /// new element up automatically and `GpuAccelerated` becomes worth
    /// re-measuring here — a VideoToolbox transfer session would keep frames in
    /// `CVPixelBuffer`s and skip both the CPU stripe work and the GL round trip
    /// that makes `glcolorconvert` unprofitable for system memory today.
    ///
    /// This test fails on the day that lands, so the decision gets revisited
    /// deliberately instead of the constant above quietly going stale.
    #[cfg(target_os = "macos")]
    #[test]
    fn no_applemedia_converter_to_reconsider() {
        gst::init().expect("gst init");

        let converters: Vec<String> = gst::Registry::get()
            .features_by_plugin("applemedia")
            .into_iter()
            .filter_map(|f| f.downcast::<gst::ElementFactory>().ok())
            .filter(|f| {
                f.metadata(gst::ELEMENT_METADATA_KLASS)
                    .map(|k| k.contains("Converter"))
                    .unwrap_or(false)
            })
            .map(|f| f.name().to_string())
            .collect();

        assert!(
            converters.is_empty(),
            "applemedia now ships a video converter ({:?}) - autovideoconvert will \
             discover it, so re-measure GpuAccelerated against threaded Software on macOS",
            converters
        );
    }
}
