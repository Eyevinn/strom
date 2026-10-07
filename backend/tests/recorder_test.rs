//! Regression tests for the recorder block, each built through the real
//! `RecorderBuilder` and fed from test sources the way a flow would feed it.
//!
//! One module per defect, each with the counterpart that keeps its fix from
//! being satisfied by breaking recording:
//! - `idle_input`: an unfed `splitmuxsink` recorder must not hold the pipeline
//!   short of PLAYING.
//! - `ts_passthrough_idle`: the same for the `multifilesink` of `ts_passthrough`.
//! - `unfed_track`: `splitmuxsink` pads go to connected tracks only, and to
//!   every connected track before data flows.
//! - `stalled_track`, in `recorder_stall_test`: a track that stops delivering
//!   must not freeze the rest.
//! - `track_resume`: a track ended that way is recorded again once it resumes.
//! - `splitmux_threading`: one upstream streaming task feeding every track must
//!   not deadlock `splitmuxsink`.

pub mod common;
#[path = "common/recorder.rs"]
pub mod recorder;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuilder, PreStopFn};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use recorder::*;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Live H.264 into `target`.
fn feed_video(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    video_source(pipeline, num_buffers, true)
        .link(target)
        .expect("link video into recorder");
}

/// An input that stays connected but never carries data, like an encoder behind
/// a WHIP slot with no publisher, or a TS ingest slot nobody has connected to.
fn feed_nothing(pipeline: &gst::Pipeline, target: &gst::Element) {
    let src = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .expect("appsrc");
    pipeline.add(&src).unwrap();
    src.link(target).expect("link silent source into recorder");
}

/// Wait up to `timeout` for EOS on the pipeline bus. Panics on a pipeline
/// error; returns false if the time runs out first.
fn wait_for_eos(pipeline: &gst::Pipeline, timeout: Duration) -> bool {
    let bus = pipeline.bus().expect("pipeline has a bus");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => return true,
            gst::MessageView::Error(err) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                err.src().map(|s| s.path_string()),
                err.error(),
                err.debug()
            ),
            _ => {}
        }
    }
    false
}

/// Run a pipeline holding recorders with data and without: it must reach
/// PLAYING, then record for three seconds. Returns the state it reached.
fn play_for_three_seconds(
    pipeline: &gst::Pipeline,
) -> (
    Result<gst::StateChangeSuccess, gst::StateChangeError>,
    gst::State,
    gst::State,
) {
    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let state = pipeline.state(gst::ClockTime::from_seconds(15));
    // Let the fed recorder write before tearing the pipeline down.
    std::thread::sleep(Duration::from_secs(3));
    state
}

fn assert_reached_playing(
    state: (
        Result<gst::StateChangeSuccess, gst::StateChangeError>,
        gst::State,
        gst::State,
    ),
    what: &str,
) {
    let (result, current, pending) = state;
    assert_eq!(
        (result.expect("pipeline state readable"), current, pending),
        (
            gst::StateChangeSuccess::Success,
            gst::State::Playing,
            gst::State::VoidPending
        ),
        "{what} with no data held the pipeline out of PLAYING"
    );
}

/// A `splitmuxsink` only completes READY->PAUSED once it has prerolled a
/// buffer, so a recorder whose input carries no data holds the whole pipeline
/// short of PLAYING. A flow where only some inputs are live — remote presenters
/// who have not connected yet, each with their own recorder — puts recorders in
/// exactly that position, and the flow then does not run at all: the recorder
/// on the input that *is* live writes nothing either.
///
/// The two tests are a pair: one asserts an input with no data does not block
/// PLAYING, the other that a recorder still records once data arrives. Keeping
/// the sink locked for good would satisfy the first and break recording.
mod idle_input {
    use super::*;

    /// One recorder with data, one without: the pipeline must reach PLAYING,
    /// and the recorder that has data must write a file.
    ///
    /// Without the fix the pipeline never leaves PAUSED, so neither recorder
    /// writes anything — which is why an unfed input costs the recordings of
    /// every input that *is* live, not just its own.
    #[test]
    fn recorder_without_data_does_not_block_playing() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();

        let pipeline = gst::Pipeline::new();
        let live = add_mp4_recorder(&pipeline, "rec_live", media_root, 1, 0);
        let idle = add_mp4_recorder(&pipeline, "rec_idle", media_root, 1, 0);
        feed_video(&pipeline, &live.input("video_input_0"), 60);
        feed_nothing(&pipeline, &idle.input("video_input_0"));
        live.run_setups();
        idle.run_setups();

        let state = play_for_three_seconds(&pipeline);
        let live_files = recordings(media_root, "rec_live");
        let idle_files = recordings(media_root, "rec_idle");
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert_reached_playing(state, "a recorder");
        assert!(
            !live_files.is_empty(),
            "the recorder with data wrote no file; files present: {:?}",
            recordings(media_root, "")
        );
        assert!(
            total_bytes(&live_files) > 0,
            "the recorder with data wrote an empty file"
        );

        // The idle recorder never got data, so it must not have written anything.
        assert_eq!(
            total_bytes(&idle_files),
            0,
            "the recorder with no data wrote content: {:?}",
            idle_files
        );
    }

    /// A recorder that gets data must still record it.
    ///
    /// Counterpart to the test above: a sink kept out of the pipeline for good
    /// would satisfy that one and stop every recording.
    #[test]
    fn recorder_with_data_still_records() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();

        let pipeline = gst::Pipeline::new();
        let rec = add_mp4_recorder(&pipeline, "rec_live", media_root, 1, 0);
        feed_video(&pipeline, &rec.input("video_input_0"), 60);
        rec.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let reached_eos = wait_for_eos(&pipeline, Duration::from_secs(30));
        let files = recordings(media_root, "rec_live");
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert!(reached_eos, "pipeline never reached EOS within 30s");
        assert!(!files.is_empty(), "no recording was written");
        assert!(
            total_bytes(&files) > 0,
            "the recording is empty: {:?}",
            files
        );
    }
}

/// #750 kept an unfed recorder from holding the pipeline short of PLAYING, but
/// only on the `splitmuxsink` path. A `ts_passthrough` recorder builds a
/// `multifilesink` instead, which #750 left untouched — and a `GstBaseSink`
/// with `async` at its default still waits for a preroll buffer it will never
/// get, so one idle TS recorder stalls every other recorder in the flow.
///
/// The two tests are a pair, mirroring `idle_input`: one asserts an input with
/// no data does not block PLAYING, the other that a fed recorder still writes.
/// Locking the sink for good would satisfy the first and break recording.
mod ts_passthrough_idle {
    use super::*;

    /// A `ts_passthrough` recorder, returning its TS input element. The fixed
    /// build declares no internal links and links its sink from the caps probe
    /// instead; `add_recorder` making whatever links the block declares is what
    /// keeps this honest against the unfixed build too.
    fn add_ts_recorder(
        pipeline: &gst::Pipeline,
        instance_id: &str,
        media_root: &Path,
    ) -> gst::Element {
        add_recorder(
            pipeline,
            instance_id,
            media_root,
            &[("container", PropertyValue::String("ts_passthrough".into()))],
        )
        .input("ts_input")
    }

