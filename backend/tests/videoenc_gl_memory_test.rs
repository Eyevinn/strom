//! `builtin.videoenc` fed raw video in GL memory, at runtime.
//!
//! The block's converter is a `videoconvert`, whose sink template is
//! `video/x-raw(ANY)`: it accepts GL memory and then cannot hand it to the
//! encoder, so the stream stops `not-negotiated`. On macOS this stops an
//! MPEG-TS/SRT input that feeds a Video Encoder, intermittently: `decodebin`
//! can answer the autoplug-query of `vtdechw` before its pad is linked, so the
//! decoder settles on GL memory without asking the encoder, and its CAPS event
//! then reaches the block.
//!
//! These tests build the real block through `VideoEncBuilder`, feed it GL
//! frames from `gltestsrc`, and require encoded H.264 out of it, through a
//! `gldownload` the block put in. The decoder's choice is emulated: a probe on
//! the queue in front of the block answers the producer's caps queries with
//! whatever it asks for, as `decodebin` does for an unlinked pad. `vtdec`
//! itself only runs on macOS.
//!
//! It needs a GL context. On a Linux host with no display, which is CI,
//! `common::init_gl` asks for a surfaceless EGL context, which Mesa's software
//! rasteriser provides. Not built on Windows, whose CI runner has no OpenGL
//! driver GStreamer can use.

#![cfg(not(target_os = "windows"))]

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

const INSTANCE: &str = "venc0";

const GL_REQUIRED: &[&str] = &["gltestsrc", "gldownload"];
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "identity",
    "queue",
    "capsfilter",
    "videoconvert",
    "x264enc",
    "h264parse",
    "fakesink",
];

const GL_RGBA: &str =
    "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1";
const SYSTEM_RGBA: &str = "video/x-raw, format=RGBA, width=320, height=240, framerate=30/1";
const SYSTEM_NV12: &str = "video/x-raw, format=NV12, width=320, height=240, framerate=30/1";

fn available() -> bool {
    if !common::gl_available(GL_REQUIRED) || !common::plugins_available(REQUIRED) {
        return false;
    }
    // `VideoEncBuilder::build` reads the process-global video convert mode.
    strom::gpu::detect_gpu_capabilities();
    true
}

/// How the producer settles its caps.
#[derive(Clone, Copy)]
enum Producer {
    /// It asks the block, as any element linked to it does.
    Asks,
    /// Nothing downstream constrains it: the queue in front of the block
    /// answers every caps query with whatever is asked and accepts any caps,
    /// as `decodebin` does for a decoder whose pad is not linked yet. Its
    /// CAPS event then reaches the block without the block having been asked.
    DecidesAlone,
}

struct Outcome {
    /// Buffers out of the block.
    encoded: usize,
    /// Media type on the block's output.
    media: Option<String>,
    /// `gldownload` elements in the pipeline.
    downloads: usize,
}

/// Build the videoenc block into `pipeline` and return its declared input and
/// output pads.
fn add_block(pipeline: &gst::Pipeline) -> (gst::Pad, gst::Pad) {
    let mut props = HashMap::new();
    props.insert(
        "codec".to_string(),
        PropertyValue::String("h264".to_string()),
    );
    // The same encoder on every host.
    props.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".to_string()),
    );
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = VideoEncBuilder
        .build(INSTANCE, &props, &ctx)
        .expect("videoenc block builds");

    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        by_id[&from.element_id]
            .link(&by_id[&to.element_id])
            .expect("internal link");
    }
    // The block's declared external pads, not the element names they point at
    // today.
    let definition = strom::blocks::builtin::videoenc::get_blocks()
        .into_iter()
        .next()
        .expect("videoenc definition");
    let input = &definition.external_pads.inputs[0];
    let output = &definition.external_pads.outputs[0];
    let block_in = by_id[&format!("{}:{}", INSTANCE, input.internal_element_id)]
        .static_pad(&input.internal_pad_name)
        .expect("block input pad");
    let block_out = by_id[&format!("{}:{}", INSTANCE, output.internal_element_id)]
        .static_pad(&output.internal_pad_name)
        .expect("block output pad");
    (block_in, block_out)
}

