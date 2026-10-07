//! Regression test for media a WHIP Input slot's previous session left queued.
//!
//! A slot's `appsrc` queues a publisher's frames, up to `APPSRC_MAX_TIME`,
//! when the chain below it falls behind. If the publisher leaves with frames
//! still queued and the next session claims the slot, those frames used to
//! drain into the new session: through the decoder restarted for it, as its
//! first output, with the previous session's timestamps.
//!
//! The guards: frames the previous session left queued never come out of the
//! slot once a new session has claimed it, and the work that keeps them out
//! (the release's flush, the claim's decoder restart) never stops the slot's
//! input.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// A running single-slot WHIP Input, built through the real block builder,
/// with a consumer on its video output that can be made slow.
struct Slot {
    pipeline: gst::Pipeline,
    config: WhipEndpointConfig,
    /// Frames out of the slot's video tee.
    frames: Arc<AtomicUsize>,
    /// While set, the consumer holds each frame for 100 ms: 10 fps against a
    /// 30 fps publisher, so the slot's appsrc fills up.
    slow: Arc<AtomicBool>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn start_slot(instance_id: &str) -> Slot {
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
    let slow = Arc::new(AtomicBool::new(false));
    let frames_for_handoff = frames.clone();
    let slow_for_handoff = slow.clone();
    sink.connect("handoff", false, move |_| {
        frames_for_handoff.fetch_add(1, Ordering::Relaxed);
        if slow_for_handoff.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    });
    pipeline.add(&sink).expect("add consumer");
    tee.link(&sink).expect("link tee -> consumer");

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
        config,
        frames,
        slow,
    }
}

