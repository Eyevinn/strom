//! GL memory across an appsink/appsrc boundary, with the internal pipeline
//! sharing the flow's contexts through `gst::context_forward`.
//!
//! The Media Player decodes in a pipeline of its own and hands the frames to
//! the flow over an appsink/appsrc pair. A decoder that outputs GL memory,
//! VideoToolbox on macOS, made a GL context of its own, and the flow's first
//! GL converter failed on the first frame: "input memory OpenGL context is
//! different" (#1091, #1099). This test pushes `gltestsrc` frames from an
//! internal pipeline into a flow whose `glcolorconvert` has to convert them.
//! With the forwarding removed it fails with that error.
//!
//! It needs a GL context. On a Linux host with no display, which is CI,
//! `common::init_gl` asks for a surfaceless EGL context, which Mesa's software
//! rasteriser provides.
//!
//! Not built on Windows. The Windows CI runner has no OpenGL driver that
//! GStreamer can use, and there is no software GL context to fall back to.

#![cfg(not(target_os = "windows"))]

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::time::{Duration, Instant};
use strom::gst::context_forward;

const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "gldownload"];
const REQUIRED: &[&str] = &["appsrc", "appsink", "fakesink"];
const FRAMES: i32 = 15;

/// How a run ended: the frames the flow's sink got, and the first error.
struct Outcome {
    buffers: u64,
    error: Option<String>,
}

/// An internal `gltestsrc` pipeline feeding a flow that converts its GL frames
/// in GL, as a GPU Vision Mixer input does. The flow starts first, as a
/// Media Player's flow does.
fn run() -> Outcome {
    let internal = gst::parse::launch(&format!(
        "gltestsrc num-buffers={FRAMES} ! \
         video/x-raw(memory:GLMemory),format=RGBA,width=320,height=240,framerate=30/1 ! \
         appsink name=out sync=false"
    ))
    .expect("internal pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let flow = gst::parse::launch(
        "appsrc name=in format=time ! glcolorconvert ! \
         video/x-raw(memory:GLMemory),format=NV12 ! gldownload ! \
         fakesink name=sink sync=false",
    )
    .expect("flow pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let appsrc = flow
        .by_name("in")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    context_forward::install(&internal, &appsrc);

    let appsink = internal
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let push_to = appsrc.downgrade();
    let eos_to = appsrc.downgrade();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                push_to
                    .upgrade()
                    .ok_or(gst::FlowError::Flushing)?
                    .push_sample(&sample)
            })
            .eos(move |_| {
                if let Some(src) = eos_to.upgrade() {
                    let _ = src.end_of_stream();
                }
            })
            .build(),
    );

    let buffers = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = buffers.clone();
    flow.by_name("sink")
        .unwrap()
        .static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

    flow.set_state(gst::State::Playing).expect("flow plays");
    internal
        .set_state(gst::State::Playing)
        .expect("internal plays");

    let mut error = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    'wait: while Instant::now() < deadline {
        for bus in [flow.bus().unwrap(), internal.bus().unwrap()] {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        error = Some(format!("{} ({:?})", e.error(), e.debug()));
                        break 'wait;
                    }
                    gst::MessageView::Eos(_) if msg.src() == Some(flow.upcast_ref()) => {
                        break 'wait;
                    }
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = internal.set_state(gst::State::Null);
    let _ = flow.set_state(gst::State::Null);

    Outcome {
        buffers: buffers.load(std::sync::atomic::Ordering::Relaxed),
        error,
    }
}

#[test]
fn gl_frames_from_an_internal_pipeline_convert_in_the_flow() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }
    let shared = run();
    assert!(
        shared.error.is_none(),
        "the flow's GL converter failed on the internal pipeline's frames: {:?}",
        shared.error
    );
    assert_eq!(
        shared.buffers, FRAMES as u64,
        "every frame reached the flow's sink"
    );
}
