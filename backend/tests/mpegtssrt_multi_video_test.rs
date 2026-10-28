//! MPEG-TS/SRT Output with more than one video track (#1150).
//!
//! With `num_video_tracks` above 1 the block exposes `video_in_0..N`, mapped to
//! internal elements `video_input_0..N`, but it built one element called
//! `video_input`. No flow link to a video pad could resolve, so each one waited
//! in the pending links for a pad that never appeared: the sources stayed
//! unlinked, the muxer got no video, and the flow sat on "waiting" with no
//! error. Raw video was not refused either, because the refusal lives on the
//! element that was never linked.
//!
//! The flow tests build real flows through the pipeline manager, as
//! `start_flow` does, with the issue's topology: four video sources linked
//! straight into one MPEG-TS/SRT Output. `videotestsrc` stands in for NDI
//! Input, which emits raw video in system memory. The EFP/SRT Output had the
//! same mismatch; its shape test runs with the `efp` feature.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::mpegtssrt::MpegTsSrtOutputBuilder;
use strom::blocks::BlockBuilder;
use strom::gst::pipeline::PipelineManager;
use strom_types::{BlockInstance, Flow, PropertyValue as PV};

const TRACKS: usize = 4;

/// Elements the flows use beyond core GStreamer.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "capsfilter",
    "x264enc",
    "h264parse",
    "mpegtsmux",
    "srtsink",
    "srtsrc",
    "decodebin",
    "tsdemux",
    "avdec_h264",
    "videoconvert",
    "fakesink",
];

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(strom_types::Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

fn block(id: &str, definition: &str, properties: HashMap<String, PV>) -> BlockInstance {
    let computed_external_pads = strom::blocks::builtin::get_builder(definition)
        .and_then(|builder| builder.get_external_pads(&properties));
    BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads,
    }
}

/// Four live video sources, raw or H.264, each linked to its own video pad of
/// an MPEG-TS/SRT Output with no audio, listening on `port`.
fn sender_flow(block_id: &str, port: u16, encoded: bool) -> Flow {
    let mut flow = Flow::new(format!("tsout_{block_id}"));
    flow.blocks.push(block(
        block_id,
        "builtin.mpegtssrt_output",
        HashMap::from([
            ("num_video_tracks".to_string(), PV::UInt(TRACKS as u64)),
            ("num_audio_tracks".to_string(), PV::UInt(0)),
            (
                "srt_uri".to_string(),
                PV::String(format!("srt://127.0.0.1:{port}?mode=listener")),
            ),
            ("wait_for_connection".to_string(), PV::Bool(false)),
        ]),
    ));
    for i in 0..TRACKS {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![("is-live", PV::Bool(true)), ("pattern", PV::Int(i as i64))],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String("video/x-raw,width=320,height=240,framerate=25/1".to_string()),
            )],
        ));
        link(&mut flow, &format!("src{i}:src"), &format!("caps{i}:sink"));
        let feed = if encoded {
            flow.elements.push(elem(
                &format!("enc{i}"),
                "x264enc",
                vec![
                    ("key-int-max", PV::UInt(25)),
                    ("tune", PV::String("zerolatency".to_string())),
                    ("speed-preset", PV::String("ultrafast".to_string())),
                ],
            ));
            link(&mut flow, &format!("caps{i}:src"), &format!("enc{i}:sink"));
            format!("enc{i}:src")
        } else {
            format!("caps{i}:src")
        };
        link(&mut flow, &feed, &format!("{block_id}:video_in_{i}"));
    }
    flow
}

/// An MPEG-TS/SRT Input calling `port`, decoding four video tracks, each into
/// its own fakesink.
fn receiver_flow(block_id: &str, port: u16) -> Flow {
    let mut flow = Flow::new(format!("tsin_{block_id}"));
    flow.blocks.push(block(
        block_id,
        "builtin.mpegtssrt_input",
        HashMap::from([
            ("num_video_tracks".to_string(), PV::UInt(TRACKS as u64)),
            ("num_audio_tracks".to_string(), PV::UInt(0)),
            (
                "srt_uri".to_string(),
                PV::String(format!("srt://127.0.0.1:{port}?mode=caller")),
            ),
        ]),
    ));
    for i in 0..TRACKS {
        flow.elements.push(elem(
            &format!("sink{i}"),
            "fakesink",
            vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
        ));
        link(
            &mut flow,
            &format!("{block_id}:video_out_{i}"),
            &format!("sink{i}:sink"),
        );
    }
    flow
}

