//! A WHIP Input block builds one decode chain per session slot at flow start,
//! whether or not anyone is publishing to that slot. A `decodebin` cannot
//! complete READY->PAUSED until data arrives and it can typefind, and a
//! pipeline with any child still ASYNC never completes its own transition, so
//! a slot with no publisher can hold the whole flow short of PLAYING.
//!
//! The first two tests are a pair: one asserts idle slots do not block PLAYING,
//! the other that a slot claimed by a session still decodes. Either alone could
//! be satisfied by a change that breaks the other.

use std::collections::HashMap;
use strom::blocks::builtin::whip::WHIPInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::element::ElementPadRef;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
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

/// Resolve one side of a block's declared internal link to a pad.
fn resolve_pad(
    by_id: &HashMap<String, gst::Element>,
    reference: &ElementPadRef,
    request: bool,
) -> gst::Pad {
    let element = by_id.get(&reference.element_id).unwrap_or_else(|| {
        panic!(
            "block declares a link to unknown element {}",
            reference.element_id
        )
    });
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

/// Build `inputs` WHIP Input blocks through the real block builder and wire up
/// the internal links they declare, exactly as the pipeline manager does.
///
/// Returns the pipeline, the elements by ID, and the build contexts (which own
/// the endpoint configs the session manager later takes).
fn build_whip_inputs(
    inputs: usize,
) -> (
    gst::Pipeline,
    HashMap<String, gst::Element>,
    Vec<BlockBuildContext>,
) {
    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    let mut contexts = Vec::new();

    for input in 0..inputs {
        let instance_id = format!("whip_p{}", input + 1);

        let mut props: HashMap<String, PropertyValue> = HashMap::new();
        props.insert(
            "endpoint_id".to_string(),
            PropertyValue::String(format!("p{}", input + 1)),
        );
        props.insert(
            "mode".to_string(),
            PropertyValue::String("audio_video".to_string()),
        );
        props.insert("max_sessions".to_string(), PropertyValue::Int(1));
        props.insert("decode".to_string(), PropertyValue::Bool(true));

        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let built = WHIPInputBuilder
            .build(&instance_id, &props, &ctx)
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
        contexts.push(ctx);
    }

    (pipeline, by_id, contexts)
}

/// Wait for the pipeline to settle and report the state it settled in.
fn wait_for_playing(pipeline: &gst::Pipeline, secs: u64) -> (gst::StateChangeSuccess, gst::State) {
    let (result, current, _pending) = pipeline.state(gst::ClockTime::from_seconds(secs));
    let success = result.unwrap_or_else(|e| panic!("pipeline state change failed: {:?}", e));
    (success, current)
}

/// Decodebins still short of PLAYING, so a failure names the culprits.
fn stuck_decodebins(by_id: &HashMap<String, gst::Element>) -> Vec<String> {
    by_id
        .iter()
        .filter(|(id, _)| id.contains("decodebin"))
        .filter_map(|(id, element)| {
            let (_, current, pending) = element.state(gst::ClockTime::ZERO);
            (current != gst::State::Playing)
                .then(|| format!("{} (current {:?}, pending {:?})", id, current, pending))
        })
        .collect()
}

/// Five WHIP inputs, nobody publishing: the pipeline must still reach PLAYING.
///
/// The flow shape this guards is a production with several remote presenters,
/// where the mixer and the recorders must run before all of them connect.
#[test]
fn idle_whip_slots_do_not_block_playing() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let (pipeline, by_id, _contexts) = build_whip_inputs(5);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");

    let (success, current) = wait_for_playing(&pipeline, 10);
    let stuck = stuck_decodebins(&by_id);
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert_eq!(
        (success, current),
        (gst::StateChangeSuccess::Success, gst::State::Playing),
        "five idle WHIP slots held the pipeline out of PLAYING; still short of it: {:?}",
        stuck
    );
}

