//! Regression test (#1077): with one audio output, the MPEG-TS/SRT input must
//! play the first audio stream of a feed that carries two.
//!
//! The block took an output that was already fed for any new pad, as if the
//! new pad replaced the old one (the reconnect case of #856). A second audio
//! stream present at the same time looks the same, so it took over
//! `audio_out_0` and the feed played its second stream.
//!
//! `tsdemux` ends each stream-id with the stream's PID, and the id travels
//! through `decodebin`, so the STREAM_START events on the output say which
//! stream it carries in both modes. A reconnect with both PIDs moved must
//! still end on the first stream.

pub mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::BlockBuilder;
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

/// An SRT caller with two audio streams, stereo on `first_pad` and mono on
/// `second_pad`. `sink_65` is PID 0x41, `sink_66` is 0x42, and so on.
fn dual_audio_caller(port: u16, first_pad: &str, second_pad: &str) -> gst::Pipeline {
    let description = format!(
        "mpegtsmux name=m alignment=7 ! srtsink uri=srt://127.0.0.1:{port}?mode=caller \
         latency=20 wait-for-connection=true sync=false \
         audiotestsrc is-live=true samplesperbuffer=1024 freq=440 \
         ! audio/x-raw,rate=48000,channels=2 ! audioconvert ! avenc_aac ! aacparse ! m.{first_pad} \
         audiotestsrc is-live=true samplesperbuffer=1024 freq=880 \
         ! audio/x-raw,rate=48000,channels=1 ! audioconvert ! avenc_aac ! aacparse ! m.{second_pad}"
    );
    gst::parse::launch(&description)
        .expect("caller pipeline should parse")
        .downcast::<gst::Pipeline>()
        .expect("parse::launch returns a pipeline")
}

/// The stream-id of every STREAM_START that reached the audio output, in
/// order, while one caller ran.
type StreamLog = Arc<Mutex<Vec<String>>>;

/// Run two dual-audio callers one after the other: the first on PIDs 0x41 and
/// 0x42, the second on 0x51 and 0x52. Returns the stream-ids the output saw
/// from each caller, and the first error the block's pipeline posted.
fn streams_per_caller(decode: bool) -> (Vec<String>, Vec<String>, Option<String>) {
    let instance_id = "tsin";
    let port = common::free_udp_port();

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

    let ctx = common::block::context();
    let built = MpegTsSrtInputBuilder
        .build(instance_id, &props, &ctx)
        .expect("mpegts/srt input block builds");

    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);

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

    let log: StreamLog = Arc::new(Mutex::new(Vec::new()));
    let writer = log.clone();
    audio_output
        .static_pad("src")
        .expect("identity has a src pad")
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if let gst::EventView::StreamStart(start) = event.view() {
                    writer.lock().unwrap().push(start.stream_id().to_string());
                }
            }
            gst::PadProbeReturn::Ok
        })
        .expect("probe attaches to the audio output");

    common::block::run_setups(&ctx);

    let bus = pipeline.bus().expect("pipeline has a bus");
    pipeline
        .set_state(gst::State::Playing)
        .expect("block pipeline goes to PLAYING");
    let mut error =
        common::bus::first_error(&bus, Duration::from_millis(500)).map(|e| e.to_string());

    let mut run_caller = |first_pad: &str, second_pad: &str| -> Vec<String> {
        log.lock().unwrap().clear();
        let caller = dual_audio_caller(port, first_pad, second_pad);
        caller
            .set_state(gst::State::Playing)
            .expect("caller goes to PLAYING");
        // Both streams must have been exposed and settled before the result
        // is read, so this waits out a fixed window rather than the first one.
        let found =
            common::bus::first_error(&bus, Duration::from_millis(2500)).map(|e| e.to_string());
        if error.is_none() {
            error = found;
        }
        let _ = caller.set_state(gst::State::Null);
        let seen = log.lock().unwrap().clone();
        // Let the listener notice the caller is gone before the next dials.
        let found =
            common::bus::first_error(&bus, Duration::from_millis(700)).map(|e| e.to_string());
        if error.is_none() {
            error = found;
        }
        seen
    };

    let from_a = run_caller("sink_65", "sink_66");
    let from_b = run_caller("sink_81", "sink_82");

    let _ = pipeline.set_state(gst::State::Null);

    (from_a, from_b, error)
}

fn assert_first_stream_wins(decode: bool) {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let mode = if decode { "decode" } else { "passthrough" };
    let (from_a, from_b, error) = streams_per_caller(decode);

    for (caller, seen, first_pid) in [
        ("first caller", &from_a, "00000041"),
        ("reconnecting caller", &from_b, "00000051"),
    ] {
        assert!(
            !seen.is_empty(),
            "{mode}: the {caller} started no stream on audio_output_0, so the test proved \
             nothing (error: {error:?})"
        );
        let last = seen.last().unwrap();
        assert!(
            last.ends_with(first_pid),
            "{mode}: audio_output_0 ended on {last}, not the first audio stream (PID \
             {first_pid}), with the {caller}; stream-ids seen, in order: {seen:?}. \
             First pipeline error: {error:?}"
        );
    }
}

#[test]
fn passthrough_input_plays_the_first_of_two_audio_streams() {
    assert_first_stream_wins(false);
}

#[test]
fn decode_input_plays_the_first_of_two_audio_streams() {
    assert_first_stream_wins(true);
}
