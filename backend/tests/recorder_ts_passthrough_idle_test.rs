//! #750 kept an unfed recorder from holding the pipeline short of PLAYING, but
//! only on the `splitmuxsink` path. A `ts_passthrough` recorder builds a
//! `multifilesink` instead, which #750 left untouched — and a `GstBaseSink`
//! with `async` at its default still waits for a preroll buffer it will never
//! get, so one idle TS recorder stalls every other recorder in the flow.
//!
//! The two tests are a pair, mirroring `recorder_idle_input_test.rs`: one
//! asserts an input with no data does not block PLAYING, the other that a fed
//! recorder still writes. Locking the sink for good would satisfy the first and
//! break recording.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "multifilesink",
    "mpegtsmux",
    "x264enc",
    "h264parse",
    "videotestsrc",
    "appsrc",
    "identity",
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

/// Build one `ts_passthrough` recorder through the real builder, add it to
/// `pipeline` and return its TS input element.
fn add_ts_recorder(pipeline: &gst::Pipeline, instance_id: &str, media_root: &Path) -> gst::Element {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "container".to_string(),
        PropertyValue::String("ts_passthrough".into()),
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
        .expect("ts_passthrough recorder block builds");

    let mut input = None;
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        if id == &format!("{}:ts_input", instance_id) {
            input = Some(element.clone());
        }
    }

    // Make whatever links the block declares, as the pipeline manager would. The
    // fixed build declares none and links its sink from the caps probe instead, so
    // this loop is what keeps the test honest against the unfixed build too.
    for (from, to) in &built.internal_links {
        let src = pipeline
            .by_name(&from.element_id)
            .expect("internal link source element is in the pipeline");
        let dst = pipeline
            .by_name(&to.element_id)
            .expect("internal link sink element is in the pipeline");
        src.link(&dst).expect("internal recorder link");
    }

    input.expect("ts_passthrough recorder exposes ts_input")
}

/// MPEG-TS into `target`, from a live source so the pipeline behaves like a flow.
fn feed_mpegts(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 10u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .expect("x264enc");
    let parse = gst::ElementFactory::make("h264parse")
        .build()
        .expect("h264parse");
    let mux = gst::ElementFactory::make("mpegtsmux")
        .build()
        .expect("mpegtsmux");

    pipeline.add_many([&src, &enc, &parse, &mux]).unwrap();
    gst::Element::link_many([&src, &enc, &parse, &mux]).unwrap();
    mux.link(target).expect("link muxer into recorder");
}

/// An input that stays connected but never carries data, like a TS ingest slot
/// whose publisher has not connected yet.
fn feed_nothing(pipeline: &gst::Pipeline, target: &gst::Element) {
    let src = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .expect("appsrc");
    pipeline.add(&src).unwrap();
    src.link(target).expect("link silent source into recorder");
}

fn recordings(media_root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.retain(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().starts_with(prefix))
            .unwrap_or(false)
    });
    files.sort();
    files
}

/// One TS recorder with data, one without: the pipeline must reach PLAYING, and
/// the recorder that has data must write a file.
///
/// Without the fix the unfed `multifilesink` never prerolls, so the pipeline
/// stays in PAUSED and the *fed* recorder writes nothing either — which is why
/// one idle input costs the recordings of every input that is live.
#[test]
fn ts_passthrough_recorder_without_data_does_not_block_playing() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();

    let pipeline = gst::Pipeline::new();
    let live_input = add_ts_recorder(&pipeline, "ts_live", media_root);
    let idle_input = add_ts_recorder(&pipeline, "ts_idle", media_root);
    feed_mpegts(&pipeline, &live_input, 60);
    feed_nothing(&pipeline, &idle_input);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(15));

    // Let the fed recorder write before tearing the pipeline down.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let live = recordings(media_root, "ts_live");
    let idle = recordings(media_root, "ts_idle");
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
        "a ts_passthrough recorder with no data held the pipeline out of PLAYING"
    );

    let bytes: u64 = live
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    assert!(
        bytes > 0,
        "the TS recorder with data wrote nothing; files present: {:?}",
        live
    );

    // The idle recorder never got data, so it must not have written anything.
    assert!(
        idle.is_empty(),
        "the TS recorder with no data wrote files: {:?}",
        idle
    );
}

/// A TS recorder that gets data must still record it.
///
/// Counterpart to the test above: a sink kept out of the pipeline for good
/// would satisfy that one and stop every TS recording.
#[test]
fn ts_passthrough_recorder_with_data_still_records() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();

    let pipeline = gst::Pipeline::new();
    let input = add_ts_recorder(&pipeline, "ts_live", media_root);
    feed_mpegts(&pipeline, &input, 60);

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
    let files = recordings(media_root, "ts_live");
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert!(reached_eos, "pipeline never reached EOS within 30s");
    let bytes: u64 = files
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum();
    assert!(bytes > 0, "the TS recording is empty: {:?}", files);
}