/// Every input pad `builder` exposes for `num_video` video tracks must name an
/// element the same builder builds, or a flow link to that pad never resolves.
fn assert_inputs_resolve<B: BlockBuilder>(builder: &B, mut props: HashMap<String, PV>) {
    for num_video in [1u64, 2, 4] {
        props.insert("num_video_tracks".to_string(), PV::UInt(num_video));
        let pads = builder
            .get_external_pads(&props)
            .expect("block computes its pads");
        let built = builder
            .build("blk", &props, &common::block::context())
            .expect("block builds");
        let ids: Vec<&str> = built.elements.iter().map(|(id, _)| id.as_str()).collect();
        for pad in &pads.inputs {
            let target = format!("blk:{}", pad.internal_element_id);
            assert!(
                ids.contains(&target.as_str()),
                "with {num_video} video track(s), input pad {} maps to {target}, which the \
                 block does not build (it builds {ids:?})",
                pad.name
            );
        }
    }
}

fn output_props() -> HashMap<String, PV> {
    HashMap::from([
        ("num_audio_tracks".to_string(), PV::UInt(1)),
        (
            "srt_uri".to_string(),
            PV::String(format!(
                "srt://127.0.0.1:{}?mode=listener",
                common::free_udp_port()
            )),
        ),
    ])
}

#[test]
fn mpegtssrt_output_builds_every_input_it_exposes() {
    if !common::plugins_available(&["mpegtsmux", "srtsink"]) {
        return;
    }
    assert_inputs_resolve(&MpegTsSrtOutputBuilder, output_props());
}

/// The EFP/SRT Output had the same mismatch. `efpmux` comes from the statically
/// linked `gst-plugin-efp`, so this runs wherever the `efp` feature is built.
#[cfg(feature = "efp")]
#[test]
fn efpsrt_output_builds_every_input_it_exposes() {
    let _ = gst::init();
    let _ = gst_plugin_efp::plugin_register_static();
    if !common::plugins_available(&["efpmux", "srtsink"]) {
        return;
    }
    assert_inputs_resolve(
        &strom::blocks::builtin::efpsrt::EfpSrtOutputBuilder,
        output_props(),
    );
}

fn start(flow: &Flow) -> PipelineManager {
    let mut manager = common::manager::build(flow).expect("flow builds");
    manager.start().expect("flow starts");
    manager
}

/// The guard for the refusal: four raw tracks fail the flow, and the error
/// names the Video Encoder block. Before the fix the flow ran with no error at
/// all, because no video link was ever made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_raw_video_tracks_fail_the_flow_naming_the_video_encoder() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let block_id = "tsout_raw";
    let mut manager = start(&sender_flow(block_id, common::free_udp_port(), false));
    let bus = manager.pipeline().bus().expect("pipeline bus");
    let errors = common::bus::collect_errors(&bus, Duration::from_secs(3));
    let _ = manager.stop();

    assert!(
        !errors.is_empty(),
        "raw video on {TRACKS} video tracks must fail the flow, but it ran without \
         an error: the video links never resolved to the block's inputs"
    );
    let refused: Vec<_> = errors
        .iter()
        .filter(|e| e.source.starts_with(&format!("{block_id}:video_input_")))
        .collect();
    assert!(
        !refused.is_empty(),
        "the error must come from one of the block's video inputs: {errors:?}"
    );
    assert!(
        refused
            .iter()
            .all(|e| e.message.contains("builtin.videoenc")),
        "the refusal must name builtin.videoenc: {refused:?}"
    );
}

/// The guard for the tracks themselves: four H.264 tracks go out through one
/// muxer, and a receiving MPEG-TS/SRT Input decodes frames on all four video
/// outputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_encoded_video_tracks_reach_the_receiver() {
    if !common::plugins_available(REQUIRED) {
        return;
    }
    let port = common::free_udp_port();
    let mut sender = start(&sender_flow("tsout_h264", port, true));
    let mut receiver = start(&receiver_flow("tsin_h264", port));

    let counts: Vec<Arc<AtomicU64>> = (0..TRACKS).map(|_| Arc::default()).collect();
    for (i, count) in counts.iter().enumerate() {
        let sink = receiver
            .pipeline()
            .by_name(&format!("sink{i}"))
            .expect("receiver sink");
        let count = count.clone();
        // Counts buffers into the test's own sinks only.
        sink.static_pad("sink")
            .expect("fakesink sink pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
    }

    let all_arrived = || counts.iter().all(|c| c.load(Ordering::Relaxed) >= 10);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !all_arrived() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let frames: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let sender_error = common::bus::first_error(&sender.pipeline().bus().unwrap(), Duration::ZERO);
    let receiver_error =
        common::bus::first_error(&receiver.pipeline().bus().unwrap(), Duration::ZERO);
    let _ = receiver.stop();
    let _ = sender.stop();

    assert!(
        all_arrived(),
        "every video track must reach the receiver; decoded frames per track: \
         {frames:?} (sender error: {sender_error:?}, receiver error: {receiver_error:?})"
    );
}
