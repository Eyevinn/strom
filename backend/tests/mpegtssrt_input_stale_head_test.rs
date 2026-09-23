//! Regression test: an SRT caller whose stream starts with stale frames must not
//! be stamped into the future.
//!
//! A caller that has been encoding while it waited for the listener delivers a
//! few frames from the start of its encode and then jumps to live PTS. With
//! `ignore-pcr`, `tsdemux` takes its time reference from the first frame and
//! places every later frame relative to it, so live audio comes out as far
//! ahead of its arrival as the caller waited — seconds — and every sink and
//! mixer downstream holds it that long.
//!
//! The test replays that over a real SRT connection into the block: a
//! short head from the start of a pre-encoded transport stream, then the same
//! stream from two seconds in, paced in real time. It measures how far each
//! buffer leaving the block runs ahead of the latest SRT input.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::builtin::tsdemux_anchor::{MAX_LEAD, WINDOW};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

const REQUIRED: &[&str] = &[
    "srtsrc",
    "srtsink",
    "tsdemux",
    "mpegtsmux",
    "decodebin",
    "avenc_aac",
    "avdec_aac",
    "aacparse",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "appsrc",
    "appsink",
    "fakesink",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

/// Where the live part of the replayed stream starts.
const GAP: gst::ClockTime = gst::ClockTime::from_seconds(2);
/// Stale frames sent ahead of the live part.
const HEAD: gst::ClockTime = gst::ClockTime::from_mseconds(150);
/// Live stream replayed after the head.
const LIVE: gst::ClockTime = gst::ClockTime::from_mseconds(2500);

/// A 128 kbit/s AAC transport stream in 1316-byte chunks, with each chunk's
/// content time, encoded as fast as possible.
fn encode_stream() -> Vec<(gst::ClockTime, gst::Buffer)> {
    let seconds = (GAP + 2 * LIVE + 2 * gst::ClockTime::SECOND).seconds() as i32;
    let pipeline = gst::parse::launch(&format!(
        "audiotestsrc num-buffers={} samplesperbuffer=4800 wave=ticks \
         ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! avenc_aac bitrate=128000 \
         ! aacparse ! mpegtsmux alignment=7 ! appsink name=out sync=false",
        seconds * 10
    ))
    .expect("encoder pipeline parses")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let appsink = pipeline
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut chunks = Vec::new();
    let mut last = gst::ClockTime::ZERO;
    while let Ok(sample) = appsink.pull_sample() {
        let buffer = sample.buffer_owned().unwrap();
        last = buffer.pts().unwrap_or(last);
        chunks.push((last, buffer));
    }
    pipeline.set_state(gst::State::Null).unwrap();
    assert!(
        chunks.last().is_some_and(|(t, _)| *t > GAP + 2 * LIVE),
        "encoder produced too little stream to replay"
    );
    chunks
}

struct Receiver {
    pipeline: gst::Pipeline,
    /// (latest SRT input running time, lead of the output buffer) in ns.
    leads: Arc<Mutex<Vec<(u64, i64)>>>,
}

fn build_receiver(decode: bool, port: u16, keep_listening: bool) -> Receiver {
    let instance_id = "srt_in";
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("decode".to_string(), PropertyValue::Bool(decode));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(0));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("latency".to_string(), PropertyValue::Int(20));
    props.insert(
        "keep_listening".to_string(),
        PropertyValue::Bool(keep_listening),
    );
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", port)),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = MpegTsSrtInputBuilder
        .build(instance_id, &props, &ctx)
        .expect("mpegts/srt input block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src_pad = by_id[&from.element_id]
            .static_pad(from.pad_name.as_deref().unwrap())
            .unwrap();
        let sink_pad = by_id[&to.element_id]
            .static_pad(to.pad_name.as_deref().unwrap())
            .unwrap();
        src_pad.link(&sink_pad).expect("internal link");
    }

    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .unwrap();
    pipeline.add(&sink).unwrap();
    let output = &by_id[&format!("{}:audio_output_0", instance_id)];
    output.link(&sink).expect("link audio output to fakesink");

    let last_input = Arc::new(AtomicU64::new(u64::MAX));
    let recorder = last_input.clone();
    by_id[&format!("{}:srtsrc", instance_id)]
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                recorder.store(pts.nseconds(), Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        })
        .unwrap();

    let leads: Arc<Mutex<Vec<(u64, i64)>>> = Arc::default();
    let recorder = leads.clone();
    sink.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let input = last_input.load(Ordering::Relaxed);
            let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                return gst::PadProbeReturn::Ok;
            };
            let running_time = pad.sticky_event::<gst::event::Segment>(0).and_then(|s| {
                s.segment()
                    .downcast_ref::<gst::format::Time>()
                    .and_then(|t| t.to_running_time_full(pts))
            });
            if let (Some(rt), true) = (running_time, input != u64::MAX) {
                let rt = match rt {
                    gst::Signed::Positive(t) => t.nseconds() as i64,
                    gst::Signed::Negative(t) => -(t.nseconds() as i64),
                };
                recorder.lock().unwrap().push((input, rt - input as i64));
            }
            gst::PadProbeReturn::Ok
        })
        .unwrap();

    Receiver { pipeline, leads }
}