/// Answer every caps query on `queue`'s sink pad with whatever is asked, and
/// accept any caps: [`Producer::DecidesAlone`].
fn answer_any_caps(queue: &gst::Element) {
    queue.static_pad("sink").expect("queue sink pad").add_probe(
        gst::PadProbeType::QUERY_DOWNSTREAM,
        |_, info| {
            let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };
            match query.view_mut() {
                gst::QueryViewMut::Caps(q) => {
                    q.set_result(&q.filter_owned().unwrap_or_else(gst::Caps::new_any));
                    gst::PadProbeReturn::Handled
                }
                gst::QueryViewMut::AcceptCaps(q) => {
                    q.set_result(true);
                    gst::PadProbeReturn::Handled
                }
                _ => gst::PadProbeReturn::Ok,
            }
        },
    );
}

fn count_downloads(pipeline: &gst::Pipeline) -> usize {
    pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.factory().is_some_and(|f| f.name() == "gldownload"))
        .count()
}

/// `<source> ! capsfilter(caps) ! queue ! [videoenc block] ! fakesink`, run to
/// EOS. The queue stands in for the recorder's queue behind a tee.
fn run(source: &str, caps: &str, producer: Producer) -> Outcome {
    let pipeline = gst::Pipeline::new();
    let (block_in, block_out) = add_block(&pipeline);

    let src = gst::ElementFactory::make(source)
        .property("num-buffers", 30i32)
        .build()
        .expect("source");
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps.parse::<gst::Caps>().expect("caps"))
        .build()
        .expect("capsfilter");
    let queue = gst::ElementFactory::make("queue").build().expect("queue");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([&src, &filter, &queue, &sink])
        .expect("add");
    if let Producer::DecidesAlone = producer {
        answer_any_caps(&queue);
    }

    queue
        .static_pad("src")
        .expect("queue src pad")
        .link(&block_in)
        .expect("link into the block");
    block_out
        .link(&sink.static_pad("sink").expect("fakesink sink pad"))
        .expect("link out of the block");
    // Last, as in a flow: the block is linked in before decodebin exposes a pad.
    gst::Element::link_many([&src, &filter, &queue]).expect("link producer");

    let encoded = Arc::new(AtomicUsize::new(0));
    let counter = encoded.clone();
    block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).expect("play");
    let bus = pipeline.bus().expect("bus");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut error = None;
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(e) => {
                error = Some(format!("{} ({:?})", e.error(), e.debug()));
                break;
            }
            _ => {}
        }
    }

    let media = block_out
        .current_caps()
        .and_then(|c| c.structure(0).map(|s| s.name().to_string()));
    let downloads = count_downloads(&pipeline);
    pipeline.set_state(gst::State::Null).expect("null");

    if let Some(error) = error {
        panic!("pipeline error: {}", error);
    }
    Outcome {
        encoded: encoded.load(Ordering::Relaxed),
        media,
        downloads,
    }
}

/// The macOS failure: the decoder settles on GL memory without asking the
/// block, and its CAPS event arrives anyway.
#[test]
fn gl_memory_decided_without_asking_is_encoded() {
    if !available() {
        return;
    }
    let outcome = run("gltestsrc", GL_RGBA, Producer::DecidesAlone);
    assert_eq!(outcome.media.as_deref(), Some("video/x-h264"));
    assert!(outcome.encoded > 0, "no encoded buffers out of the block");
    assert_eq!(outcome.downloads, 1, "expected one gldownload");
}

/// System memory goes straight to the converter, with no download.
#[test]
fn system_memory_is_not_downloaded() {
    if !available() {
        return;
    }
    let outcome = run("videotestsrc", SYSTEM_RGBA, Producer::Asks);
    assert_eq!(outcome.media.as_deref(), Some("video/x-h264"));
    assert!(outcome.encoded > 0, "no encoded buffers out of the block");
    assert_eq!(outcome.downloads, 0, "system memory was given a gldownload");
}

