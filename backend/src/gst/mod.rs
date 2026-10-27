//! GStreamer integration.

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