/// One SRT caller, connected and ready to be fed.
fn connect(port: u16) -> (gst::Pipeline, gst_app::AppSrc) {
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=in is-live=true format=time \
         caps=video/mpegts,systemstream=true,packetsize=188 \
         ! srtsink name=out uri=srt://127.0.0.1:{}?mode=caller latency=20 \
           wait-for-connection=true sync=false",
        port
    ))
    .expect("sender pipeline parses")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let appsrc = pipeline
        .by_name("in")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    (pipeline, appsrc)
}

/// The stale head, then the tables of the skipped part so the caller's PSI
/// stays continuous across the jump.
fn push_head(appsrc: &gst_app::AppSrc, chunks: &[(gst::ClockTime, gst::Buffer)]) {
    for (_, buffer) in chunks.iter().filter(|(t, _)| *t < HEAD) {
        appsrc.push_buffer(buffer.copy()).unwrap();
    }
    for buffer in tables_between(chunks, HEAD, GAP) {
        appsrc.push_buffer(buffer).unwrap();
    }
}

/// [`LIVE`] of stream from `from`, paced in real time.
fn push_live(
    appsrc: &gst_app::AppSrc,
    chunks: &[(gst::ClockTime, gst::Buffer)],
    from: gst::ClockTime,
) {
    let start = Instant::now();
    for (t, buffer) in chunks
        .iter()
        .filter(|(t, _)| *t >= from && *t < from + LIVE)
    {
        let due = Duration::from_nanos((*t - from).nseconds());
        if let Some(wait) = due.checked_sub(start.elapsed()) {
            std::thread::sleep(wait);
        }
        appsrc.push_buffer(buffer.copy()).unwrap();
    }
    std::thread::sleep(Duration::from_millis(300));
}

/// Sends the head, then the live part in real time, over one SRT connection.
/// Returns the sender still connected; see [`stop_srt`] for the teardown order.
fn replay(port: u16, chunks: &[(gst::ClockTime, gst::Buffer)]) -> gst::Pipeline {
    let (pipeline, appsrc) = connect(port);
    push_head(&appsrc, chunks);
    push_live(&appsrc, chunks, GAP);
    pipeline
}

const TS_PACKET: usize = 188;

fn ts_pid(packet: &[u8]) -> u16 {
    (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
}

/// Whether a TS packet starts a PES packet, i.e. carries media rather than
/// tables.
fn starts_pes(packet: &[u8]) -> bool {
    if packet[1] & 0x40 == 0 {
        return false;
    }
    let payload = match (packet[3] >> 4) & 0x3 {
        1 => 4,
        3 => 5 + usize::from(packet[4]),
        _ => return false,
    };
    packet.get(payload..payload + 3) == Some(&[0, 0, 1][..])
}

/// The table packets (PAT, PMT) of the chunks in `[from, to)`, without media.
///
/// Keeps the tables continuous across the jump to the live part. GStreamer
/// 1.24.2, which CI runs, re-applies the PMT after a continuity break and, with
/// `ignore-pcr`, mistakes it for a new program (fixed in 1.24.3, "mpegtsbase:
/// Fix Program equality check"). tsdemux then replaces its source pad, the new
/// pad is not linked, and the stream stops before any live audio leaves the
/// block.
fn tables_between(
    chunks: &[(gst::ClockTime, gst::Buffer)],
    from: gst::ClockTime,
    to: gst::ClockTime,
) -> Vec<gst::Buffer> {
    let mut media_pids = std::collections::HashSet::new();
    for (_, buffer) in chunks {
        let map = buffer.map_readable().unwrap();
        for packet in map.chunks_exact(TS_PACKET).filter(|p| starts_pes(p)) {
            media_pids.insert(ts_pid(packet));
        }
    }
    chunks
        .iter()
        .filter(|(t, _)| *t >= from && *t < to)
        .filter_map(|(_, buffer)| {
            let map = buffer.map_readable().unwrap();
            let tables: Vec<u8> = map
                .chunks_exact(TS_PACKET)
                .filter(|p| !media_pids.contains(&ts_pid(p)))
                .flatten()
                .copied()
                .collect();
            (!tables.is_empty()).then(|| gst::Buffer::from_mut_slice(tables))
        })
        .collect()
}

/// Stops the named SRT element on its own, then the rest of its pipeline.
fn stop_srt(pipeline: &gst::Pipeline, element: &str) {
    let _ = pipeline
        .by_name(element)
        .unwrap()
        .set_state(gst::State::Null);
    let _ = pipeline.set_state(gst::State::Null);
}

fn max_live_lead(decode: bool) -> Option<(i64, usize)> {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return None;
    }

    let chunks = encode_stream();
    let port = srt_port();
    let receiver = build_receiver(decode, port, false);
    receiver
        .pipeline
        .set_state(gst::State::Playing)
        .expect("receiver goes to PLAYING");

    let sender = replay(port, &chunks);

    // Receiver first, while the caller is still connected: an srtsrc whose
    // caller has just left can park waiting for the next one and never wake
    // for the state change.
    stop_srt(&receiver.pipeline, "srt_in:srtsrc");
    stop_srt(&sender, "out");

    let leads = receiver.leads.lock().unwrap().clone();
    let first_input = leads.first().map(|(i, _)| *i).expect(
        "no audio left the block — the SRT stream never decoded, so the test proved nothing",
    );
    // The first window closes over the head, whose lead is small; the second
    // is all live and triggers the correction. After that every buffer must be
    // on time.
    let settle = first_input + (2 * WINDOW + gst::ClockTime::from_mseconds(250)).nseconds();
    let settled: Vec<i64> = leads
        .iter()
        .filter(|(input, _)| *input >= settle)
        .map(|(_, lead)| *lead)
        .collect();
    assert!(
        settled.len() > 20,
        "only {} buffers after the correction window — too few to judge",
        settled.len()
    );
    Some((settled.into_iter().max().unwrap(), leads.len()))
}

