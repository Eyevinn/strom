//! Regression tests for how the recorder hands out `splitmuxsink` sink pads.
//!
//! `splitmuxsink` releases a GOP only once every requested pad has reached the
//! next GOP, so a pad that is never fed makes it write nothing at all. An
//! unconnected track must not get one — but every connected track must, before
//! any data flows. Requesting lazily from the caps probes fails that second way:
//! the muxer starts on the first track's data, after which mp4mux refuses the
//! second pad and matroskamux grants it but drops the data.
//!
//! These tests assert on the streams inside the file, not its size: a dropped
//! track still leaves a large, valid, single-stream recording.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder, BlockRegistry};
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
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
    // Used to read the recordings back and check which streams they contain.
    "filesrc",
    "fakesink",
    "qtdemux",
    "matroskademux",
    "tsdemux",
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

    // The pipeline manager runs these after linking, before PLAYING. The hook is what
    // decides which tracks are connected, so skipping it would exercise nothing.
    for setup in ctx.take_element_setups() {
        setup(uuid::Uuid::new_v4(), EventBroadcaster::new(16));
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

/// Demux a recording and report which media kinds it contains, as
/// `(has_video, has_audio)`.
fn stream_kinds(path: &Path, container: &str) -> (bool, bool) {
    let demux_factory = match container {
        "mkv" => "matroskademux",
        "mpegts" => "tsdemux",
        _ => "qtdemux",
    };

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_string_lossy().to_string())
        .build()
        .expect("filesrc");
    let demux = gst::ElementFactory::make(demux_factory)
        .build()
        .unwrap_or_else(|_| panic!("{} available", demux_factory));
    pipeline.add_many([&src, &demux]).unwrap();
    src.link(&demux).expect("filesrc -> demux");

    let found = std::sync::Arc::new(std::sync::Mutex::new((false, false)));
    let found_for_cb = std::sync::Arc::clone(&found);
    let pipeline_weak = pipeline.downgrade();
    demux.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let media = pad
            .current_caps()
            .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
            .unwrap_or_default();
        {
            let mut f = found_for_cb.lock().unwrap();
            if media.starts_with("video/") {
                f.0 = true;
            } else if media.starts_with("audio/") {
                f.1 = true;
            }
        }
        // Drain the branch so the file plays through to EOS.
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink");
        pipeline.add(&sink).expect("add fakesink");
        sink.sync_state_with_parent().expect("sync fakesink");
        let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
        let _ = pad.link(&sink_pad);
    });

    // no-more-pads means all streams are known. EOS would mean playing the file
    // through, and does not reliably arrive for every recording these tests produce.
    let all_pads_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let all_pads_seen_for_cb = std::sync::Arc::clone(&all_pads_seen);
    demux.connect_no_more_pads(move |_| {
        all_pads_seen_for_cb.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    pipeline
        .set_state(gst::State::Playing)
        .expect("demux plays");
    let bus = pipeline.bus().expect("demux bus");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if all_pads_seen.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(err) => {
                panic!("demuxing {} failed: {}", path.display(), err.error());
            }
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();

    let f = *found.lock().unwrap();
    f
}

/// Assert the recording exists, is non-empty, and contains exactly the streams the
/// connected inputs should have produced.
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

    let expect_video = feed == Feed::VideoOnly || feed == Feed::Both;
    let expect_audio = feed == Feed::AudioOnly || feed == Feed::Both;
    let (has_video, has_audio) = stream_kinds(&files[0], container);

    assert_eq!(
        has_video,
        expect_video,
        "{} / {}: recording {} video stream (file has video={}, audio={})",
        container,
        label,
        if expect_video {
            "is missing its"
        } else {
            "has an unexpected"
        },
        has_video,
        has_audio
    );
    assert_eq!(
        has_audio,
        expect_audio,
        "{} / {}: recording {} audio stream (file has video={}, audio={})",
        container,
        label,
        if expect_audio {
            "is missing its"
        } else {
            "has an unexpected"
        },
        has_video,
        has_audio
    );
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

/// Both tracks connected, so both must end up in the file.
///
/// Catches pads requested too late: `avenc_aac` negotiates caps before `x264enc`, so
/// a recorder that requests on the caps event loses the video track. Repeated because
/// that failure is a race — 11 of 12 runs for mp4, so one green run proves nothing.
#[test]
fn both_tracks_fed_records_both_streams() {
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
        for attempt in 1..=3 {
            assert_recorded(
                container,
                Feed::Both,
                &format!("video + audio, run {}", attempt),
            );
        }
    }
}

