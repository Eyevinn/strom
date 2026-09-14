//! `builtin.whep_output` must not add BaseSink's processing deadline to the
//! flow's latency.
//!
//! whepserversink's input appsinks only hand buffers to the per-viewer session
//! pipelines, yet their default 20 ms deadline is added to the pipeline
//! latency and waited out twice before a packet is sent: by the appsink, and
//! by webrtcbin's clocksync in the session.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::{builtin, BlockBuildContext};
use strom_types::PropertyValue;

const DEFAULT_PROCESSING_DEADLINE: gst::ClockTime = gst::ClockTime::from_mseconds(20);

#[test]
fn whep_output_latency_excludes_appsink_processing_deadline() {
    gst::init().expect("gstreamer init");
    gstrswebrtc::plugin_register_static().expect("register webrtc plugins");

    let properties: HashMap<String, PropertyValue> = [
        (
            "endpoint_id".to_string(),
            PropertyValue::String("latency-test".to_string()),
        ),
        ("num_audio_tracks".to_string(), PropertyValue::UInt(1)),
        ("num_video_tracks".to_string(), PropertyValue::UInt(0)),
    ]
    .into();
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let result = builtin::get_builder("builtin.whep_output")
        .expect("whep_output builder")
        .build("whep", &properties, &ctx)
        .expect("whep_output build");

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
            .expect("sink pad");
        src_pad.link(&dst_pad).expect("internal link");
    }

    // audiotestsrc reports one buffer of latency; the block's share is the
    // pipeline latency minus that.
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("audiotestsrc");
    pipeline.add(&src).expect("add source");
    src.link(&elements["whep:audio_queue"])
        .expect("link source to the block's audio input");

    pipeline.set_state(gst::State::Playing).expect("PLAYING");
    let (res, state, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    assert!(
        res.is_ok() && state == gst::State::Playing,
        "pipeline did not reach PLAYING: {res:?} {state:?}"
    );

    // whepserversink's sinks answer the latency query only once data has
    // reached them, so poll until the query reports a live result.
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
        "whep_output adds {added} to its source's {src_min} of latency; \
         the appsink's processing deadline is still counted"
    );
}
