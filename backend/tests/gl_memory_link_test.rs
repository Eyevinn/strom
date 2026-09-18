//! Regression test: a GL-memory producer must reach a system-memory consumer.
//!
//! A `videoconvert` or `videoscale` with an encoder behind it answers a caps
//! query with system memory only, so it shares no format with a
//! `video/x-raw(memory:GLMemory)` producer and the link is refused before any
//! caps are negotiated. Nothing retries a link that failed for want of a common
//! format, so without the fallback the consumer and everything below it carry
//! no data while the flow reports Playing: a GPU Vision Mixer with
//! `gl_download` off feeds a Video Encoder nothing.
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

/// `gldownload` and `gltestsrc` both ship in the GL plugin (`gstreamer1.0-gl`
/// on Ubuntu), which CI installs. A silent skip here would let the guard pass
/// green while testing nothing, so their absence is a CI regression and must
/// fail.
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
    let link = (format!("{}:src", source.id), "convert:sink".to_string());
    flow_into_encoder_via(name, source, link)
}

/// [`flow_into_encoder`], with the producer-to-converter link spelled as `link`.
fn flow_into_encoder_via(
    name: &str,
    source: strom_types::Element,
    (link_from, link_to): (String, String),
) -> Flow {
    let mut flow = Flow::new(name.to_string());
    flow.elements.push(source);
    flow.elements.push(elem("convert", "videoconvert", vec![]));
    flow.elements.push(elem("enc", "x264enc", vec![]));
    flow.elements.push(elem(
        "sink",
        "fakesink",
        vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
    ));

    // Consumer chain first, producer last. A block's internal links are made
    // before the links between blocks, so by the time the flow-level link into
    // it is attempted the converter already has an encoder behind it and
    // answers a caps query with system memory only. Linked the other way round
    // the converter is still unconstrained, advertises `video/x-raw(ANY)`, and
    // takes the GL producer unaided — which is not the shape that fails.
    for (from, to) in [
        ("convert:src".to_string(), "enc:sink".to_string()),
        ("enc:src".to_string(), "sink:sink".to_string()),
        (link_from, link_to),
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

/// A link that leaves either pad unnamed is made element to element, and
/// GStreamer refuses it without saying why. Each spelling must be adapted too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gl_only_producer_is_adapted_when_a_pad_is_unnamed() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    for (from, to) in [
        ("glsrc", "convert:sink"),
        ("glsrc:src", "convert"),
        ("glsrc", "convert"),
    ] {
        let flow = flow_into_encoder_via(
            "gl_into_encoder_unnamed",
            elem("glsrc", "gltestsrc", vec![]),
            (from.to_string(), to.to_string()),
        );
        let manager = build_manager(&flow);

        assert_eq!(
            peer_factory(manager.pipeline(), "glsrc").as_deref(),
            Some("gldownload"),
            "{} -> {}: the GL producer was not linked through a gldownload",
            from,
            to
        );
    }
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

/// A bare pipeline holding `elements`, as `(factory, name)`, all unlinked.
fn bare_pipeline(elements: &[(&str, &str)]) -> (gstreamer::Pipeline, Vec<gstreamer::Element>) {
    let pipeline = gstreamer::Pipeline::new();
    let elements: Vec<_> = elements
        .iter()
        .map(|(factory, name)| {
            gstreamer::ElementFactory::make(factory)
                .name(*name)
                .build()
                .unwrap_or_else(|e| panic!("{} could not be created: {}", factory, e))
        })
        .collect();
    pipeline.add_many(&elements).unwrap();
    (pipeline, elements)
}

fn gldownloads_in(pipeline: &gstreamer::Pipeline) -> usize {
    pipeline
        .children()
        .iter()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "gldownload"))
        .count()
}

/// A consumer pad that is already linked passes the caps test just as a
/// system-memory one does, but a download cannot link to it either. The
/// refusal is not a format one, so it must come back unadapted.
#[test]
fn a_link_refused_for_another_reason_is_not_adapted() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    let (pipeline, elements) = bare_pipeline(&[
        ("gltestsrc", "glsrc"),
        ("videotestsrc", "syssrc"),
        ("videoconvert", "convert"),
        ("x264enc", "enc"),
        ("fakesink", "sink"),
    ]);
    gstreamer::Element::link_many(&elements[1..]).unwrap();

    let src = elements[0].static_pad("src").unwrap();
    let sink = elements[2].static_pad("sink").unwrap();
    let refusal = src.link(&sink).expect_err("convert:sink is already linked");
    assert_eq!(refusal, gstreamer::PadLinkError::WasLinked);

    assert_eq!(
        strom::gst::gl_link::retry_link_with_gl_download(&src, &sink, refusal),
        Ok(false),
        "a WasLinked refusal was treated as a memory-format one"
    );
    assert_eq!(gldownloads_in(&pipeline), 0);
    assert!(!src.is_linked());
}

