//! Regression test: CUDA memory into the GPU Vision Mixer on a host without
//! `cudadownload` fails the flow with a reason (#682).
//!
//! Its own test binary, so no stand-in `cudadownload` is ever registered in
//! this process: `vision_mixer_cuda_input_test` registers one for the life of
//! its process, and a GStreamer registry feature cannot be safely removed.

pub mod common;
pub mod vision_mixer_cuda;

use gstreamer as gst;
use strom::gst::gl_input_front::CUDA_ADAPTER_FACTORY;
use vision_mixer_cuda::*;

/// Without `cudadownload` (no nvcodec plugin), CUDA memory must fail the flow
/// with a message that names what is missing, posted by the mixer input —
/// not only the Media Player's `Internal data stream error`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cuda_memory_without_cudadownload_fails_the_flow_with_a_reason() {
    common::require_elements(GL_ELEMENTS);
    gst::init().unwrap();
    if gst::ElementFactory::find(CUDA_ADAPTER_FACTORY).is_some() {
        // Only an NVIDIA build has the real element; CI never does.
        eprintln!(
            "SKIP: {} is installed, so the missing-adapter path cannot be reached here",
            CUDA_ADAPTER_FACTORY
        );
        return;
    }

    let manager = build_manager(&player_into_mixer("cuda_no_adapter", "video_in_0"));
    let pipeline = manager.pipeline();
    let queue = format!("{}:queue_0", MIXER);
    let errors = push_from_player(pipeline, CUDA_CAPS).errors;
    let ours = errors.iter().find(|(source, _, _)| *source == queue);
    let Some((_, message, debug)) = ours else {
        panic!(
            "no error from {}: CUDA memory without {} fails the flow without saying \
             why. Errors on the bus: {:?}",
            queue, CUDA_ADAPTER_FACTORY, errors
        );
    };
    assert!(
        message.contains(CUDA_ADAPTER_FACTORY)
            && message.contains("CUDA memory")
            && message.contains("video_in_0"),
        "the error does not say which input lacks what: {}",
        message
    );
    assert!(
        debug.contains("nvcodec"),
        "the error does not say how to fix it: {}",
        debug
    );
}
