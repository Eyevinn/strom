//! Regression test: a qos-enabled sink that a block builds must not send its
//! QoS events upstream.
//!
//! `add_element` drops the upstream QoS events of every qos-enabled sink, but
//! block elements bypass it. The RTMP output's `rtmp2sink` (qos=true, here
//! with its `sync` property off) then reported, through `flvmux`, the
//! running time of the audio whenever the video arrived late against the
//! clock, and the encoder's `videoconvert` dropped every video frame as too
//! late: the stream carried no video for as long as the video stayed behind.
//!
//! The test builds a flow with an RTMP output through `PipelineManager::new`
//! (without starting it, so no server is needed), pushes a QoS event upstream
//! from the sink's pad and checks that it does not reach the muxer.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::Flow;

const REQUIRED: &[&str] = &["rtmp2sink", "flvmux", "identity"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_sink_does_not_send_qos_events_upstream() {
    if !common::plugins_available(REQUIRED) {
        return;
    }

    let mut flow = Flow::new("block_sink_qos");
    flow.blocks.push(strom_types::BlockInstance {
        id: "rtmp".to_string(),
        block_definition_id: "builtin.rtmp_output".to_string(),
        name: None,
        properties: HashMap::new(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    let registry_file = tempfile::NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let manager = PipelineManager::new(
        &flow,
        EventBroadcaster::with_capacity(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("build pipeline");

    let block_element = |suffix: &str| {
        manager
            .find_block_elements("rtmp")
            .into_iter()
            .find(|(id, _)| id.ends_with(suffix))
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("the RTMP block builds no element ending in {suffix}"))
    };
    let sink = block_element("rtmp_sink");
    let mux = block_element("rtmp_flvmux");
    assert!(sink.property::<bool>("qos"), "the RTMP sink enables QoS");

    // Pads of a pipeline in NULL are flushing and refuse every event; activate
    // just these two instead of starting the flow (no RTMP server here).
    let sink_pad = sink.static_pad("sink").unwrap();
    let mux_src = mux.static_pad("src").unwrap();
    assert_eq!(
        sink_pad.peer().as_ref(),
        Some(&mux_src),
        "the muxer is linked to the sink"
    );
    mux_src.set_active(true).unwrap();
    sink_pad.set_active(true).unwrap();

    let qos = Arc::new(AtomicU32::new(0));
    let other = Arc::new(AtomicU32::new(0));
    {
        let (qos, other) = (Arc::clone(&qos), Arc::clone(&other));
        mux_src.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                let seen = if event.type_() == gst::EventType::Qos {
                    &qos
                } else {
                    &other
                };
                seen.fetch_add(1, Ordering::SeqCst);
            }
            gst::PadProbeReturn::Pass
        });
    }

    sink_pad.push_event(gst::event::Qos::new(
        gst::QOSType::Underflow,
        0.1,
        6_000_000_000,
        gst::ClockTime::from_seconds(10),
    ));
    // The control: any other upstream event still gets through, so the path
    // from the sink to the muxer is open and the QoS check means something.
    sink_pad.push_event(gst::event::Reconfigure::new());

    mux_src.set_active(false).unwrap();
    sink_pad.set_active(false).unwrap();
    assert_eq!(
        other.load(Ordering::SeqCst),
        1,
        "the control event did not reach the muxer"
    );
    assert_eq!(
        qos.load(Ordering::SeqCst),
        0,
        "a QoS event from the block's sink reached the muxer and with it the encoder"
    );
}
