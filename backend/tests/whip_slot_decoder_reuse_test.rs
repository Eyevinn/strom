//! Regression test for a WHIP Input slot reused by a second publisher.
//!
//! A slot's video chain (`appsrc_video_<slot>` → decodebin → videoconvert →
//! tee) stays in the running pipeline while sessions come and go. The decoder
//! `decodebin` plugged for the first session kept that session's reference
//! frames, reorder queue and configuration, and on macOS VideoToolbox then
//! decoded a later Safari publisher slowly or not at all.
//!
//! The guard: a session that reuses a slot decodes through a different video
//! decoder instance than the previous session did, and still produces frames.
//! A fresh decoder also lets a publisher that negotiated another codec reuse
//! the slot: the kept one refused it with `not-negotiated`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom::blocks::builtin::whip::WHIPInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::whip_session_manager::WhipEndpointConfig;
use strom_types::element::ElementPadRef;
use strom_types::PropertyValue;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "appsrc",
    "decodebin",
    "videoconvert",
    "audioconvert",
    "audioresample",
    "tee",
    "videotestsrc",
    "x264enc",
    "h264parse",
    "vp8enc",
    "vp8dec",
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

/// Build one single-slot WHIP Input through the real block builder and wire up
/// its declared internal links, as the pipeline manager does.
fn build_whip_input(
    instance_id: &str,
) -> (
    gst::Pipeline,
    HashMap<String, gst::Element>,
    WhipEndpointConfig,
) {
    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String(instance_id.to_string()),
    );
    props.insert(
        "mode".to_string(),
        PropertyValue::String("audio_video".to_string()),
    );
    props.insert("max_sessions".to_string(), PropertyValue::Int(1));
    props.insert("decode".to_string(), PropertyValue::Bool(true));

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = WHIPInputBuilder
        .build(instance_id, &props, &ctx)
        .expect("WHIP Input block builds");
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = resolve_pad(&by_id, from, true);
        let sink = resolve_pad(&by_id, to, true);
        src.link(&sink)
            .unwrap_or_else(|e| panic!("link {:?} -> {:?}: {:?}", from, to, e));
    }

    let (_, config) = ctx
        .take_whip_endpoint_configs()
        .into_iter()
        .next()
        .expect("the WHIP input registered an endpoint config");
    (pipeline, by_id, config)
}

const H264: &str = "x264enc tune=zerolatency key-int-max=15 ! h264parse";
const VP8: &str = "vp8enc deadline=1 keyframe-max-dist=15";

/// One publisher: video encoded with `encoder`, pushed into the slot appsrc
/// the way a session's appsink bridge does.
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

/// Wait until the slot has put `count` frames out past `from`.
fn wait_for_frames(frames: &AtomicUsize, from: usize, count: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && frames.load(Ordering::Relaxed) < from + count {
        std::thread::sleep(Duration::from_millis(50));
    }
    frames.load(Ordering::Relaxed) - from
}

/// The video decoder `decodebin` has plugged, if any.
fn video_decoder(decodebin: &gst::Element) -> Option<gst::Element> {
    decodebin
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .find(|element| {
            element.factory().is_some_and(|factory| {
                let klass = factory.metadata(gst::ELEMENT_METADATA_KLASS).unwrap_or("");
                klass.contains("Decoder") && klass.contains("Video")
            })
        })
}

