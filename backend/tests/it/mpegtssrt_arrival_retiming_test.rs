//! The MPEG-TS/SRT Input stamps what it outputs from when the SRT data arrived
//! (`gst::ts_arrival_clock`), not from the first PES tsdemux parsed.
//!
//! Each test builds the real block, feeds it from an in-process H.264 + AAC
//! caller over SRT, and measures every buffer's *lead* at the block's outputs:
//! its running time minus the pipeline's running time as it leaves the block.
//! Not srtsrc's stamp: that lags the clock by up to 1.3 s in the burst a
//! caller sends on connecting, which made late buffers look on time. The block
//! may spend the latency it reports upstream (the SRT latency), so a buffer is
//! late only below `-BUDGET_MS`. The latest stream sits `MARGIN_MS` before
//! that deadline (#1265).
//!
//! A caller that starts before the listener blocks in `srtsink` until it can
//! connect, then sends the start of its encode followed by live media. tsdemux
//! anchors on that start, so without retiming the whole connection leads by as
//! long as the caller waited.

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE_ID: &str = "srt_r";

const REQUIRED: &[&str] = &[
    "srtsrc",
    "srtsink",
    "tsdemux",
    "mpegtsmux",
    "decodebin",
    "videotestsrc",
    "x264enc",
    "h264parse",
    "avdec_h264",
    "audiotestsrc",
    "avenc_aac",
    "aacparse",
    "avdec_aac",
    "identity",
    "fakesink",
];

/// The SRT latency the block reports, its default.
const BUDGET_MS: i64 = strom_types::DEFAULT_SRT_LATENCY_MS as i64;
/// The block's default arrival margin.
const MARGIN_MS: i64 = strom_types::DEFAULT_TS_ARRIVAL_MARGIN_MS;
/// Where the latest stream's earliest buffers land against their arrival.
const TARGET_MS: i64 = MARGIN_MS - BUDGET_MS;

/// How long the caller runs before the listener exists.
const EARLY_START: Duration = Duration::from_secs(2);

/// A free UDP port for the SRT listener, below the ephemeral range (macOS
/// from 49152, Linux from 32768). The early-caller test leaves the port
/// unbound for `EARLY_START` while the parallel suite's SRT callers bind
/// ephemeral ports, and a port the OS handed out as ephemeral could be taken
/// in that window: once in 8 runs the receiver then failed to start.
fn srt_port() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let spread = (std::process::id() % 500) as u16 * 20;
    (0..1000)
        .map(|_| {
            20_000 + (spread + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) % 10_000
        })
        .find(|&port| std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok())
        .expect("no free UDP port for the SRT listener")
}

/// Start the receiver, or fail with what its bus says.
fn play(r: &Receiver) {
    if r.pipeline.set_state(gst::State::Playing).is_err() {
        let error = r
            .pipeline
            .bus()
            .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
            .map(|m| format!("{m:?}"))
            .unwrap_or_default();
        panic!("the receiver did not start: {error}");
    }
}

/// One measured buffer: when it left the block (ms after the first one) and
/// its lead (ms).
#[derive(Clone, Copy, Debug)]
struct Sample {
    media: &'static str,
    at_ms: i64,
    lead_ms: i64,
    /// Running time minus PTS (ms): changes only when the pad offset does.
    shift_ms: i64,
}

struct Receiver {
    pipeline: gst::Pipeline,
    srtsrc: gst::Element,
    samples: Arc<Mutex<Vec<Sample>>>,
}

fn running_time(pad: &gst::Pad, pts: gst::ClockTime) -> Option<i64> {
    let seg = pad
        .sticky_event::<gst::event::Segment>(0)?
        .segment()
        .clone()
        .downcast::<gst::ClockTime>()
        .ok()?;
    Some(match seg.to_running_time_full(pts)? {
        gst::Signed::Positive(t) => t.nseconds() as i64,
        gst::Signed::Negative(t) => -(t.nseconds() as i64),
    })
}

