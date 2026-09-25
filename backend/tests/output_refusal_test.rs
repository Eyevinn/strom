//! A refused input must fail the flow with the block's own reason (#840).
//!
//! Output blocks decide what to do with a stream in a caps probe on their input
//! `identity`. Before #840 a refusal was a log line: the input's src pad was
//! left unlinked, so upstream went `not-linked` and the flow's only error was
//! the source's `Internal data stream error`, or, where the block also dropped
//! the buffers (the recorder), there was no error at all and the track was
//! simply missing from the recording.
//!
//! Each test builds the real block, feeds it something it refuses, and asserts
//! on the bus: an error posted by the block's input that names the fix, and no
//! error from the test source. Reverting the fix fails both halves.
//!
//! RTMP is covered in `rtmp_output_pipeline_test.rs`, which keeps its sink out
//! of the pipeline for reasons explained there. TAMS Output is not covered: its
//! builder needs a TAMS server to register the flow with.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt::MpegTsSrtOutputBuilder;
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this file needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "capsfilter",
    "mpegtsmux",
    "srtsink",
    "splitmuxsink",
    "mp4mux",
];

/// The name every test source gets, so its errors can be told from the block's.
const TEST_SOURCE: &str = "test_source";

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    gst::init().expect("gst init");
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
    eprintln!(
        "skipping: missing GStreamer elements: {}",
        missing.join(", ")
    );
    false
}

fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

/// What to feed the block's input.
#[derive(Clone, Copy)]
enum Feed {
    RawVideo,
    RawAudio,
}

/// One error from the bus: the posting element's name and the error message.
#[derive(Debug)]
struct BusError {
    source: String,
    message: String,
}

/// Build `builder`'s block, feed `feed` into the element named `input`, run it
/// briefly and return every error the bus carried.
///
/// Applies the block's internal links and element-setup hooks the way the
/// pipeline manager does, so the probe under test runs in the graph that ships.
fn errors_after_feeding<B: BlockBuilder>(
    builder: B,
    instance: &str,
    props: HashMap<String, PropertyValue>,
    input: &str,
    feed: Feed,
) -> Vec<BusError> {
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = builder.build(instance, &props, &ctx).expect("block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = &by_id[&from.element_id];
        let sink = &by_id[&to.element_id];
        match (&from.pad_name, &to.pad_name) {
            (Some(src_pad), Some(sink_pad)) => src
                .link_pads(Some(src_pad.as_str()), sink, Some(sink_pad.as_str()))
                .expect("internal pad link"),
            _ => src.link(sink).expect("internal element link"),
        }
    }

    let input_id = format!("{}:{}", instance, input);
    let input_element = by_id
        .get(&input_id)
        .unwrap_or_else(|| panic!("block builds {}", input_id))
        .clone();
    let (src, caps) = match feed {
        Feed::RawVideo => (
            gst::ElementFactory::make("videotestsrc")
                .name(TEST_SOURCE)
                .property("num-buffers", 60i32)
                .property("is-live", true)
                .build()
                .expect("videotestsrc"),
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        ),
        Feed::RawAudio => (
            gst::ElementFactory::make("audiotestsrc")
                .name(TEST_SOURCE)
                .property("num-buffers", 60i32)
                .property("is-live", true)
                .build()
                .expect("audiotestsrc"),
            gst::Caps::builder("audio/x-raw").build(),
        ),
    };
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .expect("capsfilter");
    pipeline.add_many([&src, &filter]).expect("add source");
    gst::Element::link_many([&src, &filter, &input_element]).expect("link source to block");

    let flow_id = strom_types::flow::FlowId::new_v4();
    let events = EventBroadcaster::new(16);
    for setup in ctx.take_element_setups() {
        setup(flow_id, events.clone());
    }

    let bus = pipeline.bus().expect("pipeline bus");
    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    // Long enough for the source to have run into an unlinked pad many times
    // over, if nothing stopped it: 60 buffers is under three seconds at 25 fps.
    let mut errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Error],
        ) else {
            continue;
        };
        if let gst::MessageView::Error(err) = msg.view() {
            errors.push(BusError {
                source: msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                message: err.error().to_string(),
            });
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    errors
}

