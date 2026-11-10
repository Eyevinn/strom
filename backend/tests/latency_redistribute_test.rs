//! A live source through Video Encoder into MPEG-TS/SRT Output must not drop
//! frames to QoS (#1183).
//!
//! The encoder, the muxer and srtsink post a LATENCY message once they have
//! configured themselves, after the pipeline is already PLAYING. Sinks keep the
//! latency they were given until the application redistributes it. When the
//! flow's bus handler ignored the message, srtsink (sync and QoS on) judged
//! every buffer a few ms late, and the flow's capsfilters dropped about a
//! third of the frames: a 30 fps source came out at about 20 fps.
//!
//! The flow is the one from the issue: live test video and audio, through
//! capsfilters (which the flow builds with QoS on), the real Video Encoder
//! block and the real MPEG-TS/SRT Output block, to a local SRT listener.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::events::EventBroadcaster;
use strom_types::{Flow, PropertyValue as PV, StromEvent};

/// Elements this test needs beyond core GStreamer.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "x264enc",
    "h264parse",
    "mpegtsmux",
    "avenc_aac",
    "aacparse",
    "srtsink",
    "srtsrc",
    "fakesink",
];

/// Time the flow gets to start, connect and settle before drops are counted.
const SETTLE: Duration = Duration::from_secs(3);
/// How long drops are counted for.
const WINDOW: Duration = Duration::from_secs(5);

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn block(id: &str, definition: &str, props: Vec<(&str, PV)>) -> strom_types::BlockInstance {
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(strom_types::Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

/// A free UDP port for the SRT listener.
fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

fn issue_flow(port: u16) -> Flow {
    let mut flow = Flow::new("latency_redistribute");
    flow.elements.push(elem(
        "video",
        "videotestsrc",
        vec![
            ("is-live", PV::Bool(true)),
            ("pattern", PV::String("smpte".into())),
        ],
    ));
    flow.elements.push(elem(
        "video_caps",
        "capsfilter",
        vec![(
            "caps",
            PV::String("video/x-raw,width=1280,height=720,framerate=30/1".into()),
        )],
    ));
    flow.elements.push(elem(
        "audio",
        "audiotestsrc",
        vec![("is-live", PV::Bool(true))],
    ));
    flow.elements.push(elem(
        "audio_caps",
        "capsfilter",
        vec![(
            "caps",
            PV::String("audio/x-raw,rate=48000,channels=2".into()),
        )],
    ));
    flow.blocks.push(block(
        "enc",
        "builtin.videoenc",
        vec![
            ("codec", PV::String("h264".into())),
            ("encoder_preference", PV::String("software".into())),
            ("quality_preset", PV::String("ultrafast".into())),
            ("tune", PV::String("zerolatency".into())),
            ("rate_control", PV::String("cbr".into())),
            ("bitrate", PV::UInt(6000)),
            ("keyframe_interval", PV::UInt(25)),
        ],
    ));
    flow.blocks.push(block(
        "out",
        "builtin.mpegtssrt_output",
        vec![
            (
                "srt_uri",
                PV::String(format!("srt://127.0.0.1:{port}?mode=caller")),
            ),
            ("latency", PV::UInt(200)),
        ],
    ));
    link(&mut flow, "video:src", "video_caps:sink");
    link(&mut flow, "video_caps:src", "enc:video_in");
    link(&mut flow, "enc:encoded_out", "out:video_in");
    link(&mut flow, "audio:src", "audio_caps:sink");
    link(&mut flow, "audio_caps:src", "out:audio_in_0");
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_flow_into_srt_output_drops_no_frames_to_qos() {
    if common::skip_known_ci_failure(
        &["macos"],
        "the virtual macOS runner wakes timed waits 40 ms late on average and up to about \
         110 ms, so the live sources deliver late and the sinks drop frames to QoS whatever \
         the pipeline latency",
    ) {
        return;
    }
    gst::init().unwrap();
    let missing: Vec<_> = REQUIRED
        .iter()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "missing GStreamer elements: {missing:?}"
    );

    let port = srt_port();
    let receiver = gst::parse::launch(&format!(
        "srtsrc uri=srt://127.0.0.1:{port}?mode=listener latency=200 ! fakesink sync=false"
    ))
    .expect("SRT receiver");
    receiver
        .set_state(gst::State::Playing)
        .expect("receiver to PLAYING");

    // The bus watch is a glib signal watch, so it only dispatches while a
    // main loop runs. The real app has one; a test does not.
    let main_loop = gst::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };

    let events = EventBroadcaster::with_capacity(4096);
    let mut event_rx = events.subscribe();
    let mut manager = common::manager::build_with(&issue_flow(port), events, std::env::temp_dir())
        .expect("build pipeline");
    manager.start().expect("start pipeline");

    let start = Instant::now();
    let mut drops: HashMap<String, u64> = HashMap::new();
    let mut errors = Vec::new();
    let _ = tokio::time::timeout(SETTLE + WINDOW, async {
        loop {
            match event_rx.recv().await {
                Ok(StromEvent::QoSStats {
                    element_name,
                    event_count,
                    ..
                }) if start.elapsed() >= SETTLE => {
                    *drops.entry(element_name).or_default() += event_count;
                }
                Ok(StromEvent::PipelineError { error, source, .. }) => {
                    errors.push(format!("{error} (from {source:?})"))
                }
                Ok(_) => continue,
                Err(e) => panic!("event stream broke: {e}"),
            }
        }
    })
    .await;

    manager.stop().expect("stop");
    drop(manager);
    main_loop.quit();
    main_loop_thread.join().unwrap();
    let _ = receiver.set_state(gst::State::Null);

    assert!(errors.is_empty(), "the flow failed: {errors:?}");
    // About 11 drops a second on the video capsfilter alone when latency is
    // not redistributed. A loaded runner may drop a frame or two for real.
    let total: u64 = drops.values().sum();
    assert!(
        total <= 5,
        "{total} buffers dropped to QoS in {WINDOW:?} of steady state: {drops:?}. \
         The flow's bus handler has to recalculate the pipeline latency when an \
         element posts a LATENCY message."
    );
}
