//! A block that runs part of its work in a pipeline of its own gives that
//! pipeline's streaming threads the flow's thread priority, CPU affinity and
//! thread registry (#1238).
//!
//! The flow's sync handler sees only the flow's own bus, so the threads of a
//! Media Player's `mediaplayer-internal-*` pipeline, a WHIP Input's
//! `whip-session-*` pipeline and a WHIP Output's webrtcsink session pipeline
//! used to run at normal priority, unpinned, and missing from the thread
//! registry.
//!
//! Each test builds the real block with a thread registry and asserts that the
//! registry holds a thread of the block's own pipeline: filed under the block,
//! for an element that is not one of the block's elements in the flow (those
//! are named `block_id:...`). The registry check needs no privileges, so it
//! runs on every CI platform. Revert the install in a block and its test
//! fails: nothing else registers those threads.

pub mod common;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom::blocks::builtin::whip::{
    create_whipserversrc_for_session, WHIPInputBuilder, WHIPOutputBuilder,
};
use strom::blocks::BlockBuilder;
use strom::gst::ThreadPriorityState;
use strom::thread_registry::{ThreadInfo, ThreadRegistry};
use strom::whip_session_manager::WhipSessionManager;
use strom_types::flow::ThreadPriority;
use strom_types::{Element, Flow, FlowId, Link, PropertyValue};

/// The registered threads of `block_id` that belong to a pipeline of the
/// block's own: filed under the block, for an element without the block's
/// `block_id:` prefix, which every element the block adds to the flow has.
fn internal_threads(registry: &ThreadRegistry, block_id: &str) -> Vec<ThreadInfo> {
    let prefix = format!("{}:", block_id);
    registry
        .get_all()
        .into_iter()
        .filter(|t| t.block_id.as_deref() == Some(block_id) && !t.element_name.starts_with(&prefix))
        .collect()
}

