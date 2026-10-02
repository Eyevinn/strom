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
//! - `stalled_track`: a track that stops delivering must not freeze the rest.
//! - `splitmux_threading`: one upstream streaming task feeding every track must
//!   not deadlock `splitmuxsink`.
//! - `file_start_time`: each file's reported start must line its content up with
//!   the pipeline, and so with every other recorder in the flow.

pub mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
/// The per-container muxers and demuxers of `unfed_track` are checked there.
const REQUIRED: &[&str] = &[
    "splitmuxsink",
    "multifilesink",
    "mp4mux",
    "mpegtsmux",
    "tsdemux",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "appsrc",
    "identity",
    "queue",
    "filesrc",
    "fakesink",
];

fn plugins_available() -> bool {
    common::plugins_available(REQUIRED)
}

/// A recorder block built through the real builder and added to a pipeline.
struct Recorder {
    instance_id: String,
    /// Holds the element-setup hooks; see [`Recorder::run_setups`].
    ctx: BlockBuildContext,
    elements: HashMap<String, gst::Element>,
}

impl Recorder {
    /// The block element with the internal id `name`, e.g. `video_input_0`.
    fn input(&self, name: &str) -> gst::Element {
        self.elements
            .get(&format!("{}:{}", self.instance_id, name))
            .cloned()
            .unwrap_or_else(|| panic!("recorder {} exposes no {}", self.instance_id, name))
    }

    /// Run the element-setup hooks, as the pipeline manager does after linking
    /// and before PLAYING. They decide which tracks are connected and get a
    /// `splitmuxsink` pad, so they must run after the inputs are linked.
    fn run_setups(&self) {
        self.run_setups_for(uuid::Uuid::new_v4(), &EventBroadcaster::with_capacity(16));
    }

    /// [`Recorder::run_setups`] as part of flow `flow_id`, reporting to `events`.
    fn run_setups_for(&self, flow_id: uuid::Uuid, events: &EventBroadcaster) {
        for setup in self.ctx.take_element_setups() {
            setup(flow_id, events.clone());
        }
    }
}

/// Build a recorder named `instance_id` that writes to `<media_root>/recordings`
/// with its instance id as filename prefix, add it to `pipeline`, and make the
/// links it declares, as the pipeline manager would. `props` is the rest of
/// its configuration.
fn add_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    props: &[(&str, PropertyValue)],
) -> Recorder {
    let mut properties: HashMap<String, PropertyValue> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    properties.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".into()),
    );
    properties.insert(
        "filename_prefix".to_string(),
        PropertyValue::String(instance_id.to_string()),
    );
    properties.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RecorderBuilder
        .build(instance_id, &properties, &ctx)
        .expect("recorder block builds");

    let mut elements = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = pipeline
            .by_name(&from.element_id)
            .expect("internal link source element is in the pipeline");
        let dst = pipeline
            .by_name(&to.element_id)
            .expect("internal link sink element is in the pipeline");
        src.link_pads(from.pad_name.as_deref(), &dst, to.pad_name.as_deref())
            .expect("internal recorder link");
    }

    Recorder {
        instance_id: instance_id.to_string(),
        ctx,
        elements,
    }
}

/// An mp4 recorder with `num_video` video and `num_audio` audio tracks.
fn add_mp4_recorder(
    pipeline: &gst::Pipeline,
    instance_id: &str,
    media_root: &Path,
    num_video: u64,
    num_audio: u64,
) -> Recorder {
    add_recorder(
        pipeline,
        instance_id,
        media_root,
        &[
            ("container", PropertyValue::String("mp4".into())),
            ("num_video_tracks", PropertyValue::UInt(num_video)),
            ("num_audio_tracks", PropertyValue::UInt(num_audio)),
        ],
    )
}

/// 320x240@30 H.264 with a keyframe every 10 frames, added to `pipeline`.
/// Returns the encoder, for the caller to link on. `num_buffers` of -1 runs
/// until the pipeline stops.
fn video_source(pipeline: &gst::Pipeline, num_buffers: i32, live: bool) -> gst::Element {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", live)
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
    enc
}

