//! Regression test (#856): the MPEG-TS/SRT input must survive a reconnecting
//! caller whose stream is laid out differently from the previous caller's.
//!
//! With `keep_listening` on, a new caller on the same listener is a program
//! change to the demuxer: it replaces its source pad for a stream whose PID
//! moved. The block claimed each output once, on the first `pad-added`, and
//! never gave it back, so the replacement pad found no free output, stayed
//! unlinked, and the demuxer stopped the whole input with `not-linked`.
//!
//! The two demuxers replace pads in opposite orders — `tsdemux` adds the new
//! pad before removing the old one, `decodebin` removes first — so both modes
//! are driven here.

// Not on Windows: `gstsrt.dll` intermittently fails to load inside the test
// process there (#834); see `mpegtssrt_streamheader_test.rs`.
#![cfg(not(target_os = "windows"))]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "srtsrc",
    "srtsink",
    "tsdemux",
    "mpegtsmux",
    "decodebin",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "avenc_aac",
    "avdec_aac",
    "aacparse",
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

/// A free UDP port for the SRT listener. Binding one and dropping it races with
/// anything else on the host; a failure to bind shows up as a named error.
fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

/// An audio-only SRT caller. `mux_pad` picks the audio PID: `sink_65` is 0x41,
/// `sink_66` is 0x42.
fn caller(port: u16, mux_pad: &str) -> gst::Pipeline {
    let description = format!(
        "mpegtsmux name=m alignment=7 ! srtsink uri=srt://127.0.0.1:{port}?mode=caller \
         latency=20 wait-for-connection=true sync=false \
         audiotestsrc is-live=true samplesperbuffer=1024 ! audio/x-raw,rate=48000,channels=2 \
         ! audioconvert ! avenc_aac ! aacparse ! m.{mux_pad}"
    );
    gst::parse::launch(&description)
        .expect("caller pipeline should parse")
        .downcast::<gst::Pipeline>()
        .expect("parse::launch returns a pipeline")
}

/// First error on the bus within `wait`, named by the element that posted it.
fn first_error(bus: &gst::Bus, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
            if let gst::MessageView::Error(err) = msg.view() {
                let src = err
                    .src()
                    .map(|o| o.name().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                return Some(format!("{}: {} ({:?})", src, err.error(), err.debug()));
            }
        }
    }
    None
}

/// Run caller A on audio PID 0x41, stop it, then run caller B on 0x42. Returns
/// the buffers that reached the block's audio output from A and from B, and
/// the first error the block's pipeline posted.
fn buffers_across_pid_change(decode: bool) -> (u64, u64, Option<String>) {
    let instance_id = "tsin";
    let port = srt_port();

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("decode".to_string(), PropertyValue::Bool(decode));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(0));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", port)),
    );
    props.insert("latency".to_string(), PropertyValue::UInt(20));
    props.insert("keep_listening".to_string(), PropertyValue::Bool(true));

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
        let src = by_id
            .get(&from.element_id)
            .unwrap_or_else(|| panic!("internal link source {} missing", from.element_id));
        let sink = by_id
            .get(&to.element_id)
            .unwrap_or_else(|| panic!("internal link target {} missing", to.element_id));
        match (&from.pad_name, &to.pad_name) {
            (Some(src_pad), Some(sink_pad)) => {
                let src_pad = src
                    .static_pad(src_pad)
                    .unwrap_or_else(|| panic!("{} has no pad {}", from.element_id, src_pad));
                let sink_pad = sink
                    .static_pad(sink_pad)
                    .unwrap_or_else(|| panic!("{} has no pad {}", to.element_id, sink_pad));
                src_pad
                    .link(&sink_pad)
                    .unwrap_or_else(|e| panic!("internal pad link failed: {:?}", e));
            }
            _ => src
                .link(sink)
                .unwrap_or_else(|e| panic!("internal element link failed: {:?}", e)),
        }
    }

    let audio_output = by_id
        .get(&format!("{}:audio_output_0", instance_id))
        .expect("block exposes audio_output_0")
        .clone();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .expect("fakesink");
    pipeline.add(&sink).unwrap();
    audio_output
        .link(&sink)
        .expect("link audio output to fakesink");

    let buffers = Arc::new(AtomicU64::new(0));
    let counter = buffers.clone();
    audio_output
        .static_pad("src")
        .expect("identity has a src pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        })
        .expect("probe attaches to the audio output");

    for setup in ctx.take_element_setups() {
        setup(
            uuid::Uuid::new_v4(),
            strom::events::EventBroadcaster::new(16),
        );
    }

    let bus = pipeline.bus().expect("pipeline has a bus");
    pipeline
        .set_state(gst::State::Playing)
        .expect("block pipeline goes to PLAYING");
    // Let the listener come up before the first caller dials it.
    let mut error = first_error(&bus, Duration::from_millis(500));

    let caller_a = caller(port, "sink_65");
    caller_a
        .set_state(gst::State::Playing)
        .expect("caller A goes to PLAYING");
    if error.is_none() {
        error = first_error(&bus, Duration::from_millis(2500));
    }
    let _ = caller_a.set_state(gst::State::Null);
    let from_a = buffers.load(Ordering::Relaxed);

    if error.is_none() {
        error = first_error(&bus, Duration::from_millis(700));
    }
    let before_b = buffers.load(Ordering::Relaxed);

    let caller_b = caller(port, "sink_66");
    caller_b
        .set_state(gst::State::Playing)
        .expect("caller B goes to PLAYING");
    if error.is_none() {
        error = first_error(&bus, Duration::from_secs(3));
    } else {
        std::thread::sleep(Duration::from_secs(3));
    }
    let from_b = buffers.load(Ordering::Relaxed) - before_b;

    let _ = caller_b.set_state(gst::State::Null);
    let _ = pipeline.set_state(gst::State::Null);

    (from_a, from_b, error)
}

/// At ~47 AAC frames a second, three seconds of a working caller B delivers
/// well over this. The regression delivers one buffer, then nothing.
const MIN_BUFFERS_FROM_B: u64 = 10;

fn assert_survives_pid_change(decode: bool) {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let mode = if decode { "decode" } else { "passthrough" };
    let (from_a, from_b, error) = buffers_across_pid_change(decode);

    assert!(
        from_a > 0,
        "{mode}: caller A delivered no buffers, so the test proved nothing (error: {error:?})"
    );
    assert!(
        from_b >= MIN_BUFFERS_FROM_B,
        "{mode}: caller B on a new audio PID delivered {from_b} buffer(s) to the block output \
         (caller A delivered {from_a}); the replacement demuxer pad found no free output. \
         First pipeline error: {error:?}"
    );
}

#[test]
fn passthrough_input_survives_a_caller_with_a_different_audio_pid() {
    assert_survives_pid_change(false);
}

#[test]
fn decode_input_survives_a_caller_with_a_different_audio_pid() {
    assert_survives_pid_change(true);
}
