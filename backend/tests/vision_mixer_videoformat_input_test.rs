//! `videotestsrc ! Video Format ! Vision Mixer`, the flow #1156 and #1157 were
//! reported on.
//!
//! Since #1117 Video Format's input answered a CAPS query with raw video at
//! any size, so a caps-less `videotestsrc` fixated to 320x240 and the block
//! pillarboxed it into its 16:9 output, with pink bars in `A444_16LE`. These
//! build the real flow, with the mixer as the consumer, and require the
//! source to deliver the Video Format block's size, on the software and the
//! GL mixer.
//!
//! The GL case runs one at a time with the other GL tests (`serial(gl)`): GL
//! setup in parallel tests races on the Linux runner (#1095).

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom_types::{Flow, PropertyValue as PV};

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glupload", "glcolorconvert"];

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

/// Run `videotestsrc ! Video Format 640x360 ! Vision Mixer` on the mixer
/// `backend` and return the caps the source settles on.
fn source_caps(backend: &str) -> gst::Caps {
    let mixer = format!("vm_vf_{backend}");
    let mut flow = Flow::new(format!("vm_videoformat_input_{backend}"));
    flow.blocks.push(block(
        &mixer,
        "builtin.vision_mixer",
        vec![
            ("compositor_preference", PV::String(backend.into())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String("640x360".into())),
            ("multiview_resolution", PV::String("640x360".into())),
        ],
    ));
    for i in 0..2 {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![("is-live", PV::Bool(true))],
        ));
        flow.blocks.push(block(
            &format!("vf{i}"),
            "builtin.videoformat",
            vec![("resolution", PV::String("640x360".into()))],
        ));
        link(
            &mut flow,
            &format!("src{i}:src"),
            &format!("vf{i}:video_in"),
        );
        link(
            &mut flow,
            &format!("vf{i}:video_out"),
            &format!("{mixer}:video_in_{i}"),
        );
    }
    for (sink, pad) in [("pgmsink", "pgm_out"), ("mvsink", "multiview_out")] {
        flow.elements
            .push(elem(sink, "fakesink", vec![("sync", PV::Bool(false))]));
        link(
            &mut flow,
            &format!("{mixer}:{pad}"),
            &format!("{sink}:sink"),
        );
    }

    let mut manager = common::manager::build(&flow).expect("build the flow");
    manager.start().expect("start the flow");
    let src_pad = manager
        .pipeline()
        .by_name("src0")
        .expect("src0 in the pipeline")
        .static_pad("src")
        .expect("videotestsrc src pad");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut caps = src_pad.current_caps();
    while caps.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        caps = src_pad.current_caps();
    }
    let _ = manager.stop();
    let caps = caps.expect("the source negotiated within 20s");
    eprintln!("{backend} mixer: videotestsrc settled on {caps}");
    caps
}

fn assert_block_size(backend: &str, caps: &gst::Caps) {
    let info = gstreamer_video::VideoInfo::from_caps(caps).expect("raw video caps");
    assert_eq!(
        (info.width(), info.height()),
        (640, 360),
        "on the {backend} mixer, videotestsrc settled on {caps} in front of Video Format \
         640x360; the block must offer its own size first, or the source fixates to \
         320x240 and is pillarboxed, in pink for high-bit-depth YUV (#1156, #1157)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_source_delivers_the_video_format_size_into_the_software_mixer() {
    gst::init().unwrap();
    common::require_elements(&["videotestsrc", "compositor", "videoscale", "videoconvert"]);
    strom::gpu::detect_gpu_capabilities();
    let caps = source_caps("cpu");
    assert_block_size("software", &caps);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn the_source_delivers_the_video_format_size_into_the_gl_mixer() {
    gst::init().unwrap();
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    strom::gpu::detect_gpu_capabilities();
    let caps = source_caps("gpu");
    assert_block_size("GL", &caps);
}