/// The refusal must come from the block's input and name the fix, and nothing
/// may follow it from upstream.
fn assert_refused_by_block(errors: &[BusError], input_id: &str, names_fix: &str) {
    let from_block: Vec<&BusError> = errors.iter().filter(|e| e.source == input_id).collect();
    assert_eq!(
        from_block.len(),
        1,
        "expected exactly one error from {}, got: {:?}",
        input_id,
        errors
    );
    assert!(
        from_block[0].message.contains(names_fix),
        "the refusal must name {}: {:?}",
        names_fix,
        from_block[0]
    );
    assert!(
        !errors.iter().any(|e| e.source == TEST_SOURCE),
        "the refused stream must be silenced, or upstream's not-linked error replaces \
         the block's reason as the flow's error: {:?}",
        errors
    );
}

fn mpegtssrt_props() -> HashMap<String, PropertyValue> {
    let mut props = HashMap::new();
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(0));
    // A listener nothing connects to; wait_for_connection=false keeps srtsink
    // from blocking.
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", srt_port())),
    );
    props.insert(
        "wait_for_connection".to_string(),
        PropertyValue::Bool(false),
    );
    props
}

fn recorder_props(media_root: &std::path::Path) -> HashMap<String, PropertyValue> {
    let mut props = HashMap::new();
    props.insert(
        "container".to_string(),
        PropertyValue::String("mp4".to_string()),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".to_string()),
    );
    props.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );
    props
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "strom-output-refusal-{}-{}",
        name,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn mpegtssrt_output_refuses_raw_video_by_failing_the_flow() {
    if !plugins_available() {
        return;
    }
    let errors = errors_after_feeding(
        MpegTsSrtOutputBuilder,
        "tsout",
        mpegtssrt_props(),
        "video_input",
        Feed::RawVideo,
    );
    assert_refused_by_block(&errors, "tsout:video_input", "builtin.videoenc");
}

#[test]
fn recorder_refuses_raw_video_by_failing_the_flow() {
    if !plugins_available() {
        return;
    }
    let dir = scratch_dir("video");
    let errors = errors_after_feeding(
        RecorderBuilder,
        "rec",
        recorder_props(&dir),
        "video_input_0",
        Feed::RawVideo,
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_refused_by_block(&errors, "rec:video_input_0", "builtin.videoenc");
}

#[test]
fn recorder_refuses_raw_audio_by_failing_the_flow() {
    if !plugins_available() {
        return;
    }
    let dir = scratch_dir("audio");
    let errors = errors_after_feeding(
        RecorderBuilder,
        "rec",
        recorder_props(&dir),
        "audio_input_0",
        Feed::RawAudio,
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_refused_by_block(&errors, "rec:audio_input_0", "builtin.audioenc");
}

#[cfg(feature = "efp")]
#[test]
fn efpsrt_output_refuses_raw_video_by_failing_the_flow() {
    use strom::blocks::builtin::efpsrt::EfpSrtOutputBuilder;
    if !plugins_available() {
        return;
    }
    // efpmux is a Rust plugin linked into Strom, not installed system-wide, so
    // it has to be registered before the block can make one.
    gst_plugin_efp::plugin_register_static().expect("register the EFP plugin");
    let mut props = mpegtssrt_props();
    props.insert("num_data_tracks".to_string(), PropertyValue::UInt(0));
    let errors = errors_after_feeding(
        EfpSrtOutputBuilder,
        "efpout",
        props,
        "video_input",
        Feed::RawVideo,
    );
    assert_refused_by_block(&errors, "efpout:video_input", "builtin.videoenc");
}
