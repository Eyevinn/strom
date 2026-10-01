//! Regression tests for the TAMS Output block, built through the real
//! `TamsOutputBuilder` and fed from test sources the way a flow would feed it.
//!
//! - `idle_input`: an unfed TAMS Output must not hold the pipeline short of
//!   PLAYING, and one that gets data must still write segments.
//!
//! The gateway URL points at a closed local port. The block registers its flow
//! and uploads lazily, so building and running it needs no TAMS server; failed
//! uploads leave the segment files on disk, where these tests read them.

pub mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use strom::blocks::builtin::tams_output::TamsOutputBuilder;
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
        let _guard = runtime.enter();
        for setup in self.ctx.take_element_setups() {
            setup(uuid::Uuid::new_v4(), EventBroadcaster::with_capacity(16));
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
    let instance_id = format!("{}_{}", name, uuid::Uuid::new_v4().simple());
    let properties: HashMap<String, PropertyValue> = [
        (
            "gateway_url",
            PropertyValue::String(UNREACHABLE_GATEWAY.into()),
        ),
        ("container", PropertyValue::String("mpegts".into())),
        ("num_video_tracks", PropertyValue::UInt(0)),
        ("num_audio_tracks", PropertyValue::UInt(1)),
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
