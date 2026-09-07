//! A `splitmuxsink` only completes READY->PAUSED once it has prerolled a
//! buffer, so a recorder whose input carries no data holds the whole pipeline
//! short of PLAYING. A flow where only some inputs are live — remote presenters
//! who have not connected yet, each with their own recorder — puts recorders in
//! exactly that position, and the flow then does not run at all: the recorder
//! on the input that *is* live writes nothing either.
//!
//! The two tests are a pair: one asserts an input with no data does not block
//! PLAYING, the other that a recorder still records once data arrives. Keeping
//! the sink locked for good would satisfy the first and break recording.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "mp4mux",
    "x264enc",
    "h264parse",
    "videotestsrc",
    "appsrc",
    "fakesink",
    "queue",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

/// Build one video-only recorder block through the real builder and add it to
/// `pipeline`, returning its video input element and its build context. The
/// context's setup hooks must run after the input is linked — that is what
/// decides which tracks are connected and get a splitmuxsink pad.
fn add_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
) -> (gst::Element, BlockBuildContext) {
    let (input, _, ctx) = add_recorder_with_audio(pipeline, instance_id, media_root, 0);
    (input, ctx)
}

/// As `add_recorder`, with `num_audio_tracks` audio tracks; returns the first
/// audio input alongside the video one.
fn add_recorder_with_audio(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    num_audio_tracks: u64,
) -> (gst::Element, Option<gst::Element>, BlockBuildContext) {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("container".to_string(), PropertyValue::String("mp4".into()));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert(
        "num_audio_tracks".to_string(),
        PropertyValue::UInt(num_audio_tracks),
    );
    props.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".into()),
    );
    props.insert(
        "filename_prefix".to_string(),
        PropertyValue::String(instance_id.to_string()),
    );
    props.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RecorderBuilder
        .build(instance_id, &props, &ctx)
        .expect("recorder block builds");

    let mut input = None;
    let mut audio_input = None;
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        if id == &format!("{}:video_input_0", instance_id) {
            input = Some(element.clone());
        }
        if id == &format!("{}:audio_input_0", instance_id) {
            audio_input = Some(element.clone());
        }
    }
    (
        input.expect("recorder exposes video_input_0"),
        audio_input,
        ctx,
    )
}

/// Run a recorder's element-setup hooks, as the pipeline manager does after
/// linking and before PLAYING.
fn run_setups(ctx: &BlockBuildContext) {
    for setup in ctx.take_element_setups() {
        setup(uuid::Uuid::new_v4(), EventBroadcaster::new(16));
    }
}

/// H.264 into `target`, from a live source so the pipeline behaves like a flow.
fn feed_h264(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    let enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 10u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .expect("x264enc");

    pipeline.add_many([&src, &caps, &enc]).unwrap();
    gst::Element::link_many([&src, &caps, &enc]).unwrap();
    enc.link(target).expect("link encoder into recorder");
}

/// An input that stays connected but never carries data, like an encoder behind
/// a WHIP slot with no publisher.
fn feed_nothing(pipeline: &gst::Pipeline, target: &gst::Element) {
    let src = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .expect("appsrc");
    pipeline.add(&src).unwrap();
    src.link(target).expect("link silent source into recorder");
}

fn recordings(media_root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// One recorder with data, one without: the pipeline must reach PLAYING, and
/// the recorder that has data must write a file.
///
/// Without the fix the pipeline never leaves PAUSED, so neither recorder writes
/// anything — which is why an unfed input costs the recordings of every input
/// that *is* live, not just its own.
#[test]
fn recorder_without_data_does_not_block_playing() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();

    let pipeline = gst::Pipeline::new();
    let (live_input, live_ctx) = add_recorder(&pipeline, "rec_live", media_root);
    let (idle_input, idle_ctx) = add_recorder(&pipeline, "rec_idle", media_root);
    feed_h264(&pipeline, &live_input, 60);
    feed_nothing(&pipeline, &idle_input);
    run_setups(&live_ctx);
    run_setups(&idle_ctx);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(15));

    // Let the fed recorder write, then close it cleanly so the file is usable.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let files = recordings(media_root);
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert_eq!(
        (result.expect("pipeline state readable"), current, pending),
        (
            gst::StateChangeSuccess::Success,
            gst::State::Playing,
            gst::State::VoidPending
        ),
        "a recorder with no data held the pipeline out of PLAYING"
    );

    let live: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rec_live")
        })
        .collect();
    assert!(
        !live.is_empty(),
        "the recorder with data wrote no file; files present: {:?}",
        files
    );
    let bytes: u64 = live
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    assert!(bytes > 0, "the recorder with data wrote an empty file");

    // The idle recorder never got data, so it must not have written anything.
    let idle: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rec_idle")
        })
        .collect();
    assert!(
        idle.iter()
            .all(|p| p.metadata().map(|m| m.len()).unwrap_or(0) == 0),
        "the recorder with no data wrote content: {:?}",
        idle
    );
}

/// A recorder that gets data must still record it.
///
/// Counterpart to the test above: a sink kept out of the pipeline for good
/// would satisfy that one and stop every recording.
#[test]
fn recorder_with_data_still_records() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();

    let pipeline = gst::Pipeline::new();
    let (input, ctx) = add_recorder(&pipeline, "rec_live", media_root);
    feed_h264(&pipeline, &input, 60);
    run_setups(&ctx);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");

    let bus = pipeline.bus().expect("pipeline has a bus");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut reached_eos = false;
    while std::time::Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(1)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => {
                reached_eos = true;
                break;
            }
            gst::MessageView::Error(err) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                err.src().map(|s| s.path_string()),
                err.error(),
                err.debug()
            ),
            _ => {}
        }
    }
    let files = recordings(media_root);
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert!(reached_eos, "pipeline never reached EOS within 30s");
    assert!(!files.is_empty(), "no recording was written");
    let bytes: u64 = files
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    assert!(bytes > 0, "the recording is empty: {:?}", files);
}