/// Wait up to 20 s for a thread of `block_id`'s own pipeline to register.
fn wait_for_internal_threads(registry: &ThreadRegistry, block_id: &str) -> Vec<ThreadInfo> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let threads = internal_threads(registry, block_id);
        if !threads.is_empty() || Instant::now() > deadline {
            return threads;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn describe(registry: &ThreadRegistry) -> Vec<(String, Option<String>)> {
    registry
        .get_all()
        .into_iter()
        .map(|t| (t.element_name, t.block_id))
        .collect()
}

fn element(id: &str, kind: &str, props: &[(&str, PropertyValue)]) -> Element {
    Element {
        id: id.to_string(),
        element_type: kind.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// The pipeline manager spawns tokio tasks, so it needs a runtime, but the
/// WebRTC elements block on their own runtime while they stop, which tokio
/// refuses on a worker thread. A blocking-pool thread has both.
async fn on_blocking_thread(f: fn()) {
    if let Err(e) = tokio::task::spawn_blocking(f).await {
        std::panic::resume_unwind(e.into_panic());
    }
}

// --- Media Player -----------------------------------------------------------

const PLAYER: &str = "player";

/// A short H.264 clip in MP4. `x264enc` is in gstreamer1.0-plugins-ugly,
/// `mp4mux` in -good, both installed in CI.
fn write_h264_clip(path: &std::path::Path) -> String {
    let writer = gst::parse::launch(
        "videotestsrc num-buffers=250 ! video/x-raw,width=160,height=90,framerate=25/1 \
         ! x264enc tune=zerolatency key-int-max=10 ! h264parse ! mp4mux ! filesink name=out",
    )
    .expect("clip writer pipeline");
    writer
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("out")
        .unwrap()
        .set_property("location", path);
    writer.set_state(gst::State::Playing).unwrap();
    let msg = writer
        .bus()
        .unwrap()
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(20),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("writing the clip finishes");
    assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{:?}", msg);
    writer.set_state(gst::State::Null).unwrap();
    gst::glib::filename_to_uri(path, None).unwrap().to_string()
}

/// Run the default GLib main context, where the player's bus handlers start
/// its internal pipeline once the flow plays, as Strom does in production.
fn run_glib_main_loop() {
    std::thread::spawn(|| {
        gst::glib::MainLoop::new(None, false).run();
    });
}

fn media_player_registers_its_internal_threads() {
    run_glib_main_loop();
    let dir = tempfile::tempdir().expect("temp dir");
    let uri = write_h264_clip(&dir.path().join("clip.mp4"));

    let mut flow = Flow::new("internal_thread_priority_player");
    // Not `async`: the player starts its internal pipeline once the flow
    // plays, so a sink waiting for a preroll would hold the flow in PAUSED.
    flow.elements.push(element(
        "sink",
        "fakesink",
        &[("async", PropertyValue::Bool(false))],
    ));
    flow.blocks.push(strom_types::BlockInstance {
        id: PLAYER.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: HashMap::from([
            (
                "playlist".to_string(),
                PropertyValue::String(serde_json::to_string(&[uri]).unwrap()),
            ),
            ("decode".to_string(), PropertyValue::Bool(true)),
            ("num_audio_tracks".to_string(), PropertyValue::UInt(0)),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow.links.push(Link {
        from: format!("{}:video_out", PLAYER),
        to: "sink:sink".to_string(),
    });

    let registry = ThreadRegistry::new();
    let mut manager = common::manager::build(&flow).expect("PipelineManager builds");
    manager.set_thread_registry(registry.clone());
    manager.start().expect("flow starts");

    let threads = wait_for_internal_threads(&registry, PLAYER);
    let all = describe(&registry);
    manager.stop().expect("flow stops");

    assert!(
        !threads.is_empty(),
        "no thread of the Media Player's internal pipeline is registered under the block; registry: {:?}",
        all
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_player_internal_pipeline_threads_join_the_flow() {
    if !common::plugins_available(&[
        "uridecodebin3",
        "clocksync",
        "appsink",
        "x264enc",
        "mp4mux",
        "fakesink",
    ]) {
        return;
    }
    on_blocking_thread(media_player_registers_its_internal_threads).await;
}

// --- WHIP Input -------------------------------------------------------------

const WHIP_INPUT: &str = "whipin";

/// A WHIP publisher: H.264 from a test source into `whipclientsink`.
fn start_publisher(endpoint: &str) -> gst::Pipeline {
    let publisher = gst::parse::launch(
        "videotestsrc is-live=true ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 \
         ! x264enc tune=zerolatency key-int-max=15 bframes=0 speed-preset=ultrafast \
         ! h264parse ! whipclientsink name=pub",
    )
    .expect("publisher pipeline")
    .downcast::<gst::Pipeline>()
    .unwrap();
    let sink = publisher.by_name("pub").unwrap();
    sink.set_property("stun-server", None::<&str>);
    sink.property::<gst::glib::Object>("signaller")
        .set_property("whip-endpoint", endpoint);
    publisher
        .set_state(gst::State::Playing)
        .expect("publisher plays");
    publisher
}

fn whip_input_registers_its_session_threads() {
    let flow_id = FlowId::new_v4();
    let registry = ThreadRegistry::new();

    // The block, in a flow pipeline whose video output goes to a fakesink.
    let ctx = common::block::context();
    let built = WHIPInputBuilder
        .build(
            WHIP_INPUT,
            &HashMap::from([
                (
                    "endpoint_id".to_string(),
                    PropertyValue::String(WHIP_INPUT.to_string()),
                ),
                (
                    "mode".to_string(),
                    PropertyValue::String("video".to_string()),
                ),
                ("max_sessions".to_string(), PropertyValue::Int(1)),
                ("decode".to_string(), PropertyValue::Bool(true)),
            ]),
            &ctx,
        )
        .expect("WHIP Input block builds");
    let flow = gst::Pipeline::new();
    let by_id = common::block::install(&flow, &built);
    let consumer = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .unwrap();
    flow.add(&consumer).unwrap();
    by_id[&format!("{}:video_out_tee_0", WHIP_INPUT)]
        .link(&consumer)
        .expect("link the slot to its consumer");
    let (_, config) = ctx
        .take_whip_endpoint_configs()
        .into_iter()
        .next()
        .expect("the WHIP input registered an endpoint config");
    flow.set_state(gst::State::Playing).expect("flow plays");

    // What `PipelineManager::start` does once the flow's handler is in.
    ctx.session_thread_config().populate(
        &ThreadPriorityState::new(ThreadPriority::Normal),
        None,
        flow_id,
        Some(registry.clone()),
    );

    let slot = config.allocate_slot("publisher").expect("a free slot");
    let (cleanup_tx, _cleanup_rx) = tokio::sync::mpsc::unbounded_channel();
    let session = create_whipserversrc_for_session(
        &config,
        slot,
        cleanup_tx,
        Arc::new(AtomicBool::new(false)),
    )
    .expect("session starts");
    let publisher = start_publisher(&format!("http://127.0.0.1:{}/whip/endpoint", session.port));

    let threads = wait_for_internal_threads(&registry, WHIP_INPUT);
    let all = describe(&registry);

    let _ = publisher.set_state(gst::State::Null);
    WhipSessionManager::teardown_session_pipeline(&session.session_pipeline);
    let _ = flow.set_state(gst::State::Null);

    assert!(
        !threads.is_empty(),
        "no thread of the WHIP Input's session pipeline is registered under the block; registry: {:?}",
        all
    );
    assert!(
        threads.iter().all(|t| t.flow_id == flow_id),
        "session threads registered under another flow: {:?}",
        all
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whip_input_session_pipeline_threads_join_the_flow() {
    common::init_webrtc_plugins();
    if !common::plugins_available(&[
        "whipserversrc",
        "whipclientsink",
        "x264enc",
        "h264parse",
        "nicesrc",
        "nicesink",
        "fakesink",
    ]) {
        return;
    }
    on_blocking_thread(whip_input_registers_its_session_threads).await;
}

// --- WHIP Output ------------------------------------------------------------

const WHIP_OUTPUT: &str = "out";

/// A standalone `whipserversrc` on a free local port, draining what it
/// receives into fakesinks.
fn start_server() -> (gst::Pipeline, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("whipserversrc").build().unwrap();
    src.set_property("stun-server", None::<&str>);
    src.property::<gst::glib::Object>("signaller")
        .set_property("host-addr", format!("http://127.0.0.1:{}", port));
    src.set_property("video-codecs", gst::Array::new(["H264"]));
    src.set_property("audio-codecs", gst::Array::new(["OPUS"]));
    pipeline.add(&src).unwrap();
    let pipeline_weak = pipeline.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        let _ = sink.sync_state_with_parent();
        let _ = pad.link(&sink.static_pad("sink").unwrap());
    });
    pipeline
        .set_state(gst::State::Playing)
        .expect("server pipeline plays");
    (pipeline, format!("http://127.0.0.1:{}/whip/endpoint", port))
}

fn whip_output_registers_its_session_threads() {
    let (server, endpoint) = start_server();

    let properties = HashMap::from([
        (
            "implementation".to_string(),
            PropertyValue::String("whipclientsink".to_string()),
        ),
        ("whip_endpoint".to_string(), PropertyValue::String(endpoint)),
        ("num_audio_tracks".to_string(), PropertyValue::UInt(1)),
        ("num_video_tracks".to_string(), PropertyValue::UInt(1)),
    ]);
    let mut flow = Flow::new("internal_thread_priority_whip_output");
    flow.elements.push(element(
        "vsrc",
        "videotestsrc",
        &[("is-live", PropertyValue::Bool(true))],
    ));
    // 4:2:0, or x264enc picks Y444 and High 4:4:4, which the block refuses.
    flow.elements.push(element(
        "vcaps",
        "capsfilter",
        &[(
            "caps",
            PropertyValue::String("video/x-raw,format=I420".to_string()),
        )],
    ));
    flow.elements.push(element(
        "venc",
        "x264enc",
        &[
            ("bframes", PropertyValue::UInt(0)),
            ("key-int-max", PropertyValue::UInt(25)),
            ("tune", PropertyValue::String("zerolatency".to_string())),
            (
                "speed-preset",
                PropertyValue::String("ultrafast".to_string()),
            ),
        ],
    ));
    flow.elements.push(element(
        "asrc",
        "audiotestsrc",
        &[("is-live", PropertyValue::Bool(true))],
    ));
    flow.blocks.push(strom_types::BlockInstance {
        id: WHIP_OUTPUT.to_string(),
        block_definition_id: "builtin.whip_output".to_string(),
        name: None,
        properties: properties.clone(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: WHIPOutputBuilder.get_external_pads(&properties),
    });
    for (from, to) in [
        ("vsrc:src", "vcaps:sink"),
        ("vcaps:src", "venc:sink"),
        ("venc:src", "out:video_in"),
        ("asrc:src", "out:audio_in"),
    ] {
        flow.links.push(Link {
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    let registry = ThreadRegistry::new();
    let mut manager = common::manager::build(&flow).expect("PipelineManager builds");
    manager.set_thread_registry(registry.clone());
    manager.start().expect("flow starts");

    let threads = wait_for_internal_threads(&registry, WHIP_OUTPUT);
    let all = describe(&registry);
    manager.stop().expect("flow stops");
    drop(manager);
    let _ = server.set_state(gst::State::Null);

    assert!(
        !threads.is_empty(),
        "no thread of the WHIP Output's session pipeline is registered under the block; registry: {:?}",
        all
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whip_output_session_pipeline_threads_join_the_flow() {
    common::init_webrtc_plugins();
    if !common::plugins_available(&[
        "whipserversrc",
        "whipclientsink",
        "x264enc",
        "h264parse",
        "opusenc",
        "fakesink",
    ]) {
        return;
    }
    on_blocking_thread(whip_output_registers_its_session_threads).await;
}