/// One H.264 publisher, pushed into the slot appsrc the way a session's
/// appsink bridge does.
fn start_publisher(slot_appsrc: gst_app::AppSrc) -> gst::Element {
    let feeder = gst::parse::launch(
        "videotestsrc is-live=true ! video/x-raw,width=320,height=240,framerate=30/1 \
         ! x264enc tune=zerolatency key-int-max=15 ! h264parse \
         ! appsink name=out emit-signals=true sync=false",
    )
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

fn wait_for_frames(frames: &AtomicUsize, count: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && frames.load(Ordering::Relaxed) < count {
        std::thread::sleep(Duration::from_millis(50));
    }
    frames.load(Ordering::Relaxed)
}

#[test]
fn a_new_session_gets_none_of_the_frames_its_predecessor_left_queued() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    // VideoToolbox outputs decoded frames from a thread of its own, so the few
    // it already holds at the release keep draining until the claim restarts
    // the decoder. That is decoder latency, not the appsrc backlog this test
    // guards: decode as CI's software decoder does, so the output count is
    // the same on every platform.
    for name in ["vtdec_hw", "vtdec"] {
        if let Some(feature) = gst::Registry::get().lookup_feature(name) {
            feature.set_rank(gst::Rank::NONE);
        }
    }
    // Count GStreamer criticals from here on: dropping the flush events at the
    // appsrc's pad with `Drop` logged one per event on GStreamer before 1.26.
    let criticals = Arc::new(AtomicUsize::new(0));
    let handler = {
        let criticals = Arc::clone(&criticals);
        gst::glib::log_set_handler(
            Some("GStreamer"),
            gst::glib::LogLevels::LEVEL_CRITICAL,
            false,
            false,
            move |_domain, _level, message| {
                eprintln!("GStreamer critical: {message}");
                criticals.fetch_add(1, Ordering::Relaxed);
            },
        )
    };
    let slot = start_slot("whip_flush");
    let config = &slot.config;

    // The first session decodes, then its consumer falls behind and the slot's
    // appsrc fills with two seconds of its frames.
    let index = config.allocate_slot("first").expect("a free slot");
    let appsrc = config.slot_video_appsrcs[index].clone();
    let publisher = start_publisher(appsrc.clone());
    // Buffers out of the slot's appsrc, whatever the decoder makes of them.
    let pushed = Arc::new(AtomicUsize::new(0));
    let pushed_in_probe = pushed.clone();
    appsrc
        .static_pad("src")
        .expect("appsrc has a src pad")
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            pushed_in_probe.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    assert!(
        wait_for_frames(&slot.frames, 10) >= 10,
        "the first session never decoded"
    );
    slot.slow.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(2));
    let queued = appsrc.current_level_buffers();
    publisher
        .set_state(gst::State::Null)
        .expect("publisher stops");
    assert!(config.release_slot(index, "first"));
    // Releasing has to wait out the frame the consumer is holding.
    std::thread::sleep(Duration::from_millis(500));

    // A new session claims the slot. It has not sent anything yet, so every
    // frame out of the slot from here on is one the first session left.
    // `create_whipserversrc_for_session` re-arms the "video is decoding" flag.
    config.video_decoding[index].store(false, Ordering::Relaxed);
    slot.slow.store(false, Ordering::Relaxed);
    let before = slot.frames.load(Ordering::Relaxed);
    let pushed_before = pushed.load(Ordering::Relaxed);
    let index = config.allocate_slot("second").expect("a free slot");
    std::thread::sleep(Duration::from_secs(3));
    let leaked = slot.frames.load(Ordering::Relaxed) - before;
    let drained = pushed.load(Ordering::Relaxed) - pushed_before;
    let flagged = config.video_decoding[index].load(Ordering::Relaxed);

    assert!(
        queued >= 10,
        "the setup did not back the slot up: only {} frame(s) queued",
        queued
    );
    // The backlog itself: a fresh decoder may drop what it cannot decode, so
    // the output count alone does not show what was fed into it.
    assert!(
        drained <= 1,
        "{} of the previous session's frames (of {} queued) left the slot's appsrc after the next session claimed it",
        drained,
        queued
    );
    assert!(
        leaked <= 2,
        "{} of the previous session's frames (of {} queued) came out as the new session's output",
        leaked,
        queued
    );
    gst::glib::log_remove_handler(Some("GStreamer"), handler);
    assert_eq!(
        criticals.load(Ordering::Relaxed),
        0,
        "GStreamer logged criticals while the slot was flushed"
    );
    // Set, it stops the new session's keyframe requester before its own
    // publisher has sent anything.
    assert!(
        !flagged,
        "the previous session's frames marked the new session's video as decoding"
    );
}

/// The bug this guards: the claim's decoder restart unlinked the slot's appsrc
/// from its decoder while the appsrc kept pushing, so a push landed on the
/// unlinked pad, failed `not-linked`, and stopped the slot's input for good.
#[test]
fn a_claim_while_the_input_is_pushing_keeps_the_slot_running() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }
    let slot = start_slot("whip_busy_claim");
    let config = &slot.config;

    let index = config.allocate_slot("first").expect("a free slot");
    let appsrc = config.slot_video_appsrcs[index].clone();
    // Stands for the media of both sessions: it keeps pushing across the
    // release and the claim, so the appsrc has a push in flight at the claim.
    let publisher = start_publisher(appsrc.clone());
    assert!(
        wait_for_frames(&slot.frames, 10) >= 10,
        "the first session never decoded"
    );
    slot.slow.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(1));
    assert!(config.release_slot(index, "first"));
    std::thread::sleep(Duration::from_millis(300));

    let index = config.allocate_slot("second").expect("a free slot");
    std::thread::sleep(Duration::from_millis(300));
    slot.slow.store(false, Ordering::Relaxed);
    let before = slot.frames.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(3));
    let after = slot.frames.load(Ordering::Relaxed);
    let _ = publisher.set_state(gst::State::Null);

    assert_eq!(index, 0);
    // 30 fps for 3 s, less the wait for a keyframe (one every 0.5 s).
    assert!(
        after - before >= 30,
        "the slot's input stopped after the claim: {} frame(s) in 3 s",
        after - before
    );
}
