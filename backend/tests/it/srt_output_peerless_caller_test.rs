//! An SRT output in caller mode without a peer must not stall the flow (#1259).
//!
//! `srtsink` in caller mode with `auto-reconnect` reconnects inside `render`,
//! on the streaming thread, with blocking connect attempts. Strom links one
//! output pad to several blocks through a `tee` with no queues, so a sink that
//! blocks there holds up everything upstream: a live mixer stalls, every other
//! output stops, and when a peer finally connects the backlog bursts out.
//!
//! Each test builds the real output block, feeds it from a live source through
//! a tee the way a flow does, and counts the buffers out of a second tee
//! branch that stands in for the rest of the flow. That branch must keep
//! receiving at the source rate while the SRT output has no peer.
//!
//! - `..._with_no_listener` dials a port where nothing listens.
//! - `..._after_the_listener_goes_away` connects to a listener, stops it (the
//!   field case: a receiver disconnects), checks the flow keeps running, then
//!   starts a listener again and checks the output reconnects and media
//!   arrives. That second half guards against a fix that decouples the sink
//!   by giving up on reconnecting.

use crate::common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::get_builder;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "srtsink",
    "srtsrc",
    "mpegtsmux",
    "avenc_aac",
    "aacparse",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "identity",
    "tee",
    "fakesink",
];

const INSTANCE_ID: &str = "srt_out";

/// The live source's buffer rate: 48 kHz in 480-sample buffers.
const BUFFERS_PER_SECOND: usize = 100;

/// The longest the flow branch may go without a buffer. A connect attempt
/// blocks for about 3 s, so a stall shows up as a gap of seconds; a healthy
/// branch gets a buffer every 10 ms.
const MAX_GAP: Duration = Duration::from_millis(1000);

fn counting_sink(name: &str) -> (gst::Element, Arc<AtomicUsize>) {
    let sink = gst::ElementFactory::make("fakesink")
        .name(name)
        .property("sync", false)
        .property("async", false)
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

/// A live audio source teed, as a flow tees a block output with several
/// links, into (a) the real SRT output block calling `port`, and (b) a
/// counting sink standing in for the rest of the flow.
struct Sender {
    pipeline: gst::Pipeline,
    flow_buffers: Arc<AtomicUsize>,
}

fn build_sender(definition_id: &str, port: u16) -> Sender {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=caller", port)),
    );
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(0));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));

    let ctx = common::block::context();
    let built = get_builder(definition_id)
        .unwrap_or_else(|| panic!("no builder for {definition_id}"))
        .build(INSTANCE_ID, &props, &ctx)
        .expect("SRT output block builds");

    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);
    common::block::run_setups(&ctx);

    let source = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("samplesperbuffer", (48_000 / BUFFERS_PER_SECOND) as i32)
        .build()
        .expect("audiotestsrc");
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", 48_000i32)
                .field("channels", 2i32)
                .build(),
        )
        .build()
        .expect("capsfilter");
    // As the pipeline manager inserts it: no queues on the branches.
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()
        .expect("tee");
    let (flow_sink, flow_buffers) = counting_sink("flow_sink");

    pipeline
        .add_many([&source, &caps, &tee, &flow_sink])
        .expect("add sender elements");
    gst::Element::link_many([&source, &caps, &tee]).expect("link source -> tee");
    tee.link(&flow_sink).expect("link tee -> flow sink");
    tee.link(
        by_id
            .get(&format!("{}:audio_input_0", INSTANCE_ID))
            .expect("block exposes audio_input_0"),
    )
    .expect("link tee -> SRT output");

    Sender {
        pipeline,
        flow_buffers,
    }
}

/// An SRT listener on `port` that counts the buffers it receives.
fn start_listener(port: u16) -> (gst::Pipeline, Arc<AtomicUsize>) {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("srtsrc")
        .property(
            "uri",
            format!("srt://127.0.0.1:{}?mode=listener", port).as_str(),
        )
        .build()
        .expect("srtsrc");
    let (sink, count) = counting_sink("received_sink");
    pipeline.add_many([&src, &sink]).expect("add listener");
    src.link(&sink).expect("link srtsrc -> sink");
    pipeline
        .set_state(gst::State::Playing)
        .expect("listener to PLAYING");
    (pipeline, count)
}

