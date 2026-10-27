//! Guard test (#1081): with `map_audio_by_pid`, the MPEG-TS/SRT input feeds
//! `audio_out_k` from the k-th audio stream in ascending PID order, whatever
//! order the demuxer adds its pads in. An output with no stream of its own
//! carries the first stream and is reported as a fallback, until a feed with
//! more streams arrives.
//!
//! Each caller lists its audio streams in its PMT out of PID order
//! (`PMT_ORDER_*` in `mpegtsmux`'s `prog-map`), so the demuxer adds the pads
//! out of PID order too. `tsdemux` ends each stream-id with the stream's PID,
//! and the id travels through `decodebin`, so the STREAM_START events on each
//! output say which stream it carries, in both modes.
//!
//! The second caller reconnects on the same listener with three streams on
//! new PIDs: every output moves to its own stream (the reconnect case of #877).

pub mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom::blocks::builtin::mpegtssrt_input::audio_by_pid::AUDIO_MAP_MESSAGE;
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
    "tee",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "avenc_aac",
    "avdec_aac",
    "aacparse",
    "fakesink",
];

const OUTPUTS: usize = 3;

/// An SRT caller with one AAC stream per PID in `pids`. The PMT lists the
/// streams in the order given, which need not be PID order.
fn caller(port: u16, pids: &[u32]) -> gst::Pipeline {
    let order: Vec<String> = pids
        .iter()
        .enumerate()
        .map(|(i, pid)| format!("PMT_ORDER_{pid}=(int){i}"))
        .collect();
    let mut description = format!(
        "mpegtsmux name=m alignment=7 prog-map=\"progmap,{}\" \
         ! srtsink uri=srt://127.0.0.1:{port}?mode=caller \
         latency=20 wait-for-connection=true sync=false",
        order.join(",")
    );
    for (i, pid) in pids.iter().enumerate() {
        description.push_str(&format!(
            " audiotestsrc is-live=true samplesperbuffer=1024 freq={} \
             ! audio/x-raw,rate=48000,channels={} ! audioconvert ! avenc_aac ! aacparse \
             ! m.sink_{pid}",
            440 * (i + 1),
            1 + i % 2
        ));
    }
    gst::parse::launch(&description)
        .expect("caller pipeline should parse")
        .downcast::<gst::Pipeline>()
        .expect("parse::launch returns a pipeline")
}

/// What one caller left on the block: the stream-ids each output saw, in
/// order, and the audio maps the block posted.
struct Seen {
    stream_ids: Vec<Vec<String>>,
    maps: Vec<AudioMap>,
}

/// One audio map message: the PID each output carries, and which outputs
/// are fallbacks.
type AudioMap = (Vec<i32>, Vec<bool>);