/// A running WHIP Input with one slot, and a count of the frames leaving it.
struct Slot {
    pipeline: gst::Pipeline,
    decodebin: gst::Element,
    config: WhipEndpointConfig,
    frames: Arc<AtomicUsize>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn start_slot(instance_id: &str) -> Slot {
    let (pipeline, by_id, config) = build_whip_input(instance_id);
    let decodebin = by_id
        .get(&format!("{}:decodebin_video_0", instance_id))
        .expect("slot 0 has a video decodebin")
        .clone();

    // Count frames where they leave the slot.
    let tee = by_id
        .get(&format!("{}:video_out_tee_0", instance_id))
        .expect("slot 0 has a video output tee");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        // Not `async`: an unprerolled sink would hold the pipeline ASYNC.
        .property("async", false)
        .property("signal-handoffs", true)
        .build()
        .expect("fakesink");
    let frames = Arc::new(AtomicUsize::new(0));
    let frames_for_handoff = frames.clone();
    sink.connect("handoff", false, move |_| {
        frames_for_handoff.fetch_add(1, Ordering::Relaxed);
        None
    });
    pipeline.add(&sink).expect("add probe sink");
    tee.link(&sink).expect("link tee -> probe sink");

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (result, current, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    assert_eq!(
        (result.expect("pipeline state readable"), current),
        (gst::StateChangeSuccess::Success, gst::State::Playing)
    );

    Slot {
        pipeline,
        decodebin,
        config,
        frames,
    }
}

#[test]
fn reused_slot_decodes_through_a_fresh_video_decoder() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    let slot = start_slot("whip_reuse");
    let (decodebin, config, frames) = (&slot.decodebin, &slot.config, &slot.frames);

    // First session.
    let slot = config.allocate_slot("first").expect("a free slot");
    let publisher = start_publisher(config.slot_video_appsrcs[slot].clone(), H264);
    let first_frames = wait_for_frames(frames, 0, 10);
    let first_decoder = video_decoder(decodebin);
    publisher
        .set_state(gst::State::Null)
        .expect("publisher stops");
    config.release_slot(slot);

    // Second session on the same slot.
    let before = frames.load(Ordering::Relaxed);
    let slot_again = config.allocate_slot("second").expect("a free slot");
    let publisher = start_publisher(config.slot_video_appsrcs[slot_again].clone(), H264);
    let second_frames = wait_for_frames(frames, before, 10);
    let second_decoder = video_decoder(decodebin);
    publisher
        .set_state(gst::State::Null)
        .expect("publisher stops");

    assert!(
        first_frames >= 10,
        "the first session decoded {} frames",
        first_frames
    );
    assert_eq!(
        slot_again, slot,
        "the second session did not reuse the slot"
    );
    let first_decoder = first_decoder.expect("the first session plugged a video decoder");
    let second_decoder = second_decoder.expect("the second session has a video decoder");
    assert_ne!(
        first_decoder,
        second_decoder,
        "the second session decoded through the first session's {} instead of a fresh decoder",
        first_decoder.name()
    );
    assert!(
        second_frames >= 10,
        "the second session on a reused slot decoded {} frames",
        second_frames
    );
}

/// A publisher whose browser negotiated VP8 reuses a slot an H.264 publisher
/// left. WHIP publishers choose their codec.
#[test]
fn reused_slot_accepts_a_different_codec() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    let slot = start_slot("whip_codec");
    let bus = slot.pipeline.bus().expect("pipeline bus");

    let index = slot.config.allocate_slot("h264").expect("a free slot");
    let publisher = start_publisher(slot.config.slot_video_appsrcs[index].clone(), H264);
    let h264_frames = wait_for_frames(&slot.frames, 0, 10);
    publisher
        .set_state(gst::State::Null)
        .expect("publisher stops");
    slot.config.release_slot(index);

    let before = slot.frames.load(Ordering::Relaxed);
    let index = slot.config.allocate_slot("vp8").expect("a free slot");
    let publisher = start_publisher(slot.config.slot_video_appsrcs[index].clone(), VP8);
    let vp8_frames = wait_for_frames(&slot.frames, before, 10);
    publisher
        .set_state(gst::State::Null)
        .expect("publisher stops");

    let errors: Vec<String> = bus
        .iter_filtered(&[gst::MessageType::Error])
        .map(|message| format!("{:?}", message))
        .collect();
    assert!(
        h264_frames >= 10,
        "the H.264 session decoded {} frames",
        h264_frames
    );
    assert!(
        vp8_frames >= 10,
        "the VP8 session on a reused slot decoded {} frames; errors: {:?}",
        vp8_frames,
        errors
    );
}
