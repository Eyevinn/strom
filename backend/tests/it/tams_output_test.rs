//! Regression tests for the TAMS Output block, built through the real
//! `TamsOutputBuilder` and fed from test sources the way a flow would feed it.
//!
//! - `idle_input`: an unfed TAMS Output must not hold the pipeline short of
//!   PLAYING, and one that gets data must still write segments.
//! - `shared_timeline`: two TAMS Outputs in one pipeline place the same buffer at
//!   the same TAI, even when one of them gets its data later (#966).
//!
//! Except in `shared_timeline` (which runs a stub gateway), the gateway URL
//! points at a closed local port. The block registers its flow
//! and uploads lazily, so building and running it needs no TAMS server; failed
//! uploads leave the segment files on disk, where these tests read them.

use crate::common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use strom::blocks::builtin::tams_output::TamsOutputBuilder;
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "appsink",
    "filesink",
    "mpegtsmux",
    "aacparse",
    "avenc_aac",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "appsrc",
    "identity",
];

/// Nothing listens here, so every upload fails and the segments stay on disk.
const UNREACHABLE_GATEWAY: &str = "http://127.0.0.1:9";

fn plugins_available() -> bool {
    common::plugins_available(REQUIRED)
}

/// A TAMS Output block built through the real builder and added to a pipeline.
struct TamsOutput {
    instance_id: String,
    /// Holds the element-setup hooks; see [`TamsOutput::run_setups`].
    ctx: BlockBuildContext,
    elements: HashMap<String, gst::Element>,
}

impl TamsOutput {
    /// The block element with the internal id `name`, e.g. `audio_input_0`.
    fn input(&self, name: &str) -> gst::Element {
        self.elements
            .get(&format!("{}:{}", self.instance_id, name))
            .cloned()
            .unwrap_or_else(|| panic!("TAMS Output {} exposes no {}", self.instance_id, name))
    }

    /// Run the element-setup hooks, as the pipeline manager does before PLAYING.
    /// They start the uploader, which needs a Tokio runtime to spawn on.
    fn run_setups(&self, runtime: &tokio::runtime::Runtime) {
        self.run_setups_in_flow(runtime, uuid::Uuid::new_v4());
    }

    /// Run the element-setup hooks as part of the Strom flow `flow_id`. Blocks of
    /// one pipeline share a flow id.
    fn run_setups_in_flow(&self, runtime: &tokio::runtime::Runtime, flow_id: uuid::Uuid) {
        let _guard = runtime.enter();
        for setup in self.ctx.take_element_setups() {
            setup(flow_id, EventBroadcaster::with_capacity(16));
        }
    }

    /// The directory the block writes its MPEG-TS segments to.
    fn segment_dir(&self) -> PathBuf {
        let safe_id: String = self
            .instance_id
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        std::env::temp_dir().join(format!("strom-tams-{}-mux", safe_id))
    }

    /// Drop the block's elements but keep its segment directory, so the
    /// pipeline can be finalized and the uploader flushes the last segment.
    fn release_elements(&mut self) {
        self.elements.clear();
    }

    /// The segment files written so far.
    fn segments(&self) -> Vec<PathBuf> {
        segment_files(&self.segment_dir())
    }
}

impl Drop for TamsOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.segment_dir());
    }
}

fn segment_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with("seg_") && n.ends_with(".ts")
                    })
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

/// Build an audio-only MPEG-TS TAMS Output, add it to `pipeline`, and make the
/// links it declares, as the pipeline manager would. The instance id is unique
/// per call, so parallel tests never share a segment directory.
fn add_tams_output(pipeline: &gst::Pipeline, name: &str) -> TamsOutput {
    add_tams_output_to(pipeline, name, UNREACHABLE_GATEWAY, 2)
}

fn add_tams_output_to(
    pipeline: &gst::Pipeline,
    name: &str,
    gateway_url: &str,
    segment_secs: u64,
) -> TamsOutput {
    add_tams_output_with(pipeline, name, gateway_url, segment_secs, false)
}

/// As [`add_tams_output_to`], with one video track instead of one audio track
/// when `video` is set.
fn add_tams_output_with(
    pipeline: &gst::Pipeline,
    name: &str,
    gateway_url: &str,
    segment_secs: u64,
    video: bool,
) -> TamsOutput {
    add_tams_output_segment(
        pipeline,
        name,
        gateway_url,
        ("segment_duration_ms", segment_secs * 1000),
        video,
    )
}

