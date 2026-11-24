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
    "splitmuxsink",
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
    let instance_id = format!("{}_{}", name, uuid::Uuid::new_v4().simple());
    let properties: HashMap<String, PropertyValue> = [
        ("gateway_url", PropertyValue::String(gateway_url.into())),
        ("segment_duration_secs", PropertyValue::UInt(segment_secs)),
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
    use std::sync::{Arc, Mutex};

    pub type Registered = Arc<Mutex<Vec<(String, String)>>>;

    #[derive(Clone)]
    struct Stub {
        base: String,
        registered: Registered,
    }

    /// Start the stub on a free local port; returns its base URL.
    pub fn start(runtime: &tokio::runtime::Runtime) -> (String, Registered) {
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind stub gateway");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let registered: Registered = Arc::default();
        let stub = Stub {
            base: base.clone(),
            registered: registered.clone(),
        };
        let app = Router::new()
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
                put(|_body: axum::body::Bytes| async { axum::http::StatusCode::OK }),
            )
            .route(
                "/flows/{id}/segments",
                post(
                    |State(s): State<Stub>,
                     Path(id): Path<String>,
                     Json(body): Json<serde_json::Value>| async move {
                        let tr = body["timerange"].as_str().unwrap_or_default().to_string();
                        s.registered.lock().unwrap().push((id, tr));
                        axum::http::StatusCode::CREATED
                    },
                ),
            )
            .with_state(stub);
        runtime.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, registered)
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
    /// `min_keyframe_request_interval_ms` 0. Its 1 s default drops grid
    /// requests that come sooner than a second after the last forced
    /// keyframe, and the boundaries leave the grid.
    #[test]
    fn encoder_keyframes_follow_the_grid() {
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
        let out = add_tams_output_with(&pipeline, "tams_video", &gateway, 1, true);
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
        // The Video Encoder block as an ingest flow sets it up: a 0.5 s GOP,
        // and `min_keyframe_request_interval_ms` 0 so no grid request is
        // dropped (the 1 s default is for WebRTC PLI storms). Its natural
        // keyframes fall wherever the stream started, not on the grid.
        let enc_props: HashMap<String, PropertyValue> = [
            ("codec", PropertyValue::String("h264".into())),
            (
                "encoder_preference",
                PropertyValue::String("software".into()),
            ),
            ("quality_preset", PropertyValue::String("fast".into())),
            ("tune", PropertyValue::String("fastdecode".into())),
            ("keyframe_interval", PropertyValue::UInt(25)),
            ("min_keyframe_request_interval_ms", PropertyValue::UInt(0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
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
        // The first split is requested too late to be forced and the second may
        // be; from the third segment on, each starts within a frame (20 ms) of a
        // whole TAI second.
        for tr in &ranges[2..] {
            let into_second = timerange_start_ns(tr) % 1_000_000_000;
            assert!(
                into_second < 20_000_000,
                "segment {} starts {} ms past the 1 s TAI grid (all: {:?})",
                tr,
                into_second / 1_000_000,
                ranges
            );
        }
    }
}
