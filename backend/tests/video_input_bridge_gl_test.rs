//! The WHEP Output video input bridge on real GL memory.
//!
//! The unit tests in `gst::video_input_bridge` see GL memory only as caps
//! strings. This pushes `gltestsrc` frames, RGBA in GL memory as a GPU Vision
//! Mixer hands them to a WHEP Output, through the bridge and checks that the
//! peer receives NV12 in system memory, through `gldownload` and a converter,
//! with `videoconvert`, the converter the WHEP Output uses.
//!
//! It needs a GL context. On a Linux host with no display, which is CI,
//! `common::init_gl` asks for a surfaceless EGL context, which Mesa's software
//! rasteriser provides.
//!
//! Not built on Windows. The Windows CI runner has no OpenGL driver that
//! GStreamer can use ("No GL shader support available"), and there is no
//! software GL context to fall back to. Linux CI runs it.

#![cfg(not(target_os = "windows"))]

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::gst::video_input_bridge::install_video_input_bridge;

const REQUIRED: [&str; 4] = ["gltestsrc", "gldownload", "videoconvert", "fakesink"];

const GL_RGBA: &str =
    "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1";

struct Outcome {
    format: String,
    gl_memory: bool,
    buffers: usize,
    /// Factories between the queue and the sink, in link order.
    spliced: Vec<String>,
}

fn run(convert_factory: &str) -> Outcome {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("gltestsrc")
        .property("num-buffers", 15i32)
        .build()
        .expect("gltestsrc");
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", GL_RGBA.parse::<gst::Caps>().expect("caps"))
        .build()
        .expect("capsfilter");
    let queue = gst::ElementFactory::make("queue")
        .name("video_queue")
        .build()
        .expect("queue");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([&src, &filter, &queue, &sink])
        .expect("add");
    gst::Element::link_many([&src, &filter, &queue, &sink]).expect("link");

    install_video_input_bridge(
        &queue.static_pad("src").expect("queue src pad"),
        "video_queue",
        convert_factory,
    );

    let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
    let buffers = Arc::new(AtomicUsize::new(0));
    let counter = buffers.clone();
    sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).expect("play");
    let bus = pipeline.bus().expect("bus");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                gst::MessageView::Error(e) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    panic!("pipeline error: {} ({:?})", e.error(), e.debug())
                }
                _ => {}
            }
        }
    }

    let caps = sink_pad.current_caps();
    let format = caps
        .as_ref()
        .and_then(|c| c.structure(0))
        .and_then(|s| s.get::<String>("format").ok())
        .unwrap_or_default();
    let gl_memory = caps
        .as_ref()
        .and_then(|c| c.features(0))
        .is_some_and(|f| f.contains("memory:GLMemory"));

    let mut spliced = Vec::new();
    let mut next = queue.static_pad("src").and_then(|pad| pad.peer());
    while let Some(pad) = next {
        let element = pad.parent_element().expect("linked pad has an element");
        if element == sink {
            break;
        }
        spliced.push(
            element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default(),
        );
        next = element.static_pad("src").and_then(|pad| pad.peer());
    }

    pipeline.set_state(gst::State::Null).expect("null");
    Outcome {
        format,
        gl_memory,
        buffers: buffers.load(Ordering::Relaxed),
        spliced,
    }
}

/// Without the download the converter cannot take the frames, and without the
/// conversion every consumer converts RGBA for itself.
#[test]
fn gl_rgba_reaches_the_peer_as_system_memory_nv12() {
    if !common::gl_available(&REQUIRED) {
        return;
    }

    let convert_factory = "videoconvert";
    let outcome = run(convert_factory);
    assert_eq!(
        outcome.spliced,
        ["gldownload", convert_factory, "capsfilter"],
        "{}: expected a download, then the converter and the format pin",
        convert_factory
    );
    assert_eq!(outcome.format, "NV12", "{}", convert_factory);
    assert!(
        !outcome.gl_memory,
        "{}: the peer should receive system memory",
        convert_factory
    );
    assert!(
        outcome.buffers > 0,
        "{}: no buffers reached the peer",
        convert_factory
    );
}
