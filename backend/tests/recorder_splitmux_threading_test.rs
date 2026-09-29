//! Regression test for the recorder deadlock when one upstream streaming task
//! feeds several `splitmuxsink` sink pads.
//!
//! Bug: `splitmuxsink` blocks one input pad's streaming thread while it waits for
//! the other input pads to reach the next GOP boundary — that is how it aligns
//! GOPs across streams. The reference pad (video) waits in `check_completed_gop`
//! until every context reaches the next GOP start; non-reference pads (audio)
//! wait once they catch up to `max_in_running_time`, which only the reference pad
//! advances.
//!
//! That mutual blocking is only survivable when the pads are fed by different
//! threads. With `mpegtssrt_input(decode=false) -> recorder` the recorder fed both
//! sink pads from `tsdemux`'s single streaming task, so the pad that blocked held
//! the only thread that could ever unblock it and recording stopped within a
//! packet or two — 0 files, or a single fragment that never closed.
//!
//! Fix: a `queue` per leg between the parser and `splitmuxsink`, giving every sink
//! pad its own streaming thread. This test builds the real recorder block and
//! feeds it the way `mpegtssrt_input(decode=false)` does — one `tsdemux` task
//! into both inputs — and asserts that recording actually completes. Remove the
//! recorder's queues and it stalls until the watchdog below fires.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::recorder::RecorderBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

/// Long enough for a healthy run (which finishes in ~1s) to never flake, short
/// enough that a reintroduced deadlock fails the suite promptly.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

const REQUIRED_ELEMENTS: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "x264enc",
    "avenc_aac",
    "mpegtsmux",
    "tsdemux",
    "h264parse",
    "aacparse",
    "splitmuxsink",
    "mp4mux",
    "identity",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn missing_element() -> Option<&'static str> {
    let missing = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .find(|e| gst::ElementFactory::find(e).is_none())?;
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but this element is missing: {missing}"
    );
    Some(missing)
}

/// Run a pipeline to EOS, returning an error on bus error or if `RUN_TIMEOUT`
/// elapses first. A deadlock shows up here as the timeout.
fn run_to_eos(pipeline: &gst::Pipeline) -> Result<(), String> {
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("failed to start pipeline: {e}"))?;

    let bus = pipeline.bus().expect("pipeline has no bus");
    let deadline = Instant::now() + RUN_TIMEOUT;
    let mut result = Err("timed out waiting for EOS (deadlock?)".to_string());

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(
            remaining.as_nanos().min(u64::MAX as u128) as u64,
        )) else {
            break;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => {
                result = Ok(());
                break;
            }
            gst::MessageView::Error(err) => {
                result = Err(format!(
                    "pipeline error from {:?}: {}",
                    err.src().map(|s| s.path_string()),
                    err.error()
                ));
                break;
            }
            _ => {}
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Produce a short MPEG-TS file carrying both H.264 video and AAC audio.
fn write_test_transport_stream(path: &std::path::Path) -> Result<(), String> {
    let pipeline = gst::Pipeline::new();

    let video_src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 150i32) // 5s at 30fps, enough for several GOPs
        .property_from_str("pattern", "smpte")
        .build()
        .map_err(|e| e.to_string())?;
    // Short GOPs so splitmuxsink has frequent split points to align on.
    let encoder = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 30u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .map_err(|e| e.to_string())?;
    let video_parse = gst::ElementFactory::make("h264parse")
        .build()
        .map_err(|e| e.to_string())?;

    let audio_src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", 240i32) // ~5s of 1024-sample AAC frames at 48kHz
        .build()
        .map_err(|e| e.to_string())?;
    let audio_convert = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| e.to_string())?;
    let audio_resample = gst::ElementFactory::make("audioresample")
        .build()
        .map_err(|e| e.to_string())?;
    let audio_enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .map_err(|e| e.to_string())?;
    let audio_parse = gst::ElementFactory::make("aacparse")
        .build()
        .map_err(|e| e.to_string())?;

    let mux = gst::ElementFactory::make("mpegtsmux")
        .build()
        .map_err(|e| e.to_string())?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_str().expect("temp path is valid UTF-8"))
        .build()
        .map_err(|e| e.to_string())?;

    let elements = [
        &video_src,
        &encoder,
        &video_parse,
        &audio_src,
        &audio_convert,
        &audio_resample,
        &audio_enc,
        &audio_parse,
        &mux,
        &sink,
    ];
    pipeline.add_many(elements).map_err(|e| e.to_string())?;

    gst::Element::link_many([&video_src, &encoder, &video_parse, &mux])
        .map_err(|e| e.to_string())?;
    gst::Element::link_many([
        &audio_src,
        &audio_convert,
        &audio_resample,
        &audio_enc,
        &audio_parse,
        &mux,
    ])
    .map_err(|e| e.to_string())?;
    mux.link(&sink).map_err(|e| e.to_string())?;

    run_to_eos(&pipeline)
}

