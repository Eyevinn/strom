//! A connected caller SRT output must not drop media because the flow has a
//! high latency and a high bitrate.
//!
//! A calling `srtsink` sits behind a leaky queue so that an output without a
//! peer does not stall the flow (#1259). A syncing sink holds each buffer
//! until its timestamp plus the pipeline latency, and another branch of the
//! flow can set that latency, so while the output is connected the queue holds
//! about the pipeline latency's worth of media. The queue's limits must never
//! bind there. Fixed limits did: 1 s of `max-size-time` binds as soon as the
//! flow's latency passes 1 s, and the 10 MB `max-size-bytes` default binds
//! whenever latency x bitrate passes 80 Mbit (1.6 s at 50 Mbit/s, 0.8 s at
//! 100 Mbit/s).
//!
//! Each test builds the real output block, calling a local SRT listener,
//! feeds it a ~55 Mbit/s H.264 stream, forces the pipeline latency to 2.5 s
//! (what another branch of the flow would set), and requires that the queue
//! in front of `srtsink` never overruns. With `leaky=downstream` every
//! overrun is a drop. The queue then holds ~2.5 s and ~17 MB, above both old
//! limits, which the test checks so the case is not vacuous. It also checks
//! that the queue followed the pipeline latency, that the listener received
//! the stream, and that the muxer sent no buffer without a timestamp (the
//! queue is bounded by time only, and such buffers do not move its time
//! level).
//!
//! The bitrate is kept moderate on purpose: at ~145 Mbit/s the sink alone
//! keeps up, but with the whole test binary running in parallel it fell more
//! than 2 s behind and the queue rightly dropped (a link that cannot keep up).

use crate::common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::get_builder;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// Elements these tests need beyond core GStreamer.
const REQUIRED: &[&str] = &[
    "srtsink",
    "srtsrc",
    "videotestsrc",
    "x264enc",
    "h264parse",
    "appsrc",
    "appsink",
    "queue",
    "fakesink",
];

const INSTANCE_ID: &str = "srt_out";

/// Frames per second of the stream fed to the output.
const FPS: u64 = 50;

/// Encoded frames, looped. Every frame is an IDR, so the loop is a valid
/// stream. 400x224 snow, lossless: ~135 kB a frame, ~55 Mbit/s at 50 fps.
const FRAMES: usize = 10;

/// How long the output is watched once the listener receives data.
const WINDOW: Duration = Duration::from_secs(10);

/// The old fixed limits, to show the case would have hit them.
const OLD_MAX_SIZE_TIME: gst::ClockTime = gst::ClockTime::from_seconds(1);
const OLD_MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024;

