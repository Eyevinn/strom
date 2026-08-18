//! Regression test for recorder tracks that are configured but never fed.
//!
//! `splitmuxsink` only releases a completed GOP once *every* requested sink pad
//! has advanced to the start of the next GOP. A pad that is requested but never
//! receives data keeps its input running time at `GST_CLOCK_STIME_NONE`, so the
//! sink waits on it forever and writes nothing at all.
//!
//! The recorder therefore must not request a splitmuxsink pad for a track that
//! carries no data. These tests configure a recorder with both a video and an
//! audio track, feed only one of them, and assert that a non-empty file is still
//! written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "videotestsrc",
    "audiotestsrc",
];

fn plugins_available() -> bool {
    REQUIRED
        .iter()
        .all(|e| gst::ElementFactory::find(e).is_some())
}

fn container_available(container: &str) -> bool {
    let muxer = match container {
        "mkv" => "matroskamux",
        "mpegts" => "mpegtsmux",
        _ => "mp4mux",
    };
    gst::ElementFactory::find(muxer).is_some()
}

/// Which inputs the test actually connects to the recorder.
#[derive(Clone, Copy, PartialEq)]
enum Feed {
    VideoOnly,
    AudioOnly,
    Both,
}

/// Build a recorder block configured for one video and one audio track, wire up
/// only the inputs named by `feed`, run until EOS, and return the files written.
///
/// Both tracks are always configured — the point of the test is that configuring
/// a track the flow never connects must not stop the other track from recording.
fn run_recorder(container: &str, feed: Feed, media_root: &Path) -> Vec<PathBuf> {
    let instance_id = "rec";

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "container".to_string(),
        PropertyValue::String(container.to_string()),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".to_string()),
    );
    props.insert(
        "filename_prefix".to_string(),
        PropertyValue::String("test".to_string()),
    );
    props.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RecorderBuilder
        .build(instance_id, &props, &ctx)
        .expect("recorder block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }

    // Short, deterministic sources. 30 buffers at 30fps = 1s of video, which is
    // several GOPs at key-int-max=10 — enough for splitmuxsink to complete and
    // release at least one GOP if it is not stalled.
    if feed == Feed::VideoOnly || feed == Feed::Both {
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 30i32)
            .property_from_str("pattern", "smpte")
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

        let target = by_id
            .get(&format!("{}:video_input_0", instance_id))
            .expect("recorder exposes video_input_0");
        enc.link(target).expect("link video into recorder");
    }

    if feed == Feed::AudioOnly || feed == Feed::Both {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 50i32)
            .build()
            .expect("audiotestsrc");
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let resample = gst::ElementFactory::make("audioresample").build().unwrap();
        let enc = gst::ElementFactory::make("avenc_aac")
            .build()
            .expect("avenc_aac");

        pipeline.add_many([&src, &conv, &resample, &enc]).unwrap();
        gst::Element::link_many([&src, &conv, &resample, &enc]).unwrap();

        let target = by_id
            .get(&format!("{}:audio_input_0", instance_id))
            .expect("recorder exposes audio_input_0");
        enc.link(target).expect("link audio into recorder");
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    let bus = pipeline.bus().expect("pipeline has a bus");
    let mut reached_eos = false;
    // 30 s is generous for ~1 s of content; a stalled splitmuxsink burns all of it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(1)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => {
                reached_eos = true;
                break;
            }
            gst::MessageView::Error(err) => {
                panic!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        reached_eos,
        "pipeline never reached EOS within 30s (container={}, feed connected={})",
        container,
        match feed {
            Feed::VideoOnly => "video only",
            Feed::AudioOnly => "audio only",
            Feed::Both => "video + audio",
        }
    );

    let dir = media_root.join("recordings");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
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

fn assert_recorded(container: &str, feed: Feed, label: &str) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let files = run_recorder(container, feed, tmp.path());

    assert!(
        !files.is_empty(),
        "{} / {}: splitmuxsink wrote no file at all",
        container,
        label
    );
    for f in &files {
        let len = std::fs::metadata(f).expect("stat recording").len();
        assert!(
            len > 0,
            "{} / {}: recording {} is empty",
            container,
            label,
            f.display()
        );
    }
}

/// The regression: an audio track is configured but the flow connects only video.
/// Before the fix, splitmuxsink waited forever on the unfed audio pad and no file
/// was ever written.
#[test]
fn video_only_feed_records_despite_configured_audio_track() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements not installed");
        return;
    }
    for container in ["mp4", "mkv", "mpegts"] {
        if !container_available(container) {
            eprintln!("skipping container {}: muxer not installed", container);
            continue;
        }
        assert_recorded(container, Feed::VideoOnly, "video only");
    }
}

/// The mirror case: a video track is configured but the flow connects only audio.
#[test]
fn audio_only_feed_records_despite_configured_video_track() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements not installed");
        return;
    }
    for container in ["mp4", "mkv", "mpegts"] {
        if !container_available(container) {
            eprintln!("skipping container {}: muxer not installed", container);
            continue;
        }
        assert_recorded(container, Feed::AudioOnly, "audio only");
    }
}

/// Control arm: both configured tracks are connected. This is the case that
/// already worked, and it must keep working.
#[test]
fn both_tracks_fed_records_normally() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements not installed");
        return;
    }
    for container in ["mp4", "mkv", "mpegts"] {
        if !container_available(container) {
            eprintln!("skipping container {}: muxer not installed", container);
            continue;
        }
        assert_recorded(container, Feed::Both, "video + audio");
    }
}
