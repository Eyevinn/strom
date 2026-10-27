//! The GPU Vision Mixer emits GL memory, and its consumers adapt (#969).
//!
//! The mixer had a `gl_download` property that downloaded PGM and multiview to
//! system memory inside the block, a producer-side guess at what is
//! downstream. It is gone: the block emits the GL memory it produces, and each
//! consumer adapts its own input (docs/BLOCK_GUIDELINES.md, "GStreamer Memory
//! Formats"). Stored flows still carry the property, so it must be tolerated
//! and must not bring the download back.
//!
//! * [`a_stored_gl_download_still_emits_gl_memory`] builds an old flow with
//!   `gl_download=true` and checks that both outputs leave the block in GL
//!   memory, with no `gldownload` inside it. It links in NULL, so it needs the
//!   GL plugin but no GL context.
//! * [`a_video_encoder_encodes_the_gl_output`] runs the mixer into a Video
//!   Encoder with the software encoder, which takes system memory only. It
//!   fails if no encoded frame comes out, or if the download is not the one
//!   the block's input front puts in: the linker's GL-download retry (#802)
//!   would otherwise hide a block that stopped adapting.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom_types::{Flow, PropertyValue as PV};

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glshader", "glupload", "gldownload"];

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

/// A GPU mixer with two `videotestsrc` inputs. `extra` adds stored properties.
fn mixer_flow(name: &str, block_id: &str, extra: Vec<(&str, PV)>) -> Flow {
    let mut flow = Flow::new(name);
    let mut props = vec![
        ("compositor_preference", PV::String("gpu".into())),
        ("num_inputs", PV::UInt(2)),
        ("pgm_resolution", PV::String("320x180".into())),
        ("multiview_resolution", PV::String("320x180".into())),
    ];
    props.extend(extra);
    flow.blocks
        .push(block(block_id, "builtin.vision_mixer", props));
    for i in 0..2 {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![("is-live", PV::Bool(true))],
        ));
        link(
            &mut flow,
            &format!("src{i}:src"),
            &format!("{block_id}:video_in_{i}"),
        );
    }
    flow
}

fn has_gl_memory(caps: &gst::Caps) -> bool {
    caps.features(0)
        .is_some_and(|f| f.contains(strom::gst::video_adapt::GL_MEMORY_FEATURE))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_gl_download_still_emits_gl_memory() {
    gst::init().unwrap();
    if !common::gl_elements_available(GL_ELEMENTS) {
        return;
    }
    strom::gpu::detect_gpu_capabilities();
    let block_id = "vm_gl_out_stored";
    let mut flow = mixer_flow(
        "vm_gl_output_stored",
        block_id,
        vec![("gl_download", PV::Bool(true))],
    );
    for (sink, pad) in [("pgmsink", "pgm_out"), ("mvsink", "multiview_out")] {
        flow.elements
            .push(elem(sink, "fakesink", vec![("sync", PV::Bool(false))]));
        link(
            &mut flow,
            &format!("{block_id}:{pad}"),
            &format!("{sink}:sink"),
        );
    }
    let manager = common::manager::build(&flow).expect("an old flow with gl_download still builds");
    let pipeline = manager.pipeline();

    let downloads: Vec<String> = pipeline
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.factory().is_some_and(|f| f.name() == "gldownload"))
        .map(|e| e.name().to_string())
        .collect();
    assert!(
        downloads.is_empty(),
        "the mixer downloads its output itself ({downloads:?}); it must emit GL memory \
         and leave the download to the consumer"
    );

    for capsfilter in ["capsfilter_dist", "capsfilter_mv"] {
        let caps = pipeline
            .by_name(&format!("{block_id}:{capsfilter}"))
            .unwrap_or_else(|| panic!("{capsfilter} in the block"))
            .property::<gst::Caps>("caps");
        assert!(
            has_gl_memory(&caps),
            "{capsfilter} pins {caps}, not GL memory: the output no longer leaves \
             the block in the memory the mixer produces"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_video_encoder_encodes_the_gl_output() {
    gst::init().unwrap();
    let mut required = GL_ELEMENTS.to_vec();
    required.push("x264enc");
    if !common::gl_available(&required) {
        return;
    }
    strom::gpu::detect_gpu_capabilities();
    let block_id = "vm_gl_out_enc";
    let mut flow = mixer_flow("vm_gl_output_encoder", block_id, vec![]);
    flow.blocks.push(block(
        "enc",
        "builtin.videoenc",
        vec![
            ("codec", PV::String("h264".into())),
            ("encoder_preference", PV::String("software".into())),
        ],
    ));
    flow.elements
        .push(elem("encsink", "appsink", vec![("sync", PV::Bool(false))]));
    flow.elements
        .push(elem("mvsink", "fakesink", vec![("sync", PV::Bool(false))]));
    link(&mut flow, &format!("{block_id}:pgm_out"), "enc:video_in");
    link(&mut flow, "enc:encoded_out", "encsink:sink");
    link(
        &mut flow,
        &format!("{block_id}:multiview_out"),
        "mvsink:sink",
    );

    let mut manager = common::manager::build(&flow).expect("build mixer to encoder flow");
    manager.start().expect("start mixer to encoder flow");
    let appsink = manager
        .pipeline()
        .by_name("encsink")
        .expect("encsink in pipeline")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink type");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut encoded = 0;
    let mut caps = None;
    while encoded < 5 && Instant::now() < deadline {
        if let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_mseconds(500)) {
            caps = sample.caps().map(|c| c.to_owned());
            encoded += 1;
        }
    }
    // Each download, with the element that feeds it.
    let downloads: Vec<(String, Option<String>)> = manager
        .pipeline()
        .iterate_recurse()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.factory().is_some_and(|f| f.name() == "gldownload"))
        .map(|e| {
            let upstream = e
                .static_pad("sink")
                .and_then(|p| p.peer())
                .and_then(|p| p.parent_element())
                .map(|p| p.name().to_string());
            (e.name().to_string(), upstream)
        })
        .collect();
    let _ = manager.stop();

    assert_eq!(
        encoded, 5,
        "the Video Encoder behind the GPU mixer encoded {encoded} frames in 30s; \
         it has to download the mixer's GL memory for x264enc"
    );
    // The linker's GL-download retry (#802) also rescues a refused link, so
    // frames alone would not show which side adapted. The block's own input
    // front has to.
    assert!(
        downloads
            .iter()
            .any(|(_, upstream)| upstream.as_deref() == Some("enc:input")),
        "no download behind the Video Encoder's input front ({downloads:?}); \
         the block must adapt the GL memory itself, from the negotiated caps"
    );
    let caps = caps.expect("encoded caps");
    assert_eq!(
        caps.structure(0).map(|s| s.name().as_str()),
        Some("video/x-h264"),
        "the encoder's output is {caps}"
    );
}
