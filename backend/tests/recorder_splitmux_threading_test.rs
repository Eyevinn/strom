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
//! pad its own streaming thread. This test reproduces that topology (a single
//! `tsdemux` task feeding both a video and an audio `splitmuxsink` pad) and
//! asserts that recording actually completes. Remove the queues and it stalls
//! until the watchdog below fires.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
];

fn missing_element() -> Option<&'static str> {
    REQUIRED_ELEMENTS
        .iter()
        .copied()
        .find(|e| gst::ElementFactory::find(e).is_none())
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

/// Record `ts_path` through the recorder's topology: one `tsdemux` streaming task
/// feeding both `splitmuxsink` sink pads, with or without the per-leg queue.
///
/// Returns (file count, total bytes) of the produced fragments.
fn record_transport_stream(
    ts_path: &std::path::Path,
    out_dir: &std::path::Path,
    use_queues: bool,
) -> Result<(usize, u64), String> {
    let pipeline = gst::Pipeline::new();

    let src = gst::ElementFactory::make("filesrc")
        .property("location", ts_path.to_str().expect("path is valid UTF-8"))
        .build()
        .map_err(|e| e.to_string())?;
    let demux = gst::ElementFactory::make("tsdemux")
        .build()
        .map_err(|e| e.to_string())?;
    let splitmux = gst::ElementFactory::make("splitmuxsink")
        .property(
            "location",
            out_dir
                .join("segment%05d.mp4")
                .to_str()
                .expect("path is valid UTF-8"),
        )
        // Split partway through so a healthy run yields several fragments; a run
        // that stalls after the first GOP yields zero or one.
        .property("max-size-time", 2_000_000_000u64)
        .build()
        .map_err(|e| e.to_string())?;

    pipeline
        .add_many([&src, &demux, &splitmux])
        .map_err(|e| e.to_string())?;
    src.link(&demux).map_err(|e| e.to_string())?;

    // tsdemux exposes its streams dynamically, exactly as it does behind
    // mpegtssrt_input in passthrough mode.
    let (err_tx, err_rx) = mpsc::channel::<String>();
    let pipeline_weak = pipeline.downgrade();
    let splitmux_weak = splitmux.downgrade();
    demux.connect_pad_added(move |_demux, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let Some(splitmux) = splitmux_weak.upgrade() else {
            return;
        };

        let report = |msg: String| {
            let _ = err_tx.send(msg);
        };

        let Some(caps) = pad.current_caps() else {
            report("demux pad exposed without caps".to_string());
            return;
        };
        let Some(structure) = caps.structure(0) else {
            report("demux pad caps had no structure".to_string());
            return;
        };
        let media_type = structure.name();

        let (parser_factory, sink_pad_name) = if media_type.starts_with("video/x-h264") {
            ("h264parse", "video")
        } else if media_type.starts_with("audio/mpeg") {
            ("aacparse", "audio_%u")
        } else {
            // Not a stream this recorder handles.
            return;
        };

        let parser = match gst::ElementFactory::make(parser_factory).build() {
            Ok(p) => p,
            Err(e) => {
                report(format!("failed to create {parser_factory}: {e}"));
                return;
            }
        };
        if let Err(e) = pipeline.add(&parser) {
            report(format!("failed to add {parser_factory}: {e}"));
            return;
        }
        if let Err(e) = parser.sync_state_with_parent() {
            report(format!("failed to sync {parser_factory} state: {e}"));
            return;
        }

        // The element whose src pad feeds splitmuxsink: the parser directly, or
        // the queue that decouples this leg onto its own streaming thread.
        let tail = if use_queues {
            let queue = match gst::ElementFactory::make("queue").build() {
                Ok(q) => q,
                Err(e) => {
                    report(format!("failed to create queue: {e}"));
                    return;
                }
            };
            if let Err(e) = pipeline.add(&queue) {
                report(format!("failed to add queue: {e}"));
                return;
            }
            if let Err(e) = queue.sync_state_with_parent() {
                report(format!("failed to sync queue state: {e}"));
                return;
            }
            if let Err(e) = parser.link(&queue) {
                report(format!("failed to link {parser_factory} to queue: {e}"));
                return;
            }
            queue
        } else {
            parser.clone()
        };

        let Some(parser_sink) = parser.static_pad("sink") else {
            report(format!("{parser_factory} has no sink pad"));
            return;
        };
        if let Err(e) = pad.link(&parser_sink) {
            report(format!("failed to link demux to {parser_factory}: {e:?}"));
            return;
        }

        let Some(mux_pad) = splitmux.request_pad_simple(sink_pad_name) else {
            report(format!("splitmuxsink refused a {sink_pad_name} pad"));
            return;
        };
        let Some(tail_src) = tail.static_pad("src") else {
            report("tail element has no src pad".to_string());
            return;
        };
        if let Err(e) = tail_src.link(&mux_pad) {
            report(format!("failed to link into splitmuxsink: {e:?}"));
        }
    });

    let run_result = run_to_eos(&pipeline);

    // Surface a link-time failure in preference to the resulting timeout.
    if let Ok(link_error) = err_rx.try_recv() {
        return Err(link_error);
    }
    run_result?;

    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(out_dir).map_err(|e| e.to_string())? {
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
/// completion. Without a queue per leg the two `splitmuxsink` sink pads deadlock
/// on that shared thread and this times out.
#[test]
fn records_video_and_audio_from_a_single_demux_thread() {
    gst::init().expect("failed to initialize GStreamer");

    if let Some(missing) = missing_element() {
        eprintln!("SKIP: required element '{missing}' is unavailable");
        return;
    }

    let tmp = std::env::temp_dir().join(format!("strom-recorder-threading-{}", std::process::id()));
    let out_dir = tmp.join("segments");
    std::fs::create_dir_all(&out_dir).expect("failed to create temp output directory");
    let ts_path = tmp.join("source.ts");

    let result = (|| -> Result<(usize, u64), String> {
        write_test_transport_stream(&ts_path)?;
        record_transport_stream(&ts_path, &out_dir, true)
    })();

    let cleanup = std::fs::remove_dir_all(&tmp);

    let (files, bytes) = result.unwrap_or_else(|e| {
        panic!("recording with a queue per leg failed: {e}");
    });
    cleanup.expect("failed to clean up temp directory");

    // A healthy 5s recording split every 2s yields multiple non-empty fragments.
    assert!(
        files >= 2,
        "expected several recorded fragments, got {files}"
    );
    assert!(bytes > 0, "recorded fragments were empty");
}
