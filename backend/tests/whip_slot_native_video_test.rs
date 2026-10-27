//! A WHIP Input slot emits the video its decoder produces (#969, item 2).
//!
//! The slot used to put a `videoconvert` between `decodebin` and the output
//! tee, with no capsfilter after it. It negotiated passthrough, so it did no
//! useful work, and when a consumer asked for another pixel format it
//! converted silently on the CPU, frame by frame, inside the producer. A block
//! emits what it naturally produces, and each consumer adapts its own input
//! (BLOCK_GUIDELINES), so the decoder's pad now links straight to the tee.
//!
//! `the_decoder_feeds_the_output_tee_directly` guards that: it fails when
//! anything sits between the decoder and the tee. It is a structural check:
//! a passthrough `videoconvert` delivers the same frames, so no frame-level
//! assertion tells the two apart.
//!
//! `a_video_encoder_takes_what_the_slot_emits` runs the slot into the real
//! Video Encoder block, which adapts its own input. It shows the consumer
//! side works with the decoder's output; it passes with or without the
//! change.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::builtin::whip::WHIPInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::whip_session_manager::WhipEndpointConfig;
use strom_types::element::ElementPadRef;
use strom_types::PropertyValue;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "appsrc",
    "decodebin",
    "audioconvert",
    "audioresample",
    "tee",
    "identity",
    "videoconvert",
    "videotestsrc",
    "vp8enc",
    "vp8dec",
    "x264enc",
    "h264parse",
    "appsink",
    "fakesink",
    // `WHIPInputBuilder::build` refuses to build without ICE.
    "nicesrc",
    "nicesink",
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