/// Encode `FRAMES` frames of incompressible video, intra only, and return
/// them with their caps. Not live and not real time: this is fast.
fn encode_frames() -> (gst::Caps, Vec<gst::Buffer>) {
    let pipeline = gst::parse::launch(&format!(
        "videotestsrc pattern=snow num-buffers={FRAMES} ! \
         video/x-raw,format=I420,width=400,height=224,framerate={FPS}/1 ! \
         x264enc speed-preset=ultrafast tune=zerolatency pass=quant quantizer=0 \
         key-int-max=1 ! video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink sync=false"
    ))
    .expect("encoder pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let mut caps = None;
    let mut frames = Vec::new();
    while let Ok(sample) = sink.pull_sample() {
        caps = sample.caps().map(|c| c.to_owned());
        frames.push(sample.buffer().unwrap().to_owned());
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert_eq!(frames.len(), FRAMES, "encoder produced too few frames");
    (caps.expect("encoded caps"), frames)
}

/// An SRT listener on `port` that counts the bytes it receives.
fn start_listener(port: u16) -> (gst::Pipeline, Arc<AtomicU64>) {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("srtsrc")
        .property(
            "uri",
            format!("srt://127.0.0.1:{}?mode=listener", port).as_str(),
        )
        .build()
        .expect("srtsrc");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .expect("fakesink");
    pipeline.add_many([&src, &sink]).expect("add listener");
    src.link(&sink).expect("link srtsrc -> sink");
    let bytes = Arc::new(AtomicU64::new(0));
    let counter = bytes.clone();
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(ref b)) = info.data {
                counter.fetch_add(b.size() as u64, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    pipeline
        .set_state(gst::State::Playing)
        .expect("listener to PLAYING");
    (pipeline, bytes)
}

struct Outcome {
    overruns: usize,
    max_size_time: gst::ClockTime,
    peak_level_time: gst::ClockTime,
    peak_level_bytes: u64,
    mux_bytes: u64,
    received_bytes: u64,
    untimestamped: u64,
    errors: Vec<String>,
}

fn bus_errors(pipeline: &gst::Pipeline) -> Vec<String> {
    let bus = pipeline.bus().expect("pipeline bus");
    let mut errors = Vec::new();
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
        if let gst::MessageView::Error(err) = msg.view() {
            let src = err
                .src()
                .map(|o| o.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            errors.push(format!("{}: {} ({:?})", src, err.error(), err.debug()));
        }
    }
    errors
}

/// Run the real `definition_id` block calling a local listener, with the
/// pipeline latency forced to `latency`, and watch its srtsink queue.
fn run(definition_id: &str, latency: gst::ClockTime) -> Option<Outcome> {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return None;
    }
    let (caps, frames) = encode_frames();

    let port = common::free_udp_port();
    let (listener, received) = start_listener(port);

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=caller", port)),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(0));
    // A syncing sink is what holds media for the pipeline latency (EFP/SRT
    // Output defaults to sync=false, MPEG-TS/SRT Output to true).
    props.insert("sync".to_string(), PropertyValue::Bool(true));
    let ctx = common::block::context();
    let built = get_builder(definition_id)
        .unwrap_or_else(|| panic!("no builder for {definition_id}"))
        .build(INSTANCE_ID, &props, &ctx)
        .expect("SRT output block builds");

    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);
    common::block::run_setups(&ctx);
    // What another branch of the flow (an encoder with lookahead, a mixer,
    // another output) would set.
    pipeline.set_latency(latency);

    let queue = by_id
        .get(&format!("{}:srtsink_queue", INSTANCE_ID))
        .expect("a calling SRT output has a queue in front of srtsink")
        .clone();

    let overruns = Arc::new(AtomicUsize::new(0));
    let counter = overruns.clone();
    queue.connect("overrun", false, move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
        None
    });

    // What the muxer hands the queue: bytes, and buffers without a timestamp.
    let mux_bytes = Arc::new(AtomicU64::new(0));
    let untimestamped = Arc::new(AtomicU64::new(0));
    let queue_sink = queue.static_pad("sink").unwrap();
    let mux_src = queue_sink.peer().expect("queue linked to the muxer");
    let (bytes_counter, untimestamped_counter) = (mux_bytes.clone(), untimestamped.clone());
    // mpegtsmux pushes buffer lists.
    mux_src.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_, info| {
            let count = |b: &gst::BufferRef| {
                bytes_counter.fetch_add(b.size() as u64, Ordering::Relaxed);
                if b.dts_or_pts().is_none() {
                    untimestamped_counter.fetch_add(1, Ordering::Relaxed);
                }
            };
            match info.data {
                Some(gst::PadProbeData::Buffer(ref b)) => count(b),
                Some(gst::PadProbeData::BufferList(ref list)) => list.iter().for_each(count),
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );

    let appsrc = gst_app::AppSrc::builder()
        .caps(&caps)
        .is_live(true)
        .format(gst::Format::Time)
        .build();
    pipeline.add(&appsrc).unwrap();
    appsrc
        .link(
            by_id
                .get(&format!("{}:video_input_0", INSTANCE_ID))
                .or_else(|| by_id.get(&format!("{}:video_input", INSTANCE_ID)))
                .expect("block exposes a video input"),
        )
        .expect("link appsrc -> SRT output");

    pipeline
        .set_state(gst::State::Playing)
        .expect("sender to PLAYING");

    // Feed frames at FPS, in real time.
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        let appsrc = appsrc.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // Stamp with the running time, as a live source does.
                let mut buffer = frames[(n as usize) % frames.len()].copy();
                let now = appsrc.current_running_time();
                {
                    let b = buffer.make_mut();
                    b.set_pts(now);
                    b.set_dts(now);
                }
                if appsrc.push_buffer(buffer).is_err() {
                    break;
                }
                n += 1;
                let next = start + Duration::from_micros(n * 1_000_000 / FPS);
                if let Some(wait) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        })
    };

    // Wait for the listener, then watch the queue.
    let deadline = Instant::now() + Duration::from_secs(15);
    while received.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut peak_level_time = 0u64;
    let mut peak_level_bytes = 0u64;
    let start = Instant::now();
    while start.elapsed() < WINDOW {
        std::thread::sleep(Duration::from_millis(50));
        peak_level_time = peak_level_time.max(queue.property::<u64>("current-level-time"));
        peak_level_bytes =
            peak_level_bytes.max(u64::from(queue.property::<u32>("current-level-bytes")));
    }

    let outcome = Outcome {
        overruns: overruns.load(Ordering::Relaxed),
        max_size_time: gst::ClockTime::from_nseconds(queue.property::<u64>("max-size-time")),
        peak_level_time: gst::ClockTime::from_nseconds(peak_level_time),
        peak_level_bytes,
        mux_bytes: mux_bytes.load(Ordering::Relaxed),
        received_bytes: received.load(Ordering::Relaxed),
        untimestamped: untimestamped.load(Ordering::Relaxed),
        errors: bus_errors(&pipeline),
    };
    stop.store(true, Ordering::Relaxed);
    let _ = pipeline.set_state(gst::State::Null);
    let _ = feeder.join();
    let _ = listener.set_state(gst::State::Null);

    eprintln!(
        "{definition_id} latency {latency}: overruns {}, max-size-time {}, peak level {} / {:.1} MB, \
         muxer sent {:.1} MB (~{:.0} Mbit/s), listener received {:.1} MB, untimestamped {}",
        outcome.overruns,
        outcome.max_size_time,
        outcome.peak_level_time,
        outcome.peak_level_bytes as f64 / 1e6,
        outcome.mux_bytes as f64 / 1e6,
        outcome.mux_bytes as f64 * 8.0 / 1e6 / (WINDOW.as_secs_f64() + latency.seconds_f64()),
        outcome.received_bytes as f64 / 1e6,
        outcome.untimestamped,
    );
    Some(outcome)
}