fn build_receiver(port: u16, decode: bool, retiming: bool) -> Receiver {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://:{}?mode=listener", port)),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("decode".to_string(), PropertyValue::Bool(decode));
    props.insert(
        "arrival_retiming".to_string(),
        PropertyValue::Bool(retiming),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = MpegTsSrtInputBuilder
        .build(INSTANCE_ID, &props, &ctx)
        .expect("MPEG-TS/SRT Input block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = by_id[&from.element_id]
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("internal link source pad");
        let sink = by_id[&to.element_id]
            .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
            .expect("internal link sink pad");
        src.link(&sink).expect("link internal pads");
    }

    let srtsrc = by_id[&format!("{}:srtsrc", INSTANCE_ID)].clone();

    let samples: Arc<Mutex<Vec<Sample>>> = Default::default();
    let first_at = Arc::new(std::sync::atomic::AtomicI64::new(-1));
    for (media, id) in [
        ("video", format!("{}:video_output", INSTANCE_ID)),
        ("audio", format!("{}:audio_output_0", INSTANCE_ID)),
    ] {
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        by_id[&id].link(&sink).expect("link output -> fakesink");
        let (samples, first_at) = (samples.clone(), first_at.clone());
        by_id[&id].static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, info| {
                // The pipeline's running time now: test code may read the
                // clock in a buffer probe.
                let now = pad
                    .parent_element()
                    .and_then(|e| e.current_running_time())
                    .map(|t| t.nseconds() as i64);
                let pts = info.buffer().and_then(|b| b.pts());
                let rt = pts.and_then(|pts| running_time(pad, pts));
                if let (Some(now), Some(rt), Some(pts)) = (now, rt, pts) {
                    let _ = first_at.compare_exchange(
                        -1,
                        now,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    let t0 = first_at.load(std::sync::atomic::Ordering::Relaxed);
                    samples.lock().unwrap().push(Sample {
                        media,
                        at_ms: (now - t0) / 1_000_000,
                        lead_ms: (rt - now) / 1_000_000,
                        shift_ms: (rt - pts.nseconds() as i64) / 1_000_000,
                    });
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    Receiver {
        pipeline,
        srtsrc,
        samples,
    }
}

/// x264 settings for the caller: no B-frames, or two of them.
const NO_B_FRAMES: &str = "tune=zerolatency speed-preset=ultrafast";
const B_FRAMES: &str = "speed-preset=superfast bframes=2 b-adapt=false rc-lookahead=5";

fn start_caller(port: u16) -> gst::Pipeline {
    start_caller_with(port, NO_B_FRAMES)
}

fn start_caller_with(port: u16, x264: &str) -> gst::Pipeline {
    let caller = gst::parse::launch(&format!(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=180,framerate=30/1 \
         ! x264enc {x264} key-int-max=30 ! h264parse ! mux. \
         audiotestsrc is-live=true ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! mux. \
         mpegtsmux name=mux alignment=7 \
         ! srtsink name=caller uri=srt://127.0.0.1:{port}?mode=caller latency=125"
    ))
    .expect("caller pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();
    caller.set_state(gst::State::Playing).expect("caller plays");
    caller
}

fn stop(pipeline: &gst::Pipeline, srt_element: Option<gst::Element>) {
    if let Some(e) = srt_element {
        let _ = e.set_state(gst::State::Null);
    }
    pipeline.set_state(gst::State::Null).expect("to NULL");
}

/// Stops the caller, waits, then the receiver. With `keep-listening`, srtsrc
/// restarts its listener when the caller leaves, and the restart resets the
/// cancellable a stop has just cancelled: a stop that lands in that window
/// hangs in `gst_srt_object_wait_caller` forever (see `docs/UPSTREAM_NOTES.md`).
fn stop_caller_then_receiver(caller: &gst::Pipeline, r: &Receiver) {
    stop(caller, caller.by_name("caller"));
    std::thread::sleep(Duration::from_millis(500));
    stop(&r.pipeline, Some(r.srtsrc.clone()));
}

fn summary(samples: &[Sample]) -> String {
    let mut by_second: std::collections::BTreeMap<(i64, &str), (i64, i64)> = Default::default();
    for s in samples {
        let e = by_second
            .entry((s.at_ms / 1000, s.media))
            .or_insert((i64::MAX, i64::MIN));
        e.0 = e.0.min(s.lead_ms);
        e.1 = e.1.max(s.lead_ms);
    }
    by_second
        .iter()
        .map(|((sec, media), (lo, hi))| format!("{sec}s {media} {lo}..{hi}ms"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Leads from `from_ms` on, after the first buffer out.
fn leads_from(samples: &[Sample], from_ms: i64) -> Vec<i64> {
    samples
        .iter()
        .filter(|s| s.at_ms >= from_ms)
        .map(|s| s.lead_ms)
        .collect()
}

fn early_caller_is_retimed(decode: bool) {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = srt_port();
    let caller = start_caller(port);
    std::thread::sleep(EARLY_START);

    let r = build_receiver(port, decode, true);
    play(&r);
    std::thread::sleep(Duration::from_secs(5));
    stop_caller_then_receiver(&caller, &r);

    let samples = r.samples.lock().unwrap().clone();
    let media: std::collections::HashSet<_> = samples.iter().map(|s| s.media).collect();
    assert_eq!(
        media.len(),
        2,
        "both outputs carry media: {}",
        summary(&samples)
    );
    // Without retiming, every buffer leads by about EARLY_START (~2000 ms).
    // Decoded output is measured after the decoder, so its lead also loses the
    // decode time, which varies with load.
    let lowest = if decode { TARGET_MS - 250 } else { -BUDGET_MS };
    // The stale head (the encode before the connect) is late by design. Decoded,
    // its last frames can leave the decoder after 1.5 s under load (seen at
    // 1.54 s with the integration suite in parallel), so look from 2.5 s.
    let from_ms = if decode { 2500 } else { 1500 };
    let after = leads_from(&samples, from_ms);
    assert!(
        !after.is_empty(),
        "no buffers after {from_ms} ms: {}",
        summary(&samples)
    );
    assert!(
        after
            .iter()
            .all(|&l| (lowest..=TARGET_MS + 400).contains(&l)),
        "a caller that encoded {} ms before connecting is still placed off its \
         arrival: {}",
        EARLY_START.as_millis(),
        summary(&samples)
    );
}

#[test]
fn a_caller_that_encoded_before_connecting_is_retimed_passthrough() {
    early_caller_is_retimed(false);
}

#[test]
fn a_caller_that_encoded_before_connecting_is_retimed_decoded() {
    early_caller_is_retimed(true);
}

/// A caller that connects to a running listener: never late, and the latest
/// stream no later than the margin before its deadline. Retiming against the
/// raw arrival put it `+MARGIN_MS` after arrival, on top of the SRT latency
/// the block reports (#1265).
#[test]
fn a_clean_caller_stays_within_the_margin() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = srt_port();
    let r = build_receiver(port, false, true);
    play(&r);
    std::thread::sleep(Duration::from_millis(500));
    let caller = start_caller(port);
    std::thread::sleep(Duration::from_secs(4));
    stop_caller_then_receiver(&caller, &r);

    let samples = r.samples.lock().unwrap().clone();
    let leads = leads_from(&samples, 500);
    assert!(!leads.is_empty(), "no buffers: {}", summary(&samples));
    assert!(
        leads
            .iter()
            .all(|&l| (-BUDGET_MS..=TARGET_MS + 200).contains(&l)),
        "a clean caller missed its deadline or left the margin: {}",
        summary(&samples)
    );
    let mut audio: Vec<i64> = samples
        .iter()
        .filter(|s| s.media == "audio" && s.at_ms >= 1500)
        .map(|s| s.lead_ms)
        .collect();
    audio.sort();
    let lowest_audio = audio[audio.len() / 20];
    assert!(
        lowest_audio <= TARGET_MS + 40,
        "the latest stream waits {lowest_audio} ms past arrival, more than the \
         margin before its deadline ({TARGET_MS} ms): {}",
        summary(&samples)
    );
}

/// After a caller whose stale head was corrected, the next caller (keep
/// listening) is clean: tsdemux's fresh reference puts it behind the first's,
/// and the correction must not stamp it late, from its first buffer on. Without
/// the `caller-added` signal its first ~100 ms go out late until the lateness
/// is confirmed.
#[test]
fn a_clean_caller_after_a_corrected_one_is_not_late() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = srt_port();
    let first = start_caller(port);
    std::thread::sleep(EARLY_START);
    let r = build_receiver(port, false, true);
    play(&r);
    std::thread::sleep(Duration::from_secs(3));
    stop(&first, first.by_name("caller"));
    let switch_at = r.samples.lock().unwrap().last().map_or(0, |s| s.at_ms);
    std::thread::sleep(Duration::from_millis(500));

    let second = start_caller(port);
    std::thread::sleep(Duration::from_secs(4));
    stop_caller_then_receiver(&second, &r);

    let samples = r.samples.lock().unwrap().clone();
    let second_first = samples
        .iter()
        .find(|s| s.at_ms > switch_at + 400)
        .map(|s| s.at_ms)
        .unwrap_or_else(|| panic!("the second caller delivered nothing: {}", summary(&samples)));
    let after = leads_from(&samples, second_first);
    assert!(!after.is_empty(), "{}", summary(&samples));
    assert!(
        after
            .iter()
            .all(|&l| (-BUDGET_MS..=TARGET_MS + 400).contains(&l)),
        "the second caller is placed off its arrival: {}",
        summary(&samples)
    );
}

/// Video with B-frames leaves tsdemux in decode order, its PTS stepping back
/// at every B-frame. Every buffer of every stream must carry the one offset:
/// compared by PTS, each B-frame raised video's offset and the next P-frame
/// lowered it, so B-frames went out at the wrong time and video drifted 40 to
/// 120 ms behind audio.
#[test]
fn b_frames_get_the_offset_of_every_other_stream() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = srt_port();
    let r = build_receiver(port, false, true);
    play(&r);
    std::thread::sleep(Duration::from_millis(500));
    let caller = start_caller_with(port, B_FRAMES);
    std::thread::sleep(Duration::from_secs(5));
    stop_caller_then_receiver(&caller, &r);

    let samples = r.samples.lock().unwrap().clone();
    let shifts = |media: &str| -> Vec<i64> {
        samples
            .iter()
            .filter(|s| s.media == media && s.at_ms >= 2500)
            .map(|s| s.shift_ms)
            .collect()
    };
    let (video, audio) = (shifts("video"), shifts("audio"));
    assert!(
        !video.is_empty() && !audio.is_empty(),
        "both outputs carry media: {}",
        summary(&samples)
    );
    let mut sorted = audio.clone();
    sorted.sort();
    let audio_shift = sorted[sorted.len() / 2];
    let off: Vec<i64> = video
        .iter()
        .map(|v| v - audio_shift)
        .filter(|d| d.abs() > 10)
        .collect();
    assert!(
        off.is_empty(),
        "{} of {} video buffers carry another offset than audio (ms off): {:?}",
        off.len(),
        video.len(),
        &off[..off.len().min(30)]
    );
}
