//! `builtin.whip_output` (whipclientsink) must not add BaseSink's processing
//! deadline to the flow's latency.
//!
//! whipclientsink's input appsinks only hand buffers to the session pipeline,
//! yet their default 20 ms deadline is added to the pipeline latency and
//! waited out twice before a packet is sent: by the appsink, and by
//! webrtcbin's clocksync in the session. A flow adopts the largest latency any
//! sink reports, so the deadline also delays every other sink in the flow.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::{builtin, BlockBuildContext};
use strom_types::PropertyValue;

const DEFAULT_PROCESSING_DEADLINE: gst::ClockTime = gst::ClockTime::from_mseconds(20);

fn init_gst() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("gstreamer init");
        gstrswebrtc::plugin_register_static().expect("register webrtc plugins");
    });
}

/// Build the real block and wire its internal links into a pipeline. Linking
/// requests whipclientsink's input pad, which is when webrtcsink creates the
/// input appsink, so no endpoint has to be reachable.
fn build_linked_block() -> (gst::Pipeline, HashMap<String, gst::Element>) {
    init_gst();

    let properties: HashMap<String, PropertyValue> = [
        (
            "implementation".to_string(),
            PropertyValue::String("whipclientsink".to_string()),
        ),
        (
            "whip_endpoint".to_string(),
            PropertyValue::String("http://192.0.2.1/whip".to_string()),
        ),
    ]
    .into();
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let result = builtin::get_builder("builtin.whip_output")
        .expect("whip_output builder")
        .build("whip", &properties, &ctx)
        .expect("whip_output build");

    let pipeline = gst::Pipeline::new();
    let mut elements: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &result.elements {
        pipeline.add(element).expect("add block element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &result.internal_links {
        let src = &elements[&from.element_id];
        let dst = &elements[&to.element_id];
        let src_pad = src
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("source pad");
        let dst_name = to.pad_name.as_deref().unwrap_or("sink");
        let dst_pad = dst
            .static_pad(dst_name)
            .or_else(|| dst.request_pad_simple(dst_name))
            .unwrap_or_else(|| panic!("no sink pad {dst_name} on {}", dst.name()));
        src_pad.link(&dst_pad).expect("internal link");
    }

    (pipeline, elements)
}

#[test]
fn every_whipclientsink_input_appsink_has_no_processing_deadline() {
    let (pipeline, elements) = build_linked_block();

    let bin = elements["whip:whipclientsink"]
        .clone()
        .downcast::<gst::Bin>()
        .expect("whipclientsink is a bin");
    let appsinks: Vec<(String, u64)> = bin
        .iterate_elements()
        .into_iter()
        .flatten()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "appsink"))
        .map(|e| {
            (
                e.name().to_string(),
                e.property::<u64>("processing-deadline"),
            )
        })
        .collect();
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        !appsinks.is_empty(),
        "linking the block created no input appsink inside whipclientsink"
    );
    for (name, deadline) in &appsinks {
        assert_eq!(
            *deadline, 0,
            "appsink {name} kept processing-deadline {deadline}; all of {appsinks:?}"
        );
    }
}

#[test]
fn whip_output_latency_excludes_appsink_processing_deadline() {
    let (pipeline, elements) = build_linked_block();

    // audiotestsrc reports one buffer of latency; the block's share is the
    // pipeline latency minus that.
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("audiotestsrc");
    pipeline.add(&src).expect("add source");
    src.link(&elements["whip:audioconvert"])
        .expect("link source to the block's audio input");

    pipeline.set_state(gst::State::Playing).expect("PLAYING");
    let (res, state, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    assert!(
        res.is_ok() && state == gst::State::Playing,
        "pipeline did not reach PLAYING: {res:?} {state:?}"
    );

    // The appsink answers the latency query only once data has reached it,
    // so poll until the query reports a live result.
    let deadline = Instant::now() + Duration::from_secs(10);
    let min = loop {
        let mut query = gst::query::Latency::new();
        if pipeline.query(&mut query) {
            let (live, min, _) = query.result();
            if live {
                break min;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pipeline never answered the latency query as live"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut src_query = gst::query::Latency::new();
    assert!(src.query(&mut src_query), "source latency query failed");
    let (_, src_min, _) = src_query.result();
    let _ = pipeline.set_state(gst::State::Null);

    let added = min.saturating_sub(src_min);
    assert!(
        added < DEFAULT_PROCESSING_DEADLINE,
        "whip_output adds {added} to its source's {src_min} of latency; \
         the appsink's processing deadline is still counted"
    );
}