/// A slot claimed by a session must actually decode.
///
/// Counterpart to the test above: keeping every `decodebin` out of the pipeline
/// for good would satisfy that one and break WHIP input entirely.
#[test]
fn allocated_slot_decodes_incoming_media() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let (pipeline, by_id, contexts) = build_whip_inputs(2);

    // Tap the first input's video output tee, where decoded frames come out.
    let tee = by_id
        .get("whip_p1:video_out_tee_0")
        .expect("the first WHIP input has a video output tee");
    let sink = gst::ElementFactory::make("fakesink")
        .name("probe_sink")
        .property("sync", false)
        // Not `async`: an unprerolled sink would hold the pipeline ASYNC by
        // itself and mask what this test is measuring.
        .property("async", false)
        .property("signal-handoffs", true)
        .build()
        .expect("fakesink");
    let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frames_for_handoff = frames.clone();
    sink.connect("handoff", false, move |_| {
        frames_for_handoff.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    });
    pipeline.add(&sink).expect("add probe sink");
    tee.link(&sink).expect("link tee -> probe sink");

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (success, current) = wait_for_playing(&pipeline, 10);
    assert_eq!(
        (success, current),
        (gst::StateChangeSuccess::Success, gst::State::Playing),
        "pipeline did not reach PLAYING with idle slots; still short of it: {:?}",
        stuck_decodebins(&by_id)
    );

    // A publisher arrives: the session manager claims a slot on the endpoint
    // config the block registered, which is what brings the slot's decode
    // chain into the running pipeline.
    let configs = contexts[0].take_whip_endpoint_configs();
    let (_, config) = configs
        .into_iter()
        .next()
        .expect("the first WHIP input registered an endpoint config");
    let slot = config.allocate_slot("test-session").expect("a free slot");
    assert_eq!(slot, 0);

    // Feed it real H.264, the way a session's appsink bridge does.
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
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    let slot_appsrc = config.slot_video_appsrcs[slot].clone();
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let _ = slot_appsrc.push_sample(&sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    feeder.set_state(gst::State::Playing).expect("feeder plays");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline
        && frames.load(std::sync::atomic::Ordering::Relaxed) == 0
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let decoded = frames.load(std::sync::atomic::Ordering::Relaxed);
    let (_, still_playing, _) = pipeline.state(gst::ClockTime::ZERO);

    feeder.set_state(gst::State::Null).expect("feeder to NULL");
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert!(
        decoded > 0,
        "the slot claimed by the session decoded nothing; decodebins: {:?}",
        stuck_decodebins(&by_id)
    );
    assert_eq!(
        still_playing,
        gst::State::Playing,
        "pipeline left PLAYING after a session claimed a slot"
    );
}

/// A session that claims a slot and then sends nothing must not stall a
/// pipeline that is already running.
///
/// Unlocking a slot's `decodebin` hands it back to the pipeline's state
/// management, and it goes ASYNC again the moment it is asked for PAUSED with
/// nothing to typefind. `async-handling` is what keeps an abandoned WHIP
/// negotiation, or a publisher whose video never negotiates, from pulling a
/// live pipeline back out of PLAYING.
#[test]
fn allocated_slot_without_media_does_not_stall_a_running_pipeline() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    let (pipeline, by_id, contexts) = build_whip_inputs(2);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (success, current) = wait_for_playing(&pipeline, 10);
    assert_eq!(
        (success, current),
        (gst::StateChangeSuccess::Success, gst::State::Playing),
        "pipeline did not reach PLAYING with idle slots; still short of it: {:?}",
        stuck_decodebins(&by_id)
    );

    let configs = contexts[0].take_whip_endpoint_configs();
    let (_, config) = configs
        .into_iter()
        .next()
        .expect("the first WHIP input registered an endpoint config");
    config.allocate_slot("test-session").expect("a free slot");

    // No media follows. Give the decodebins time to go ASYNC and, if the guard
    // is missing, to take the pipeline down with them.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (result, current, pending) = pipeline.state(gst::ClockTime::ZERO);

    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert_eq!(
        (result.expect("pipeline state readable"), current),
        (gst::StateChangeSuccess::Success, gst::State::Playing),
        "a slot claimed but never fed pulled the pipeline out of PLAYING \
         (pending {:?}); decodebins: {:?}",
        pending,
        stuck_decodebins(&by_id)
    );
}