/// Watch `count` for `duration`. Returns how many buffers arrived and the
/// longest stretch in which none did.
fn watch(count: &AtomicUsize, duration: Duration) -> (usize, Duration) {
    let start = Instant::now();
    let first = count.load(Ordering::Relaxed);
    let mut last = first;
    let mut last_change = start;
    let mut longest_gap = Duration::ZERO;
    while start.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(20));
        let now = count.load(Ordering::Relaxed);
        if now != last {
            last = now;
            last_change = Instant::now();
        }
        longest_gap = longest_gap.max(last_change.elapsed());
    }
    (last - first, longest_gap)
}

/// Wait until `count` moves past zero, for at most `timeout`.
fn wait_for_data(count: &AtomicUsize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if count.load(Ordering::Relaxed) > 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn assert_flow_kept_running(what: &str, received: usize, longest_gap: Duration, window: Duration) {
    let expected = BUFFERS_PER_SECOND * window.as_millis() as usize / 1000;
    assert!(
        longest_gap < MAX_GAP && received * 10 >= expected * 8,
        "{what}: the rest of the flow got {received} of ~{expected} buffers in \
         {window:?}, with a gap of {longest_gap:?} without any. The SRT output \
         in caller mode blocked the streaming thread while it had no peer, and \
         the whole flow stalled with it (#1259)."
    );
}

fn stop(pipeline: &gst::Pipeline) {
    let _ = pipeline.set_state(gst::State::Null);
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

/// How long the flow branch is watched while the output has no peer: several
/// ~3 s connect attempts.
const PEERLESS_WINDOW: Duration = Duration::from_secs(10);

fn flow_keeps_running_with_no_listener(definition_id: &str) {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }

    let sender = build_sender(definition_id, common::free_udp_port());
    sender
        .pipeline
        .set_state(gst::State::Playing)
        .expect("sender to PLAYING");

    let (received, longest_gap) = watch(&sender.flow_buffers, PEERLESS_WINDOW);
    let errors = bus_errors(&sender.pipeline);
    stop(&sender.pipeline);

    assert!(errors.is_empty(), "sender errored: {errors:?}");
    assert_flow_kept_running(definition_id, received, longest_gap, PEERLESS_WINDOW);
}

fn flow_keeps_running_after_the_listener_goes_away(definition_id: &str) {
    gst::init().unwrap();
    if !common::plugins_available(REQUIRED) {
        return;
    }

    let port = common::free_udp_port();
    let (listener, received) = start_listener(port);
    let sender = build_sender(definition_id, port);
    sender
        .pipeline
        .set_state(gst::State::Playing)
        .expect("sender to PLAYING");

    assert!(
        wait_for_data(&received, Duration::from_secs(10)),
        "{definition_id}: the listener received nothing; listener errors: {:?}, sender errors: {:?}",
        bus_errors(&listener),
        bus_errors(&sender.pipeline)
    );

    // The receiver goes away, as in the field case.
    stop(&listener);
    drop(listener);

    let (flow_received, longest_gap) = watch(&sender.flow_buffers, PEERLESS_WINDOW);
    assert_flow_kept_running(definition_id, flow_received, longest_gap, PEERLESS_WINDOW);

    // A receiver comes back: the output must reconnect on its own.
    let (listener, received) = start_listener(port);
    let reconnected = wait_for_data(&received, Duration::from_secs(15));
    let errors = bus_errors(&sender.pipeline);
    stop(&listener);
    stop(&sender.pipeline);

    assert!(errors.is_empty(), "sender errored: {errors:?}");
    assert!(
        reconnected,
        "{definition_id}: the output did not reconnect to a listener that came back"
    );
}

#[test]
fn mpegtssrt_output_flow_keeps_running_with_no_listener() {
    flow_keeps_running_with_no_listener("builtin.mpegtssrt_output");
}

#[test]
fn mpegtssrt_output_flow_keeps_running_after_the_listener_goes_away() {
    flow_keeps_running_after_the_listener_goes_away("builtin.mpegtssrt_output");
}

#[cfg(feature = "efp")]
#[test]
fn efpsrt_output_flow_keeps_running_with_no_listener() {
    flow_keeps_running_with_no_listener("builtin.efpsrt_output");
}