/// An adaptation that cannot be completed is undone. `gltestsrc` offers RGBA
/// only, so the `gldownload` behind it cannot produce the I420 the capsfilter
/// demands: the first half of the splice links and the second is refused.
#[test]
fn a_failed_adaptation_leaves_nothing_behind() {
    gstreamer::init().unwrap();
    require_gl_plugin();

    let (pipeline, elements) = bare_pipeline(&[
        ("gltestsrc", "glsrc"),
        ("capsfilter", "i420"),
        ("fakesink", "sink"),
    ]);
    elements[1].set_property(
        "caps",
        gstreamer::Caps::builder("video/x-raw")
            .field("format", "I420")
            .build(),
    );
    elements[1].link(&elements[2]).unwrap();

    let src = elements[0].static_pad("src").unwrap();
    let sink = elements[1].static_pad("sink").unwrap();
    let refusal = src
        .link(&sink)
        .expect_err("GL memory into a system-memory capsfilter");
    assert_eq!(refusal, gstreamer::PadLinkError::Noformat);

    let result = strom::gst::gl_link::retry_link_with_gl_download(&src, &sink, refusal);
    assert!(
        result.is_err(),
        "expected the splice to fail, got {:?}",
        result
    );
    assert_eq!(
        gldownloads_in(&pipeline),
        0,
        "the gldownload from a failed adaptation was left in the pipeline"
    );
    assert!(
        !src.is_linked(),
        "glsrc:src still feeds the removed gldownload"
    );
}

/// The Local Input capture front must not demand system memory.
///
/// `devicesrc` puts a capsfilter between the camera and its converter so the
/// device negotiates the configured resolution and framerate at capture time.
/// A capsfilter naming only `video/x-raw` means `memory:SystemMemory`, so a
/// camera that delivers GL memory and nothing else — an AVFoundation camera
/// behind `avfvideosrc` — shares no format with it and the capture front never
/// negotiates. Offering both memory types lets it negotiate; the download that
/// the system-memory output then needs is spliced by the linker, which is what
/// the tests above cover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_local_input_capture_front_accepts_gl_memory() {
    gstreamer::init().unwrap();

    let mut flow = Flow::new("local_input_capture_front".to_string());
    flow.elements.push(elem(
        "sink",
        "fakesink",
        vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
    ));
    flow.blocks.push(strom_types::BlockInstance {
        id: "cam".to_string(),
        block_definition_id: "builtin.local_input".to_string(),
        name: None,
        properties: HashMap::from([
            ("stream_mode".to_string(), PV::String("video".to_string())),
            (
                "video_resolution".to_string(),
                PV::String("1280x720".to_string()),
            ),
            (
                "video_framerate".to_string(),
                PV::String("25/1".to_string()),
            ),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }
    flow.links.push(strom_types::Link {
        from: "cam:video_out".to_string(),
        to: "sink:sink".to_string(),
    });

    let manager = build_manager(&flow);
    let capsfilter = manager
        .pipeline()
        .by_name("cam:videosrc_caps")
        .expect("the capture front's pre-convert capsfilter exists");
    let caps: gstreamer::Caps = capsfilter.property("caps");

    let gl_caps = "video/x-raw(memory:GLMemory),width=1280,height=720,framerate=25/1"
        .parse::<gstreamer::Caps>()
        .expect("GL caps parse");
    assert!(
        caps.can_intersect(&gl_caps),
        "the capture front demands system memory ({}), so a GL-memory-only camera \
         cannot negotiate it and the capture stalls",
        caps
    );

    let system_caps = "video/x-raw,width=1280,height=720,framerate=25/1"
        .parse::<gstreamer::Caps>()
        .expect("system caps parse");
    assert!(
        caps.can_intersect(&system_caps),
        "the capture front stopped accepting system memory ({}), which would break \
         every camera that works today",
        caps
    );
}