/// The pipeline latency the tests force: above the old 1 s, and at ~55 Mbit/s
/// ~17 MB, above the old 10 MB.
const LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(2500);

fn assert_no_drops(definition_id: &str) {
    let latency = LATENCY;
    let Some(o) = run(definition_id, latency) else {
        return;
    };
    assert!(o.errors.is_empty(), "sender errored: {:?}", o.errors);
    assert!(
        o.received_bytes > 0,
        "the listener received nothing; the output never connected"
    );
    assert_eq!(
        o.overruns, 0,
        "the leaky queue in front of a connected srtsink overran {} time(s), dropping media \
         while connected (pipeline latency {latency}, peak level {} / {} bytes, limit {})",
        o.overruns, o.peak_level_time, o.peak_level_bytes, o.max_size_time
    );
    // The block's rule: max(latency + 2 s, 3 s).
    let expected = (latency + gst::ClockTime::from_seconds(2)).max(gst::ClockTime::from_seconds(3));
    assert_eq!(
        o.max_size_time, expected,
        "the srtsink queue did not follow the pipeline latency ({latency})"
    );
    assert_eq!(
        o.untimestamped, 0,
        "the muxer sent buffers without a timestamp; the time-bounded queue cannot bound them"
    );
    assert!(
        o.peak_level_time > OLD_MAX_SIZE_TIME && o.peak_level_bytes > OLD_MAX_SIZE_BYTES,
        "the case is vacuous: the queue peaked at {} / {} bytes, not above both old limits \
         (1 s / 10 MB)",
        o.peak_level_time,
        o.peak_level_bytes
    );
    // The listener got what the sink sent: everything but what is still
    // queued, less what is in flight.
    assert!(
        o.received_bytes * 10 >= o.mux_bytes.saturating_sub(o.peak_level_bytes) * 8,
        "the listener received {} of {} bytes sent to the output",
        o.received_bytes,
        o.mux_bytes
    );
}

#[test]
fn mpegtssrt_connected_caller_drops_nothing_at_high_latency_and_bitrate() {
    assert_no_drops("builtin.mpegtssrt_output");
}

#[cfg(feature = "efp")]
#[test]
fn efpsrt_connected_caller_drops_nothing_at_high_latency_and_bitrate() {
    assert_no_drops("builtin.efpsrt_output");
}
