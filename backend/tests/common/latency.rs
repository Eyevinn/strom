//! A source that reports a chosen latency, and the sink check that catches a
//! source reporting less than a syncing sink needs.

use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

/// Make `src` answer latency queries with `min` and `max`, the way a capture
/// source with a small buffer does.
pub fn report_latency(src: &gst::Element, min: gst::ClockTime, max: gst::ClockTime) {
    src.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::QUERY_UPSTREAM,
        move |_pad, info| {
            let Some(gst::PadProbeData::Query(query)) = info.data.as_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            let gst::QueryViewMut::Latency(q) = query.view_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            q.set(true, min, max);
            gst::PadProbeReturn::Handled
        },
    );
}

/// A sink that syncs, with the default 20 ms processing deadline, as
/// `ndisink` and `srtsink` have.
pub fn syncing_sink() -> gst::Element {
    gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("fakesink")
}

/// Bring `pipeline` to PLAYING, play it for a second, and return the warnings
/// it posted. The second starts once the pipeline is PLAYING, so a slow host
/// cannot end the window before the sink has configured its latency.
pub fn warnings(pipeline: &gst::Pipeline) -> Vec<String> {
    let bus = pipeline.bus().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let (reached, state, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    let mut warnings = Vec::new();
    let mut error = None;
    if reached.is_err() || state != gst::State::Playing {
        error = Some(format!("the pipeline did not reach PLAYING: {state:?}"));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while error.is_none() && Instant::now() < deadline {
        let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Warning, gst::MessageType::Error],
        ) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Warning(w) => warnings.push(w.error().to_string()),
            gst::MessageView::Error(e) => error = Some(e.error().to_string()),
            _ => {}
        }
    }
    // Warnings posted while reaching PLAYING are still on the bus.
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Warning]) {
        if let gst::MessageView::Warning(w) = msg.view() {
            warnings.push(w.error().to_string());
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
    if let Some(error) = error {
        panic!("the pipeline failed: {error}");
    }
    warnings
}

/// How many of `warnings` are a syncing sink's "please add queues": the
/// latency upstream reports leaves it no room for its processing deadline.
pub fn add_queues_warnings(warnings: &[String]) -> usize {
    warnings
        .iter()
        .filter(|w| w.contains("please add queues"))
        .count()
}