/// AAC from `audiotestsrc`, added to `pipeline`. Returns the encoder, for the
/// caller to link on. `num_buffers` of -1 runs until the pipeline stops.
fn audio_source(pipeline: &gst::Pipeline, num_buffers: i32, live: bool) -> gst::Element {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", live)
        .build()
        .expect("audiotestsrc");
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .expect("avenc_aac");

    pipeline.add_many([&src, &conv, &resample, &enc]).unwrap();
    gst::Element::link_many([&src, &conv, &resample, &enc]).unwrap();
    enc
}

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

/// The files under `<media_root>/recordings` whose name starts with `prefix`,
/// sorted.
fn recordings(media_root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(media_root.join("recordings"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn total_bytes(files: &[PathBuf]) -> u64 {
    files
        .iter()
        .filter_map(|p| p.metadata().ok())
        .map(|m| m.len())
        .sum()
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

/// `splitmuxsink` releases a GOP only once every one of its sink pads has
/// advanced past it, so one track that stops delivering freezes the whole
/// recording — and, through the tee that feeds the recorder, every other branch
/// of that source with it. A participant whose microphone dies mid-session
/// takes their video and their recording down with it, and the program output
/// too if they were the only live source.
///
/// EOS is what takes a pad out of that wait; a GAP event does not, splitmuxsink
/// ignores it on a non-reference stream. So the recorder ends the track rather
/// than trying to keep it idling.
///
/// The first test asserts that a stopped track does not freeze the rest; the
/// other two, that tracks which are still running are left alone — both when
/// the recording is healthy and when the muxer itself stops for a while. Ending
/// every track on a timer would satisfy the first and destroy every recording.
/// The first also pins down *which* track is ended: if the recorder ended the
/// video track — the one still delivering — no video would reach the muxer
/// either.
///
/// The sleeps are fixed because the recorder's stall timeout is: tracks have to
/// be watched for longer than it to show that nothing was ended.
mod stalled_track {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Swallow EOS on `element`'s src pad, so a source that runs out of buffers
    /// looks like a track that simply stopped arriving. This is what the
    /// recorder actually sees: EOS does not cross a WebRTC hop, so a publisher
    /// whose microphone dies just stops sending RTP.
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

    /// Link `source` into `target` through an `identity` that swallows EOS.
    fn link_without_eos(pipeline: &gst::Pipeline, source: &gst::Element, target: &gst::Element) {
        let gate = gst::ElementFactory::make("identity")
            .build()
            .expect("identity");
        drop_eos(&gate);
        pipeline.add(&gate).unwrap();
        source.link(&gate).unwrap();
        gate.link(target).expect("link source into recorder");
    }

    /// A one-video, one-audio recorder with live sources on both inputs that
    /// never send EOS. `audio_buffers` of -1 keeps the audio running.
    fn start_recorder(
        pipeline: &gst::Pipeline,
        instance_id: &str,
        media_root: &Path,
        audio_buffers: i32,
    ) {
        let rec = add_mp4_recorder(pipeline, instance_id, media_root, 1, 1);
        let video = video_source(pipeline, -1, true);
        link_without_eos(pipeline, &video, &rec.input("video_input_0"));
        let audio = audio_source(pipeline, audio_buffers, true);
        link_without_eos(pipeline, &audio, &rec.input("audio_input_0"));
        rec.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(15));
    }

    /// Buffers reaching splitmuxsink's `pad`: what the muxer is actually
    /// consuming, which is what stops when it is waiting on another pad.
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

    /// Video keeps arriving, audio stops after two seconds. The recording must
    /// go on without the audio rather than freeze on it.
    ///
    /// Reverting the fix pins the counted video at zero: with no watchdog to
    /// end the audio track, splitmuxsink never releases another GOP.
    #[test]
    fn a_track_that_stops_does_not_freeze_the_recording() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_stall", media_root, 86); // ~2 s at 1024 samples / 44.1 kHz
        let video_into_muxer = count_into_muxer(&pipeline, "rec_stall", "video");

        // Audio stops at ~2 s and the watchdog ends that track five seconds
        // later. Measure the window after that, so this is about recovery, not
        // the stall.
        std::thread::sleep(Duration::from_secs(8));
        let (buffers_before, bytes_before) = (
            video_into_muxer.load(Ordering::Relaxed),
            total_bytes(&recordings(media_root, "")),
        );
        std::thread::sleep(Duration::from_secs(5));
        let (buffers_after, bytes_after) = (
            video_into_muxer.load(Ordering::Relaxed),
            total_bytes(&recordings(media_root, "")),
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        // 30 fps over five seconds is 150 frames; anything above a couple of
        // seconds worth means the muxer is running rather than waiting on the
        // dead track.
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
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_live", media_root, -1);
        let audio_into_muxer = count_into_muxer(&pipeline, "rec_live", "audio_0");
        let video_into_muxer = count_into_muxer(&pipeline, "rec_live", "video");

        // Well past the stall timeout, so a watchdog that ignored liveness has fired.
        std::thread::sleep(Duration::from_secs(8));
        let (audio_before, video_before) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_secs(5));
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

    /// The muxer stops writing for longer than the stall timeout — a disk or
    /// network share that stalls — while both sources keep delivering. Every
    /// track goes quiet at the muxer, exactly as when one of them dies, but none
    /// of them is at fault, and once the write goes through both have to keep
    /// recording.
    ///
    /// A watchdog that trusts a frozen recording alone ends one track here, and
    /// that track never comes back: it fails the assertion on the track it
    /// picked.
    #[test]
    fn a_muxer_that_stalls_for_every_track_ends_none() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_disk", media_root, -1);
        let audio_into_muxer = count_into_muxer(&pipeline, "rec_disk", "audio_0");
        let video_into_muxer = count_into_muxer(&pipeline, "rec_disk", "video");
        std::thread::sleep(Duration::from_secs(2));

        // Park the muxer's output where its file write would block.
        let splitmuxsink = pipeline
            .by_name("rec_disk:splitmuxsink")
            .expect("splitmuxsink in pipeline")
            .downcast::<gst::Bin>()
            .expect("splitmuxsink is a bin");
        let file_sink_pad = splitmuxsink
            .iterate_sinks()
            .into_iter()
            .filter_map(Result::ok)
            .find_map(|sink| sink.static_pad("sink"))
            .expect("splitmuxsink has created its file sink");
        let block = file_sink_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
            .expect("block the file sink");

        // Long enough for the stall to reach every input, and then the timeout.
        std::thread::sleep(Duration::from_secs(3));
        let stalled_from = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_secs(7));
        let stalled_to = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        file_sink_pad.remove_probe(block);

        // Give the backlog time to drain, then measure.
        std::thread::sleep(Duration::from_secs(3));
        let (audio_before, video_before) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(Duration::from_secs(5));
        let (audio_after, video_after) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        // Without a real stall at the muxer pads this test proves nothing.
        assert_eq!(
            stalled_to, stalled_from,
            "blocking the file sink did not stop the muxer taking buffers (audio, video)"
        );
        assert!(
            audio_after - audio_before >= 45,
            "the audio track was ended during a stall that was not its fault: {} buffers reached the muxer in 5 s after it cleared",
            audio_after - audio_before
        );
        assert!(
            video_after - video_before >= 45,
            "the video track was ended during a stall that was not its fault: {} buffers reached the muxer in 5 s after it cleared",
            video_after - video_before
        );
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

/// `RecorderFileChanged` reports where each file's timeline starts, so that files
/// from separate recorders can be lined up: a sample at file time `t` was in the
/// pipeline at running time `start_running_time_ns + t`.
///
/// Each test checks every file against what went into the recorder, not against
/// another file, so a start that is off for one recorder cannot hide behind the
/// same error in the other.
mod file_start_time {
    use super::*;
    use std::sync::{Arc, Mutex};
    use strom_types::StromEvent;

    const ELEMENTS: &[&str] = &[
        "tee",
        "valve",
        "qtdemux",
        "avdec_h264",
        "videoconvert",
        "appsink",
    ];

    /// 1 ms. Two frames of 30 fps video are 33 ms apart and two AAC frames 21 ms,
    /// so this identifies a sample unambiguously and is inside the 2 ms the
    /// alignment has to meet.
    const TOLERANCE_NS: i64 = 1_000_000;

    /// Per track ("video", "audio"): each sample's file time and size.
    type Samples = HashMap<String, Vec<(i64, usize)>>;

    struct FileStart {
        path: PathBuf,
        start_running_time_ns: u64,
        start_utc_us: u64,
    }

    /// The `RecorderFileChanged` events for `block_id` so far, as absolute paths.
    fn file_starts(
        rx: &mut tokio::sync::broadcast::Receiver<StromEvent>,
        media_root: &Path,
        seen: &mut Vec<(String, FileStart)>,
    ) {
        while let Ok(event) = rx.try_recv() {
            if let StromEvent::RecorderFileChanged {
                block_id,
                filename,
                start_running_time_ns,
                start_utc_us,
                ..
            } = event
            {
                seen.push((
                    block_id,
                    FileStart {
                        path: media_root.join(&filename),
                        start_running_time_ns: start_running_time_ns
                            .unwrap_or_else(|| panic!("{filename}: no start_running_time_ns")),
                        start_utc_us: start_utc_us
                            .unwrap_or_else(|| panic!("{filename}: no start_utc_us")),
                    },
                ));
            }
        }
    }

    fn running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<u64> {
        let event = pad.sticky_event::<gst::event::Segment>(0)?;
        let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
        segment.to_running_time(pts).map(|t| t.nseconds())
    }

    /// Record the running time and size of every buffer leaving `element`: what
    /// the recorder is given, so what its files have to contain.
    fn log_buffers(element: &gst::Element) -> Arc<Mutex<Vec<(u64, usize)>>> {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        element.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, info| {
                if let Some(buffer) = info.buffer() {
                    if let Some(rt) = buffer.pts().and_then(|pts| running_time(pad, pts)) {
                        sink.lock().unwrap().push((rt, buffer.size()));
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );
        log
    }

    /// Each sample in `file`, per track: its time on the file's timeline (stream
    /// time, which follows mp4mux's edit lists) and its size.
    fn demux(file: &Path) -> Samples {
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("filesrc")
            .property("location", file.to_str().unwrap())
            .build()
            .unwrap();
        let demux = gst::ElementFactory::make("qtdemux").build().unwrap();
        pipeline.add_many([&src, &demux]).unwrap();
        src.link(&demux).unwrap();

        let samples: Arc<Mutex<Samples>> = Default::default();
        let pipeline_weak = pipeline.downgrade();
        let collected = Arc::clone(&samples);
        demux.connect_pad_added(move |_, pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            let kind = if pad.name().starts_with("video") {
                "video"
            } else {
                "audio"
            };
            // Not async: qtdemux feeds every track from one thread, so a sink that
            // waits to preroll would stop the other track from ever getting data.
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .unwrap();
            pipeline.add(&sink).unwrap();
            sink.sync_state_with_parent().unwrap();
            pad.link(&sink.static_pad("sink").unwrap()).unwrap();
            let collected = Arc::clone(&collected);
            pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let buffer = info.buffer().unwrap();
                let event = pad.sticky_event::<gst::event::Segment>(0).unwrap();
                let segment = event.segment().downcast_ref::<gst::ClockTime>().unwrap();
                let file_time = segment.to_stream_time(buffer.pts().unwrap()).unwrap();
                collected
                    .lock()
                    .unwrap()
                    .entry(kind.to_string())
                    .or_default()
                    .push((file_time.nseconds() as i64, buffer.size()));
                gst::PadProbeReturn::Ok
            });
        });

        pipeline.set_state(gst::State::Playing).unwrap();
        assert!(
            wait_for_eos(&pipeline, Duration::from_secs(20)),
            "demuxing {file:?} timed out"
        );
        pipeline.set_state(gst::State::Null).unwrap();
        let samples = samples.lock().unwrap().clone();
        samples
    }

    /// Every sample of `track` in `file` must be an input buffer, at running time
    /// `start + file time`. Matching on time alone would pass a start that is off
    /// by a whole number of samples, so audio is matched on size too. Video cannot
    /// be — the recorder's h264parse rewrites every frame — and its content is
    /// checked with flashes instead.
    fn assert_samples_are_inputs(
        file: &FileStart,
        track: &str,
        samples: &[(i64, usize)],
        inputs: &[(u64, usize)],
    ) {
        let check_size = track == "audio";
        assert!(
            !samples.is_empty(),
            "{:?} has no {track} samples",
            file.path
        );
        for &(file_time, size) in samples {
            let at = file.start_running_time_ns as i64 + file_time;
            let nearest = inputs
                .iter()
                .min_by_key(|(rt, _)| (*rt as i64 - at).abs())
                .unwrap();
            assert!(
                (nearest.0 as i64 - at).abs() <= TOLERANCE_NS && (!check_size || nearest.1 == size),
                "{:?}: the {track} sample at file time {:.3} ms, {size} bytes, maps to running \
                 time {:.3} ms, but the nearest input buffer is at {:.3} ms with {} bytes",
                file.path,
                file_time as f64 / 1e6,
                at as f64 / 1e6,
                nearest.0 as f64 / 1e6,
                nearest.1,
            );
        }
    }

    /// Black 320x240@30 from a live source, with every 30th frame painted white.
    /// Returns the element to link on and the running times of the white frames.
    fn flashing_video(
        pipeline: &gst::Pipeline,
        num_buffers: i32,
    ) -> (gst::Element, Arc<Mutex<Vec<u64>>>) {
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", num_buffers)
            .property("is-live", true)
            .property_from_str("pattern", "black")
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "I420")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .unwrap();
        pipeline.add_many([&src, &caps]).unwrap();
        src.link(&caps).unwrap();

        let flashes = Arc::new(Mutex::new(Vec::new()));
        let painted = Arc::clone(&flashes);
        caps.static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() else {
                    return gst::PadProbeReturn::Ok;
                };
                if buffer.offset() % 30 != 0 {
                    return gst::PadProbeReturn::Ok;
                }
                let rt = running_time(pad, buffer.pts().unwrap()).unwrap();
                let mut map = buffer.make_mut().map_writable().unwrap();
                map[..320 * 240].fill(235);
                painted.lock().unwrap().push(rt);
                gst::PadProbeReturn::Ok
            });
        (caps, flashes)
    }

    /// `x264enc` keeping a keyframe every `gop` frames, between `from` and `to`.
    fn encode_video(pipeline: &gst::Pipeline, from: &gst::Element, to: &gst::Element, gop: u32) {
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        let enc = gst::ElementFactory::make("x264enc")
            .property("key-int-max", gop)
            .property_from_str("tune", "zerolatency")
            .build()
            .unwrap();
        pipeline.add_many([&queue, &enc]).unwrap();
        gst::Element::link_many([from, &queue, &enc]).unwrap();
        enc.link(to).unwrap();
    }

    /// A valve that drops everything until [`open`] — a recorder that starts late.
    fn closed_valve(pipeline: &gst::Pipeline) -> gst::Element {
        let valve = gst::ElementFactory::make("valve")
            .property("drop", true)
            .build()
            .unwrap();
        pipeline.add(&valve).unwrap();
        valve
    }

    fn open_after(valve: &gst::Element, delay: Duration) {
        let valve = valve.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            valve.set_property("drop", false);
        });
    }

    /// File times of the white frames in `file`.
    fn flash_file_times(file: &Path) -> Vec<i64> {
        let pipeline = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! qtdemux ! h264parse ! avdec_h264 ! videoconvert ! \
             video/x-raw,format=GRAY8 ! appsink name=out sync=false",
            file.display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let sink = pipeline
            .by_name("out")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut times = Vec::new();
        while let Ok(sample) = sink.pull_sample() {
            let buffer = sample.buffer().unwrap();
            let map = buffer.map_readable().unwrap();
            let mean = map.iter().map(|&y| y as u64).sum::<u64>() / map.len() as u64;
            if mean > 128 {
                let segment = sample
                    .segment()
                    .unwrap()
                    .downcast_ref::<gst::ClockTime>()
                    .unwrap();
                times.push(
                    segment
                        .to_stream_time(buffer.pts().unwrap())
                        .unwrap()
                        .nseconds() as i64,
                );
            }
        }
        pipeline.set_state(gst::State::Null).unwrap();
        times
    }

    /// Every white frame in `file` must be at the running time it was painted at,
    /// one of `flashes`. Returns how many there were.
    fn assert_flashes_land(file: &FileStart, flashes: &[u64]) -> usize {
        let times = flash_file_times(&file.path);
        for &file_time in &times {
            let at = file.start_running_time_ns as i64 + file_time;
            let nearest = flashes
                .iter()
                .min_by_key(|&&rt| (rt as i64 - at).abs())
                .unwrap();
            assert!(
                (*nearest as i64 - at).abs() <= TOLERANCE_NS,
                "{:?} shows a flash at file time {:.3} ms, running time {:.3} ms; the \
                 nearest flash was painted at {:.3} ms",
                file.path,
                file_time as f64 / 1e6,
                at as f64 / 1e6,
                *nearest as f64 / 1e6,
            );
        }
        times.len()
    }

    /// Two video recorders on one camera, each with its own encoder: one records
    /// from the start and splits every second, the other starts 1.3 s late and
    /// splits every two. Every white frame in either recorder's files must land on
    /// the running time it was painted at, and the two recorders' UTC starts must
    /// differ by exactly their running-time difference.
    #[test]
    fn two_video_recorders_line_up_by_their_reported_starts() {
        if !plugins_available() || !common::plugins_available(ELEMENTS) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let events = EventBroadcaster::with_capacity(1024);
        let mut rx = events.subscribe();
        let flow_id = uuid::Uuid::new_v4();

        let early = add_recorder(
            &pipeline,
            "early",
            dir.path(),
            &[
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(0)),
                ("max_size_time_secs", PropertyValue::UInt(1)),
            ],
        );
        let late = add_recorder(
            &pipeline,
            "late",
            dir.path(),
            &[
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(0)),
                ("max_size_time_secs", PropertyValue::UInt(2)),
            ],
        );

        let (camera, flashes) = flashing_video(&pipeline, 150);
        let tee = gst::ElementFactory::make("tee").build().unwrap();
        pipeline.add(&tee).unwrap();
        camera.link(&tee).unwrap();
        encode_video(&pipeline, &tee, &early.input("video_input_0"), 15);
        let valve = closed_valve(&pipeline);
        tee.link(&valve).unwrap();
        encode_video(&pipeline, &valve, &late.input("video_input_0"), 20);

        early.run_setups_for(flow_id, &events);
        late.run_setups_for(flow_id, &events);
        pipeline.set_state(gst::State::Playing).unwrap();
        open_after(&valve, Duration::from_millis(1300));
        assert!(
            wait_for_eos(&pipeline, Duration::from_secs(30)),
            "recording timed out"
        );
        pipeline.set_state(gst::State::Null).unwrap();

        let mut starts = Vec::new();
        file_starts(&mut rx, dir.path(), &mut starts);
        let flashes = flashes.lock().unwrap().clone();
        let mut zero_utc_us = Vec::new();
        for block in ["early", "late"] {
            let files: Vec<&FileStart> = starts
                .iter()
                .filter(|(b, _)| b == block)
                .map(|(_, f)| f)
                .collect();
            assert!(
                files.len() >= 2,
                "{block} wrote {} files, wanted several",
                files.len()
            );
            let mut seen = 0;
            for file in files {
                seen += assert_flashes_land(file, &flashes);
                zero_utc_us
                    .push(file.start_utc_us as i64 - (file.start_running_time_ns / 1000) as i64);
            }
            assert!(seen >= 2, "{block}: only {seen} flashes recorded");
        }
        let (lo, hi) = (
            zero_utc_us.iter().min().unwrap(),
            zero_utc_us.iter().max().unwrap(),
        );
        assert!(
            hi - lo <= 1,
            "files in one flow run map running time to UTC differently: {:?}",
            zero_utc_us
        );
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64;
        assert!(
            (now_us - starts[0].1.start_utc_us as i64).abs() < 60_000_000,
            "start_utc_us {} is not near now {now_us}",
            starts[0].1.start_utc_us
        );
    }

    /// An audio-only recorder that starts late, next to a video-only one. Every
    /// sample in every file must be the input buffer at `start + file time`.
    #[test]
    fn one_track_audio_and_video_files_map_to_their_inputs() {
        if !plugins_available() || !common::plugins_available(ELEMENTS) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let events = EventBroadcaster::with_capacity(4096);
        let mut rx = events.subscribe();
        let flow_id = uuid::Uuid::new_v4();

        let video = add_recorder(
            &pipeline,
            "video",
            dir.path(),
            &[
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(0)),
                ("max_size_time_secs", PropertyValue::UInt(1)),
            ],
        );
        let audio = add_recorder(
            &pipeline,
            "audio",
            dir.path(),
            &[
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
                ("max_size_time_secs", PropertyValue::UInt(1)),
            ],
        );

        let (camera, flashes) = flashing_video(&pipeline, 120);
        let video_tap = gst::ElementFactory::make("identity").build().unwrap();
        pipeline.add(&video_tap).unwrap();
        encode_video(&pipeline, &camera, &video_tap, 15);
        video_tap.link(&video.input("video_input_0")).unwrap();
        let video_in = log_buffers(&video_tap);

        let audio_enc = audio_source(&pipeline, 200, true);
        let valve = closed_valve(&pipeline);
        audio_enc.link(&valve).unwrap();
        valve.link(&audio.input("audio_input_0")).unwrap();
        let audio_in = log_buffers(&valve);

        video.run_setups_for(flow_id, &events);
        audio.run_setups_for(flow_id, &events);
        pipeline.set_state(gst::State::Playing).unwrap();
        open_after(&valve, Duration::from_millis(700));
        assert!(
            wait_for_eos(&pipeline, Duration::from_secs(30)),
            "recording timed out"
        );
        pipeline.set_state(gst::State::Null).unwrap();

        let mut starts = Vec::new();
        file_starts(&mut rx, dir.path(), &mut starts);
        for (block, track, inputs) in [("video", "video", &video_in), ("audio", "audio", &audio_in)]
        {
            let inputs = inputs.lock().unwrap().clone();
            let files: Vec<&FileStart> = starts
                .iter()
                .filter(|(b, _)| b == block)
                .map(|(_, f)| f)
                .collect();
            assert!(
                files.len() >= 2,
                "{block} wrote {} files, wanted several",
                files.len()
            );
            let mut flashes_seen = 0;
            for file in files {
                let samples = demux(&file.path);
                assert_samples_are_inputs(file, track, &samples[track], &inputs);
                if track == "video" {
                    flashes_seen += assert_flashes_land(file, &flashes.lock().unwrap());
                }
            }
            assert!(
                track == "audio" || flashes_seen >= 2,
                "only {flashes_seen} flashes recorded"
            );
        }
    }

    /// A recorder with a video and an audio track, where audio arrives half a
    /// second before video. mp4mux starts the first file at the audio, so that is
    /// where the reported start has to be — not at the video keyframe
    /// splitmuxsink hands `format-location-full`.
    #[test]
    fn a_first_file_starts_at_the_track_that_arrived_first() {
        if !plugins_available() || !common::plugins_available(ELEMENTS) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pipeline = gst::Pipeline::new();
        let events = EventBroadcaster::with_capacity(1024);
        let mut rx = events.subscribe();

        let rec = add_recorder(
            &pipeline,
            "rec",
            dir.path(),
            &[
                ("num_video_tracks", PropertyValue::UInt(1)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
                ("max_size_time_secs", PropertyValue::UInt(1)),
            ],
        );

        let (camera, flashes) = flashing_video(&pipeline, 90);
        let valve = closed_valve(&pipeline);
        camera.link(&valve).unwrap();
        encode_video(&pipeline, &valve, &rec.input("video_input_0"), 10);
        let video_in = log_buffers(&valve);

        let audio_enc = audio_source(&pipeline, 150, true);
        audio_enc.link(&rec.input("audio_input_0")).unwrap();
        let audio_in = log_buffers(&audio_enc);

        rec.run_setups_for(uuid::Uuid::new_v4(), &events);
        pipeline.set_state(gst::State::Playing).unwrap();
        open_after(&valve, Duration::from_millis(500));
        assert!(
            wait_for_eos(&pipeline, Duration::from_secs(30)),
            "recording timed out"
        );
        pipeline.set_state(gst::State::Null).unwrap();

        let mut starts = Vec::new();
        file_starts(&mut rx, dir.path(), &mut starts);
        assert!(
            starts.len() >= 2,
            "wrote {} files, wanted several",
            starts.len()
        );
        let video_in = video_in.lock().unwrap().clone();
        let audio_in = audio_in.lock().unwrap().clone();
        assert!(
            video_in[0].0 > audio_in[0].0 + 300_000_000,
            "video did not start well after audio: {} vs {}",
            video_in[0].0,
            audio_in[0].0
        );
        let flashes = flashes.lock().unwrap().clone();
        for (_, file) in &starts {
            let samples = demux(&file.path);
            assert_samples_are_inputs(file, "audio", &samples["audio"], &audio_in);
            assert_flashes_land(file, &flashes);
        }
    }

    /// A flow with direct media timing keeps base time 0 on every run, so a run on
    /// another clock must not reuse the UTC mapping of the run before it. Two runs
    /// of one flow, as `configure_clock` sets them up: monotonic, then realtime.
    #[test]
    fn a_rerun_on_another_clock_maps_to_utc_afresh() {
        if !plugins_available() || !common::plugins_available(ELEMENTS) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let flow_id = uuid::Uuid::new_v4();
        let realtime: gst::Clock = gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build()
            .upcast();
        for (run, clock) in [
            ("monotonic", gst::SystemClock::obtain()),
            ("realtime", realtime),
        ] {
            let pipeline = gst::Pipeline::new();
            pipeline.use_clock(Some(&clock));
            pipeline.set_base_time(gst::ClockTime::ZERO);
            pipeline.set_start_time(gst::ClockTime::NONE);
            let events = EventBroadcaster::with_capacity(64);
            let mut rx = events.subscribe();
            let rec = add_recorder(
                &pipeline,
                run,
                dir.path(),
                &[
                    ("num_video_tracks", PropertyValue::UInt(1)),
                    ("num_audio_tracks", PropertyValue::UInt(0)),
                ],
            );
            feed_video(&pipeline, &rec.input("video_input_0"), 30);
            rec.run_setups_for(flow_id, &events);
            pipeline.set_state(gst::State::Playing).unwrap();
            assert!(
                wait_for_eos(&pipeline, Duration::from_secs(20)),
                "{run}: timed out"
            );
            pipeline.set_state(gst::State::Null).unwrap();

            let mut starts = Vec::new();
            file_starts(&mut rx, dir.path(), &mut starts);
            let now_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64;
            let start_us = starts[0].1.start_utc_us as i64;
            assert!(
                (now_us - start_us).abs() < 60_000_000,
                "{run}: start_utc_us {start_us} is {:.0} s from now",
                (start_us - now_us) as f64 / 1e6
            );
        }
    }
}
