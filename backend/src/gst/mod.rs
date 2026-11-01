//! GStreamer integration.

pub(crate) mod aggregator_catch_up;
pub(crate) mod aggregator_start;
mod block_expansion;
pub mod buffer_age_probe;
pub mod context_forward;
pub(crate) mod control_bindings;
pub(crate) mod crop;
pub mod discovery;
pub mod download_front;
pub mod gl_input_front;
pub mod gl_link;
pub mod ice_preflight;
pub mod keyframe_request;
pub mod latency_diagnosis;
pub mod orphan_guard;
pub mod pipeline;
pub mod pipeline_bridge;
pub mod pipeline_monitor;
pub mod rtp_hdrext;
pub mod scale_front;
pub mod shaders;
pub mod software_decoder;
pub mod thread_priority;
pub mod thumbnail;
pub mod thumbnail_tap;
pub mod tls;
pub mod transitions;
pub(crate) mod underlay;
pub mod video_adapt;
pub mod video_input_bridge;
pub mod volume_ramp;
pub mod whep_probe;

/// Register the GStreamer plugins Strom links in statically (gst-plugins-rs
/// WebRTC, inter, RTP, audiofx, isobmff, lsp-dsp-rs, and EFP with the `efp`
/// feature). Call once, after `gst::init()`, before any flow is built. Panics
/// when a plugin fails to register: the blocks that use it could not work.
///
/// isobmff is the exception. GStreamer 1.28 installs its own `isobmff` plugin
/// with the same element names as Strom's fork (`isofmp4mux`, `cmafmux`, ...).
/// Registering the fork statically replaces those registry entries, so the
/// fork's elements win. If the system plugin was already loaded into this
/// process, its GTypes exist and the fork cannot register; the system plugin
/// then provides the elements, which is not a reason to stop the server.
pub fn register_static_plugins() {
    gstwebrtchttp::plugin_register_static().expect("Could not register webrtchttp plugins");
    gstrswebrtc::plugin_register_static().expect("Could not register webrtc plugins");
    gstrsinter::plugin_register_static().expect("Could not register inter plugins");
    gstrsrtp::plugin_register_static().expect("Could not register rtp plugins");
    gstrsaudiofx::plugin_register_static().expect("Could not register audiofx plugins");
    if let Err(err) = gstisobmff::plugin_register_static() {
        tracing::warn!(
            "Could not register Strom's isobmff plugin ({}); the system isobmff plugin is used instead, if one is installed",
            err
        );
    }
    gst_plugins_lsp::plugin_register_static().expect("Could not register lsp-dsp-rs plugins");
    #[cfg(feature = "efp")]
    gst_plugin_efp::plugin_register_static().expect("Could not register efp mux/demux plugins");
}

/// The Intel Quick Sync Video encoders from GStreamer's `qsv` plugin.
pub const QSV_ENCODERS: [&str; 4] = ["qsvh264enc", "qsvh265enc", "qsvvp9enc", "qsvav1enc"];

/// The first Intel graphics driver for Windows whose D3D11 driver no longer
/// corrupts the heap when it refuses an encode query (#1163).
pub const QSV_FIXED_INTEL_DRIVER: [u16; 4] = [32, 0, 101, 9150];

/// Where to get it.
const INTEL_DRIVER_DOWNLOAD: &str =
    "https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html";

/// True when Intel driver `version` has the fix for #1163. `None` (no Intel
/// adapter found, or its version could not be read) counts as not fixed.
pub fn intel_driver_has_qsv_fix(version: Option<[u16; 4]>) -> bool {
    version.is_some_and(|v| v >= QSV_FIXED_INTEL_DRIVER)
}