fn resolve_pad(
    by_id: &HashMap<String, gst::Element>,
    reference: &ElementPadRef,
    request: bool,
) -> gst::Pad {
    let element = by_id
        .get(&reference.element_id)
        .unwrap_or_else(|| panic!("unknown element {}", reference.element_id));
    let pad_name = reference.pad_name.as_deref().unwrap_or("src");
    element
        .static_pad(pad_name)
        .or_else(|| {
            if request {
                element.request_pad_simple(pad_name)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("{} has no pad {}", reference.element_id, pad_name))
}

/// Add every element of `built` to `pipeline` and make its internal links.
fn install(
    pipeline: &gst::Pipeline,
    built: &strom::blocks::BlockBuildResult,
    by_id: &mut HashMap<String, gst::Element>,
) {
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = resolve_pad(by_id, from, true);
        let sink = resolve_pad(by_id, to, true);
        src.link(&sink)
            .unwrap_or_else(|e| panic!("link {:?} -> {:?}: {:?}", from, to, e));
    }
}

/// One single-slot, video-only WHIP Input with decoding on, built through the
/// real block builder, its output tee linked to `consumer_sink`.
fn start_slot(
    instance_id: &str,
    pipeline: &gst::Pipeline,
    by_id: &mut HashMap<String, gst::Element>,
    consumer_sink: &gst::Pad,
) -> WhipEndpointConfig {
    let props = HashMap::from([
        (
            "endpoint_id".to_string(),
            PropertyValue::String(instance_id.to_string()),
        ),
        (
            "mode".to_string(),
            PropertyValue::String("video".to_string()),
        ),
        ("max_sessions".to_string(), PropertyValue::Int(1)),
        ("decode".to_string(), PropertyValue::Bool(true)),
    ]);
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = WHIPInputBuilder
        .build(instance_id, &props, &ctx)
        .expect("WHIP Input block builds");
    install(pipeline, &built, by_id);

    let tee = &by_id[&format!("{}:video_out_tee_0", instance_id)];
    tee.request_pad_simple("src_%u")
        .expect("tee src pad")
        .link(consumer_sink)
        .expect("link the slot to its consumer");

    let (_, config) = ctx
        .take_whip_endpoint_configs()
        .into_iter()
        .next()
        .expect("the WHIP input registered an endpoint config");
    config
}

/// One publisher, its video encoded with `encoder`, pushed into the slot
/// appsrc the way a session's appsink bridge does.
fn start_publisher(slot_appsrc: gst_app::AppSrc, encoder: &str) -> gst::Element {
    let feeder = gst::parse::launch(&format!(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! {} ! appsink name=out emit-signals=true sync=false",
        encoder
    ))
    .expect("feeder pipeline");
    let appsink = feeder
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("out")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let _ = slot_appsrc.push_sample(&sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    feeder.set_state(gst::State::Playing).expect("feeder plays");
    feeder
}

/// A fakesink counting the buffers it gets.
fn counting_sink() -> (gst::Element, Arc<AtomicUsize>) {
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        // Not `async`: an unprerolled sink would hold the pipeline ASYNC.
        .property("async", false)
        .property("signal-handoffs", true)
        .build()
        .expect("fakesink");
    let frames = Arc::new(AtomicUsize::new(0));
    let counter = frames.clone();
    sink.connect("handoff", false, move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
        None
    });
    (sink, frames)
}

fn wait_for(frames: &AtomicUsize, count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if frames.load(Ordering::Relaxed) >= count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn play(pipeline: &gst::Pipeline) {
    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (result, current, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    assert_eq!(
        (result.expect("pipeline state readable"), current),
        (gst::StateChangeSuccess::Success, gst::State::Playing)
    );
}

/// The element the decodebin's video pad is linked to, outside the decodebin.
fn decoder_peer(decodebin: &gst::Element) -> Option<gst::Element> {
    decodebin
        .src_pads()
        .into_iter()
        .find_map(|pad| pad.peer())
        .and_then(|peer| peer.parent_element())
}

#[test]
fn the_decoder_feeds_the_output_tee_directly() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    let pipeline = gst::Pipeline::new();
    let mut by_id = HashMap::new();
    let (sink, frames) = counting_sink();
    pipeline.add(&sink).unwrap();
    let config = start_slot(
        "whip_native",
        &pipeline,
        &mut by_id,
        &sink.static_pad("sink").unwrap(),
    );
    play(&pipeline);

    let slot = config.allocate_slot("publisher").expect("a free slot");
    // VP8: `vp8dec` on every platform, so the decoder and its I420 output
    // are the same on every runner.
    let publisher = start_publisher(
        config.slot_video_appsrcs[slot].clone(),
        "vp8enc deadline=1 keyframe-max-dist=15",
    );
    let decoded = wait_for(&frames, 10);
    let decodebin = &by_id["whip_native:decodebin_video_0"];
    let peer = decoder_peer(decodebin);
    let tee = &by_id["whip_native:video_out_tee_0"];
    let tee_caps = tee.static_pad("sink").and_then(|p| p.current_caps());
    publisher.set_state(gst::State::Null).unwrap();
    pipeline.set_state(gst::State::Null).unwrap();

    assert!(decoded, "the slot put out fewer than 10 frames");
    assert_eq!(
        peer.as_ref(),
        Some(tee),
        "the decoder feeds {:?}, not the slot's output tee: the slot must emit what its \
         decoder produces and leave conversion to the consumer",
        peer.as_ref().map(|p| p.name())
    );
    let tee_caps = tee_caps.expect("the tee negotiated");
    assert_eq!(
        tee_caps
            .structure(0)
            .and_then(|s| s.get::<String>("format").ok())
            .as_deref(),
        Some("I420"),
        "the slot emitted {tee_caps}, not vp8dec's I420"
    );
}

#[test]
fn a_video_encoder_takes_what_the_slot_emits() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    let pipeline = gst::Pipeline::new();
    let mut by_id = HashMap::new();

    let props = HashMap::from([
        ("codec".to_string(), PropertyValue::String("h264".into())),
        (
            "encoder_preference".to_string(),
            PropertyValue::String("software".into()),
        ),
    ]);
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let encoder = VideoEncBuilder
        .build("venc", &props, &ctx)
        .expect("videoenc block builds");
    install(&pipeline, &encoder, &mut by_id);
    let (sink, frames) = counting_sink();
    pipeline.add(&sink).unwrap();
    by_id["venc:capsfilter"]
        .link(&sink)
        .expect("link the encoder to the sink");

    let encoder_in = by_id["venc:input"].static_pad("sink").unwrap();
    let config = start_slot("whip_enc", &pipeline, &mut by_id, &encoder_in);
    play(&pipeline);

    let slot = config.allocate_slot("publisher").expect("a free slot");
    // H.264: on macOS `decodebin` plugs VideoToolbox, which can decode to GL
    // memory; elsewhere a software decoder.
    let publisher = start_publisher(
        config.slot_video_appsrcs[slot].clone(),
        "x264enc tune=zerolatency key-int-max=15 ! h264parse",
    );
    let encoded = wait_for(&frames, 10);
    let errors: Vec<String> = {
        let bus = pipeline.bus().unwrap();
        std::iter::from_fn(|| bus.pop_filtered(&[gst::MessageType::Error]))
            .filter_map(|m| match m.view() {
                gst::MessageView::Error(e) => Some(format!("{} ({:?})", e.error(), e.debug())),
                _ => None,
            })
            .collect()
    };
    publisher.set_state(gst::State::Null).unwrap();
    pipeline.set_state(gst::State::Null).unwrap();

    assert!(errors.is_empty(), "pipeline errors: {errors:?}");
    assert!(encoded, "fewer than 10 encoded frames left the encoder");
}
