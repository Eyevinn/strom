//! The WHEP Output video input bridge on real GL memory.
//!
//! The unit tests in `gst::video_input_bridge` see GL memory only as caps
//! strings. This pushes `gltestsrc` frames, RGBA in GL memory as a GPU Vision
//! Mixer hands them to a WHEP Output, through the bridge and checks that the
//! peer receives NV12 in system memory, through `gldownload` and a converter,
//! and that GL memory arriving mid-stream, after the stream started in system
//! memory, is downloaded too.
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

/// The GL elements this test needs, and the rest. A missing GL element also
/// fails under `STROM_REQUIRE_GL`; a missing other one only under
/// `STROM_REQUIRE_GST_PLUGINS`, so the message names the right install.
const GL_REQUIRED: &[&str] = &["gltestsrc", "glcolorconvert", "gldownload"];
const REQUIRED: &[&str] = &["videoconvert", "fakesink"];
/// What the mid-stream switch adds: a system-memory source and the selector.
const SWITCH_REQUIRED: &[&str] = &["videotestsrc", "input-selector"];

const GL_RGBA: &str =
    "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1";

struct Outcome {
    format: String,
    gl_memory: bool,
    buffers: usize,
    /// Factories between the queue and the sink, in link order.
    spliced: Vec<String>,
}

const GL_NV12: &str =
    "video/x-raw(memory:GLMemory), format=NV12, width=320, height=240, framerate=30/1";

/// `gltestsrc ! glcolorconvert ! capsfilter(caps)`, then the bridge, then
/// `fakesink`. `fakesink` takes anything, GL memory included, as
/// whepserversink advertises it does.
fn run(caps: &str) -> Outcome {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("gltestsrc")
        .property("num-buffers", 15i32)
        .build()
        .expect("gltestsrc");
    let colorconvert = gst::ElementFactory::make("glcolorconvert")
        .build()
        .expect("glcolorconvert");
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps.parse::<gst::Caps>().expect("caps"))
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
        .add_many([&src, &colorconvert, &filter, &queue, &sink])
        .expect("add");
    gst::Element::link_many([&src, &colorconvert, &filter, &queue, &sink]).expect("link");

    install_video_input_bridge(
        &queue.static_pad("src").expect("queue src pad"),
        "video_queue",
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

    let spliced = spliced(&queue, &sink);

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
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }

    let outcome = run(GL_RGBA);
    assert_eq!(
        outcome.spliced,
        ["gldownload", "videoconvert", "capsfilter"],
        "expected a download, then the converter and the format pin"
    );
    assert_eq!(outcome.format, "NV12");
    assert!(!outcome.gl_memory, "the peer should receive system memory");
    assert!(outcome.buffers > 0, "no buffers reached the peer");
}

/// GL memory already in NV12 needs only the download. `gldownload` alone
/// passes GL memory through to a peer that advertises it, which
/// whepserversink does, so the download is pinned to system memory.
#[test]
fn gl_nv12_is_downloaded_to_system_memory() {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return;
    }

    let outcome = run(GL_NV12);
    assert_eq!(
        outcome.spliced,
        ["gldownload", "capsfilter"],
        "expected a download and its system-memory pin"
    );
    assert_eq!(outcome.format, "NV12");
    assert!(!outcome.gl_memory, "GL memory reached the peer");
    assert!(outcome.buffers > 0, "no buffers reached the peer");
}

/// A producer that starts in system memory and switches to GL memory
/// mid-stream: a Media Player moving on to a file the GPU decodes. The bridge
/// used to decide once, from the first caps, and remove its probe, so the GL
/// frames reached the sink as they were. `whepserversink` advertises GL memory
/// and then finds no encoder for it, so video went missing.
struct Switch {
    /// Factories spliced in before the switch, and after it.
    before: Vec<String>,
    after: Vec<String>,
    /// Whether frames kept reaching the peer after the switch.
    flowing: bool,
    /// What the peer negotiated after the switch.
    caps: Option<gst::Caps>,
}