/// Drop the Intel Quick Sync Video encoders' rank to NONE on Windows when the
/// Intel graphics driver is older than [`QSV_FIXED_INTEL_DRIVER`], so nothing
/// picks them: neither the Video Encoder block, which skips an encoder of rank
/// NONE, nor `webrtcsink`'s encoder discovery in WHEP Output.
///
/// Before 32.0.101.9150, Intel's D3D11 driver corrupts the process heap
/// (STATUS_HEAP_CORRUPTION) when it refuses a VP9 or AV1 encode query. The
/// `qsv` plugin probes sizes it refuses when it loads, so a plain
/// `videotestsrc ! qsvh264enc ! fakesink` crashed about one run in three on
/// Core Ultra, and inside Strom that takes the whole server down (#1163).
/// With 32.0.101.9150 or later, or on other platforms, the encoders keep their
/// rank. Call after `gst::init()` and before `register_static_plugins()`:
/// `webrtcsink` reads encoder ranks when it registers.
pub fn demote_qsv_encoders() {
    use gstreamer::prelude::PluginFeatureExtManual;

    if !cfg!(windows) {
        return;
    }
    let version = intel_driver_version();
    let shown = version
        .map(|v| format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]))
        .unwrap_or_else(|| "unknown".to_string());
    if intel_driver_has_qsv_fix(version) {
        tracing::info!("Intel graphics driver {}: QSV encoders enabled", shown);
        return;
    }
    let installed: Vec<&str> = QSV_ENCODERS
        .into_iter()
        .filter(|name| {
            gstreamer::ElementFactory::find(name)
                .map(|factory| {
                    let ranked = factory.rank() != gstreamer::Rank::NONE;
                    factory.set_rank(gstreamer::Rank::NONE);
                    ranked
                })
                .unwrap_or(false)
        })
        .collect();
    if !installed.is_empty() {
        let [a, b, c, d] = QSV_FIXED_INTEL_DRIVER;
        tracing::warn!(
            "Intel QSV encoders disabled ({}): Intel graphics driver {} corrupts the heap \
             when QSV probes its encoders (#1163). Install {}.{}.{}.{} or later to use them: {}",
            installed.join(", "),
            shown,
            a,
            b,
            c,
            d,
            INTEL_DRIVER_DOWNLOAD
        );
    }
}

/// The lowest driver version among the Intel display adapters, from DXGI.
/// `None` when there is no Intel adapter or its version cannot be read.
#[cfg(windows)]
pub fn intel_driver_version() -> Option<[u16; 4]> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};

    const INTEL: u32 = 0x8086;
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut lowest: Option<[u16; 4]> = None;
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.VendorId != INTEL {
            continue;
        }
        // The user-mode driver version, packed as four 16-bit parts.
        let Ok(packed) = (unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }) else {
            continue;
        };
        let v = [
            (packed >> 48) as u16,
            (packed >> 32) as u16,
            (packed >> 16) as u16,
            packed as u16,
        ];
        lowest = Some(lowest.map_or(v, |l| l.min(v)));
    }
    lowest
}

#[cfg(not(windows))]
pub fn intel_driver_version() -> Option<[u16; 4]> {
    None
}

/// Start GLib main loop in a background thread.
/// This is required for GStreamer bus watch callbacks to be dispatched.
pub fn start_glib_main_loop() {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        tracing::info!("GLib main loop thread started");
        let main_loop = gstreamer::glib::MainLoop::new(None, false);

        // Signal that the main loop is ready
        tx.send(()).ok();

        main_loop.run();
        tracing::info!("GLib main loop thread exiting");
    });

    // Wait for confirmation that the GLib main loop thread has started
    rx.recv().expect("Failed to start GLib main loop");
    tracing::info!("GLib main loop is now running");
}

pub use discovery::ElementDiscovery;
pub use pipeline::{PipelineError, PipelineManager};
pub use thread_priority::{
    setup_thread_priority_handler, SessionThreadConfig, ThreadPriorityState,
};
pub use thumbnail::ThumbnailError;
pub use thumbnail_tap::{new_tap_store, ThumbnailTap, ThumbnailTapConfig, ThumbnailTapStore};
pub use transitions::{TransitionController, TransitionError, TransitionType};
