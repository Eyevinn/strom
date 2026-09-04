//! `splitmuxsink` releases a GOP only once every one of its sink pads has
//! advanced past it, so one track that stops delivering freezes the whole
//! recording — and, through the tee that feeds the recorder, every other branch
//! of that source with it. A participant whose microphone dies mid-session takes
//! their video and their recording down with it, and the program output too if
//! they were the only live source.
//!
//! EOS is what takes a pad out of that wait; a GAP event does not, splitmuxsink
//! ignores it on a non-reference stream. So the recorder ends the track rather
//! than trying to keep it idling.
//!
//! The two tests are a pair: one asserts that a stopped track does not freeze the
//! rest, the other that tracks which are still running are left alone. Ending
//! every track on a timer would satisfy the first and destroy every recording.
//! The first also pins down *which* track is ended: if the recorder ended the
//! video track — the one still delivering — no video would reach the muxer either.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "mp4mux",
    "x264enc",
    "h264parse",
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "avenc_aac",
    "aacparse",
    "identity",
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

/// Build a one-video, one-audio recorder through the real builder and add it to
/// `pipeline`, returning both input elements and the build context. The
/// context's setup hooks must run after the inputs are linked — that is what
/// decides which tracks are connected, get a splitmuxsink pad, and are watched.
fn add_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
) -> (gst::Element, gst::Element, BlockBuildContext) {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("container".to_string(), PropertyValue::String("mp4".into()));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
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

    let mut video = None;
    let mut audio = None;
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        if id == &format!("{}:video_input_0", instance_id) {
            video = Some(element.clone());
        }
        if id == &format!("{}:audio_input_0", instance_id) {
            audio = Some(element.clone());
        }
    }
    (
        video.expect("recorder exposes video_input_0"),
        audio.expect("recorder exposes audio_input_0"),
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

/// Swallow EOS on `element`'s src pad, so a source that runs out of buffers
/// looks like a track that simply stopped arriving. This is what the recorder
/// actually sees: EOS does not cross a WebRTC hop, so a publisher whose
/// microphone dies just stops sending RTP.
fn drop_eos(element: &gst::Element) {
    element.static_pad("src").expect("src pad").add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM,
        |_pad, info| match info.data.as_ref() {
            Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos => {
                gst::PadProbeReturn::Drop
            }
            _ => gst::PadProbeReturn::Ok,
        },
    );
}

/// Live H.264 into `target`. `num_buffers` of -1 runs until the pipeline stops.
fn feed_video(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", num_buffers)
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
    let gate = gst::ElementFactory::make("identity")
        .build()
        .expect("identity");
    drop_eos(&gate);

    pipeline.add_many([&src, &caps, &enc, &gate]).unwrap();
    gst::Element::link_many([&src, &caps, &enc, &gate]).unwrap();
    gate.link(target).expect("link video into recorder");
}

/// Live AAC into `target`. `num_buffers` of -1 runs until the pipeline stops.
fn feed_audio(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("num-buffers", num_buffers)
        .build()
        .expect("audiotestsrc");
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make("avenc_aac").build().unwrap();
    let gate = gst::ElementFactory::make("identity").build().unwrap();
    drop_eos(&gate);

    pipeline
        .add_many([&src, &conv, &resample, &enc, &gate])
        .unwrap();
    gst::Element::link_many([&src, &conv, &resample, &enc, &gate]).unwrap();
    gate.link(target).expect("link audio into recorder");
}

/// Buffers reaching splitmuxsink's primary video pad: what the muxer is
/// actually consuming, which is what stops when it is waiting on another pad.
fn count_into_muxer(pipeline: &gst::Pipeline, instance_id: &str, pad: &str) -> Arc<AtomicU64> {
    let counter = Arc::new(AtomicU64::new(0));
    let sink = pipeline
        .by_name(&format!("{}:splitmuxsink", instance_id))
        .expect("splitmuxsink in pipeline");
    let sink_pad = sink
        .static_pad(pad)
        .unwrap_or_else(|| panic!("splitmuxsink has a {} pad", pad));
    let c = Arc::clone(&counter);
    sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
        c.fetch_add(1, Ordering::Relaxed);
        gst::PadProbeReturn::Ok
    });
    counter
}