/// As [`add_tams_output_with`], with the segment length given as a property
/// name and value (`segment_duration_ms`).
fn add_tams_output_segment(
    pipeline: &gst::Pipeline,
    name: &str,
    gateway_url: &str,
    segment: (&str, u64),
    video: bool,
) -> TamsOutput {
    let instance_id = format!("{}_{}", name, uuid::Uuid::new_v4().simple());
    let properties: HashMap<String, PropertyValue> = [
        ("gateway_url", PropertyValue::String(gateway_url.into())),
        (segment.0, PropertyValue::UInt(segment.1)),
        ("container", PropertyValue::String("mpegts".into())),
        ("num_video_tracks", PropertyValue::UInt(video as u64)),
        ("num_audio_tracks", PropertyValue::UInt(!video as u64)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = TamsOutputBuilder
        .build(&instance_id, &properties, &ctx)
        .expect("TAMS Output block builds");

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
            .expect("internal TAMS Output link");
    }

    TamsOutput {
        instance_id,
        ctx,
        elements,
    }
}

/// Live AAC from `audiotestsrc` into `target`. `num_buffers` of -1 runs until
/// the pipeline stops.
fn feed_audio(pipeline: &gst::Pipeline, target: &gst::Element, num_buffers: i32) {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", true)
        .build()
        .expect("audiotestsrc");
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .expect("avenc_aac");

    pipeline.add_many([&src, &conv, &resample, &enc]).unwrap();
    gst::Element::link_many([&src, &conv, &resample, &enc, target]).unwrap();
}

/// An input that stays connected but never carries data, like an Audio
/// Encoder behind an empty mixer channel's direct out.
fn feed_nothing(pipeline: &gst::Pipeline, target: &gst::Element) {
    let src = gst::ElementFactory::make("appsrc")
        .property("is-live", true)
        .property_from_str("format", "time")
        .build()
        .expect("appsrc");
    pipeline.add(&src).unwrap();
    src.link(target)
        .expect("link silent source into TAMS Output");
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

/// The first error on the bus, if any, as "<source>: <message>".
fn bus_error(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus().expect("pipeline has a bus");
    while let Some(msg) = bus.pop() {
        if let gst::MessageView::Error(err) = msg.view() {
            return Some(format!(
                "{:?}: {} ({:?})",
                err.src().map(|s| s.path_string()),
                err.error(),
                err.debug()
            ));
        }
    }
    None
}

/// x264 with a GOP of 25 and B-frames (two per GOP gap, always used), at low
/// latency otherwise: the segmenter routes buffers and takes where media
/// ends from PTS, which B-frames reorder (see the repo rule on tests that
/// read video timestamps). `tune=zerolatency` makes no B-frames.
fn x264_with_bframes() -> gst::Element {
    gst::ElementFactory::make("x264enc")
        .property("key-int-max", 25u32)
        .property("bframes", 2u32)
        .property("b-adapt", false)
        .property("rc-lookahead", 2i32)
        .property("sync-lookahead", 0i32)
        .property("sliced-threads", true)
        .property_from_str("tune", "fastdecode")
        .property_from_str("speed-preset", "ultrafast")
        .build()
        .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// A `splitmuxsink` only completes READY->PAUSED once it has prerolled a
/// buffer, so a TAMS Output whose input carries no data holds the whole
/// pipeline short of PLAYING, and the flow reports PAUSED for as long as the
/// input stays empty.
///
/// The two tests are a pair: one asserts an input with no data does not block
/// PLAYING, the other that the block still writes segments once data arrives.
/// Keeping the sink locked for good would satisfy the first and break the second.
mod idle_input {
    use super::*;

    /// One TAMS Output with data, one without: the pipeline must reach PLAYING,
    /// and the one with data must write a segment.
    #[test]
    fn tams_output_without_data_does_not_block_playing() {
        if !plugins_available() {
            return;
        }
        let runtime = runtime();

        let pipeline = gst::Pipeline::new();
        let live = add_tams_output(&pipeline, "tams_live");
        let idle = add_tams_output(&pipeline, "tams_idle");
        feed_audio(&pipeline, &live.input("audio_input_0"), -1);
        feed_nothing(&pipeline, &idle.input("audio_input_0"));
        live.run_setups(&runtime);
        idle.run_setups(&runtime);

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(15));
        // Long enough for the fed block to close its first 2 s segment.
        std::thread::sleep(Duration::from_secs(3));
        let live_segments = live.segments();
        let idle_segments = idle.segments();
        let error = bus_error(&pipeline);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
        runtime.shutdown_background();

        assert_eq!(
            (result.expect("pipeline state readable"), current, pending),
            (
                gst::StateChangeSuccess::Success,
                gst::State::Playing,
                gst::State::VoidPending
            ),
            "a TAMS Output with no data held the pipeline out of PLAYING"
        );
        assert_eq!(error, None, "the pipeline posted an error");
        assert!(
            total_bytes(&live_segments) > 0,
            "the TAMS Output with data wrote nothing: {:?}",
            live_segments
        );
        assert_eq!(
            total_bytes(&idle_segments),
            0,
            "the TAMS Output with no data wrote content: {:?}",
            idle_segments
        );
    }

    /// A TAMS Output that gets data must still write it, through to EOS.
    ///
    /// Counterpart to the test above: a sink kept out of the pipeline for good
    /// would satisfy that one and write nothing.
    #[test]
    fn tams_output_with_data_still_writes_segments() {
        if !plugins_available() {
            return;
        }
        let runtime = runtime();

        let pipeline = gst::Pipeline::new();
        let tams = add_tams_output(&pipeline, "tams_live");
        // About 2.3 s of audio: more than one 2 s segment.
        feed_audio(&pipeline, &tams.input("audio_input_0"), 100);
        tams.run_setups(&runtime);

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let reached_eos = wait_for_eos(&pipeline, Duration::from_secs(30));
        let segments = tams.segments();
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
        runtime.shutdown_background();

        assert!(reached_eos, "pipeline never reached EOS within 30s");
        assert!(
            segments.len() >= 2,
            "expected the audio to span two segments, got {:?}",
            segments
        );
        assert!(
            total_bytes(&segments) > 0,
            "the segments are empty: {:?}",
            segments
        );
    }
}

/// A stub TAMS gateway: accepts flows, hands out upload URLs on itself, and
/// records each registered segment's `(tams flow id, timerange)`.
mod stub_gateway {
    use axum::extract::{Path, State};
    use axum::routing::{post, put};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub type Registered = Arc<Mutex<Vec<(String, String)>>>;
    /// Each registered segment's timerange and the bytes uploaded for it.
    pub type Segments = Arc<Mutex<Vec<(String, axum::body::Bytes)>>>;

    #[derive(Clone)]
    struct Stub {
        base: String,
        registered: Registered,
        uploads: Arc<Mutex<HashMap<String, axum::body::Bytes>>>,
        segments: Segments,
    }

    /// Start the stub on a free local port; returns its base URL.
    pub fn start(runtime: &tokio::runtime::Runtime) -> (String, Registered) {
        let (base, registered, _) = start_keeping_bytes(runtime);
        (base, registered)
    }

    /// As [`start`], also keeping each registered segment's bytes.
    pub fn start_keeping_bytes(
        runtime: &tokio::runtime::Runtime,
    ) -> (String, Registered, Segments) {
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind stub gateway");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let registered: Registered = Arc::default();
        let segments: Segments = Arc::default();
        let stub = Stub {
            base: base.clone(),
            registered: registered.clone(),
            uploads: Arc::default(),
            segments: segments.clone(),
        };
        let app =
            Router::new()
                .route(
                    "/flows/{id}",
                    put(|| async { axum::http::StatusCode::CREATED }),
                )
                .route(
                    "/flows/{id}/storage",
                    post(|State(s): State<Stub>, Path(id): Path<String>| async move {
                        let object_id = format!("{}-{}", id, uuid::Uuid::new_v4());
                        Json(serde_json::json!({ "media_objects": [{
                            "object_id": object_id,
                            "put_url": {
                                "url": format!("{}/upload/{}", s.base, object_id),
                                "content-type": "video/mp2t"
                            }
                        }]}))
                    }),
                )
                // Read the whole body before answering. Answering first resets the
                // connection under a large upload, and the uploader's retry backoff
                // (1 s and up) then held segments back past the end of the test.
                .route(
                    "/upload/{obj}",
                    put(
                        |State(s): State<Stub>,
                         Path(obj): Path<String>,
                         body: axum::body::Bytes| async move {
                            s.uploads.lock().unwrap().insert(obj, body);
                            axum::http::StatusCode::OK
                        },
                    ),
                )
                .route(
                    "/flows/{id}/segments",
                    post(
                        |State(s): State<Stub>,
                         Path(id): Path<String>,
                         Json(body): Json<serde_json::Value>| async move {
                            let tr = body["timerange"].as_str().unwrap_or_default().to_string();
                            let obj = body["object_id"].as_str().unwrap_or_default();
                            if let Some(bytes) = s.uploads.lock().unwrap().remove(obj) {
                                s.segments.lock().unwrap().push((tr.clone(), bytes));
                            }
                            s.registered.lock().unwrap().push((id, tr));
                            axum::http::StatusCode::CREATED
                        },
                    ),
                )
                .with_state(stub);
        runtime.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, registered, segments)
    }
}

/// Start of a TAMS timerange `[s:ns_s:ns)` in nanoseconds.
fn timerange_start_ns(tr: &str) -> u64 {
    let start = tr
        .trim_start_matches(['[', '('])
        .split('_')
        .next()
        .unwrap_or("");
    let (s, ns) = start.split_once(':').unwrap_or((start, "0"));
    s.parse::<u64>().unwrap() * 1_000_000_000 + ns.parse::<u64>().unwrap()
}

/// Several TAMS Outputs in one pipeline must agree on where a buffer sits on the
/// TAI timeline. Each block used to sample TAI on its own when its first segment
/// opened, so a block whose data arrived later put the same buffers later on the
/// timeline (#966).
mod shared_timeline {
    use super::*;

    /// The same encoded audio goes to two TAMS Outputs, one of them through a
    /// queue that holds it back 600 ms. Both must register the same timeranges,
    /// split on the TAI grid.
    #[test]
    fn late_arrival_does_not_shift_the_timeline() {
        if !plugins_available() || !common::plugins_available(&["tee", "queue"]) {
            return;
        }
        let runtime = runtime();
        let (gateway, registered) = stub_gateway::start(&runtime);

        let pipeline = gst::Pipeline::new();
        let prompt = add_tams_output_to(&pipeline, "tams_prompt", &gateway, 1);
        let late = add_tams_output_to(&pipeline, "tams_late", &gateway, 1);

        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
        let resample = gst::ElementFactory::make("audioresample").build().unwrap();
        let enc = gst::ElementFactory::make("avenc_aac").build().unwrap();
        let tee = gst::ElementFactory::make("tee").build().unwrap();
        let q_prompt = gst::ElementFactory::make("queue").build().unwrap();
        // Holds buffers back without changing their timestamps: late arrival.
        let q_late = gst::ElementFactory::make("queue")
            .property("min-threshold-time", 600_000_000u64)
            .property("max-size-time", 0u64)
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .build()
            .unwrap();
        pipeline
            .add_many([&src, &conv, &resample, &enc, &tee, &q_prompt, &q_late])
            .unwrap();
        gst::Element::link_many([&src, &conv, &resample, &enc, &tee]).unwrap();
        gst::Element::link_many([&tee, &q_prompt, &prompt.input("audio_input_0")]).unwrap();
        gst::Element::link_many([&tee, &q_late, &late.input("audio_input_0")]).unwrap();

        let flow_id = uuid::Uuid::new_v4();
        prompt.run_setups_in_flow(&runtime, flow_id);
        late.run_setups_in_flow(&runtime, flow_id);

        pipeline.set_state(gst::State::Playing).unwrap();
        // Several 1 s segments per block, then let the uploads land.
        std::thread::sleep(Duration::from_millis(4_500));
        let error = bus_error(&pipeline);
        pipeline.set_state(gst::State::Null).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let registered = registered.lock().unwrap().clone();
        runtime.shutdown_background();
        assert_eq!(error, None, "the pipeline posted an error");

        let flow_of = |t: &TamsOutput| {
            uuid::Uuid::new_v5(
                &uuid::Uuid::from_u128(0x7a3f_2b10_5c84_4e6d_9f12_a8b7_c6d5_e4f3),
                format!("{}:mux", t.instance_id).as_bytes(),
            )
            .to_string()
        };
        let ranges = |flow: String| -> Vec<String> {
            registered
                .iter()
                .filter(|(f, _)| *f == flow)
                .map(|(_, tr)| tr.clone())
                .collect()
        };
        let a = ranges(flow_of(&prompt));
        let b = ranges(flow_of(&late));
        assert!(
            a.len() >= 2 && b.len() >= 2,
            "too few segments registered: {:?} / {:?}",
            a,
            b
        );
        // Same input buffers, same split points: the timeranges must be equal,
        // not shifted by the 600 ms the late block waited for its data.
        let n = a.len().min(b.len());
        assert_eq!(
            a[..n],
            b[..n],
            "first segment starts differ by {} ms",
            timerange_start_ns(&b[0]) as i128 / 1_000_000
                - timerange_start_ns(&a[0]) as i128 / 1_000_000
        );
        // After the first, segments start on the 1 s TAI grid: at the first AAC
        // frame (1024 samples, ~21 ms) at or after a whole second.
        for tr in &a[1..] {
            let into_second = timerange_start_ns(tr) % 1_000_000_000;
            assert!(
                into_second < 25_000_000,
                "segment {} starts {} ms past the 1 s TAI grid",
                tr,
                into_second / 1_000_000
            );
        }
    }

    /// With an encoder in front, segments after the first two start on the TAI
    /// grid, also when the GOP is shorter than a segment and its keyframes are
    /// off the grid: TAMS Output asks for a keyframe one grid point ahead.
    /// splitmuxsink's own request comes only when the fragment before the split
    /// opens, after the encoder has passed the split point, so it forced a
    /// keyframe at once, off the grid, and every split followed it.
    ///
    /// The encoder is the Video Encoder block with
    /// `min_keyframe_request_interval_ms` 0.
    #[test]
    fn encoder_keyframes_follow_the_grid() {
        keyframes_follow_the_grid(Some(0));
    }

    /// The same with the Video Encoder block's default 1 s keyframe-request
    /// floor (kept for WebRTC PLI storms). The base-class floor it used to set
    /// dropped grid requests that came sooner than a second after the last
    /// forced keyframe, so boundaries left the grid with nothing in the log.
    /// The block now limits only immediate requests.
    #[test]
    fn encoder_keyframes_follow_the_grid_with_default_floor() {
        keyframes_follow_the_grid(None);
    }

    /// 500 ms segments (`segment_duration_ms`): boundaries on the 500 ms TAI
    /// grid, with a GOP of one segment, from the fourth segment on: the
    /// keyframe for the third is asked for when the first opens, only 0.5 to
    /// 1 s ahead, less than x264's latency in about a third of the runs.
    #[test]
    fn encoder_keyframes_follow_a_subsecond_grid() {
        keyframes_follow_a_grid(None, ("segment_duration_ms", 500), 500_000_000);
    }

    fn keyframes_follow_the_grid(min_keyframe_request_interval_ms: Option<u64>) {
        keyframes_follow_a_grid(
            min_keyframe_request_interval_ms,
            ("segment_duration_ms", 1000),
            1_000_000_000,
        );
    }

    fn keyframes_follow_a_grid(
        min_keyframe_request_interval_ms: Option<u64>,
        segment: (&str, u64),
        segment_ns: u64,
    ) {
        if !plugins_available()
            || !common::plugins_available(&[
                "videotestsrc",
                "x264enc",
                "h264parse",
                "videoconvert",
                "capsfilter",
            ])
        {
            return;
        }
        let runtime = runtime();
        let (gateway, registered) = stub_gateway::start(&runtime);

        let pipeline = gst::Pipeline::new();
        let out = add_tams_output_segment(&pipeline, "tams_video", &gateway, segment, true);
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 320)
                    .field("height", 180)
                    .field("framerate", gst::Fraction::new(50, 1))
                    .build(),
            )
            .build()
            .unwrap();
        // The Video Encoder block as an ingest flow sets it up: a 0.5 s GOP.
        // Its natural keyframes fall wherever the stream started, not on the
        // grid.
        let mut enc_props: HashMap<String, PropertyValue> = [
            ("codec", PropertyValue::String("h264".into())),
            (
                "encoder_preference",
                PropertyValue::String("software".into()),
            ),
            ("quality_preset", PropertyValue::String("fast".into())),
            ("tune", PropertyValue::String("fastdecode".into())),
            ("keyframe_interval", PropertyValue::UInt(25)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if let Some(ms) = min_keyframe_request_interval_ms {
            enc_props.insert(
                "min_keyframe_request_interval_ms".to_string(),
                PropertyValue::UInt(ms),
            );
        }
        let enc_id = format!("videoenc_{}", uuid::Uuid::new_v4().simple());
        let enc_built = VideoEncBuilder
            .build(&enc_id, &enc_props, &common::block::context())
            .expect("Video Encoder block builds");
        let enc_elements = common::block::install(&pipeline, &enc_built);
        // B-frames, which the block does not set: see the repo rule on tests
        // that read video timestamps.
        enc_elements[&format!("{}:encoder", enc_id)].set_property("bframes", 2u32);
        let enc_in = &enc_elements[&format!("{}:input", enc_id)];
        let enc_out = &enc_elements[&format!("{}:capsfilter", enc_id)];
        pipeline.add_many([&src, &caps]).unwrap();
        gst::Element::link_many([&src, &caps, enc_in]).unwrap();
        enc_out.link(&out.input("video_input_0")).unwrap();

        out.run_setups_in_flow(&runtime, uuid::Uuid::new_v4());
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(6_500));
        let error = bus_error(&pipeline);
        pipeline.set_state(gst::State::Null).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let registered = registered.lock().unwrap().clone();
        runtime.shutdown_background();
        assert_eq!(error, None, "the pipeline posted an error");

        let ranges: Vec<String> = registered.into_iter().map(|(_, tr)| tr).collect();
        assert!(
            ranges.len() >= 4,
            "too few segments registered: {:?}",
            ranges
        );
        // The first split is requested too late to be forced, and those
        // requested at the same moment less than the encoder's latency ahead
        // may be forced off the grid (with 500 ms segments the second split).
        // From the first fragment requested far enough ahead on, each starts
        // within a frame (20 ms) of a TAI grid point (a multiple of the
        // segment length).
        let first_on_grid = strom::tams::tai_mapping::first_on_grid_fragment(segment_ns) as usize;
        assert!(
            ranges.len() > first_on_grid + 1,
            "too few segments registered: {:?}",
            ranges
        );
        for tr in &ranges[first_on_grid..] {
            let into_segment = timerange_start_ns(tr) % segment_ns;
            assert!(
                into_segment < 20_000_000,
                "segment {} starts {} ms past the {} ms TAI grid (all: {:?})",
                tr,
                into_segment / 1_000_000,
                segment_ns / 1_000_000,
                ranges
            );
        }
    }
}

