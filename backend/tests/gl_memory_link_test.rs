//! Regression test: a GL-memory producer must reach a system-memory consumer.
//!
//! A `videoconvert` or `videoscale` with an encoder behind it answers a caps
//! query with system memory only, so it shares no format with a
//! `video/x-raw(memory:GLMemory)` producer and the link is refused before any
//! caps are negotiated. Nothing retries a link that failed for want of a common
//! format, so the consumer and everything below it carried no data while the
//! flow reported Playing — a Vision Mixer on the GPU backend with `gl_download`
//! off silently recorded nothing through a Video Encoder.
//!
//! The linker adapts such a pair with a `gldownload`, and only such a pair.
//! These tests build a real pipeline through `PipelineManager`, which links at
//! construction, and inspect the resulting topology. Linking happens in NULL,
//! so they need the GL plugin installed but no GL context.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

/// `gldownload` and `gltestsrc` both ship in `gstreamer1.0-plugins-base`, which
/// CI installs. A silent skip here would let the guard pass green while testing
/// nothing, so their absence is a CI regression and must fail.
fn require_gl_plugin() {
    for factory in ["gltestsrc", "gldownload"] {
        assert!(
            gstreamer::ElementFactory::find(factory).is_some(),
            "{} is missing, so this guard would test nothing. Install the GStreamer GL plugin.",
            factory
        );
    }
}

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn build_manager(flow: &Flow) -> PipelineManager {
    gstreamer::init().unwrap();
    strom::gpu::detect_gpu_capabilities();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    PipelineManager::new(
        flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("build pipeline")
}

/// `source` feeding a bare `videoconvert -> x264enc -> fakesink`.
///
/// The consumer is spelled out rather than taken from `builtin.videoenc` so the
/// test does not depend on which converter `video_convert_mode` picks: on a
/// CUDA host that is `autovideoconvert`, whose sink pad accepts anything and
/// would link to GL memory unaided.
fn flow_into_encoder(name: &str, source: strom_types::Element) -> Flow {
    let mut flow = Flow::new(name.to_string());
    let source_id = source.id.clone();
    flow.elements.push(source);
    flow.elements.push(elem("convert", "videoconvert", vec![]));
    flow.elements.push(elem("enc", "x264enc", vec![]));
    flow.elements.push(elem(
        "sink",
        "fakesink",
        vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
    ));

    // Consumer chain first, producer last. A block's internal links are made
    // when the block is built, so by the time the flow-level link into it is
    // attempted the converter already has an encoder behind it and answers a
    // caps query with system memory only. Linked the other way round the
    // converter is still unconstrained, advertises `video/x-raw(ANY)`, and
    // takes the GL producer unaided — which is not the shape that fails.
    for (from, to) in [
        ("convert:src".to_string(), "enc:sink".to_string()),
        ("enc:src".to_string(), "sink:sink".to_string()),
        (format!("{}:src", source_id), "convert:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// The factory behind whatever `element`'s src pad is linked to.
fn peer_factory(pipeline: &gstreamer::Pipeline, element: &str) -> Option<String> {
    let element = pipeline.by_name(element)?;
    let peer = element.static_pad("src")?.peer()?;
    peer.parent_element()?
        .factory()
        .map(|f| f.name().to_string())
}

/// The defect: `gltestsrc` offers GL memory and nothing else, `videoconvert`
/// with `x264enc` behind it offers system memory and nothing else. Without the
/// adaptation the link is refused and `gltestsrc:src` is left with no peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gl_only_producer_reaches_a_system_memory_encoder() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    let flow = flow_into_encoder("gl_into_encoder", elem("glsrc", "gltestsrc", vec![]));
    let manager = build_manager(&flow);
    let pipeline = manager.pipeline();

    let src_pad = pipeline
        .by_name("glsrc")
        .expect("glsrc exists")
        .static_pad("src")
        .expect("gltestsrc src pad");
    assert!(
        src_pad.is_linked(),
        "glsrc:src has no peer — the GL producer never reached the encoder, \
         so this branch carries no data while the flow reports Playing"
    );

    assert_eq!(
        peer_factory(pipeline, "glsrc").as_deref(),
        Some("gldownload"),
        "the GL producer is linked, but not through a gldownload"
    );

    let convert_sink = pipeline
        .by_name("convert")
        .expect("convert exists")
        .static_pad("sink")
        .expect("videoconvert sink pad");
    assert!(
        convert_sink.is_linked(),
        "the inserted gldownload does not feed the consumer"
    );
}

/// The adaptation must cost nothing where it is not needed: a system-memory
/// producer links straight through, with no download inserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_system_memory_producer_is_left_alone() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    let flow = flow_into_encoder(
        "system_into_encoder",
        elem("syssrc", "videotestsrc", vec![("is-live", PV::Bool(true))]),
    );
    let manager = build_manager(&flow);

    assert_eq!(
        peer_factory(manager.pipeline(), "syssrc").as_deref(),
        Some("videoconvert"),
        "a system-memory producer was given a gldownload it does not need, \
         costing a GPU round trip per frame"
    );
}

/// The block the defect was reported against, with the converter its own
/// platform picks. Asserts only that the link stands: on a CUDA host
/// `autovideoconvert` takes GL memory directly and no download is inserted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gl_only_producer_reaches_the_video_encoder_block() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    let mut flow = Flow::new("gl_into_videoenc".to_string());
    flow.elements.push(elem("glsrc", "gltestsrc", vec![]));
    flow.elements.push(elem(
        "sink",
        "fakesink",
        vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
    ));
    flow.blocks.push(strom_types::BlockInstance {
        id: "venc".to_string(),
        block_definition_id: "builtin.videoenc".to_string(),
        name: None,
        properties: HashMap::from([("codec".to_string(), PV::String("h264".to_string()))]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }
    for (from, to) in [
        ("glsrc:src".to_string(), "venc:video_in".to_string()),
        ("venc:encoded_out".to_string(), "sink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }

    let manager = build_manager(&flow);
    let src_pad = manager
        .pipeline()
        .by_name("glsrc")
        .expect("glsrc exists")
        .static_pad("src")
        .expect("gltestsrc src pad");
    assert!(
        src_pad.is_linked(),
        "glsrc:src has no peer — the Video Encoder block records nothing from a \
         GPU vision mixer"
    );
}
