//! The startup GL probe must find the renderer wherever GL works.
//!
//! On GStreamer 1.28, `glupload` towards an unconstrained appsink negotiates
//! DMABuf instead of GL memory; the probe buffer then carries no GL context and
//! the renderer reads as unknown, so the "auto" compositor backends fall back
//! to the CPU.

pub mod common;

#[test]
fn the_gl_probe_finds_the_renderer() {
    if !common::gl_available(&["glupload"]) {
        return;
    }
    strom::gpu::detect_gpu_capabilities();
    assert!(
        strom::gpu::gl_renderer_info().is_some(),
        "GL works here, but the startup probe found no renderer"
    );
}