/// `videotestsrc` in `system_format` and `gltestsrc` in GL RGBA into an
/// `input-selector`, then the bridge, then `fakesink`: plays the system input,
/// then switches to the GL one.
fn switch_to_gl(system_format: &str) -> Switch {
    let pipeline = gst::Pipeline::new();
    let system = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let system_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            format!(
                "video/x-raw, format={}, width=320, height=240, framerate=30/1",
                system_format
            )
            .parse::<gst::Caps>()
            .expect("caps"),
        )
        .build()
        .expect("capsfilter");
    let gl = gst::ElementFactory::make("gltestsrc")
        .property("is-live", true)
        .build()
        .expect("gltestsrc");
    let gl_caps = gst::ElementFactory::make("capsfilter")
        .property("caps", GL_RGBA.parse::<gst::Caps>().expect("caps"))
        .build()
        .expect("capsfilter");
    // The inactive input is dropped, not held back to the active one's
    // running time.
    let selector = gst::ElementFactory::make("input-selector")
        .property("sync-streams", false)
        .build()
        .expect("input-selector");
    let queue = gst::ElementFactory::make("queue")
        .name("video_queue")
        .build()
        .expect("queue");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([
            &system,
            &system_caps,
            &gl,
            &gl_caps,
            &selector,
            &queue,
            &sink,
        ])
        .expect("add");
    gst::Element::link_many([&system, &system_caps]).expect("link");
    gst::Element::link_many([&gl, &gl_caps]).expect("link");
    let system_pad = selector.request_pad_simple("sink_%u").expect("sink pad");
    let gl_pad = selector.request_pad_simple("sink_%u").expect("sink pad");
    system_caps
        .static_pad("src")
        .expect("src")
        .link(&system_pad)
        .expect("link");
    gl_caps
        .static_pad("src")
        .expect("src")
        .link(&gl_pad)
        .expect("link");
    gst::Element::link_many([&selector, &queue, &sink]).expect("link");
    selector.set_property("active-pad", &system_pad);

    install_video_input_bridge(&queue.static_pad("src").expect("queue src"), "video_queue");

    let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
    let buffers = Arc::new(AtomicUsize::new(0));
    let counter = buffers.clone();
    sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    let bus = pipeline.bus().expect("bus");
    let wait_for = |n: usize| {
        let target = buffers.load(Ordering::Relaxed) + n;
        let deadline = Instant::now() + Duration::from_secs(10);
        while buffers.load(Ordering::Relaxed) < target && Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(50),
                &[gst::MessageType::Error],
            ) {
                if let gst::MessageView::Error(e) = msg.view() {
                    let _ = pipeline.set_state(gst::State::Null);
                    panic!("pipeline error: {} ({:?})", e.error(), e.debug());
                }
            }
        }
        buffers.load(Ordering::Relaxed) >= target
    };

    pipeline.set_state(gst::State::Playing).expect("play");
    assert!(wait_for(5), "system-memory frames never reached the peer");
    let before = spliced(&queue, &sink);

    selector.set_property("active-pad", &gl_pad);
    let flowing = wait_for(10);
    let caps = sink_pad.current_caps();
    let after = spliced(&queue, &sink);
    pipeline.set_state(gst::State::Null).expect("null");
    Switch {
        before,
        after,
        flowing,
        caps,
    }
}

fn assert_downloaded(switch: &Switch) {
    assert!(
        switch.flowing,
        "frames stopped after the switch to GL memory"
    );
    let caps = switch.caps.as_ref().expect("negotiated caps");
    assert!(
        !caps
            .features(0)
            .is_some_and(|f| f.contains("memory:GLMemory")),
        "GL memory reached the peer: {}",
        caps
    );
    assert_eq!(switch.after, ["gldownload", "videoconvert", "capsfilter"]);
}

#[test]
fn a_switch_to_gl_memory_mid_stream_is_downloaded() {
    if !common::gl_available(GL_REQUIRED)
        || !common::plugins_available(REQUIRED)
        || !common::plugins_available(SWITCH_REQUIRED)
    {
        return;
    }
    let switch = switch_to_gl("NV12");
    assert!(
        switch.before.is_empty(),
        "NV12 in system memory needs nothing: {:?}",
        switch.before
    );
    assert_downloaded(&switch);
}

/// The same behind a converter already spliced in for RGBA, which cannot take
/// GL memory: left to answer for itself, it refused the GL caps and the switch
/// failed not-negotiated.
#[test]
fn a_switch_to_gl_memory_behind_a_converter_is_downloaded() {
    if !common::gl_available(GL_REQUIRED)
        || !common::plugins_available(REQUIRED)
        || !common::plugins_available(SWITCH_REQUIRED)
    {
        return;
    }
    let switch = switch_to_gl("RGBA");
    assert_eq!(switch.before, ["videoconvert", "capsfilter"]);
    assert_downloaded(&switch);
}

/// Factories between `queue` and `sink`, in link order.
fn spliced(queue: &gst::Element, sink: &gst::Element) -> Vec<String> {
    let mut spliced = Vec::new();
    let mut next = queue.static_pad("src").and_then(|pad| pad.peer());
    while let Some(pad) = next {
        let element = pad.parent_element().expect("linked pad has an element");
        if &element == sink {
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
    spliced
}
