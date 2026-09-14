//! A decode-mode MPEG-TS/SRT Input with no SRT peer must not hold the pipeline
//! out of PLAYING (see `prepare_idle_decodebin`).
//!
//! The two tests are a pair: one asserts an absent caller does not block the
//! rest of the pipeline, the other that a caller connecting afterwards is still
//! decoded. Either alone could be satisfied by a change that breaks the other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE_ID: &str = "srt_c";

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "srtsrc",
    "srtsink",
    "decodebin",
    "tsdemux",
    "mpegtsmux",
    "avenc_aac",
    "aacparse",
    "avdec_aac",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "audiomixer",
    "identity",
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
/// anything else on the host.
fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

fn counting_sink(name: &str, is_async: bool) -> (gst::Element, Arc<AtomicUsize>) {
    let sink = gst::ElementFactory::make("fakesink")
        .name(name)
        .property("sync", false)
        .property("async", is_async)
        .property("signal-handoffs", true)
        .build()
        .expect("fakesink");
    let count = Arc::new(AtomicUsize::new(0));
    let count_for_handoff = count.clone();
    sink.connect("handoff", false, move |_| {
        count_for_handoff.fetch_add(1, Ordering::Relaxed);
        None
    });
    (sink, count)
}

struct Receiver {
    pipeline: gst::Pipeline,
    by_id: HashMap<String, gst::Element>,
    /// Buffers out of the independent live branch, standing in for the rest of
    /// the flow (a mixer fed by other contributors).
    program_buffers: Arc<AtomicUsize>,
    /// Decoded audio buffers out of the SRT input block.
    decoded_buffers: Arc<AtomicUsize>,
}

/// The SRT input block, built by the real builder and wired as the pipeline
/// manager wires it, next to an independent live branch with an ordinary
/// (async) sink.
fn build_receiver(port: u16) -> Receiver {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://:{}?mode=listener", port)),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(0));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("decode".to_string(), PropertyValue::Bool(true));

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

    // Not `async`: the SRT branch sink is a counter, not what is under test.
    let (decoded_sink, decoded_buffers) = counting_sink("decoded_sink", false);
    pipeline.add(&decoded_sink).expect("add decoded sink");
    by_id[&format!("{}:audio_output_0", INSTANCE_ID)]
        .link(&decoded_sink)
        .expect("link audio output -> decoded sink");

    let program = gst::parse::bin_from_description(
        "audiotestsrc is-live=true ! audiomixer name=program_mixer ! identity name=program_out",
        true,
    )
    .expect("program branch");
    // An ordinary async sink: it only prerolls once the pipeline reaches PLAYING.
    let (program_sink, program_buffers) = counting_sink("program_sink", true);
    pipeline
        .add_many([program.upcast_ref::<gst::Element>(), &program_sink])
        .expect("add program branch");
    program.link(&program_sink).expect("link program sink");

    Receiver {
        pipeline,
        by_id,
        program_buffers,
        decoded_buffers,
    }
}

fn stop_receiver(receiver: &Receiver) {
    let srtsrc = &receiver.by_id[&format!("{}:srtsrc", INSTANCE_ID)];
    let _ = srtsrc.set_state(gst::State::Null);
    receiver
        .pipeline
        .set_state(gst::State::Null)
        .expect("receiver to NULL");
}

fn wait_for(count: &AtomicUsize, secs: u64) -> usize {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline && count.load(Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    count.load(Ordering::Relaxed)
}

fn decodebin_state(receiver: &Receiver) -> String {
    let (_, current, pending) =
        receiver.by_id[&format!("{}:decodebin", INSTANCE_ID)].state(gst::ClockTime::ZERO);
    format!("current {:?}, pending {:?}", current, pending)
}

/// No caller ever connects: the pipeline must still reach PLAYING and the
/// other branch must run.
#[test]
fn absent_srt_caller_does_not_block_playing() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let receiver = build_receiver(srt_port());
    receiver
        .pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");

    let (result, current, pending) = receiver.pipeline.state(gst::ClockTime::from_seconds(5));
    let program_buffers = wait_for(&receiver.program_buffers, 2);
    let decodebin = decodebin_state(&receiver);
    stop_receiver(&receiver);

    assert_eq!(
        (result, current),
        (Ok(gst::StateChangeSuccess::Success), gst::State::Playing),
        "an SRT listener with no caller held the pipeline out of PLAYING \
         (pending {:?}); decodebin: {}",
        pending,
        decodebin
    );
    assert!(
        program_buffers > 0,
        "the independent live branch produced nothing while the SRT caller was absent"
    );
}

/// A caller connecting after the pipeline is PLAYING must be decoded, and must
/// not knock the pipeline out of PLAYING while its decodebin prerolls.
///
/// The decodebin changes state inside a running pipeline when data arrives.
/// Without `async-handling` its ASYNC_START makes the pipeline lose state:
/// every sink in the flow drops to PAUSED and back, a program-wide glitch on
/// each late connect that a final state check would not see.
#[test]
fn late_srt_caller_is_decoded() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let port = srt_port();
    let receiver = build_receiver(port);
    receiver
        .pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (result, current, _) = receiver.pipeline.state(gst::ClockTime::from_seconds(5));
    assert_eq!(
        (result, current),
        (Ok(gst::StateChangeSuccess::Success), gst::State::Playing),
        "pipeline did not reach PLAYING before the caller connected; decodebin: {}",
        decodebin_state(&receiver)
    );
    std::thread::sleep(Duration::from_millis(500));

    let caller = gst::parse::launch(&format!(
        "audiotestsrc is-live=true ! audioconvert ! audioresample ! avenc_aac ! aacparse \
         ! mpegtsmux ! srtsink name=caller uri=srt://127.0.0.1:{}?mode=caller latency=20 \
         sync=false",
        port
    ))
    .expect("caller pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();

    let bus = receiver.pipeline.bus().expect("pipeline bus");
    while bus.pop().is_some() {}
    caller.set_state(gst::State::Playing).expect("caller plays");

    let decoded = wait_for(&receiver.decoded_buffers, 15);
    // Let the decodebin finish prerolling before reading the bus.
    std::thread::sleep(Duration::from_millis(500));
    let pipeline_transitions: Vec<String> = std::iter::from_fn(|| bus.pop())
        .filter(|msg| msg.src() == Some(receiver.pipeline.upcast_ref::<gst::Object>()))
        .filter_map(|msg| match msg.view() {
            gst::MessageView::StateChanged(sc) => {
                Some(format!("{:?} -> {:?}", sc.old(), sc.current()))
            }
            _ => None,
        })
        .collect();
    let decodebin = decodebin_state(&receiver);

    if let Some(srtsink) = caller.by_name("caller") {
        let _ = srtsink.set_state(gst::State::Null);
    }
    caller.set_state(gst::State::Null).expect("caller to NULL");
    stop_receiver(&receiver);

    assert!(
        decoded > 0,
        "a caller connecting after PLAYING was never decoded; decodebin: {}",
        decodebin
    );
    assert!(
        pipeline_transitions.is_empty(),
        "the pipeline changed state when the late caller connected: {:?}",
        pipeline_transitions
    );
}