/// The last segment of a run, which no next segment closes, must register the
/// duration it holds, not a full segment length.
mod last_segment {
    use super::*;

    /// End of a TAMS timerange `[s:ns_s:ns)` in nanoseconds.
    fn timerange_end_ns(tr: &str) -> u64 {
        let end = tr
            .trim_end_matches([']', ')'])
            .rsplit('_')
            .next()
            .unwrap_or("");
        let (s, ns) = end.split_once(':').unwrap_or((end, "0"));
        s.parse::<u64>().unwrap() * 1_000_000_000 + ns.parse::<u64>().unwrap()
    }

    #[test]
    fn last_segment_before_eos_ends_at_its_last_frame() {
        if !plugins_available()
            || !common::plugins_available(&["videotestsrc", "x264enc", "capsfilter"])
        {
            return;
        }
        const FRAME_NS: u64 = 20_000_000;
        const FRAMES: u64 = 80; // 1.6 s at 50 fps
        let runtime = runtime();
        let (gateway, registered) = stub_gateway::start(&runtime);

        let pipeline = gst::Pipeline::new();
        let mut out = add_tams_output_with(&pipeline, "tams_tail", &gateway, 1, true);
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", FRAMES as i32)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 160)
                    .field("height", 90)
                    .field("framerate", gst::Fraction::new(50, 1))
                    .build(),
            )
            .build()
            .unwrap();
        let enc = x264_with_bframes();
        pipeline.add_many([&src, &caps, &enc]).unwrap();
        gst::Element::link_many([&src, &caps, &enc]).unwrap();
        enc.link(&out.input("video_input_0")).unwrap();

