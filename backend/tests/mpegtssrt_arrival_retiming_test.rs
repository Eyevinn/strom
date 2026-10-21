//! The MPEG-TS/SRT Input stamps what it outputs from when the SRT data arrived
//! (`gst::ts_arrival_clock`), not from the first PES tsdemux parsed.
//!
//! Each test builds the real block, feeds it from an in-process H.264 + AAC
//! caller over SRT, and measures every buffer's *lead* at the block's outputs:
//! its running time minus the latest SRT arrival. Lead is what the stream adds
//! as delay downstream; below zero, a sync sink drops the buffer.
//!
//! A caller that starts before the listener blocks in `srtsink` until it can
//! connect, then sends the start of its encode followed by live media. tsdemux
//! anchors on that start, so without retiming the whole connection leads by as
//! long as the caller waited.

pub mod common;

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

/// How long the caller runs before the listener exists.
const EARLY_START: Duration = Duration::from_secs(2);

fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

/// One measured buffer: when it left the block (ms after the first one) and
/// its lead (ms).
#[derive(Clone, Copy, Debug)]
struct Sample {
    media: &'static str,
    at_ms: i64,
    lead_ms: i64,
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
    let arrival = Arc::new(std::sync::atomic::AtomicI64::new(-1));
    let arrival_in = arrival.clone();
    srtsrc
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                arrival_in.store(pts.nseconds() as i64, std::sync::atomic::Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });

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
        let (arrival, samples, first_at) = (arrival.clone(), samples.clone(), first_at.clone());
        by_id[&id].static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, info| {
                let now = arrival.load(std::sync::atomic::Ordering::Relaxed);
                let rt = info
                    .buffer()
                    .and_then(|b| b.pts())
                    .and_then(|pts| running_time(pad, pts));
                if let (true, Some(rt)) = (now >= 0, rt) {
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

fn start_caller(port: u16) -> gst::Pipeline {
    let caller = gst::parse::launch(&format!(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=180,framerate=30/1 \
         ! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=30 ! h264parse ! mux. \
         audiotestsrc is-live=true ! audio/x-raw,rate=48000,channels=2 ! avenc_aac ! aacparse ! mux. \
         mpegtsmux name=mux alignment=7 \
         ! srtsink name=caller uri=srt://127.0.0.1:{}?mode=caller latency=125",
        port
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
    r.pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    stop(&caller, caller.by_name("caller"));
    stop(&r.pipeline, Some(r.srtsrc.clone()));

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
    let lowest = if decode { -300 } else { -50 };
    let after = leads_from(&samples, 1500);
    assert!(
        !after.is_empty(),
        "no buffers after 1.5 s: {}",
        summary(&samples)
    );
    assert!(
        after.iter().all(|&l| (lowest..=400).contains(&l)),
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

/// A caller that connects to a running listener: within the margin from the
/// start, and never late.
#[test]
fn a_clean_caller_stays_within_the_margin() {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = srt_port();
    let r = build_receiver(port, false, true);
    r.pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let caller = start_caller(port);
    std::thread::sleep(Duration::from_secs(4));
    stop(&caller, caller.by_name("caller"));
    stop(&r.pipeline, Some(r.srtsrc.clone()));

    let samples = r.samples.lock().unwrap().clone();
    let leads = leads_from(&samples, 500);
    assert!(!leads.is_empty(), "no buffers: {}", summary(&samples));
    assert!(
        leads.iter().all(|&l| (-50..=300).contains(&l)),
        "a clean caller left the margin: {}",
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
    r.pipeline.set_state(gst::State::Playing).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    stop(&first, first.by_name("caller"));
    let switch_at = r.samples.lock().unwrap().last().map_or(0, |s| s.at_ms);
    std::thread::sleep(Duration::from_millis(500));

    let second = start_caller(port);
    std::thread::sleep(Duration::from_secs(4));
    stop(&second, second.by_name("caller"));
    stop(&r.pipeline, Some(r.srtsrc.clone()));

    let samples = r.samples.lock().unwrap().clone();
    let second_first = samples
        .iter()
        .find(|s| s.at_ms > switch_at + 400)
        .map(|s| s.at_ms)
        .unwrap_or_else(|| panic!("the second caller delivered nothing: {}", summary(&samples)));
    let after = leads_from(&samples, second_first);
    assert!(!after.is_empty(), "{}", summary(&samples));
    assert!(
        after.iter().all(|&l| (-50..=400).contains(&l)),
        "the second caller is placed off its arrival: {}",
        summary(&samples)
    );
}