/// Run caller A, then caller B, on one listener. Returns what each left, and
/// the first error the block's pipeline posted.
fn run(decode: bool, a: &[u32], b: &[u32]) -> (Seen, Seen, Option<String>) {
    let instance_id = "tsin";
    let port = common::free_udp_port();

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("decode".to_string(), PropertyValue::Bool(decode));
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(0));
    props.insert(
        "num_audio_tracks".to_string(),
        PropertyValue::UInt(OUTPUTS as u64),
    );
    props.insert("map_audio_by_pid".to_string(), PropertyValue::Bool(true));
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

    let logs: Vec<Arc<Mutex<Vec<String>>>> = (0..OUTPUTS)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    for (k, log) in logs.iter().enumerate() {
        let output = by_id
            .get(&format!("{}:audio_output_{}", instance_id, k))
            .unwrap_or_else(|| panic!("block exposes audio_output_{k}"))
            .clone();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .expect("fakesink");
        pipeline.add(&sink).unwrap();
        output.link(&sink).expect("link audio output to fakesink");
        let writer = log.clone();
        output
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
    }

    let maps: Arc<Mutex<Vec<AudioMap>>> = Arc::new(Mutex::new(Vec::new()));
    let bus = pipeline.bus().expect("pipeline has a bus");
    let map_writer = maps.clone();
    bus.set_sync_handler(move |_bus, msg| {
        if let gst::MessageView::Element(element) = msg.view() {
            if let Some(s) = element
                .structure()
                .filter(|s| s.name() == AUDIO_MAP_MESSAGE)
            {
                let pids = s
                    .get::<gst::Array>("pids")
                    .map(|a| a.iter().filter_map(|v| v.get::<i32>().ok()).collect())
                    .unwrap_or_default();
                let fallback = s
                    .get::<gst::Array>("fallback")
                    .map(|a| a.iter().filter_map(|v| v.get::<bool>().ok()).collect())
                    .unwrap_or_default();
                map_writer.lock().unwrap().push((pids, fallback));
            }
        }
        gst::BusSyncReply::Pass
    });

    common::block::run_setups(&ctx);

    pipeline
        .set_state(gst::State::Playing)
        .expect("block pipeline goes to PLAYING");
    let mut error =
        common::bus::first_error(&bus, Duration::from_millis(500)).map(|e| e.to_string());

    let mut run_caller = |pids: &[u32]| -> Seen {
        for log in &logs {
            log.lock().unwrap().clear();
        }
        maps.lock().unwrap().clear();
        let caller = caller(port, pids);
        caller
            .set_state(gst::State::Playing)
            .expect("caller goes to PLAYING");
        // Every stream must have been exposed and settled before the result
        // is read, so this waits out a fixed window.
        let found =
            common::bus::first_error(&bus, Duration::from_millis(3000)).map(|e| e.to_string());
        if error.is_none() {
            error = found;
        }
        let _ = caller.set_state(gst::State::Null);
        let seen = Seen {
            stream_ids: logs.iter().map(|l| l.lock().unwrap().clone()).collect(),
            maps: maps.lock().unwrap().clone(),
        };
        // Let the listener notice the caller is gone before the next dials.
        let found =
            common::bus::first_error(&bus, Duration::from_millis(700)).map(|e| e.to_string());
        if error.is_none() {
            error = found;
        }
        seen
    };

    let from_a = run_caller(a);
    let from_b = run_caller(b);

    let _ = pipeline.set_state(gst::State::Null);
    bus.unset_sync_handler();

    // The tees and the demuxer's closures must not keep the pipeline alive.
    let weak = pipeline.downgrade();
    drop(pipeline);
    assert!(
        weak.upgrade().is_none(),
        "the block's pipeline survived its drop: a strong reference is held in a closure"
    );

    (from_a, from_b, error)
}

fn assert_outputs_follow_pid_order(decode: bool) {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let mode = if decode { "decode" } else { "passthrough" };
    // Caller A: two streams, PMT order 0x42, 0x41. Output 2 has no stream.
    // Caller B: three streams on new PIDs, PMT order 0x53, 0x51, 0x52.
    let (from_a, from_b, error) = run(decode, &[0x42, 0x41], &[0x53, 0x51, 0x52]);

    for (caller, seen, expected, fallback) in [
        (
            "first caller (2 streams)",
            &from_a,
            [0x41, 0x42, 0x41],
            [false, false, true],
        ),
        (
            "reconnecting caller (3 streams, new PIDs)",
            &from_b,
            [0x51, 0x52, 0x53],
            [false, false, false],
        ),
    ] {
        for (k, pid) in expected.iter().enumerate() {
            let ids = &seen.stream_ids[k];
            let want = format!("{pid:08x}");
            let last = ids.last().unwrap_or_else(|| {
                panic!(
                    "{mode}: with the {caller}, audio_output_{k} started no stream; it should \
                     carry PID 0x{pid:x}. Stream-ids per output: {:?}. First pipeline error: \
                     {error:?}",
                    seen.stream_ids
                )
            });
            assert!(
                last.ends_with(&want),
                "{mode}: with the {caller}, audio_output_{k} ended on {last}, not PID 0x{pid:x}. \
                 Stream-ids per output: {:?}. First pipeline error: {error:?}",
                seen.stream_ids
            );
        }

        let expected_pids: Vec<i32> = expected.to_vec();
        let last_map = seen.maps.last().unwrap_or_else(|| {
            panic!("{mode}: with the {caller}, the block posted no {AUDIO_MAP_MESSAGE} message")
        });
        assert_eq!(
            last_map,
            &(expected_pids, fallback.to_vec()),
            "{mode}: with the {caller}, the {AUDIO_MAP_MESSAGE} message reports the wrong \
             mapping (pids, fallback)"
        );
    }
}

#[test]
fn passthrough_audio_outputs_follow_pid_order() {
    assert_outputs_follow_pid_order(false);
}

#[test]
fn decode_audio_outputs_follow_pid_order() {
    assert_outputs_follow_pid_order(true);
}
