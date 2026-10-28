//! Helpers shared by the integration tests.
//!
//! Include it as `pub mod common;`. The `pub` is what keeps the helpers a given
//! test binary does not call from being reported as dead code.

pub mod block;
pub mod bus;
pub mod download;
pub mod manager;
pub mod state;

use gstreamer as gst;

/// The elements in `required` that this GStreamer install does not provide.
/// Initialises GStreamer first, so it can run before anything else.
pub fn missing_elements<'a>(required: &[&'a str]) -> Vec<&'a str> {
    gst::init().expect("GStreamer initialises");
    required
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect()
}

/// Whether a test may skip because of `missing`: true when nothing is missing.
///
/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure. Every skip on a
/// missing element goes through here, so none can quietly opt out of that.
pub fn require_or_skip(missing: &[&str]) -> bool {
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    eprintln!(
        "SKIP: required GStreamer elements missing: {}",
        missing.join(", ")
    );
    false
}

/// True when every element in `required` is available. Otherwise skips, or
/// fails under `STROM_REQUIRE_GST_PLUGINS` — see [`require_or_skip`].
pub fn plugins_available(required: &[&str]) -> bool {
    require_or_skip(&missing_elements(required))
}

/// [`plugins_available`] for GL elements. A missing one also fails under
/// `STROM_REQUIRE_GL`: a platform that must render cannot do it without them.
pub fn gl_elements_available(required: &[&str]) -> bool {
    let missing = missing_elements(required);
    assert!(
        missing.is_empty() || strom_types::env::var_opt("STROM_REQUIRE_GL").is_none(),
        "STROM_REQUIRE_GL is set but these GL elements are missing: {}",
        missing.join(", ")
    );
    require_or_skip(&missing)
}

/// True when this test should skip because it fails on the CI runner for this
/// platform but passes on a developer machine. The Linux, macOS and Windows CI
/// jobs set `STROM_SKIP_KNOWN_CI_FAILURES=1`; local runs leave it unset, so
/// there the test runs as normal. `platforms` lists the runners it fails on, as
/// `std::env::consts::OS` names them ("linux", "macos", "windows"), so it still
/// runs on the others. `reason` says what goes wrong on the runner.
pub fn skip_known_ci_failure(platforms: &[&str], reason: &str) -> bool {
    if strom_types::env::var_opt("STROM_SKIP_KNOWN_CI_FAILURES").is_none()
        || !platforms.contains(&std::env::consts::OS)
    {
        return false;
    }
    eprintln!("SKIP: known CI runner failure: {reason}");
    true
}

/// Fail outright if any element in `required` is missing, whatever
/// `STROM_REQUIRE_GST_PLUGINS` says. For tests that must never skip.
pub fn require_elements(required: &[&str]) {
    let missing = missing_elements(required);
    assert!(
        missing.is_empty(),
        "these GStreamer elements are missing, so this test would guard nothing: {}; \
         the CI image needs the package that provides them",
        missing.join(", ")
    );
}

/// Register the plugins the server links in statically, among them the
/// `gst-plugins-rs` WebRTC elements (`whipsink`, `whepsrc`, ...), through
/// `strom::gst::register_static_plugins()`, the call the server makes at
/// startup. Also initialises GStreamer. Safe to call repeatedly.
pub fn init_webrtc_plugins() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        strom::gst::demote_qsv_encoders();
        strom::gst::register_static_plugins();
    });
}

/// Initialise GStreamer for a GL test. On a Linux host with no display, which
/// is CI, first ask for a surfaceless EGL context, which Mesa's software
/// rasteriser provides. Other platforms keep their native GL (CGL on macOS).
///
/// The choice has to be in the environment before GStreamer creates a GL
/// display, so call this before any GL element exists — [`gl_available`] does.
/// Every GL test enters through this `Once`, so no test thread of the binary
/// reads the environment while it is written.
pub fn init_gl() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "linux")]
        if std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("GST_GL_WINDOW").is_none()
        {
            std::env::set_var("GST_GL_PLATFORM", "egl");
            std::env::set_var("GST_GL_WINDOW", "surfaceless");
        }
        gst::init().expect("GStreamer initialises");
    });
}

/// Why no GL context can be created here, or `None` when one can. Probed once
/// per test binary with a trivial GL run that must reach EOS. It contains no
/// `glshader` and none of the code under test, so a shader or bridge bug cannot
/// pass for a missing GL environment. Private: a test that skipped on this
/// directly would bypass the `STROM_REQUIRE_GL` check in [`gl_available`].
fn gl_context_error() -> Option<String> {
    use std::sync::OnceLock;
    static PROBE: OnceLock<Option<String>> = OnceLock::new();
    PROBE.get_or_init(probe_gl_context).clone()
}