        out.run_setups_in_flow(&runtime, uuid::Uuid::new_v4());
        pipeline.set_state(gst::State::Playing).unwrap();
        let reached_eos = wait_for_eos(&pipeline, Duration::from_secs(30));
        pipeline.set_state(gst::State::Null).unwrap();
        // Finalize the pipeline as stopping a flow does: the uploader's channel
        // closes and it flushes the last segment.
        out.release_elements();
        drop(enc);
        drop(caps);
        drop(src);
        drop(pipeline);
        let deadline = Instant::now() + Duration::from_secs(10);
        while registered.lock().unwrap().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(500));
        let mut ranges: Vec<String> = registered
            .lock()
            .unwrap()
            .iter()
            .map(|(_, tr)| tr.clone())
            .collect();
        runtime.shutdown_background();
        assert!(reached_eos, "pipeline never reached EOS");
        ranges.sort_by_key(|tr| timerange_start_ns(tr));
        assert!(ranges.len() >= 2, "too few segments: {:?}", ranges);
        // The segments cover the frames exactly: from the first frame to the
        // end of the last one, with the last segment shorter than 1 s.
        let first = timerange_start_ns(&ranges[0]);
        let last = ranges.last().unwrap();
        assert_eq!(
            timerange_end_ns(last) - first,
            FRAMES * FRAME_NS,
            "segments: {:?}",
            ranges
        );
        assert!(
            timerange_end_ns(last) - timerange_start_ns(last) < 1_000_000_000,
            "last segment claims a full second: {}",
            last
        );
    }

    /// Frames a decoder gets out of an MPEG-TS segment.
    pub(super) fn decoded_frames(ts: &[u8]) -> u64 {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), ts).unwrap();
        let pipeline = gst::parse::launch(&format!(
            "filesrc location=\"{}\" ! tsdemux ! h264parse ! avdec_h264 ! fakesink name=out",
            file.path().display()
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let frames = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = frames.clone();
        let sink = pipeline.by_name("out").unwrap();
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        let ended = wait_for_eos(&pipeline, Duration::from_secs(10));
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(ended, "decoding a segment never reached EOS");
        frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// One run of live video into a TAMS Output, stopped as stopping a flow
    /// does (stop drains, then NULL, no EOS from upstream) `run_ms` in.
    /// Returns the last segment's timerange and decoded frame count.
    fn last_segment_after_stop(run_ms: u64) -> (String, u64) {
        let mut segs = run_and_stop(run_ms, None);
        let (tr, bytes) = segs.pop().unwrap();
        (tr, decoded_frames(&bytes))
    }

    /// One run of live video into a TAMS Output, stopped as stopping a flow
    /// does `run_ms` in. The frames whose running time is in `stall` (ms,
    /// `[from, to)`) never reach the encoder, as when the input stalls.
    /// Returns every registered segment's timerange and bytes, in order.
    fn run_and_stop(run_ms: u64, stall: Option<(u64, u64)>) -> Vec<(String, axum::body::Bytes)> {
        let runtime = runtime();
        let (gateway, registered, segments) = stub_gateway::start_keeping_bytes(&runtime);

        let pipeline = gst::Pipeline::new();
        let mut out = add_tams_output_with(&pipeline, "tams_stop", &gateway, 1, true);
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 160)
                    .field("height", 90)
                    .field("framerate", gst::Fraction::new(50, 1))
                    .build(),
            )
            .build()
            .unwrap();
        let enc = x264_with_bframes();
        pipeline.add_many([&src, &caps, &enc]).unwrap();
        gst::Element::link_many([&src, &caps, &enc]).unwrap();
        enc.link(&out.input("video_input_0")).unwrap();
        if let Some((from, to)) = stall {
            let (from, to) = (from * 1_000_000, to * 1_000_000);
            caps.static_pad("src")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                    let pts = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds());
                    match pts {
                        Some(p) if p >= from && p < to => gst::PadProbeReturn::Drop,
                        _ => gst::PadProbeReturn::Ok,
                    }
                });
        }

        out.run_setups_in_flow(&runtime, uuid::Uuid::new_v4());
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(run_ms));
        // As `PipelineManager::stop`: the stop drains, then NULL.
        for drain in out.ctx.take_stop_drains() {
            drain();
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out.release_elements();
        drop(enc);
        drop(caps);
        drop(src);
        drop(pipeline);

        let mut last_len = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(300));
            let len = registered.lock().unwrap().len();
            if len >= 2 && len == last_len {
                break;
            }
            last_len = len;
        }
        let mut segs = segments.lock().unwrap().clone();
        runtime.shutdown_background();
        segs.sort_by_key(|(tr, _)| timerange_start_ns(tr));
        assert!(segs.len() >= 2, "too few segments: {}", segs.len());
        segs
    }

    /// The input stalls for 2.3 s inside a segment (frames dropped before
    /// the encoder, as by a leaky queue under load). The segment before the
    /// stall must end where its media ends, not claim the stall
    /// (seen live: a 6.248 s timerange over 0.9 s of video).
    #[test]
    fn a_stalled_input_leaves_a_hole_not_a_long_segment() {
        if !plugins_available()
            || !common::plugins_available(&[
                "videotestsrc",
                "x264enc",
                "capsfilter",
                "tsdemux",
                "h264parse",
                "avdec_h264",
            ])
        {
            return;
        }
        const FRAME_NS: u64 = 20_000_000;
        let segs = run_and_stop(4_500, Some((1_300, 3_600)));
        let ranges: Vec<(u64, u64)> = segs
            .iter()
            .map(|(tr, _)| (timerange_start_ns(tr), timerange_end_ns(tr)))
            .collect();
        let all: Vec<&String> = segs.iter().map(|(tr, _)| tr).collect();
        for (s, e) in &ranges {
            assert!(
                e - s <= 1_000_000_000 + FRAME_NS,
                "a segment claims more than its length: {:?}",
                all
            );
        }
        // The stall is a hole between two segments.
        let hole = ranges
            .windows(2)
            .map(|w| w[1].0.saturating_sub(w[0].1))
            .max()
            .unwrap_or(0);
        assert!(hole >= 2_000_000_000, "no hole for the stall: {:?}", all);
        // Every segment holds what its timerange claims.
        for ((tr, bytes), (s, e)) in segs.iter().zip(&ranges) {
            assert_eq!(
                decoded_frames(bytes) * FRAME_NS,
                e - s,
                "segment {} of {:?}",
                tr,
                all
            );
        }
    }

    /// A stopped flow sends no EOS. The muxer still holds the tail of the
    /// last frame it took in (mpegtsmux packs 7 TS packets per buffer), so
    /// without draining it the last segment claims a frame it does not hold.
    #[test]
    fn last_segment_after_stop_holds_what_its_timerange_claims() {
        if !plugins_available()
            || !common::plugins_available(&[
                "videotestsrc",
                "x264enc",
                "capsfilter",
                "tsdemux",
                "h264parse",
                "avdec_h264",
            ])
        {
            return;
        }
        const FRAME_NS: u64 = 20_000_000;
        let mut results = Vec::new();
        for run_ms in [1_330, 1_570, 1_810, 2_250] {
            let (tr, frames) = last_segment_after_stop(run_ms);
            let claimed = timerange_end_ns(&tr) - timerange_start_ns(&tr);
            results.push((tr, frames, claimed));
        }
        for (tr, frames, claimed) in &results {
            assert_eq!(
                frames * FRAME_NS,
                *claimed,
                "last segment {} holds {} frames; all runs: {:?}",
                tr,
                frames,
                results
            );
        }
    }
}