    /// Live MPEG-TS into `target`.
    fn feed_mpegts(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
        let enc = video_source(pipeline, num_buffers, true);
        let parse = gst::ElementFactory::make("h264parse")
            .build()
            .expect("h264parse");
        let mux = gst::ElementFactory::make("mpegtsmux")
            .build()
            .expect("mpegtsmux");
        pipeline.add_many([&parse, &mux]).unwrap();
        gst::Element::link_many([&enc, &parse, &mux]).unwrap();
        mux.link(target).expect("link muxer into recorder");
    }

    /// One TS recorder with data, one without: the pipeline must reach PLAYING,
    /// and the recorder that has data must write a file.
    ///
    /// Without the fix the unfed `multifilesink` never prerolls, so the
    /// pipeline stays in PAUSED and the *fed* recorder writes nothing either —
    /// which is why one idle input costs the recordings of every input that is
    /// live.
    #[test]
    fn ts_passthrough_recorder_without_data_does_not_block_playing() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();

        let pipeline = gst::Pipeline::new();
        let live_input = add_ts_recorder(&pipeline, "ts_live", media_root);
        let idle_input = add_ts_recorder(&pipeline, "ts_idle", media_root);
        feed_mpegts(&pipeline, &live_input, 60);
        feed_nothing(&pipeline, &idle_input);

        let state = play_for_three_seconds(&pipeline);
        let live = recordings(media_root, "ts_live");
        let idle = recordings(media_root, "ts_idle");
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert_reached_playing(state, "a ts_passthrough recorder");
        assert!(
            total_bytes(&live) > 0,
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
        if !plugins_available() {
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
        let reached_eos = wait_for_eos(&pipeline, Duration::from_secs(30));
        let files = recordings(media_root, "ts_live");
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert!(reached_eos, "pipeline never reached EOS within 30s");
        assert!(
            total_bytes(&files) > 0,
            "the TS recording is empty: {:?}",
            files
        );
    }
}

/// How the recorder hands out `splitmuxsink` sink pads.
///
/// `splitmuxsink` releases a GOP only once every requested pad has reached the
/// next GOP, so a pad that is never fed makes it write nothing at all. An
/// unconnected track must not get one — but every connected track must, before
/// any data flows. Requesting lazily from the caps probes fails that second
/// way: the muxer starts on the first track's data, after which mp4mux refuses
/// the second pad and matroskamux grants it but drops the data.
///
/// These tests assert on the streams inside the file, not its size: a dropped
/// track still leaves a large, valid, single-stream recording.
mod unfed_track {
    use super::*;
    use strom::blocks::BlockRegistry;
    use strom::gst::pipeline::PipelineManager;

    /// The muxers and demuxers are per-container, so they are checked where
    /// they are used rather than in `REQUIRED` — but through the same assert.
    /// Checking them directly let a runner without the isomp4 or matroska
    /// plugins skip green with `STROM_REQUIRE_GST_PLUGINS` set, which is what
    /// installing every plugin group on the Windows runner exists to prevent.
    fn container_available(container: &str) -> bool {
        let elements: &[&str] = match container {
            "mkv" => &["matroskamux", "matroskademux"],
            "mpegts" => &["mpegtsmux", "tsdemux"],
            _ => &["mp4mux", "qtdemux"],
        };
        common::plugins_available(elements)
    }

    /// Which inputs the test actually connects to the recorder.
    #[derive(Clone, Copy, PartialEq)]
    enum Feed {
        VideoOnly,
        AudioOnly,
        Both,
    }