fn recorded_bytes(media_root: &Path) -> u64 {
    std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Video keeps arriving, audio stops after two seconds. The recording must go
/// on without the audio rather than freeze on it.
///
/// Reverting the fix pins the counted video at zero: with no watchdog to end the
/// audio track, splitmuxsink never releases another GOP.
#[test]
fn a_track_that_stops_does_not_freeze_the_recording() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();
    let pipeline = gst::Pipeline::new();
    let (video_in, audio_in, ctx) = add_recorder(&pipeline, "rec_stall", media_root);
    feed_video(&pipeline, &video_in, -1);
    feed_audio(&pipeline, &audio_in, 86); // ~2 s at 1024 samples / 44.1 kHz
    run_setups(&ctx);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let _ = pipeline.state(gst::ClockTime::from_seconds(15));
    let video_into_muxer = count_into_muxer(&pipeline, "rec_stall", "video");

    // Audio stops at ~2 s and the watchdog ends that track five seconds later.
    // Measure the window after that, so this is about recovery, not the stall.
    std::thread::sleep(std::time::Duration::from_secs(8));
    let (buffers_before, bytes_before) = (
        video_into_muxer.load(Ordering::Relaxed),
        recorded_bytes(media_root),
    );
    std::thread::sleep(std::time::Duration::from_secs(5));
    let (buffers_after, bytes_after) = (
        video_into_muxer.load(Ordering::Relaxed),
        recorded_bytes(media_root),
    );
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    // 30 fps over five seconds is 150 frames; anything above a couple of seconds
    // worth means the muxer is running rather than waiting on the dead track.
    assert!(
        buffers_after - buffers_before >= 45,
        "the recording froze on the track that stopped: {} video buffers reached the muxer in 5 s ({} bytes written)",
        buffers_after - buffers_before,
        bytes_after - bytes_before
    );
}

/// Every track still delivering: none of them may be ended.
///
/// Counterpart to the test above — ending tracks on a timer regardless of
/// whether they are live would satisfy that one and lose the audio of every
/// recording.
#[test]
fn tracks_that_keep_running_are_left_alone() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let media_root = tmp.path();
    let pipeline = gst::Pipeline::new();
    let (video_in, audio_in, ctx) = add_recorder(&pipeline, "rec_live", media_root);
    feed_video(&pipeline, &video_in, -1);
    feed_audio(&pipeline, &audio_in, -1);
    run_setups(&ctx);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let _ = pipeline.state(gst::ClockTime::from_seconds(15));
    let audio_into_muxer = count_into_muxer(&pipeline, "rec_live", "audio_0");
    let video_into_muxer = count_into_muxer(&pipeline, "rec_live", "video");

    // Well past the stall timeout, so a watchdog that ignored liveness has fired.
    std::thread::sleep(std::time::Duration::from_secs(8));
    let (audio_before, video_before) = (
        audio_into_muxer.load(Ordering::Relaxed),
        video_into_muxer.load(Ordering::Relaxed),
    );
    std::thread::sleep(std::time::Duration::from_secs(5));
    let (audio_after, video_after) = (
        audio_into_muxer.load(Ordering::Relaxed),
        video_into_muxer.load(Ordering::Relaxed),
    );
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert!(
        audio_after - audio_before >= 45,
        "the audio track was ended while it was still delivering: {} buffers reached the muxer in 5 s",
        audio_after - audio_before
    );
    assert!(
        video_after - video_before >= 45,
        "the video track was ended while it was still delivering: {} buffers reached the muxer in 5 s",
        video_after - video_before
    );
}