fn assert_on_time(decode: bool) {
    let Some((max_lead, count)) = max_live_lead(decode) else {
        return;
    };
    assert!(
        max_lead < MAX_LEAD.nseconds() as i64,
        "decode={}: live audio left the block {} ms ahead of its SRT arrival \
         ({} buffers). tsdemux took its time reference from the stale head, so \
         every sink downstream holds this source back by the caller's wait.",
        decode,
        max_lead / 1_000_000,
        count
    );
}

/// Whether the caller that follows a corrected one leaves the block on time.
///
/// `keep_listening` is on by default: one `srtsrc` serves caller after caller
/// and `tsdemux` takes a fresh time reference for each, so a correction that
/// outlives its caller stamps the next one behind its own arrival, where
/// consumers that wait for running time drop it. `second_from` is where the
/// second caller's stream starts: the same encoder carrying on, or a fresh one
/// from zero.
fn assert_second_caller_on_time(second_from: gst::ClockTime) {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let chunks = encode_stream();
    let port = srt_port();
    let receiver = build_receiver(false, port, true);
    receiver
        .pipeline
        .set_state(gst::State::Playing)
        .expect("receiver goes to PLAYING");

    // A caller with a stale head, so that a correction is made.
    let (sender, appsrc) = connect(port);
    push_head(&appsrc, &chunks);
    push_live(&appsrc, &chunks, GAP);
    stop_srt(&sender, "out");
    std::thread::sleep(Duration::from_millis(500));
    let boundary = receiver.leads.lock().unwrap().len();

    // A second caller, with no stale head of its own. Nothing is replayed
    // ahead of it: re-sending tables the first caller already sent breaks the
    // continuity counters, which on GStreamer 1.24.2 costs the stream its pad.
    let (sender, appsrc) = connect(port);
    push_live(&appsrc, &chunks, second_from);
    stop_srt(&receiver.pipeline, "srt_in:srtsrc");
    stop_srt(&sender, "out");

    let leads = receiver.leads.lock().unwrap().clone();
    let first = leads
        .get(boundary)
        .map(|(i, _)| *i)
        .expect("no audio left the block for the second caller, so the test proved nothing");
    // Only the reconnect itself: the correction must go with the caller that
    // was measured, not a window or two later.
    let settle = first + gst::ClockTime::from_mseconds(300).nseconds();
    let settled: Vec<i64> = leads[boundary..]
        .iter()
        .filter(|(input, _)| *input >= settle)
        .map(|(_, lead)| *lead)
        .collect();
    assert!(
        settled.len() > 20,
        "only {} buffers from the second caller after it settled — too few to judge",
        settled.len()
    );
    let worst = settled
        .iter()
        .copied()
        .max_by_key(|lead| lead.abs())
        .unwrap();
    assert!(
        worst.abs() < MAX_LEAD.nseconds() as i64,
        "the second caller's audio left the block {} ms off its own arrival          ({} buffers). The correction made for the caller that has gone is          still on the pads, so every consumer that waits for running time          drops this caller as late.",
        worst / 1_000_000,
        settled.len()
    );
}

#[test]
fn stale_head_is_not_stamped_ahead_decoded() {
    assert_on_time(true);
}

#[test]
fn stale_head_is_not_stamped_ahead_passthrough() {
    assert_on_time(false);
}

#[test]
fn second_caller_restarting_is_not_stamped_behind() {
    assert_second_caller_on_time(gst::ClockTime::ZERO);
}

#[test]
fn second_caller_resuming_is_not_stamped_behind() {
    assert_second_caller_on_time(GAP + LIVE);
}