/// Drive a recorder through the real `PipelineManager` start path.
///
/// The tests above run the hook themselves, so they would still pass if `start()`
/// stopped running it at the right moment. This one does not.
///
/// Measured by making each change and rerunning: moving the hook before the linking
/// pass fails this deterministically; moving it after `set_state(Playing)` does not,
/// because `set_state` returns before the encoders negotiate caps. Nothing guards
/// that second direction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorder_records_when_driven_through_pipeline_start() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements not installed");
        return;
    }
    if !container_available("mp4") {
        eprintln!("skipping: mp4mux not installed");
        return;
    }

    let media_root = tempfile::tempdir().expect("tempdir");
    let registry_file = tempfile::NamedTempFile::new().expect("registry file");
    let registry = BlockRegistry::new(registry_file.path());

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
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
        "filename_prefix".to_string(),
        PropertyValue::String("viaflow".to_string()),
    );

    let mut flow = strom_types::Flow::new("recorder_start_path");

    flow.elements.push(strom_types::Element {
        id: "vsrc".to_string(),
        element_type: "videotestsrc".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert("num-buffers".to_string(), PropertyValue::Int(30));
            p
        },
        position: [100.0, 100.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.elements.push(strom_types::Element {
        id: "venc".to_string(),
        element_type: "x264enc".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert("key-int-max".to_string(), PropertyValue::UInt(10));
            p
        },
        position: [250.0, 100.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.blocks.push(strom_types::BlockInstance {
        id: "rec".to_string(),
        block_definition_id: "builtin.recorder".to_string(),
        name: None,
        properties: props.clone(),
        position: strom_types::block::Position { x: 400.0, y: 100.0 },
        runtime_data: None,
        // From the builder, so the links below do not depend on registry state.
        computed_external_pads: RecorderBuilder.get_external_pads(&props),
    });

    // Both tracks connected: the configuration that loses the caps race if pads are late.
    flow.elements.push(strom_types::Element {
        id: "asrc".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: {
            let mut p = HashMap::new();
            p.insert("num-buffers".to_string(), PropertyValue::Int(50));
            p
        },
        position: [100.0, 250.0].into(),
        pad_properties: HashMap::new(),
    });
    flow.elements.push(strom_types::Element {
        id: "aenc".to_string(),
        element_type: "avenc_aac".to_string(),
        properties: HashMap::new(),
        position: [250.0, 250.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(strom_types::Link {
        from: "vsrc:src".to_string(),
        to: "venc:sink".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: "venc:src".to_string(),
        to: "rec:video_in_0".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: "asrc:src".to_string(),
        to: "aenc:sink".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: "aenc:src".to_string(),
        to: "rec:audio_in_0".to_string(),
    });

    // The bus watch is a glib signal watch, so it only dispatches while a main loop
    // runs. The real app has one; a test does not.
    let main_loop = gst::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };

    let events = EventBroadcaster::new(16);
    let mut event_rx = events.subscribe();

    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_root.path().to_path_buf(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("PipelineManager builds");

    manager.start().expect("pipeline starts");

    // Both sources have num-buffers, so the flow ends on its own. splitmuxsink only
    // finalizes the file on EOS, and an unfinalized mp4 has no moov to demux.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match event_rx.recv().await {
                Ok(strom_types::StromEvent::PipelineEos { .. }) => break,
                Ok(_) => continue,
                Err(e) => panic!("event stream ended before EOS: {e}"),
            }
        }
    })
    .await
    .expect("pipeline reached EOS within 30s — a splitmuxsink pad for a connected track was not requested in time");

    // Shut down before reading the file back.
    manager.stop().expect("pipeline stops");
    main_loop.quit();
    main_loop_thread.join().expect("main loop thread joins");

    let dir = media_root.path().join("recordings");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("recordings directory exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let recorded = files.into_iter().next();

    let recorded = recorded.expect("splitmuxsink wrote no file at all");
    let (has_video, has_audio) = stream_kinds(&recorded, "mp4");
    assert!(has_video, "recording has no video stream");
    assert!(has_audio, "recording has no audio stream");
}
