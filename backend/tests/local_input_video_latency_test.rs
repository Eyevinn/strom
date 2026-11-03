//! Local Input reports the latency its video can wait, not only its capture
//! source's (#1183).
//!
//! `avfvideosrc` (a Mac camera) reports one frame as both its minimum and its
//! maximum latency: 33.3 ms at 30 fps. A sink that syncs adds its processing
//! deadline (20 ms by default) to the minimum, finds it above the maximum,
//! posts "Pipeline construction is invalid, please add queues" and shortens
//! the deadline to 0. With no room left, every millisecond the encoder takes
//! makes a frame late: a Mac camera into Video Encoder and MPEG-TS/SRT Output
//! lost about 10 frames a second to QoS.
//!
//! The fix is in the source block: Local Input holds its video in a queue and
//! reports that. CI has no camera, so these build the real block through
//! `LocalInputBuilder`, leave its source out, and feed the capture front from
//! a stand-in that answers latency queries the way `avfvideosrc` does.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::devicesrc::LocalInputBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const REQUIRED: &[&str] = &[
    "videotestsrc",
    "capsfilter",
    "queue",
    "fakesink",
    "autovideosrc",
];

const CAPS: &str = "video/x-raw,format=I420,width=320,height=240,framerate=30/1";

/// A live video source that reports the latency `avfvideosrc` reports: one
/// frame as both minimum and maximum.
fn camera_like_source() -> gst::Element {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let frame = gst::ClockTime::from_nseconds(1_000_000_000 / 30);
    src.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::QUERY_UPSTREAM,
        move |_pad, info| {
            let Some(gst::PadProbeData::Query(query)) = info.data.as_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::QueryViewMut::Latency(q) = query.view_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            q.set(true, frame, frame);
            gst::PadProbeReturn::Handled
        },
    );
    src
}

fn caps_filter() -> gst::Element {
    gst::ElementFactory::make("capsfilter")
        .property("caps", CAPS.parse::<gst::Caps>().unwrap())
        .build()
        .expect("capsfilter")
}

/// A sink that syncs, with the default 20 ms processing deadline, as
/// `srtsink` has.
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
    let src = camera_like_source();
    let filter = caps_filter();
    let sink = syncing_sink();
    pipeline.add_many([&src, &filter, &sink]).unwrap();
    gst::Element::link_many([&src, &filter, &sink]).unwrap();
    let warnings = warnings(&pipeline);
    assert!(
        add_queues_warnings(&warnings) > 0,
        "a camera-like source straight into a syncing sink should warn: {warnings:?}"
    );
}

#[test]
fn a_camera_like_source_through_local_input_does_not_trip_a_syncing_sink() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let id = "li_cam";
    let props = HashMap::from([
        (
            "stream_mode".to_string(),
            PropertyValue::String("video".into()),
        ),
        (
            "video_resolution".to_string(),
            PropertyValue::String("320x240".into()),
        ),
        (
            "video_framerate".to_string(),
            PropertyValue::String("30/1".into()),
        ),
    ]);
    let built = LocalInputBuilder
        .build(id, &props, &common::block::context())
        .expect("Local Input builds");
    let pipeline = gst::Pipeline::new();
    let source_id = format!("{id}:videosrc");
    let by_id = common::block::install_except(&pipeline, &built, &[source_id.as_str()]);

    let pads = LocalInputBuilder
        .get_external_pads(&props)
        .expect("Local Input pads");
    let video_out = pads
        .outputs
        .iter()
        .find(|p| p.name == "video_out")
        .expect("a video output");
    let block_out = by_id[&format!("{id}:{}", video_out.internal_element_id)]
        .static_pad(&video_out.internal_pad_name)
        .expect("block output pad");
    let capture_front = by_id[&format!("{id}:videosrc_caps")]
        .static_pad("sink")
        .expect("capture front sink pad");

    let src = camera_like_source();
    let filter = caps_filter();
    let sink = syncing_sink();
    pipeline.add_many([&src, &filter, &sink]).unwrap();
    src.link(&filter).unwrap();
    filter
        .static_pad("src")
        .unwrap()
        .link(&capture_front)
        .unwrap();
    block_out
        .link(&sink.static_pad("sink").unwrap())
        .expect("the block's video output links to the sink");

    let warnings = warnings(&pipeline);
    assert_eq!(
        add_queues_warnings(&warnings),
        0,
        "Local Input must report the latency it can hold: {warnings:?}"
    );
}