    /// Build a recorder configured for one video and one audio track, wire up
    /// only the inputs named by `feed`, run until EOS, and return the files
    /// written.
    ///
    /// Both tracks are always configured — the point of the test is that
    /// configuring a track the flow never connects must not stop the other
    /// track from recording.
    fn run_recorder(container: &str, feed: Feed, media_root: &Path) -> Vec<PathBuf> {
        let pipeline = gst::Pipeline::new();
        let rec = add_recorder(
            &pipeline,
            "rec",
            media_root,
            &[
                ("container", PropertyValue::String(container.to_string())),
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
            ],
        );

        // Short, deterministic sources. 30 buffers at 30fps = 1s of video,
        // which is several GOPs at key-int-max=10 — enough for splitmuxsink to
        // complete and release at least one GOP if it is not stalled.
        if feed == Feed::VideoOnly || feed == Feed::Both {
            video_source(&pipeline, 30, false)
                .link(&rec.input("video_input_0"))
                .expect("link video into recorder");
        }
        if feed == Feed::AudioOnly || feed == Feed::Both {
            audio_source(&pipeline, 50, false)
                .link(&rec.input("audio_input_0"))
                .expect("link audio into recorder");
        }

        // The hook is what decides which tracks are connected, so skipping it
        // would exercise nothing.
        rec.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline goes to PLAYING");
        // 30 s is generous for ~1 s of content; a stalled splitmuxsink burns all of it.
        let reached_eos = wait_for_eos(&pipeline, Duration::from_secs(30));
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

        recordings(media_root, "")
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

        // no-more-pads means all streams are known. EOS would mean playing the
        // file through, and does not reliably arrive for every recording these
        // tests produce.
        let all_pads_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let all_pads_seen_for_cb = std::sync::Arc::clone(&all_pads_seen);
        demux.connect_no_more_pads(move |_| {
            all_pads_seen_for_cb.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        pipeline
            .set_state(gst::State::Playing)
            .expect("demux plays");
        let bus = pipeline.bus().expect("demux bus");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
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

    /// Assert the recording exists, is non-empty, and contains exactly the
    /// streams the connected inputs should have produced.
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

    /// The regression: an audio track is configured but the flow connects only
    /// video. Before the fix, splitmuxsink waited forever on the unfed audio
    /// pad and no file was ever written.
    #[test]
    fn video_only_feed_records_despite_configured_audio_track() {
        if !plugins_available() {
            return;
        }
        for container in ["mp4", "mkv", "mpegts"] {
            if !container_available(container) {
                continue;
            }
            assert_recorded(container, Feed::VideoOnly, "video only");
        }
    }

    /// The mirror case: a video track is configured but the flow connects only
    /// audio.
    #[test]
    fn audio_only_feed_records_despite_configured_video_track() {
        if !plugins_available() {
            return;
        }
        for container in ["mp4", "mkv", "mpegts"] {
            if !container_available(container) {
                continue;
            }
            assert_recorded(container, Feed::AudioOnly, "audio only");
        }
    }

    /// Both tracks connected, so both must end up in the file.
    ///
    /// Catches pads requested too late: `avenc_aac` negotiates caps before
    /// `x264enc`, so a recorder that requests on the caps event loses the video
    /// track. Repeated because that failure is a race — 11 of 12 runs for mp4,
    /// so one green run proves nothing.
    #[test]
    fn both_tracks_fed_records_both_streams() {
        if !plugins_available() {
            return;
        }
        for container in ["mp4", "mkv", "mpegts"] {
            if !container_available(container) {
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
    /// The tests above run the hook themselves, so they would still pass if
    /// `start()` stopped running it at the right moment. This one does not.
    ///
    /// Measured by making each change and rerunning: moving the hook before the
    /// linking pass fails this deterministically; moving it after
    /// `set_state(Playing)` does not, because `set_state` returns before the
    /// encoders negotiate caps. Nothing guards that second direction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    // On Windows this flow never reaches EOS, so splitmuxsink never finalizes
    // the file and the wait below always elapses — raising the ceiling from 30s
    // to 180s changes nothing. That is a defect in its own right, tracked in
    // #835. Ignored rather than cfg'd out so it stays visible in the Windows
    // run, and so the other tests in this file keep running there.
    #[cfg_attr(
        target_os = "windows",
        ignore = "never reaches EOS on Windows — see #835"
    )]
    async fn recorder_records_when_driven_through_pipeline_start() {
        if !plugins_available() || !container_available("mp4") {
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

        // Both tracks connected: the configuration that loses the caps race if
        // pads are late.
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

        // The bus watch is a glib signal watch, so it only dispatches while a
        // main loop runs. The real app has one; a test does not.
        let main_loop = gst::glib::MainLoop::new(None, false);
        let main_loop_thread = {
            let ml = main_loop.clone();
            std::thread::spawn(move || ml.run())
        };

        let events = EventBroadcaster::with_capacity(16);
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

        // Both sources have num-buffers, so the flow ends on its own.
        // splitmuxsink only finalizes the file on EOS, and an unfinalized mp4 has
        // no moov to demux.
        tokio::time::timeout(Duration::from_secs(30), async {
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

        let recorded = recordings(media_root.path(), "")
            .into_iter()
            .next()
            .expect("splitmuxsink wrote no file at all");
        let (has_video, has_audio) = stream_kinds(&recorded, "mp4");
        assert!(has_video, "recording has no video stream");
        assert!(has_audio, "recording has no audio stream");
    }
}

/// The recorder deadlock when one upstream streaming task feeds several
/// `splitmuxsink` sink pads.
///
/// Bug: `splitmuxsink` blocks one input pad's streaming thread while it waits
/// for the other input pads to reach the next GOP boundary — that is how it
/// aligns GOPs across streams. The reference pad (video) waits in
/// `check_completed_gop` until every context reaches the next GOP start;
/// non-reference pads (audio) wait once they catch up to `max_in_running_time`,
/// which only the reference pad advances.
///
/// That mutual blocking is only survivable when the pads are fed by different
/// threads. With `mpegtssrt_input(decode=false) -> recorder` the recorder fed
/// both sink pads from `tsdemux`'s single streaming task, so the pad that
/// blocked held the only thread that could ever unblock it and recording
/// stopped within a packet or two — 0 files, or a single fragment that never
/// closed.
///
/// Fix: a `queue` per leg between the parser and `splitmuxsink`, giving every
/// sink pad its own streaming thread. The test feeds the real recorder the way
/// `mpegtssrt_input(decode=false)` does — one `tsdemux` task into both inputs —
/// and asserts that recording actually completes. Remove the recorder's queues
/// and it stalls until the timeout fires.
mod splitmux_threading {
    use super::*;

    /// Long enough for a healthy run (which finishes in under a second) to
    /// never flake, short enough that a reintroduced deadlock fails promptly.
    const RUN_TIMEOUT: Duration = Duration::from_secs(30);

    /// Write a 5 s MPEG-TS file carrying H.264 video and AAC audio.
    fn write_test_transport_stream(path: &Path) {
        let pipeline = gst::Pipeline::new();
        let video = video_source(&pipeline, 150, false);
        let audio = audio_source(&pipeline, 240, false);
        let video_parse = gst::ElementFactory::make("h264parse")
            .build()
            .expect("h264parse");
        let audio_parse = gst::ElementFactory::make("aacparse")
            .build()
            .expect("aacparse");
        let mux = gst::ElementFactory::make("mpegtsmux")
            .build()
            .expect("mpegtsmux");
        let sink = gst::ElementFactory::make("filesink")
            .property("location", path.to_str().expect("temp path is valid UTF-8"))
            .build()
            .expect("filesink");
        pipeline
            .add_many([&video_parse, &audio_parse, &mux, &sink])
            .unwrap();
        gst::Element::link_many([&video, &video_parse, &mux]).unwrap();
        gst::Element::link_many([&audio, &audio_parse, &mux]).unwrap();
        mux.link(&sink).unwrap();

        pipeline
            .set_state(gst::State::Playing)
            .expect("source pipeline plays");
        let reached_eos = wait_for_eos(&pipeline, RUN_TIMEOUT);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(reached_eos, "writing the source transport stream timed out");
    }

    /// A recorder fed by a single demuxer streaming task must record both legs
    /// to completion. Without its queue per leg the two `splitmuxsink` sink
    /// pads deadlock on that shared thread and this times out.
    #[test]
    fn records_video_and_audio_from_a_single_demux_thread() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let ts_path = tmp.path().join("source.ts");
        write_test_transport_stream(&ts_path);

        let pipeline = gst::Pipeline::new();
        let rec = add_recorder(
            &pipeline,
            "rec",
            tmp.path(),
            &[
                ("container", PropertyValue::String("mp4".into())),
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
                // Split partway through so a healthy run yields several
                // fragments; a run that stalls after the first GOP yields zero
                // or one.
                ("max_size_time_secs", PropertyValue::UInt(2)),
            ],
        );

        let src = gst::ElementFactory::make("filesrc")
            .property("location", ts_path.to_str().expect("path is valid UTF-8"))
            .build()
            .expect("filesrc");
        let demux = gst::ElementFactory::make("tsdemux")
            .build()
            .expect("tsdemux");
        // The passthrough outputs of mpegtssrt_input: plain identities, no queue.
        let video_out = gst::ElementFactory::make("identity")
            .build()
            .expect("identity");
        let audio_out = gst::ElementFactory::make("identity")
            .build()
            .expect("identity");
        pipeline
            .add_many([&src, &demux, &video_out, &audio_out])
            .unwrap();
        src.link(&demux).unwrap();
        video_out.link(&rec.input("video_input_0")).unwrap();
        audio_out.link(&rec.input("audio_input_0")).unwrap();

        let video_weak = video_out.downgrade();
        let audio_weak = audio_out.downgrade();
        demux.connect_pad_added(move |_demux, pad| {
            let media_type = pad
                .current_caps()
                .and_then(|c| c.structure(0).map(|s| s.name().to_string()))
                .unwrap_or_default();
            let target = if media_type.starts_with("video/") {
                video_weak.upgrade()
            } else if media_type.starts_with("audio/") {
                audio_weak.upgrade()
            } else {
                return;
            };
            // A failed link leaves that track unfed, which fails the test at
            // the EOS wait; panicking here would be on a streaming thread.
            if let Some(sink) = target.and_then(|t| t.static_pad("sink")) {
                let _ = pad.link(&sink);
            }
        });

        rec.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let reached_eos = wait_for_eos(&pipeline, RUN_TIMEOUT);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            reached_eos,
            "recording from a single demux thread timed out waiting for EOS (deadlock?)"
        );

        // A healthy 5s recording split every 2s yields multiple non-empty fragments.
        let files = recordings(tmp.path(), "rec");
        assert!(
            files.len() >= 2,
            "expected several recorded fragments, got {:?}",
            files
        );
        assert!(total_bytes(&files) > 0, "recorded fragments were empty");
    }
}

/// A track the stall watchdog ended (see `stalled_track`) is recorded again,
/// in a new file, once its source carries data again. A source that really
/// ended, with EOS, stays ended and still finishes the recording.
mod track_resume {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// A switch in front of a recorder input. While `stalled` is set, buffers are
    /// dropped before they reach the recorder, which is what the recorder sees when
    /// the source upstream stops producing: no data, no EOS.
    fn gate(pipeline: &gst::Pipeline, stalled: &Arc<AtomicBool>) -> gst::Element {
        let gate = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&gate).unwrap();
        let stalled = Arc::clone(stalled);
        gate.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_pad, _info| {
                if stalled.load(Ordering::Relaxed) {
                    gst::PadProbeReturn::Drop
                } else {
                    gst::PadProbeReturn::Ok
                }
            },
        );
        gate
    }

    /// Live H.264 into `target` through `gate`, a keyframe every `key_int_max`
    /// frames. `num_buffers` of -1 runs until the pipeline stops.
    fn feed_video(
        pipeline: &gst::Pipeline,
        target: &gst::Element,
        gate: &gst::Element,
        num_buffers: i32,
        key_int_max: u32,
    ) {
        let enc = video_source(pipeline, num_buffers, true);
        enc.set_property("key-int-max", key_int_max);
        enc.link(gate).unwrap();
        gate.link(target).expect("link video into recorder");
    }

    /// Live AAC into `target` through `gate`.
    fn feed_audio(pipeline: &gst::Pipeline, target: &gst::Element, gate: &gst::Element) {
        audio_source(pipeline, -1, true).link(gate).unwrap();
        gate.link(target).expect("link audio into recorder");
    }

    /// A one-video, one-audio mp4 recorder and its two inputs.
    fn add_av_recorder(
        pipeline: &gst::Pipeline,
        instance_id: &str,
        media_root: &Path,
    ) -> (gst::Element, gst::Element, Recorder) {
        let recorder = add_mp4_recorder(pipeline, instance_id, media_root, 1, 1);
        (
            recorder.input("video_input_0"),
            recorder.input("audio_input_0"),
            recorder,
        )
    }

    /// Send EOS into the pipeline, as a flow stop does, and wait for the recording
    /// to be finalised. Returns whether the pipeline reported EOS.
    fn stop_with_eos(pipeline: &gst::Pipeline) -> bool {
        pipeline.send_event(gst::event::Eos::new());
        wait_for_eos(pipeline, Duration::from_secs(15))
    }

    /// What one track of a recorded file holds.
    #[derive(Debug, Default, Clone)]
    struct TrackSummary {
        samples: u64,
        duration: Duration,
    }

    #[derive(Debug, Default)]
    struct FileSummary {
        path: PathBuf,
        video: TrackSummary,
        audio: TrackSummary,
    }

    /// The payload of each child box of `data`, by four-character type.
    fn boxes(data: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            let kind = &data[pos + 4..pos + 8];
            let (header, size) = match size32 {
                1 if pos + 16 <= data.len() => (
                    16,
                    u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap()) as usize,
                ),
                0 => (8, data.len() - pos),
                n => (8, n),
            };
            if size < header || pos + size > data.len() {
                break;
            }
            out.push((kind, &data[pos + header..pos + size]));
            pos += size;
        }
        out
    }

    fn child<'a>(data: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
        boxes(data)
            .into_iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, payload)| payload)
    }

    fn be32(data: &[u8], at: usize) -> u64 {
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as u64
    }

    /// Read each track's sample count and duration from a recorded mp4's `moov`.
    ///
    /// qtdemux is no use here: it drops any track shorter than a fifth of the file
    /// as a "preview image", which is exactly the short track these tests look for.
    /// A readable `moov` with sample tables is also what says the file was
    /// finalised.
    fn summarize(path: &Path) -> FileSummary {
        let data = std::fs::read(path).expect("read recording");
        let moov = child(&data, b"moov")
            .unwrap_or_else(|| panic!("{} has no moov: not finalised", path.display()));
        let mut summary = FileSummary {
            path: path.to_path_buf(),
            ..Default::default()
        };
        for (kind, trak) in boxes(moov) {
            if kind != b"trak" {
                continue;
            }
            let mdia = child(trak, b"mdia").expect("trak has mdia");
            let handler = &child(mdia, b"hdlr").expect("mdia has hdlr")[8..12];
            let mdhd = child(mdia, b"mdhd").expect("mdia has mdhd");
            let (timescale, duration) = if mdhd[0] == 1 {
                (
                    be32(mdhd, 20),
                    u64::from_be_bytes(mdhd[24..32].try_into().unwrap()),
                )
            } else {
                (be32(mdhd, 12), be32(mdhd, 16))
            };
            let stsz = child(mdia, b"minf")
                .and_then(|minf| child(minf, b"stbl"))
                .and_then(|stbl| child(stbl, b"stsz"))
                .expect("track has a sample size table");
            let track = TrackSummary {
                samples: be32(stsz, 8),
                duration: Duration::from_secs_f64(duration as f64 / timescale.max(1) as f64),
            };
            match handler {
                b"vide" => summary.video = track,
                b"soun" => summary.audio = track,
                _ => {}
            }
        }
        summary
    }

    /// Frames reaching splitmuxsink's video pad — what the muxer actually takes.
    /// A new file comes with new pads, so count on the pad there is now.
    fn count_video_into_muxer(pipeline: &gst::Pipeline, instance_id: &str) -> Arc<AtomicU64> {
        let counter = Arc::new(AtomicU64::new(0));
        let sink = pipeline
            .by_name(&format!("{}:splitmuxsink", instance_id))
            .expect("splitmuxsink in pipeline");
        let c = Arc::clone(&counter);
        sink.static_pad("video")
            .expect("splitmuxsink has a video pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                c.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        counter
    }

    /// The production failure: video stops arriving for longer than the stall
    /// timeout while audio keeps going, then comes back. The recorder ends the video
    /// track so the audio is not frozen behind it, and once video is flowing again
    /// the recording has to have video in it again.
    ///
    /// If the track stays ended, every frame in the recording is from before the
    /// stall, and the last assertion fails.
    #[test]
    fn a_track_that_stalls_and_resumes_is_recorded_again() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        let (video_in, audio_in, recorder) = add_av_recorder(&pipeline, "rec_resume", media_root);
        let video_stalled = Arc::new(AtomicBool::new(false));
        let audio_stalled = Arc::new(AtomicBool::new(false));
        let video_gate = gate(&pipeline, &video_stalled);
        let audio_gate = gate(&pipeline, &audio_stalled);
        feed_video(&pipeline, &video_in, &video_gate, -1, 30);
        feed_audio(&pipeline, &audio_in, &audio_gate);
        recorder.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(15));

        std::thread::sleep(Duration::from_secs(3));
        let bus = pipeline.bus().expect("pipeline bus");
        while bus
            .pop_filtered(&[gst::MessageType::StateChanged])
            .is_some()
        {}
        let running_before_stall = pipeline.current_running_time();
        video_stalled.store(true, Ordering::Relaxed);
        // Past the stall timeout, so the video track is ended.
        std::thread::sleep(Duration::from_secs(9));
        let running_at_resume = pipeline.current_running_time();
        video_stalled.store(false, Ordering::Relaxed);

        // Recovery takes the 3 s the track has to carry data first, a poll
        // interval and the old file's finalisation. Then measure a window of
        // steady recording.
        std::thread::sleep(Duration::from_secs(6));
        let video_into_muxer = count_video_into_muxer(&pipeline, "rec_resume");
        std::thread::sleep(Duration::from_secs(4));
        let frames_after = video_into_muxer.load(Ordering::Relaxed);

        // Closing the first file must not look like the end of the stream: the
        // pipeline would report EOS while it is still recording.
        let early_eos = bus.pop_filtered(&[gst::MessageType::Eos]);
        // And restarting the muxer must stay inside the recorder, not take the
        // whole pipeline back to PAUSED while the new file prerolls.
        let pipeline_left_playing =
            std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::StateChanged]))
                .filter(|msg| msg.src() == Some(pipeline.upcast_ref::<gst::Object>()))
                .filter_map(|msg| match msg.view() {
                    gst::MessageView::StateChanged(sc) => Some(sc.current()),
                    _ => None,
                })
                .find(|current| *current != gst::State::Playing);

        let stopped = stop_with_eos(&pipeline);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
        assert!(
            early_eos.is_none(),
            "starting the next file posted EOS on the pipeline"
        );
        assert_eq!(
            pipeline_left_playing, None,
            "starting the next file knocked the pipeline out of PLAYING"
        );
        assert!(
            stopped,
            "the recording did not finish on EOS after a resumed track"
        );

        let summaries: Vec<FileSummary> = recordings(media_root, "")
            .iter()
            .map(|p| summarize(p))
            .collect();
        for s in &summaries {
            eprintln!(
                "{}: video {} samples over {:?}, audio {} samples over {:?}",
                s.path.file_name().unwrap().to_string_lossy(),
                s.video.samples,
                s.video.duration,
                s.audio.samples,
                s.audio.duration
            );
        }
        eprintln!(
            "stall from {:?} to {:?}; {} frames into the muxer in the 4 s window",
            running_before_stall, running_at_resume, frames_after
        );

        // The file open before the stall keeps what was recorded until then.
        let first = summaries.first().expect("at least one recording");
        assert!(
            first.video.samples >= 30,
            "the recording before the stall lost its video: {:?}",
            first
        );

        // 30 fps over four seconds is 120 frames. Anything near that means video is
        // being recorded again rather than dropped at the recorder.
        assert!(
            frames_after >= 60,
            "video came back but the recorder is not taking it: {} frames reached the muxer in 4 s",
            frames_after
        );

        // And it is in a file: a file that starts after the stall has video in it,
        // alongside audio.
        let resumed = summaries
            .iter()
            .skip(1)
            .find(|s| s.video.samples > 0)
            .unwrap_or_else(|| {
                panic!(
                    "no recording after the stall has video; files: {:?}",
                    summaries
                )
            });
        assert!(
            resumed.video.samples >= 60,
            "the resumed recording has too little video: {:?}",
            resumed
        );
        assert!(
            resumed.audio.samples > 0,
            "the resumed recording dropped the audio track: {:?}",
            resumed
        );
    }

    /// A source that really ends sends EOS. That track is finished, not stalled: the
    /// recorder must not wait for it to come back, the other track keeps recording
    /// into the same file, and a flow stop still finalises the recording.
    #[test]
    fn a_track_that_really_ends_stays_ended_and_the_recording_finishes() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        let (video_in, audio_in, recorder) = add_av_recorder(&pipeline, "rec_eos", media_root);
        let never = Arc::new(AtomicBool::new(false));
        let video_gate = gate(&pipeline, &never);
        let audio_gate = gate(&pipeline, &never);
        // Two seconds of video, then a real EOS from the source.
        feed_video(&pipeline, &video_in, &video_gate, 60, 30);
        feed_audio(&pipeline, &audio_in, &audio_gate);
        recorder.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(15));

        // Well past the stall timeout after the video ended.
        std::thread::sleep(Duration::from_secs(10));
        let stopped = stop_with_eos(&pipeline);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
        assert!(
            stopped,
            "the recording did not finish on EOS after a track ended"
        );

        let summaries: Vec<FileSummary> = recordings(media_root, "")
            .iter()
            .map(|p| summarize(p))
            .collect();
        assert_eq!(
            summaries.len(),
            1,
            "a track that ended for real must not start a new file: {:?}",
            summaries
        );
        let file = &summaries[0];
        assert!(
            (50..=60).contains(&file.video.samples),
            "the video that was sent should all be recorded, and nothing more: {:?}",
            file
        );
        // Audio kept going for the whole run, well past the end of the video.
        assert!(
            file.audio.duration >= Duration::from_secs(8),
            "audio stopped with the video: {:?}",
            file
        );
    }

    /// A recorder fed live video and audio, each behind its own stall switch,
    /// running in PLAYING.
    struct Rig {
        _tmp: tempfile::TempDir,
        media_root: PathBuf,
        pipeline: gst::Pipeline,
        audio_in: gst::Element,
        video_gate: gst::Element,
        video_stalled: Arc<AtomicBool>,
        audio_stalled: Arc<AtomicBool>,
        pre_stops: Vec<PreStopFn>,
    }

    impl Rig {
        fn start(instance_id: &str) -> Self {
            Self::start_with(instance_id, 30, false)
        }

        /// `remote_encoder` drops key-unit requests on their way upstream, as for a
        /// source whose encoder sits on the far side of an SRT or RTMP link.
        fn start_with(instance_id: &str, key_int_max: u32, remote_encoder: bool) -> Self {
            let tmp = tempfile::tempdir().expect("tempdir");
            let media_root = tmp.path().to_path_buf();
            let pipeline = gst::Pipeline::new();
            let (video_in, audio_in, recorder) =
                add_av_recorder(&pipeline, instance_id, &media_root);
            let video_stalled = Arc::new(AtomicBool::new(false));
            let audio_stalled = Arc::new(AtomicBool::new(false));
            let video_gate = gate(&pipeline, &video_stalled);
            let audio_gate = gate(&pipeline, &audio_stalled);
            if remote_encoder {
                video_gate.static_pad("src").unwrap().add_probe(
                    gst::PadProbeType::EVENT_UPSTREAM,
                    |_pad, info| match info.data.as_ref() {
                        Some(gst::PadProbeData::Event(e))
                            if e.type_() == gst::EventType::CustomUpstream =>
                        {
                            gst::PadProbeReturn::Drop
                        }
                        _ => gst::PadProbeReturn::Ok,
                    },
                );
            }
            feed_video(&pipeline, &video_in, &video_gate, -1, key_int_max);
            feed_audio(&pipeline, &audio_in, &audio_gate);
            recorder.run_setups();
            let pre_stops = recorder.take_pre_stops();
            pipeline
                .set_state(gst::State::Playing)
                .expect("pipeline accepts PLAYING");
            let _ = pipeline.state(gst::ClockTime::from_seconds(15));
            Rig {
                _tmp: tmp,
                media_root,
                pipeline,
                audio_in,
                video_gate,
                video_stalled,
                audio_stalled,
                pre_stops,
            }
        }

        /// Set the pipeline to NULL as the pipeline manager stops a flow: the
        /// blocks' pre-stop hooks first.
        fn set_null(&mut self) {
            for hook in self.pre_stops.drain(..) {
                hook();
            }
            self.pipeline
                .set_state(gst::State::Null)
                .expect("pipeline to NULL");
        }

        /// The sink pad of the element splitmuxsink writes the file with.
        fn file_sink_pad(&self) -> gst::Pad {
            self.pipeline
                .iterate_all_by_element_factory_name("splitmuxsink")
                .into_iter()
                .filter_map(Result::ok)
                .find_map(|smx| {
                    smx.downcast::<gst::Bin>()
                        .ok()?
                        .iterate_sinks()
                        .into_iter()
                        .filter_map(Result::ok)
                        .find_map(|sink| sink.static_pad("sink"))
                })
                .expect("splitmuxsink has a file sink once it has started")
        }

        /// Stop the flow with EOS and read back every file it wrote.
        fn stop(mut self) -> Vec<FileSummary> {
            let stopped = stop_with_eos(&self.pipeline);
            self.set_null();
            assert!(stopped, "the recording did not finish on EOS");
            let summaries: Vec<FileSummary> = recordings(&self.media_root, "")
                .iter()
                .map(|p| summarize(p))
                .collect();
            for s in &summaries {
                eprintln!(
                    "{}: video {} samples over {:?}, audio {} samples over {:?}",
                    s.path.file_name().unwrap().to_string_lossy(),
                    s.video.samples,
                    s.video.duration,
                    s.audio.samples,
                    s.audio.duration
                );
            }
            summaries
        }
    }

    /// The same recovery with the tracks the other way round. Audio is not the
    /// track splitmuxsink splits on, so ending it takes a different path through
    /// splitmuxsink, and the new file has to bring it back all the same.
    #[test]
    fn an_audio_track_that_stalls_and_resumes_is_recorded_again() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_audio_back");
        std::thread::sleep(Duration::from_secs(3));
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        rig.audio_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(6));
        let summaries = rig.stop();

        let last = summaries.last().expect("at least one recording");
        assert!(
            summaries.len() >= 2,
            "no new file was started once the audio came back: {:?}",
            summaries
        );
        // About 5 s of AAC at 1024 samples / 44.1 kHz is 215 frames.
        assert!(
            last.audio.samples >= 100 && last.video.samples >= 60,
            "the file after the stall should have both tracks: {:?}",
            last
        );
    }

    /// A source that stalls once can stall again. Every return gets its own file,
    /// and each one has the track in it.
    #[test]
    fn a_track_that_stalls_twice_is_recorded_after_each_return() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_twice");
        // Each file gets 5 s of recording between the restart and the next stall.
        for _ in 0..2 {
            std::thread::sleep(Duration::from_secs(5));
            rig.video_stalled.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(8));
            rig.video_stalled.store(false, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_secs(5));
        let summaries = rig.stop();

        assert_eq!(
            summaries.len(),
            3,
            "expected one file per return plus the first: {:?}",
            summaries
        );
        for s in &summaries {
            assert!(
                s.video.samples >= 60 && s.audio.samples > 0,
                "every file should hold both tracks: {:?}",
                s
            );
        }
    }

    /// The flow is stopped while a track is still out. The recording has to
    /// finish as it did before: EOS reaches the pipeline and the file is written
    /// out with what it recorded.
    #[test]
    fn a_flow_stopped_while_a_track_is_out_still_finishes() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_stop_out");
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(10));
        let summaries = rig.stop();

        assert_eq!(
            summaries.len(),
            1,
            "no track came back, so no new file: {:?}",
            summaries
        );
        let file = &summaries[0];
        assert!(
            file.video.samples >= 60 && file.audio.duration >= Duration::from_secs(10),
            "the file should hold the video before the stall and all of the audio: {:?}",
            file
        );
    }

    /// The keyframe that shows a track is back has to go into the new file. The
    /// next one can be a whole GOP later, and when the encoder cannot be asked for
    /// one sooner, that is longer than the stall timeout: the new file would sit
    /// empty until the track is ended again, over and over, with the audio lost.
    #[test]
    fn a_track_back_mid_gop_is_recorded_from_the_keyframe_that_showed_it() {
        if !plugins_available() {
            return;
        }
        // Keyframes at 0, 10 and 20 s. The one at 10 s falls in the stall, so the
        // one at 20 s is the first the recorder sees, and the next is 10 s later.
        let rig = Rig::start_with("rec_long_gop", 300, true);
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(15));
        let summaries = rig.stop();

        assert_eq!(
            summaries.len(),
            2,
            "one file before the stall and one after: {:?}",
            summaries
        );
        let last = &summaries[1];
        assert!(
            last.video.samples >= 150 && last.audio.duration >= Duration::from_secs(5),
            "the file after the stall should hold everything from the 20 s keyframe on: {:?}",
            last
        );
    }

    /// Thin the video that gets through the rig's stall gate: all of it while the
    /// returned mode is 0, one keyframe every `every` once it is 1, nothing once
    /// it is 2. Added after the stall gate's probe, so it sees what that one
    /// passes.
    fn thin_video(rig: &Rig, every: Duration) -> Arc<AtomicU64> {
        let mode = Arc::new(AtomicU64::new(0));
        let state = Arc::clone(&mode);
        let last = std::sync::Mutex::new(None::<Instant>);
        rig.video_gate.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |_pad, info| match state.load(Ordering::SeqCst) {
                0 => gst::PadProbeReturn::Ok,
                1 if info
                    .buffer()
                    .is_some_and(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT))
                    && last
                        .lock()
                        .unwrap()
                        .is_none_or(|t: Instant| t.elapsed() >= every) =>
                {
                    *last.lock().unwrap() = Some(Instant::now());
                    gst::PadProbeReturn::Ok
                }
                _ => gst::PadProbeReturn::Drop,
            },
        );
        mode
    }

    /// A track that comes back for a single frame and stops again stays out, and
    /// the rest of the recording goes on in the same file. A new file for it would
    /// freeze the other tracks again once the frame is followed by nothing.
    #[test]
    fn a_track_back_for_one_frame_stays_out_and_the_rest_is_recorded() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_one_frame");
        let thin = thin_video(&rig, Duration::from_secs(600));
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        thin.store(1, Ordering::SeqCst);
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(15));
        let summaries = rig.stop();

        assert_eq!(summaries.len(), 1, "{:?}", summaries);
        assert!(
            summaries[0].audio.duration >= Duration::from_secs(25),
            "the audio should go on being recorded in the first file: {:?}",
            summaries
        );
    }

    /// A source that sends a keyframe every 6 s, just past the stall timeout, is
    /// not recorded again. Each keyframe would otherwise start a new file, and
    /// the stop after it freeze the other tracks for the stall timeout again.
    #[test]
    fn a_track_back_for_a_frame_now_and_then_starts_no_new_files() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_sparse");
        let thin = thin_video(&rig, Duration::from_secs(6));
        std::thread::sleep(Duration::from_secs(3));
        thin.store(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(30));
        let summaries = rig.stop();

        assert_eq!(summaries.len(), 1, "{:?}", summaries);
        assert!(
            summaries[0].audio.duration >= Duration::from_secs(30),
            "the audio should be recorded throughout: {:?}",
            summaries
        );
    }

    /// A track back long enough to be recorded again, which stops again as the
    /// new file takes it, is ended there like the first time, and the rest of the
    /// recording goes on.
    #[test]
    fn a_track_that_stops_again_in_the_new_file_leaves_the_rest_recorded() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_stops_again");
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        // The point the video is parked at is next linked when the new file takes
        // the track.
        let video_stalled = Arc::clone(&rig.video_stalled);
        rig.pipeline
            .by_name("rec_stops_again:video_park_0")
            .expect("the recorder parks video at video_park_0")
            .static_pad("src")
            .unwrap()
            .connect_linked(move |_pad, _peer| {
                video_stalled.store(true, Ordering::Relaxed);
            });
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(22));
        let summaries = rig.stop();

        assert_eq!(summaries.len(), 2, "{:?}", summaries);
        let last = &summaries[1];
        assert!(
            last.video.samples >= 60 && last.audio.duration >= Duration::from_secs(15),
            "the new file should hold the video's return and go on recording the audio: {:?}",
            last
        );
    }

    /// A video track still being recorded when the audio comes back is cut at its
    /// keyframe, then stops for longer than a parked input may pause, while the
    /// switch is still closing the old file. What it kept from the keyframe on is
    /// already promised to the new file, and goes into it.
    #[test]
    fn a_break_during_the_switch_loses_nothing_the_new_file_was_given() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start_with("rec_break_in_switch", 300, true);
        std::thread::sleep(Duration::from_secs(3));
        // The switch closing the old file sends EOS into its file sink. Hold the
        // switch there for 7 s and stop the video for 5.5 s of it.
        let armed = Arc::new(AtomicBool::new(false));
        let fire = Arc::clone(&armed);
        let video_stalled = Arc::clone(&rig.video_stalled);
        rig.file_sink_pad()
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                let eos = matches!(info.data.as_ref(),
                    Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos);
                if eos && fire.swap(false, Ordering::SeqCst) {
                    let video_stalled = Arc::clone(&video_stalled);
                    std::thread::spawn(move || {
                        video_stalled.store(true, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(5500));
                        video_stalled.store(false, Ordering::Relaxed);
                    });
                    std::thread::sleep(Duration::from_secs(7));
                }
                gst::PadProbeReturn::Ok
            });
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(16));
        armed.store(true, Ordering::SeqCst);
        rig.audio_stalled.store(false, Ordering::Relaxed);
        // The cut waits for the keyframe at 30 s.
        std::thread::sleep(Duration::from_secs(31));
        let summaries = rig.stop();

        let frames: u64 = summaries.iter().skip(1).map(|s| s.video.samples).sum();
        let audio: Duration = summaries.iter().skip(1).map(|s| s.audio.duration).sum();
        // 30 s to 50 s at 30 fps, less the break.
        assert!(
            frames >= 400 && audio >= Duration::from_secs(25),
            "the new file should start on the cut keyframe and hold the audio from its return: {} frames, {:?} audio: {:?}",
            frames,
            audio,
            summaries
        );
    }

    /// A slow source, here a frame about every 4 s, is recorded before a stall, so
    /// it is recorded again after one.
    #[test]
    fn a_slow_source_is_recorded_again_after_a_stall() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_slow_source");
        let thin = thin_video(&rig, Duration::from_millis(3900));
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        thin.store(1, Ordering::SeqCst);
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(24));
        let summaries = rig.stop();

        assert!(
            summaries.len() >= 2 && summaries.last().unwrap().video.samples >= 3,
            "{:?}",
            summaries
        );
    }

    /// Storage that stalls while the next file is being started delays the
    /// switch, but does not fail the flow.
    #[test]
    fn storage_that_stalls_during_the_switch_does_not_fail_the_flow() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_slow_storage");
        std::thread::sleep(Duration::from_secs(3));
        let slow = Arc::new(AtomicBool::new(false));
        let next_write_is_slow = Arc::clone(&slow);
        rig.file_sink_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                if next_write_is_slow.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(14));
                }
                gst::PadProbeReturn::Ok
            });
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        // The next write is the current file's last ones, as the switch closes it.
        slow.store(true, Ordering::SeqCst);
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(25));
        let summaries = rig.stop();

        assert!(
            summaries.iter().skip(1).any(|s| s.video.samples >= 60),
            "the video should be recorded again once the storage recovers: {:?}",
            summaries
        );
    }

    /// A video track still being recorded when another track comes back keeps
    /// every frame, even from a remote encoder with a 10 s GOP that ignores
    /// key-unit requests: the current file takes it up to its next keyframe, and
    /// the next file starts on that keyframe.
    #[test]
    fn a_long_gop_track_still_recording_loses_no_video_to_the_switch() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start_with("rec_long_gop_other", 300, true);
        std::thread::sleep(Duration::from_secs(3));
        // Keyframes at 0, 10, 20 and 30 s. The video blocks at the 10 s one
        // behind the dead audio, which is ended at about 15 s.
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(19));
        rig.audio_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(22));
        let summaries = rig.stop();

        assert!(
            summaries.iter().all(|s| s.video.samples > 0),
            "every file should have video: {:?}",
            summaries
        );
        let video: Duration = summaries.iter().map(|s| s.video.duration).sum();
        assert!(
            video >= Duration::from_secs(42),
            "the video should be recorded without a gap across the switch, got {:?}: {:?}",
            video,
            summaries
        );
        assert!(
            summaries
                .last()
                .is_some_and(|s| s.audio.duration >= Duration::from_secs(10)),
            "the audio should be recorded again after it comes back: {:?}",
            summaries
        );
    }

    /// A video track that stops just as another track comes back, so it never
    /// reaches the keyframe the switch waits for, is left out of the next file,
    /// and the track that came back is recorded from its return.
    #[test]
    fn a_video_track_that_stops_before_its_keyframe_does_not_hold_the_switch() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_cut_timeout");
        std::thread::sleep(Duration::from_secs(3));
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        rig.video_stalled.store(true, Ordering::Relaxed);
        rig.audio_stalled.store(false, Ordering::Relaxed);
        // The switch gives up on the video's keyframe after 15 s.
        std::thread::sleep(Duration::from_secs(28));
        let summaries = rig.stop();

        assert!(
            summaries
                .last()
                .is_some_and(|s| s.audio.duration >= Duration::from_secs(20)),
            "the audio should be recorded from its return: {:?}",
            summaries
        );
    }

    /// A track that dies while the next file is being started is ended like any
    /// other, even while another track is still replaying into that file.
    #[test]
    fn a_track_that_dies_during_the_switch_is_ended_while_the_rest_replays() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_dies_in_switch");
        std::thread::sleep(Duration::from_secs(3));
        // Slow storage makes the switch take long enough for the video to keep
        // more than its queue holds, so its replay waits on the audio.
        let slow = Arc::new(AtomicBool::new(false));
        let next_write_is_slow = Arc::clone(&slow);
        rig.file_sink_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                if next_write_is_slow.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_secs(14));
                }
                gst::PadProbeReturn::Ok
            });
        // The audio dies as the switch cuts it off, and never comes back.
        let audio_stalled = Arc::clone(&rig.audio_stalled);
        rig.audio_in
            .static_pad("src")
            .unwrap()
            .connect_unlinked(move |_pad, _peer| {
                audio_stalled.store(true, Ordering::Relaxed);
            });
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        slow.store(true, Ordering::SeqCst);
        rig.video_stalled.store(false, Ordering::Relaxed);
        // The switch starts 3 s after the video's return, takes about 14 s, then
        // 5 s to find the dead audio.
        std::thread::sleep(Duration::from_secs(29));
        let video_into_muxer = count_video_into_muxer(&rig.pipeline, "rec_dies_in_switch");
        std::thread::sleep(Duration::from_secs(6));
        let frames = video_into_muxer.load(Ordering::Relaxed);
        let summaries = rig.stop();

        assert!(
            frames >= 60,
            "the video should be recorded once the dead audio is ended: {} frames in 6 s: {:?}",
            frames,
            summaries
        );
    }

    /// An EOS that reaches a track while the next file is being started still ends
    /// the recording.
    #[test]
    fn an_eos_while_the_next_file_starts_still_ends_the_recording() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start("rec_eos_switch");
        // The audio input is only cut off from its chain to start the next file.
        // Its source stops there too, so the EOS is the next thing to reach it: a
        // buffer first would be held for the new file and carry the EOS in after it.
        let pipeline_weak = rig.pipeline.downgrade();
        let audio_stalled = Arc::clone(&rig.audio_stalled);
        rig.audio_in
            .static_pad("src")
            .unwrap()
            .connect_unlinked(move |_pad, _peer| {
                audio_stalled.store(true, Ordering::Relaxed);
                let pipeline_weak = pipeline_weak.clone();
                std::thread::spawn(move || {
                    if let Some(pipeline) = pipeline_weak.upgrade() {
                        pipeline.send_event(gst::event::Eos::new());
                    }
                });
            });
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        rig.video_stalled.store(false, Ordering::Relaxed);

        let finished = wait_for_eos(&rig.pipeline, Duration::from_secs(15));
        rig.pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
        assert!(finished, "the EOS never reached the pipeline");
    }

    /// Clear DELTA_UNIT on every video buffer reaching the recorder, as tsdemux
    /// output does: Strom's SRT input in passthrough links tsdemux straight to
    /// its output, with no parser to mark which frames are keyframes.
    fn strip_delta_flags(rig: &Rig) {
        rig.video_gate.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            |_pad, info| {
                if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data {
                    buffer.make_mut().unset_flags(gst::BufferFlags::DELTA_UNIT);
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    /// Video with no keyframe flags, from a remote encoder with a 10 s GOP, that
    /// stalls and comes back is recorded again from a real keyframe, and the flow
    /// does not fail.
    #[test]
    fn a_returning_track_without_keyframe_flags_is_recorded_from_a_keyframe() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start_with("rec_noflags_back", 300, true);
        strip_delta_flags(&rig);
        std::thread::sleep(Duration::from_secs(3));
        rig.video_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(9));
        rig.video_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(15));
        let summaries = rig.stop();

        let audio: Duration = summaries.iter().map(|s| s.audio.duration).sum();
        assert!(
            audio >= Duration::from_secs(22),
            "audio lost across the resume: {:?} recorded: {:?}",
            audio,
            summaries
        );
        let last = summaries.last().expect("a recording");
        assert!(
            summaries.len() >= 2 && last.video.samples >= 150,
            "the file after the stall should hold the video from its return: {:?}",
            summaries
        );
    }

    /// Video with no keyframe flags that is still being recorded when another
    /// track comes back is cut at a real keyframe, and the flow does not fail.
    #[test]
    fn a_recording_track_without_keyframe_flags_is_cut_at_a_keyframe() {
        if !plugins_available() {
            return;
        }
        let rig = Rig::start_with("rec_noflags_cut", 300, true);
        strip_delta_flags(&rig);
        std::thread::sleep(Duration::from_secs(3));
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(19));
        rig.audio_stalled.store(false, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(22));
        let summaries = rig.stop();

        let video: Duration = summaries.iter().map(|s| s.video.duration).sum();
        assert!(
            summaries.iter().all(|s| s.video.samples > 0) && video >= Duration::from_secs(42),
            "the video should be recorded without a gap across the switch, got {:?}: {:?}",
            video,
            summaries
        );
    }

    /// With a GOP short enough that nothing is ended twice, video with no
    /// keyframe flags loses no more to a switch than video with them.
    #[test]
    fn a_track_without_keyframe_flags_loses_no_video_to_the_switch() {
        if !plugins_available() {
            return;
        }
        fn run(instance_id: &str, strip: bool) -> Duration {
            let rig = Rig::start_with(instance_id, 120, true);
            if strip {
                strip_delta_flags(&rig);
            }
            std::thread::sleep(Duration::from_secs(3));
            rig.audio_stalled.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(12));
            rig.audio_stalled.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(12));
            rig.stop().iter().map(|s| s.video.duration).sum()
        }
        let (with_flags, without_flags) = std::thread::scope(|scope| {
            let with = scope.spawn(|| run("rec_flags_cut4", false));
            let without = scope.spawn(|| run("rec_noflags_cut4", true));
            (with.join().unwrap(), without.join().unwrap())
        });
        assert!(
            without_flags + Duration::from_millis(400) >= with_flags,
            "lost {:?} of video to the switch: {:?} recorded, {:?} with keyframe flags",
            with_flags.saturating_sub(without_flags),
            without_flags,
            with_flags
        );
    }

    /// A flow stops with NULL and no EOS, and right after dropping its pipeline
    /// reports a leak if anything still holds it. A stop while a switch waits for
    /// the recorded video's keyframe leaves nothing holding it.
    #[test]
    fn a_flow_stopped_during_the_switch_leaves_nothing_holding_its_pipeline() {
        if !plugins_available() {
            return;
        }
        let mut rig = Rig::start_with("rec_stop_in_switch", 300, true);
        std::thread::sleep(Duration::from_secs(3));
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(19));
        rig.audio_stalled.store(false, Ordering::Relaxed);
        // The switch starts within a poll and waits for the keyframe at 30 s.
        std::thread::sleep(Duration::from_secs(3));

        let weak = rig.pipeline.downgrade();
        rig.set_null();
        drop(rig);
        assert!(
            weak.upgrade().is_none(),
            "the pipeline was still held right after it was dropped"
        );
    }

    /// The same for a stop that lands while the switch relinks, with the muxer
    /// in NULL and the pipeline held to build the new chains. The stop waits for
    /// the relink to finish, and still reaches NULL.
    #[test]
    fn a_flow_stopped_while_the_switch_relinks_leaves_nothing_holding_its_pipeline() {
        if !plugins_available() {
            return;
        }
        let mut rig = Rig::start("rec_stop_in_relink");

        // The relink releases the old splitmuxsink pads, on the thread running
        // it. The first release holds the relink open long enough for the stop
        // to land in it.
        let (relinking_tx, relinking) = std::sync::mpsc::channel::<()>();
        let held = AtomicBool::new(false);
        rig.pipeline
            .iterate_all_by_element_factory_name("splitmuxsink")
            .into_iter()
            .find_map(Result::ok)
            .expect("the recorder has a splitmuxsink")
            .connect_pad_removed(move |_, _| {
                if !held.swap(true, Ordering::SeqCst) {
                    let _ = relinking_tx.send(());
                    std::thread::sleep(Duration::from_millis(500));
                }
            });

        std::thread::sleep(Duration::from_secs(3));
        rig.audio_stalled.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(8));
        rig.audio_stalled.store(false, Ordering::Relaxed);
        relinking
            .recv_timeout(Duration::from_secs(20))
            .expect("the switch never relinked");

        // The stop lands just as the new file opens, which can leave a
        // splitmuxsink queue waiting for good with NULL blocked behind it.
        let weak = rig.pipeline.downgrade();
        let (stopped_tx, stopped) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            rig.set_null();
            drop(rig);
            let _ = stopped_tx.send(());
        });
        stopped
            .recv_timeout(Duration::from_secs(30))
            .expect("the pipeline did not reach NULL within 30s");
        assert!(
            weak.upgrade().is_none(),
            "the pipeline was still held right after it was dropped"
        );
    }
}