/// A segment is closed, uploaded and registered as soon as the first frame
/// after it has arrived, not a GOP later. `splitmuxsink` decided on a split
/// only once the GOP after it was complete, so each segment waited one more
/// GOP (500 ms here) before it was even closed.
mod close_at_boundary {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    /// TAI minus UTC, as TAMS Output maps the host clock when nothing better
    /// is known (`strom_types::tai::TAI_MINUS_UTC_NS`).
    const TAI_MINUS_UTC_NS: u64 = 37_000_000_000;

    fn now_tai_ns() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            + TAI_MINUS_UTC_NS
    }

    fn end_ns(tr: &str) -> u64 {
        let end = tr
            .trim_end_matches([']', ')'])
            .rsplit('_')
            .next()
            .unwrap_or("");
        let (s, ns) = end.split_once(':').unwrap_or((end, "0"));
        s.parse::<u64>().unwrap() * 1_000_000_000 + ns.parse::<u64>().unwrap()
    }

    /// Live 50 fps video (x264 with B-frames, GOP 25) into a TAMS Output with
    /// 500 ms segments, also fed live AAC when `audio` is set. Returns each
    /// registered segment with how long after its end it was registered,
    /// and its bytes, in order.
    fn run(audio: bool, run_ms: u64) -> Vec<(String, u64, axum::body::Bytes)> {
        run_in(audio, run_ms, "mpegts")
    }

    /// As [`run`], in `container` (`mpegts` or `mp4`).
    fn run_in(audio: bool, run_ms: u64, container: &str) -> Vec<(String, u64, axum::body::Bytes)> {
        let runtime = runtime();
        let (gateway, registered, segments) = stub_gateway::start_keeping_bytes(&runtime);

        let pipeline = gst::Pipeline::new();
        let instance_id = format!("tams_close_{}", uuid::Uuid::new_v4().simple());
        let properties: HashMap<String, PropertyValue> = [
            ("gateway_url", PropertyValue::String(gateway.clone())),
            ("segment_duration_ms", PropertyValue::UInt(500)),
            ("container", PropertyValue::String(container.into())),
            ("num_video_tracks", PropertyValue::UInt(1)),
            ("num_audio_tracks", PropertyValue::UInt(audio as u64)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let built = TamsOutputBuilder
            .build(&instance_id, &properties, &ctx)
            .expect("TAMS Output block builds");
        let mut out = TamsOutput {
            instance_id,
            ctx,
            elements: HashMap::new(),
        };
        for (id, element) in &built.elements {
            pipeline.add(element).unwrap();
            out.elements.insert(id.clone(), element.clone());
        }

        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 160)
                    .field("height", 90)
                    .field("framerate", gst::Fraction::new(50, 1))
                    .build(),
            )
            .build()
            .unwrap();
        let enc = x264_with_bframes();
        pipeline.add_many([&src, &caps, &enc]).unwrap();
        gst::Element::link_many([&src, &caps, &enc]).unwrap();
        enc.link(&out.input("video_input_0")).unwrap();
        if audio {
            feed_audio(&pipeline, &out.input("audio_input_0"), -1);
        }

        out.run_setups_in_flow(&runtime, uuid::Uuid::new_v4());
        pipeline.set_state(gst::State::Playing).unwrap();
        // Note when each segment shows up at the gateway.
        let mut seen: Vec<(String, u64)> = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(run_ms);
        while Instant::now() < deadline {
            let now = now_tai_ns();
            for (_, tr) in registered.lock().unwrap().iter().skip(seen.len()) {
                seen.push((tr.clone(), now.saturating_sub(end_ns(tr))));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let error = bus_error(&pipeline);
        for drain in out.ctx.take_stop_drains() {
            drain();
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out.release_elements();
        drop(pipeline);
        std::thread::sleep(Duration::from_millis(500));
        let bytes = segments.lock().unwrap().clone();
        runtime.shutdown_background();
        assert_eq!(error, None, "the pipeline posted an error");
        seen.into_iter()
            .map(|(tr, age)| {
                let b = bytes
                    .iter()
                    .find(|(t, _)| *t == tr)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default();
                (tr, age, b)
            })
            .collect()
    }

    /// Whether an MPEG-TS segment carries video and audio packets, and the
    /// video frames a decoder gets out of it. `mpegtsmux` numbers its
    /// elementary streams from PID 0x41 in pad order: video, then audio.
    fn tracks_and_frames(ts: &[u8]) -> (bool, bool, u64) {
        let pid_packets = |pid: u16| {
            ts.chunks(188)
                .filter(|p| p.len() == 188 && (((p[1] as u16 & 0x1f) << 8) | p[2] as u16) == pid)
                .count()
        };
        let (video, audio) = (pid_packets(0x41) > 0, pid_packets(0x42) > 0);
        (video, audio, last_segment::decoded_frames(ts))
    }

    fn required() -> bool {
        plugins_available()
            && common::plugins_available(&[
                "videotestsrc",
                "x264enc",
                "capsfilter",
                "tsdemux",
                "h264parse",
                "avdec_h264",
                "fakesink",
            ])
    }

    #[test]
    fn a_segment_is_registered_as_soon_as_it_ends() {
        if !required() {
            return;
        }
        let segs = run(false, 4_000);
        // The first two are placed before keyframes follow the grid.
        let mut ages: Vec<u64> = segs.iter().skip(2).map(|(_, age, _)| *age).collect();
        assert!(ages.len() >= 3, "too few segments: {:?}", segs.len());
        ages.sort_unstable();
        let median = ages[ages.len() / 2];
        // The segment is closed by the first frame after it (20 ms), then
        // written and registered at a local stub. One more GOP is 500 ms.
        assert!(
            median < 250_000_000,
            "segments registered a median {} ms after their end (all: {:?})",
            median / 1_000_000,
            ages.iter().map(|a| a / 1_000_000).collect::<Vec<_>>()
        );
    }

    /// With audio muxed in, every segment holds both tracks, and the
    /// segments follow each other without holes or overlaps.
    #[test]
    fn audio_and_video_segments_follow_each_other() {
        if !required() || !common::plugins_available(&["avenc_aac"]) {
            return;
        }
        const FRAME_NS: u64 = 20_000_000;
        let segs = run(true, 3_500);
        assert!(segs.len() >= 4, "too few segments: {}", segs.len());
        for pair in segs.windows(2) {
            assert_eq!(
                end_ns(&pair[0].0),
                timerange_start_ns(&pair[1].0),
                "segments do not follow each other: {} then {}",
                pair[0].0,
                pair[1].0
            );
        }
        for (i, (tr, _, bytes)) in segs.iter().enumerate() {
            if let Ok(dir) = std::env::var("TAMS_TEST_DUMP") {
                std::fs::write(format!("{}/seg{}.ts", dir, i), bytes).unwrap();
            }
            let (video, audio, frames) = tracks_and_frames(bytes);
            // The first segment may open before the audio caps are known.
            assert!(
                video && (audio || i == 0),
                "segment {} lacks a track (video {}, audio {})",
                tr,
                video,
                audio
            );
            let claimed = end_ns(tr) - timerange_start_ns(tr);
            assert_eq!(
                frames * FRAME_NS,
                claimed,
                "segment {} holds {} frames",
                tr,
                frames
            );
        }
    }

    /// MP4 segments are cut the same way, each one a complete file.
    #[test]
    fn mp4_segments_are_complete_files() {
        if !required() || !common::plugins_available(&["mp4mux", "qtdemux"]) {
            return;
        }
        const FRAME_NS: u64 = 20_000_000;
        let segs = run_in(false, 3_000, "mp4");
        assert!(segs.len() >= 4, "too few segments: {}", segs.len());
        for pair in segs.windows(2) {
            assert_eq!(end_ns(&pair[0].0), timerange_start_ns(&pair[1].0));
        }
        for (tr, age, bytes) in segs.iter().skip(2) {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), bytes).unwrap();
            let pipeline = gst::parse::launch(&format!(
                "filesrc location=\"{}\" ! qtdemux ! h264parse ! avdec_h264 ! fakesink name=out sync=false",
                file.path().display()
            ))
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
            let frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let counted = frames.clone();
            pipeline
                .by_name("out")
                .unwrap()
                .static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            pipeline.set_state(gst::State::Playing).unwrap();
            let ended = wait_for_eos(&pipeline, Duration::from_secs(10));
            pipeline.set_state(gst::State::Null).unwrap();
            assert!(ended, "reading segment {} never reached EOS", tr);
            let frames = frames.load(std::sync::atomic::Ordering::Relaxed);
            let claimed = end_ns(tr) - timerange_start_ns(tr);
            assert_eq!(
                frames * FRAME_NS,
                claimed,
                "segment {} holds {} frames",
                tr,
                frames
            );
            assert!(
                *age < 500_000_000,
                "segment {} registered {} ms late",
                tr,
                age / 1_000_000
            );
        }
    }
}