fn probe_gl_context() -> Option<String> {
    use gst::prelude::*;
    let pipeline = match gst::parse::launch(
        "gltestsrc num-buffers=3 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64,framerate=30/1 ! fakesink sync=false",
    ) {
        Ok(p) => p,
        Err(e) => return Some(format!("probe pipeline does not parse: {}", e)),
    };
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Some(format!("probe pipeline does not start: {}", e));
    }
    let bus = pipeline.bus().expect("pipeline has a bus");
    // 20 s budget: software GL context creation can be slow on loaded CI.
    let error = match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        None => Some("timed out waiting for EOS".to_string()),
        Some(msg) => match msg.view() {
            gst::MessageView::Error(e) => {
                Some(format!("{} ({})", e.error(), e.debug().unwrap_or_default()))
            }
            _ => None,
        },
    };
    let _ = pipeline.set_state(gst::State::Null);
    error
}

/// True when the GL elements in `required` exist and a GL context can be
/// created. Otherwise skips, or fails: a missing element under
/// `STROM_REQUIRE_GST_PLUGINS` or `STROM_REQUIRE_GL` (see
/// [`gl_elements_available`]), no context under `STROM_REQUIRE_GL`.
///
/// CI sets `STROM_REQUIRE_GL` on every job whose runner can render (Linux
/// through surfaceless EGL, macOS natively), so a GL regression there fails
/// rather than skipping green.
pub fn gl_available(required: &[&str]) -> bool {
    init_gl();
    if !gl_elements_available(required) {
        return false;
    }
    let Some(error) = gl_context_error() else {
        return true;
    };
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GL").is_none(),
        "STROM_REQUIRE_GL is set but no GL context could be created ({}) — this \
         platform is supposed to render, so a skip here would hide a GL regression",
        error
    );
    eprintln!("SKIP: GL environment unavailable ({})", error);
    false
}

/// A free UDP port on 127.0.0.1. Binding one and dropping it races with
/// anything else on the host, so a caller whose listener fails to bind should
/// name the element in its error rather than read it as the regression.
pub fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port")
}

/// What takes one entry of a GPU Vision Mixer input's answer to a CAPS query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadPath {
    /// A memory type or meta that `glupload` (or `cudadownload`) takes as it is.
    DirectFeature,
    /// Plain system memory in a format `glupload` uploads.
    DirectSystemMemory,
    /// Anything else: only the converter in front of `glupload` takes it.
    /// ANY caps features land here too, as nothing shows they are taken.
    ConverterOnly,
}

/// Caps features that reach a GPU input's caps answer through what takes them
/// directly: `glupload` for GL memory, DMABuf and the texture upload meta
/// (GStreamer 1.28 with Mesa EGL lists all three), `cudadownload` for CUDA
/// memory.
pub const DIRECT_UPLOAD_FEATURES: &[&str] = &[
    "memory:GLMemory",
    "memory:DMABuf",
    "meta:GstVideoGLTextureUploadMeta",
    "memory:CUDAMemory",
];

/// Classify one caps answer entry. `uploads` is the system-memory part of
/// `glupload`'s sink template (`gl_upload_system_caps()`).
pub fn upload_path(
    structure: &gst::StructureRef,
    features: &gst::CapsFeaturesRef,
    uploads: &gst::Caps,
) -> UploadPath {
    if features.is_any() {
        return UploadPath::ConverterOnly;
    }
    if features
        .iter()
        .any(|feature| DIRECT_UPLOAD_FEATURES.contains(&feature.as_str()))
    {
        return UploadPath::DirectFeature;
    }
    let plain_system_memory = features
        .iter()
        .all(|feature| feature == gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
    if !plain_system_memory || !structure.has_field("format") {
        return UploadPath::ConverterOnly;
    }
    let mut one = gst::Caps::new_empty();
    one.make_mut().append_structure(structure.to_owned());
    if one.is_subset(uploads) {
        UploadPath::DirectSystemMemory
    } else {
        UploadPath::ConverterOnly
    }
}

/// What a pad answers to an ALLOCATION query for `caps`: the meta APIs it
/// supports, sorted, and the minimum buffers of its first pool.
#[derive(Debug, PartialEq)]
pub struct AllocationProposal {
    pub metas: Vec<String>,
    pub min_buffers: Option<u32>,
}

/// Ask `pad` for its ALLOCATION proposal. A sink pad behind a running
/// aggregator answers once its queue is empty, so this waits for that.
pub fn allocation_proposal(pad: &gst::Pad, caps: &gst::Caps) -> AllocationProposal {
    use gst::prelude::*;
    let mut query = gst::query::Allocation::new(Some(caps), false);
    assert!(pad.query(&mut query), "{} refused ALLOCATION", pad.name());
    let mut metas: Vec<String> = query
        .allocation_metas()
        .map(|(api, _)| api.name().to_string())
        .collect();
    metas.sort();
    let min_buffers = query.allocation_pools().next().map(|(_, _, min, _)| min);
    AllocationProposal { metas, min_buffers }
}
