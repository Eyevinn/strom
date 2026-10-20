//! Regression test: D3D12 memory into the GPU Vision Mixer on a host without
//! `d3d12download` fails the flow with a reason.
//!
//! Its own test binary, so no stand-in `d3d12download` is ever registered in
//! this process: `vision_mixer_d3d_input_test` registers one for the life of
//! its process, and a GStreamer registry feature cannot be safely removed.

pub mod common;
pub mod vision_mixer_cuda;

use gstreamer as gst;
use strom::gst::video_adapt::D3D12_DOWNLOAD_FACTORY;
use vision_mixer_cuda::*;

const D3D12_CAPS: &str = "video/x-raw(memory:D3D12Memory),format=NV12,width=320,height=240,\
     framerate=25/1,pixel-aspect-ratio=1/1,interlace-mode=progressive";

/// Without `d3d12download` (no d3d12 plugin), D3D12 memory must fail the flow
/// with a message that names what is missing, posted by the mixer input —
/// not only the Media Player's `Internal data stream error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d3d12_memory_without_d3d12download_fails_the_flow_with_a_reason() {
    common::require_elements(GL_ELEMENTS);
    gst::init().unwrap();
    if gst::ElementFactory::find(D3D12_DOWNLOAD_FACTORY).is_some() {
        // Only a Windows build has the real element; the Linux CI runner never does.
        eprintln!(
            "SKIP: {} is installed, so the missing-adapter path cannot be reached here",
            D3D12_DOWNLOAD_FACTORY
        );
        return;
    }

    let manager = build_manager(&player_into_mixer("d3d12_no_adapter", "video_in_0"));
    let pipeline = manager.pipeline();
    let queue = format!("{}:queue_0", MIXER);
    let errors = push_from_player(pipeline, D3D12_CAPS).errors;
    let ours = errors.iter().find(|(source, _, _)| *source == queue);
    let Some((_, message, debug)) = ours else {
        panic!(
            "no error from {}: D3D12 memory without {} fails the flow without saying \
             why. Errors on the bus: {:?}",
            queue, D3D12_DOWNLOAD_FACTORY, errors
        );
    };
    assert!(
        message.contains(D3D12_DOWNLOAD_FACTORY)
            && message.contains("D3D12 memory")
            && message.contains("video_in_0"),
        "the error does not say which input lacks what: {}",
        message
    );
    assert!(
        debug.contains("d3d12 plugin"),
        "the error does not say how to fix it: {}",
        debug
    );
}
