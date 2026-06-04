//! Compile-validation for every fragment the shader FX library can produce.
//!
//! A fragment that fails to compile posts a GST element error and kills the
//! production pipeline — this test is the gate that keeps that from ever
//! happening. Each fragment runs through a real GL pipeline
//! (`gltestsrc ! glshader ! fakesink`), which works on software GL
//! (llvmpipe) so it runs in CI and on dev boxes without a GPU.
//!
//! The whole test skips when the environment cannot create a GL context at
//! all (probed with the identity fragment, which is trivially valid GLSL).

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// Run one fragment through a short GL pipeline. Returns `Err(message)` if
/// the pipeline posts an error (shader compile failure, context failure, ...).
fn run_fragment(fragment: &str) -> Result<(), String> {
    let pipeline = gst::parse::launch(
        "gltestsrc num-buffers=3 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64,framerate=30/1 ! glshader name=fx ! fakesink sync=false",
    )
    .map_err(|e| format!("parse: {}", e))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_string())?;

    let fx = pipeline
        .by_name("fx")
        .ok_or_else(|| "glshader element not found".to_string())?;
    fx.set_property("fragment", fragment);

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("set_state: {}", e))?;

    let bus = pipeline.bus().expect("pipeline has a bus");
    // 20 s budget: software GL context creation can be slow on loaded CI.
    let timeout = gst::ClockTime::from_seconds(20);
    let result =
        match bus.timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error]) {
            None => Err("timeout waiting for EOS".to_string()),
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(e) => {
                    Err(format!("{} ({})", e.error(), e.debug().unwrap_or_default()))
                }
                _ => unreachable!("filtered"),
            },
        };
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[test]
fn all_shader_fragments_compile() {
    gst::init().expect("gst init");

    if gst::ElementFactory::find("glshader").is_none()
        || gst::ElementFactory::find("gltestsrc").is_none()
    {
        eprintln!("SKIP: GStreamer GL elements not available");
        return;
    }

    // Environment probe: the identity fragment is trivially valid — if it
    // fails, this machine cannot create a GL context (headless without
    // llvmpipe). Skip rather than fail.
    let identity = strom::gst::shaders::identity_fragment();
    if let Err(e) = run_fragment(&identity) {
        eprintln!("SKIP: GL environment unavailable ({})", e);
        return;
    }

    let mut failures = Vec::new();
    for (name, fragment) in strom::gst::shaders::all_fragments() {
        match run_fragment(&fragment) {
            Ok(()) => eprintln!("shader '{}' OK", name),
            Err(e) => {
                eprintln!("shader '{}' FAILED: {}", name, e);
                failures.push((name, e));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} shader(s) failed to compile/run: {:?}",
        failures.len(),
        failures.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
    );
}

/// Runtime fragment swaps must actually change the rendered output.
///
/// `glshader` ignores `fragment` property writes once a shader exists; the
/// swap only works through the `create-shader` signal answered by
/// `attach_create_shader_handler`. This test renders black through an
/// identity shader, swaps to a solid-red fragment at runtime, and asserts
/// the output pixels turn red — the regression test for the silent-no-op
/// swap bug found in production.
#[test]
fn runtime_fragment_swap_takes_effect() {
    gst::init().expect("gst init");

    if gst::ElementFactory::find("glshader").is_none()
        || gst::ElementFactory::find("gltestsrc").is_none()
    {
        eprintln!("SKIP: GStreamer GL elements not available");
        return;
    }
    if run_fragment(&strom::gst::shaders::identity_fragment()).is_err() {
        eprintln!("SKIP: GL environment unavailable");
        return;
    }

    let pipeline = gst::parse::launch(
        "gltestsrc pattern=black ! video/x-raw(memory:GLMemory),format=RGBA,width=32,height=32,framerate=30/1 \
         ! glshader name=fx ! gldownload ! video/x-raw,format=RGBA ! appsink name=out sync=false max-buffers=2 drop=true",
    )
    .expect("parse")
    .downcast::<gst::Pipeline>()
    .expect("pipeline");

    let fx = pipeline.by_name("fx").expect("fx element");
    fx.set_property("fragment", strom::gst::shaders::identity_fragment());
    strom::gst::shaders::attach_create_shader_handler(&fx);

    let appsink = pipeline
        .by_name("out")
        .expect("appsink")
        .downcast::<gst_app::AppSink>()
        .expect("appsink type");

    pipeline.set_state(gst::State::Playing).expect("playing");

    let first_pixel = |sample: &gst::Sample| -> (u8, u8, u8) {
        let buffer = sample.buffer().expect("buffer");
        let map = buffer.map_readable().expect("map");
        (map[0], map[1], map[2])
    };

    // Identity on black source: first pixel must be black.
    let sample = appsink
        .pull_sample()
        .expect("sample before swap (GL pipeline did not produce frames)");
    let (r, g, b) = first_pixel(&sample);
    assert!(
        r < 16 && g < 16 && b < 16,
        "expected black, got {:?}",
        (r, g, b)
    );

    // Swap to a solid-red fragment at runtime.
    let red = "\
#ifdef GL_ES\n\
precision highp float;\n\
#endif\n\
varying vec2 v_texcoord;\n\
uniform sampler2D tex;\n\
void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n";
    fx.set_property("fragment", red);
    fx.set_property("update-shader", true);

    // The swap lands on the next rendered frame; allow a few frames of slack.
    let mut swapped = false;
    for _ in 0..30 {
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) else {
            break;
        };
        let (r, g, b) = first_pixel(&sample);
        if r > 200 && g < 16 && b < 16 {
            swapped = true;
            break;
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    assert!(
        swapped,
        "runtime fragment swap never took effect — create-shader handler broken"
    );
}