/// Record `ts_path` through the real recorder block, fed the way
/// `mpegtssrt_input(decode=false)` feeds it: `tsdemux` pads linked into one
/// static `identity` per medium, so a single demuxer streaming task pushes into
/// both recorder inputs.
///
/// Returns (file count, total bytes) of the produced fragments.
fn record_transport_stream(
    ts_path: &std::path::Path,
    media_root: &std::path::Path,
) -> Result<(usize, u64), String> {
    let instance_id = "rec";
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("container".to_string(), PropertyValue::String("mp4".into()));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    // Split partway through so a healthy run yields several fragments; a run
    // that stalls after the first GOP yields zero or one.
    props.insert("max_size_time_secs".to_string(), PropertyValue::UInt(2));
    props.insert(
        "output_dir".to_string(),
        PropertyValue::String("recordings".into()),
    );
    props.insert(
        "filename_prefix".to_string(),
        PropertyValue::String("segment".into()),
    );
    props.insert(
        "_media_path".to_string(),
        PropertyValue::String(media_root.to_string_lossy().to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RecorderBuilder
        .build(instance_id, &props, &ctx)
        .map_err(|e| format!("recorder block failed to build: {e}"))?;

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).map_err(|e| e.to_string())?;
        by_id.insert(id.clone(), element.clone());
    }
    let recorder_input = |name: &str| {
        by_id
            .get(&format!("{instance_id}:{name}"))
            .cloned()
            .ok_or_else(|| format!("recorder exposes no {name}"))
    };

    let src = gst::ElementFactory::make("filesrc")
        .property("location", ts_path.to_str().expect("path is valid UTF-8"))
        .build()
        .map_err(|e| e.to_string())?;
    let demux = gst::ElementFactory::make("tsdemux")
        .build()
        .map_err(|e| e.to_string())?;
    // The passthrough outputs of mpegtssrt_input: plain identities, no queue.
    let video_out = gst::ElementFactory::make("identity")
        .build()
        .map_err(|e| e.to_string())?;
    let audio_out = gst::ElementFactory::make("identity")
        .build()
        .map_err(|e| e.to_string())?;
    pipeline
        .add_many([&src, &demux, &video_out, &audio_out])
        .map_err(|e| e.to_string())?;
    src.link(&demux).map_err(|e| e.to_string())?;
    video_out
        .link(&recorder_input("video_input_0")?)
        .map_err(|e| e.to_string())?;
    audio_out
        .link(&recorder_input("audio_input_0")?)
        .map_err(|e| e.to_string())?;

    let (err_tx, err_rx) = mpsc::channel::<String>();
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
        let Some(sink) = target.and_then(|t| t.static_pad("sink")) else {
            return;
        };
        if let Err(e) = pad.link(&sink) {
            let _ = err_tx.send(format!("failed to link demux {media_type} pad: {e:?}"));
        }
    });

    // The pipeline manager runs these after linking, before PLAYING; they
    // request the splitmuxsink pads for the connected tracks.
    for setup in ctx.take_element_setups() {
        setup(uuid::Uuid::new_v4(), EventBroadcaster::with_capacity(16));
    }

    let run_result = run_to_eos(&pipeline);

    // Surface a link-time failure in preference to the resulting timeout.
    if let Ok(link_error) = err_rx.try_recv() {
        return Err(link_error);
    }
    run_result?;

    let mut files = 0usize;
    let mut bytes = 0u64;
    let dir = media_root.join("recordings");
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        if metadata.is_file() {
            files += 1;
            bytes += metadata.len();
        }
    }
    Ok((files, bytes))
}

/// A recorder fed by a single demuxer streaming task must record both legs to
/// completion. Without its queue per leg the two `splitmuxsink` sink pads
/// deadlock on that shared thread and this times out.
#[test]
fn records_video_and_audio_from_a_single_demux_thread() {
    gst::init().expect("failed to initialize GStreamer");

    if let Some(missing) = missing_element() {
        eprintln!("SKIP: required element '{missing}' is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let ts_path = tmp.path().join("source.ts");

    let result = (|| -> Result<(usize, u64), String> {
        write_test_transport_stream(&ts_path)?;
        record_transport_stream(&ts_path, tmp.path())
    })();

    let (files, bytes) = result.unwrap_or_else(|e| {
        panic!("recording from a single demux thread failed: {e}");
    });

    // A healthy 5s recording split every 2s yields multiple non-empty fragments.
    assert!(
        files >= 2,
        "expected several recorded fragments, got {files}"
    );
    assert!(bytes > 0, "recorded fragments were empty");
}