/// A caller that reconnects through a decoder that this time picks system
/// memory: the download spliced for the first caller stays in the path, and
/// the new caps must still reach the encoder through it.
///
/// `gltestsrc` and `videotestsrc` (NV12, as `vtdec` emits in system memory)
/// into an `input-selector`, live, switched from GL to system mid-stream. The
/// selector re-sends the new input's CAPS event, as a reconnect does.
#[test]
fn system_memory_after_a_download_is_encoded() {
    if !available() || !common::plugins_available(&["input-selector"]) {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let (block_in, block_out) = add_block(&pipeline);

    let gl = gst::ElementFactory::make("gltestsrc")
        .property("is-live", true)
        .build()
        .expect("gltestsrc");
    let gl_caps = gst::ElementFactory::make("capsfilter")
        .property("caps", GL_RGBA.parse::<gst::Caps>().expect("caps"))
        .build()
        .expect("capsfilter");
    let system = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let system_caps = gst::ElementFactory::make("capsfilter")
        .property("caps", SYSTEM_NV12.parse::<gst::Caps>().expect("caps"))
        .build()
        .expect("capsfilter");
    // The inactive input is dropped, not held back to the active one's
    // running time.
    let selector = gst::ElementFactory::make("input-selector")
        .property("sync-streams", false)
        .build()
        .expect("input-selector");
    let queue = gst::ElementFactory::make("queue").build().expect("queue");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([
            &gl,
            &gl_caps,
            &system,
            &system_caps,
            &selector,
            &queue,
            &sink,
        ])
        .expect("add");
    gst::Element::link_many([&gl, &gl_caps]).expect("link");
    gst::Element::link_many([&system, &system_caps]).expect("link");
    let gl_pad = selector.request_pad_simple("sink_%u").expect("sink pad");
    let system_pad = selector.request_pad_simple("sink_%u").expect("sink pad");
    gl_caps
        .static_pad("src")
        .expect("src")
        .link(&gl_pad)
        .expect("link");
    system_caps
        .static_pad("src")
        .expect("src")
        .link(&system_pad)
        .expect("link");
    selector.set_property("active-pad", &gl_pad);
    answer_any_caps(&queue);

    queue
        .static_pad("src")
        .expect("queue src pad")
        .link(&block_in)
        .expect("link into the block");
    block_out
        .link(&sink.static_pad("sink").expect("fakesink sink pad"))
        .expect("link out of the block");
    gst::Element::link_many([&selector, &queue]).expect("link producer");

    let encoded = Arc::new(AtomicUsize::new(0));
    let counter = encoded.clone();
    block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });

    let bus = pipeline.bus().expect("bus");
    let wait_for = |n: usize| {
        let target = encoded.load(Ordering::Relaxed) + n;
        let deadline = Instant::now() + Duration::from_secs(10);
        while encoded.load(Ordering::Relaxed) < target && Instant::now() < deadline {
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
        encoded.load(Ordering::Relaxed) >= target
    };

    pipeline.set_state(gst::State::Playing).expect("play");
    let gl_flowing = wait_for(5);
    let downloads = count_downloads(&pipeline);

    selector.set_property("active-pad", &system_pad);
    // Wait out frames already past the selector, then require new ones.
    let system_flowing = wait_for(30);
    let input_caps = block_in.current_caps();
    let downloads_after = count_downloads(&pipeline);
    pipeline.set_state(gst::State::Null).expect("null");

    assert!(gl_flowing, "GL-memory frames were never encoded");
    assert_eq!(downloads, 1, "expected one gldownload for GL memory");
    let input_caps = input_caps.expect("block input caps");
    assert!(
        !input_caps
            .features(0)
            .is_some_and(|f| f.contains("memory:GLMemory")),
        "the switch to system memory never reached the block: {}",
        input_caps
    );
    assert!(
        system_flowing,
        "frames stopped after the switch to system memory"
    );
    assert_eq!(downloads_after, 1, "the download was not kept");
}
