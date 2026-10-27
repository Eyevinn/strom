//! `builtin.local_input` emits video in the memory its source delivers (#969).
//!
//! The block used to end its video branch in a converter chosen from the
//! global video convert mode (`autovideoconvert` or `videoconvert`) and a
//! `video/x-raw` capsfilter, which pins system memory. A camera that delivers
//! GL memory (an AVFoundation camera behind `avfvideosrc` prefers it) was
//! downloaded inside the producer, whatever came next. A block emits what it
//! naturally produces and each consumer adapts its own input
//! (BLOCK_GUIDELINES), so the capture front is now the block's output.
//!
//! The block always builds its own source, so these build it through
//! `LocalInputBuilder`, leave the source out, and feed the capture front from
//! a test source instead. The GL case runs `gltestsrc` on the GL runner
//! (surfaceless EGL on Linux).

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strom::blocks::builtin::devicesrc::LocalInputBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const INSTANCE: &str = "li";

const REQUIRED: &[&str] = &["capsfilter", "videotestsrc", "fakesink"];

/// `producer` (a source and its caps) into the block's capture front, the
/// block's declared video output into a `fakesink`, run to EOS. Returns the
/// frame count out of the block, its output caps and the factories in the
/// pipeline.
fn run(producer: &str, caps: &str) -> (u64, gst::Caps, Vec<String>) {
    // A reverted block reads the global mode, which panics until it is set.
    let _ = strom::gpu::force_video_convert_mode(strom::gpu::VideoConvertMode::GpuAccelerated);
    let props = HashMap::from([
        (
            "stream_mode".to_string(),
            PropertyValue::String("video".into()),
        ),
        (
            "video_resolution".to_string(),
            PropertyValue::String("320x240".into()),
        ),
        (
            "video_framerate".to_string(),
            PropertyValue::String("30/1".into()),
        ),
    ]);
    let built = LocalInputBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("local input block builds");
    let pipeline = gst::Pipeline::new();
    let source_id = format!("{}:videosrc", INSTANCE);
    let by_id = common::block::install_except(&pipeline, &built, &[source_id.as_str()]);

    let pads = LocalInputBuilder
        .get_external_pads(&props)
        .expect("local input pads");
    let video_out = pads
        .outputs
        .iter()
        .find(|p| p.name == "video_out")
        .expect("a video output");
    let block_out = by_id[&format!("{}:{}", INSTANCE, video_out.internal_element_id)]
        .static_pad(&video_out.internal_pad_name)
        .expect("block output pad");
    let capture_front = by_id[&format!("{}:videosrc_caps", INSTANCE)]
        .static_pad("sink")
        .expect("capture front sink pad");

    let src = gst::ElementFactory::make(producer)
        .property("num-buffers", 30i32)
        .build()
        .unwrap();
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps.parse::<gst::Caps>().unwrap())
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline.add_many([&src, &filter, &sink]).unwrap();
    src.link(&filter).expect("link the test source");
    filter
        .static_pad("src")
        .unwrap()
        .link(&capture_front)
        .expect("link the test source into the capture front");
    block_out
        .link(&sink.static_pad("sink").unwrap())
        .expect("link out of the block");

    let frames = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&frames);
    block_out.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        counter.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    pipeline.set_state(gst::State::Playing).unwrap();
    let msg = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("EOS or an error");
    let factories: Vec<String> = pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.factory().map(|f| f.name().to_string()))
        .collect();
    let out_caps = block_out.current_caps();
    pipeline.set_state(gst::State::Null).unwrap();
    if let gst::MessageView::Error(e) = msg.view() {
        panic!("pipeline error: {} ({:?})", e.error(), e.debug());
    }
    (
        frames.load(Ordering::Relaxed),
        out_caps.expect("the block's output caps"),
        factories,
    )
}

fn has(factories: &[String], factory: &str) -> bool {
    factories.iter().any(|f| f == factory)
}

/// A source that delivers GL memory: the block emits GL memory, with nothing
/// converting or downloading inside it.
#[cfg(not(target_os = "windows"))]
#[test]
#[serial_test::serial(gl)]
fn gl_memory_from_the_source_leaves_the_block_as_gl_memory() {
    if !common::gl_available(&["gltestsrc"]) || !common::plugins_available(REQUIRED) {
        return;
    }
    let (frames, caps, factories) = run(
        "gltestsrc",
        "video/x-raw(memory:GLMemory), format=RGBA, width=320, height=240, framerate=30/1",
    );
    assert!(frames > 0, "no frames out of the block");
    assert!(
        caps.features(0)
            .is_some_and(|f| f.contains(strom::gst::video_adapt::GL_MEMORY_FEATURE)),
        "the block emitted {caps}, not the GL memory its source delivers"
    );
    for factory in ["gldownload", "videoconvert", "autovideoconvert"] {
        assert!(
            !has(&factories, factory),
            "the block put {factory} after its source: {factories:?}"
        );
    }
}

/// A source that delivers system memory: the block emits it as it is, at the
/// configured size and framerate.
#[test]
fn system_memory_from_the_source_leaves_the_block_unchanged() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let (frames, caps, factories) = run(
        "videotestsrc",
        "video/x-raw, format=UYVY, width=320, height=240, framerate=30/1",
    );
    assert!(frames > 0, "no frames out of the block");
    let s = caps.structure(0).expect("caps structure");
    assert_eq!(s.get::<&str>("format").ok(), Some("UYVY"), "{caps}");
    assert_eq!(s.get::<i32>("width").ok(), Some(320), "{caps}");
    assert!(
        !has(&factories, "videoconvert") && !has(&factories, "autovideoconvert"),
        "the block converts its source's video: {factories:?}"
    );
}
