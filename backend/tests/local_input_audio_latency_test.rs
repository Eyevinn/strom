//! Local Input reports the latency its audio can wait, not only its capture
//! ring buffer's (#1068).
//!
//! `asiosrc` captures into a ring buffer of two ASIO buffers
//! (`gstasioringbuffer.cpp`: `segtotal = 2`) and reports that as its maximum
//! latency: 5.3 ms at 128 samples and 48 kHz, with one buffer (2.7 ms) as the
//! minimum. A sink that syncs to the clock adds its processing deadline (20 ms
//! by default) to the minimum, finds it above the maximum, and posts "Pipeline
//! construction is invalid, please add queues". That is what ASIO into NDI
//! Output logged until the ASIO buffer was raised to 1024 samples.
//!
//! The fix is in the source block: Local Input holds its audio in a queue and
//! reports that. CI has no ASIO device, so these build the real block through
//! `LocalInputBuilder`, leave its source out, and feed the capture front from
//! a stand-in that answers latency queries the way `asiosrc` does.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::devicesrc::LocalInputBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const REQUIRED: &[&str] = &[
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "queue",
    "fakesink",
    "autoaudiosrc",
];

/// ASIO buffer size in samples, at 48 kHz.
const ASIO_BUFFER: u64 = 128;
const RATE: u64 = 48_000;

/// A live audio source that reports the latency `asiosrc` reports with
/// [`ASIO_BUFFER`]-sample buffers: min one buffer, max two.
fn asio_like_source() -> gst::Element {
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property_from_str("wave", "silence")
        .property("samplesperbuffer", ASIO_BUFFER as i32)
        .build()
        .expect("audiotestsrc");
    let min = gst::ClockTime::from_nseconds(ASIO_BUFFER * 1_000_000_000 / RATE);
    src.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::QUERY_UPSTREAM,
        move |_pad, info| {
            let Some(gst::PadProbeData::Query(query)) = info.data.as_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::QueryViewMut::Latency(q) = query.view_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            q.set(true, min, min * 2);
            gst::PadProbeReturn::Handled
        },
    );
    src
}

/// A sink that syncs, with the default 20 ms processing deadline, as
/// `ndisink` has.
fn syncing_sink() -> gst::Element {
    gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("fakesink")
}

/// Play `pipeline` for a second and return the warnings it posted.
fn warnings(pipeline: &gst::Pipeline) -> Vec<String> {
    let bus = pipeline.bus().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut warnings = Vec::new();
    while Instant::now() < deadline {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Warning, gst::MessageType::Error],
        ) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Warning(w) => warnings.push(w.error().to_string()),
            gst::MessageView::Error(e) => panic!("the pipeline failed: {}", e.error()),
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
    warnings
}

fn add_queues_warnings(warnings: &[String]) -> usize {
    warnings
        .iter()
        .filter(|w| w.contains("please add queues"))
        .count()
}

#[test]
fn the_stand_in_alone_trips_a_syncing_sink() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    // The setup shows the failure the block test guards against.
    let pipeline = gst::Pipeline::new();
    let src = asio_like_source();
    let sink = syncing_sink();
    pipeline.add_many([&src, &sink]).unwrap();
    src.link(&sink).unwrap();
    let warnings = warnings(&pipeline);
    assert!(
        add_queues_warnings(&warnings) > 0,
        "an ASIO-like source straight into a syncing sink should warn: {warnings:?}"
    );
}

#[test]
fn an_asio_like_source_through_local_input_does_not_trip_a_syncing_sink() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let id = "li_asio";
    let props = HashMap::from([(
        "stream_mode".to_string(),
        PropertyValue::String("audio".into()),
    )]);
    let built = LocalInputBuilder
        .build(id, &props, &common::block::context())
        .expect("Local Input builds");
    let pipeline = gst::Pipeline::new();
    let source_id = format!("{id}:audiosrc");
    let by_id = common::block::install_except(&pipeline, &built, &[source_id.as_str()]);

    let pads = LocalInputBuilder
        .get_external_pads(&props)
        .expect("Local Input pads");
    let audio_out = pads
        .outputs
        .iter()
        .find(|p| p.name == "audio_out")
        .expect("an audio output");
    let block_out = by_id[&format!("{id}:{}", audio_out.internal_element_id)]
        .static_pad(&audio_out.internal_pad_name)
        .expect("block output pad");
    let capture_front = by_id[&format!("{id}:audiosrc_caps")]
        .static_pad("sink")
        .expect("capture front sink pad");

    let src = asio_like_source();
    let sink = syncing_sink();
    pipeline.add_many([&src, &sink]).unwrap();
    src.static_pad("src").unwrap().link(&capture_front).unwrap();
    block_out
        .link(&sink.static_pad("sink").unwrap())
        .expect("the block's audio output links to the sink");

    let warnings = warnings(&pipeline);
    assert_eq!(
        add_queues_warnings(&warnings),
        0,
        "Local Input must report the latency it can hold: {warnings:?}"
    );
}
