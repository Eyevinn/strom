//! The deprecated Compositor's CPU backend downloads GPU memory on its own
//! inputs (#969).
//!
//! Each CPU input was `queue → videoconvert → compositor`, with no download.
//! GL memory (Local Input, WHIP Input, a GPU Vision Mixer) reached the
//! compositor only if the linker's GL-download retry (#802) caught a refused
//! link. Each input now has a front (`gst::download_front`) before its
//! `videoconvert`. `gltestsrc` stands in for the GL producer.
//!
//! The tests run one at a time with the other GL tests (`serial(gl)`): GL
//! setup in parallel tests races on the Linux runner (#1095).

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};

const ELEMENTS: &[&str] = &[
    "compositor",
    "videoconvert",
    "gltestsrc",
    "gldownload",
    "appsink",
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

/// A CPU Compositor with RGBA on `video_in_0`, from `gltestsrc` in GL memory
/// when `gl`, from `videotestsrc` in system memory otherwise, and its output
/// in an appsink.
fn input_flow(block_id: &str, use_queues: bool, gl: bool) -> Flow {
    let mut flow = Flow::new(format!("compositor_cpu_{block_id}"));
    let properties: HashMap<String, PV> = HashMap::from([
        (
            "compositor_preference".to_string(),
            PV::String("cpu".into()),
        ),
        ("num_inputs".to_string(), PV::UInt(1)),
        (
            "output_resolution".to_string(),
            PV::String("320x240".into()),
        ),
        ("use_queues".to_string(), PV::Bool(use_queues)),
    ]);
    let computed_external_pads = strom::blocks::builtin::get_builder("builtin.compositor")
        .and_then(|builder| builder.get_external_pads(&properties));
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.compositor".to_string(),
        name: None,
        properties,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads,
    });
    flow.elements.push(elem(
        "src",
        if gl { "gltestsrc" } else { "videotestsrc" },
        vec![("is-live", PV::Bool(true))],
    ));
    let memory = if gl { "(memory:GLMemory)" } else { "" };
    flow.elements.push(elem(
        "srccaps",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw{memory},format=RGBA,width=320,height=240,framerate=30/1"
            )),
        )],
    ));
    flow.elements.push(elem(
        "outsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    link(&mut flow, "src:src", "srccaps:sink");
    link(&mut flow, "srccaps:src", &format!("{block_id}:video_in_0"));
    link(&mut flow, &format!("{block_id}:video_out"), "outsink:sink");
    flow
}

/// The factories between input 0's front and its `videoconvert`.
fn input_adapters(manager: &PipelineManager, block_id: &str) -> Vec<String> {
    let pipeline = manager.pipeline();
    let front = format!("{block_id}:input_0");
    let convert = pipeline
        .by_name(&format!("{block_id}:videoconvert_0"))
        .expect("videoconvert_0 in the block");
    let feeder = |e: &gst::Element| {
        e.static_pad("sink")
            .and_then(|p| p.peer())
            .and_then(|p| p.parent_element())
    };
    let mut factories = Vec::new();
    let mut next = feeder(&convert);
    while let Some(element) = next {
        if element.name() == front {
            return factories;
        }
        factories.insert(
            0,
            element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default(),
        );
        assert!(factories.len() < 5, "input 0 has a loop: {factories:?}");
        next = feeder(&element);
    }
    panic!("videoconvert_0 is not fed from input 0's front: {factories:?}");
}

/// Run the flow until input 0's converter has negotiated, and return what
/// the front spliced in before it. `None` when GL is not available.
async fn adapters_once_negotiated(
    block_id: &str,
    use_queues: bool,
    gl: bool,
) -> Option<Vec<String>> {
    gst::init().unwrap();
    if !common::gl_available(ELEMENTS) {
        return None;
    }
    strom::gpu::detect_gpu_capabilities();
    let mut manager = common::manager::build(&input_flow(block_id, use_queues, gl))
        .expect("build compositor flow");
    manager.start().expect("start compositor flow");
    // The compositor outputs its background before the input negotiates, so
    // wait for frames through the input's converter, not for output frames.
    let convert_src = manager
        .pipeline()
        .by_name(&format!("{block_id}:videoconvert_0"))
        .and_then(|e| e.static_pad("src"))
        .expect("videoconvert_0 src pad");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while convert_src.current_caps().is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let negotiated = convert_src.current_caps();
    let error = common::bus::first_error(&manager.pipeline().bus().unwrap(), Duration::ZERO);
    let adapters = input_adapters(&manager, block_id);
    let _ = manager.stop();

    assert!(
        negotiated.is_some(),
        "video_in_0 never reached the CPU compositor's converter: {error:?}"
    );
    Some(adapters)
}

async fn assert_gl_input_downloaded_by_its_front(block_id: &str, use_queues: bool) {
    let Some(adapters) = adapters_once_negotiated(block_id, use_queues, true).await else {
        return;
    };
    // The linker's GL-download retry (#802) also rescues a refused link, so
    // negotiation alone would not show which side adapted. The input's own
    // front has to.
    assert_eq!(
        adapters,
        ["gldownload", "capsfilter"],
        "input 0's front must download the GL memory, pinned to system memory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn gl_memory_on_a_queued_input_is_downloaded_by_its_front() {
    assert_gl_input_downloaded_by_its_front("comp_cpu_gl_queued", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn gl_memory_on_an_unqueued_input_is_downloaded_by_its_front() {
    assert_gl_input_downloaded_by_its_front("comp_cpu_gl_direct", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn system_memory_on_an_input_passes_without_a_download() {
    let Some(adapters) = adapters_once_negotiated("comp_cpu_sys", true, false).await else {
        return;
    };
    assert!(
        adapters.is_empty(),
        "system memory needs nothing between input 0's front and its videoconvert, got {adapters:?}"
    );
}