/// OSC PAT/SAT mode mints a SAT and sends it to `gateway_url`. An explicit
/// `osc_service_id` used to skip the host check, so a flow could name any host
/// and collect the instance's SAT (review of #1286).
mod osc_auth {
    use super::*;

    fn build_osc(gateway: &str, service_id: &str) -> Result<(), String> {
        gst::init().unwrap();
        let properties: HashMap<String, PropertyValue> = [
            ("gateway_url", gateway),
            ("auth_mode", "osc"),
            ("osc_service_id", service_id),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), PropertyValue::String(v.into())))
        .collect();
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        TamsOutputBuilder
            .build("osc_auth_test", &properties, &ctx)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn explicit_service_id_does_not_send_a_sat_to_another_host() {
        for gateway in [
            "https://a.eyevinn-tams-gateway.auto.prod.osaas.io@evil.example/",
            "https://evil.example/",
            "http://a.eyevinn-tams-gateway.auto.prod.osaas.io/",
        ] {
            let err = build_osc(gateway, "eyevinn-tams-gateway")
                .expect_err("OSC mode must refuse a gateway outside the service");
            assert!(err.contains("not sending a SAT"), "{gateway}: {err}");
        }
    }
}

/// A flow saved with the released `segment_duration_secs` is migrated on
/// load and written back, so the migration and its log line run once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_segment_duration_is_migrated_and_saved_on_load() {
    use strom_types::block::{BlockInstance, Position};
    use strom_types::Flow;
    gst::init().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut flow = Flow::new("tams-migrate");
    flow.blocks.push(BlockInstance {
        id: "tams".to_string(),
        block_definition_id: "builtin.tams_output".to_string(),
        name: None,
        properties: HashMap::from([("segment_duration_secs".to_string(), PropertyValue::UInt(3))]),
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let file = dir.path().join("flows.json");
    std::fs::write(
        &file,
        serde_json::json!({ "version": 1, "flows": [flow] }).to_string(),
    )
    .unwrap();
    let state = common::state::in_dir(dir.path(), &std::env::temp_dir());
    state.load_from_storage().await.unwrap();
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let props = &on_disk["flows"][0]["blocks"][0]["properties"];
    assert!(props.get("segment_duration_secs").is_none(), "{props}");
    assert!(props.get("segment_duration_ms").is_some(), "{props}");
}
