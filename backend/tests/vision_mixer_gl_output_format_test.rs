//! Regression test: the GPU backend must honour `output_format`.
//!
//! `glvideomixerelement` is RGBA-only on both its sink and src pad templates,
//! and `gldownload` moves GL memory to system memory without touching the
//! pixel format. So a bare `output_format` capsfilter placed after the
//! download has no common format with its peer and never links, and a
//! capsfilter carrying no format at all on the GL-passthrough path silently
//! ignores the property. The observed failure was a flow that reached
//! `Playing`, carried audio, and had no video anywhere.
//!
//! The fix converts inside GL — a `glcolorconvert` between the mixer output
//! and the format pin — so the conversion happens where the mixer's RGBA
//! still exists, on the GPU, before the buffer crosses the bus.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vmgl";
const W: u32 = 320;
const H: u32 = 180;
/// Alpha-less and 4:2:0 — the format a hardware H.264 encoder wants, and the
/// one that took the video path down.
const OUTPUT_FORMAT: &str = "NV12";

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// The GPU backend cannot be built without this factory, so a silent skip here
/// would let the guard pass green while testing nothing. The GL plugin ships in
/// `gstreamer1.0-plugins-base`, which CI installs, so its absence is a CI
/// regression. Only the end-to-end test below needs a GL *context*, which
/// headless runners do not have.
fn require_gl_plugin() {
    assert!(
        gstreamer::ElementFactory::find("glvideomixerelement").is_some(),
        "glvideomixerelement is missing, so the GPU backend cannot be built and this \
         guard would test nothing. Install the GStreamer GL plugin."
    );
}

/// Probe whether this environment can actually render through GL. The GL
/// plugins being installed is not enough: on headless runners the elements
/// exist but no context can be created. Same probe as `vision_mixer_fx_test`.
fn gl_environment_available() -> bool {
    if gstreamer::ElementFactory::find("gltestsrc").is_none() {
        return false;
    }
    let Ok(pipeline) = gstreamer::parse::launch(
        "gltestsrc num-buffers=3 ! video/x-raw(memory:GLMemory),format=RGBA,width=64,height=64,framerate=30/1 ! fakesink sync=false",
    ) else {
        return false;
    };
    let Ok(pipeline) = pipeline.downcast::<gstreamer::Pipeline>() else {
        return false;
    };
    if pipeline.set_state(gstreamer::State::Playing).is_err() {
        return false;
    }
    let bus = pipeline.bus().expect("pipeline has a bus");
    let ok = matches!(
        bus.timed_pop_filtered(
            gstreamer::ClockTime::from_seconds(20),
            &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
        ),
        Some(msg) if matches!(msg.view(), gstreamer::MessageView::Eos(_))
    );
    let _ = pipeline.set_state(gstreamer::State::Null);
    ok
}

/// A vision mixer forced onto the GPU backend with one keyed DSK pad, both
/// outputs terminated in a sink. `gl_download` selects between the two GPU
/// output shapes: download to system memory, or hand GL memory downstream.
fn build_flow(gl_download: bool) -> Flow {
    let mut flow = Flow::new(format!("vm_gl_output_format_{}", gl_download));
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String("gpu".to_string()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(2));
            p.insert("num_dsk_inputs".to_string(), PV::String("1".to_string()));
            p.insert(
                "output_format".to_string(),
                PV::String(OUTPUT_FORMAT.to_string()),
            );
            p.insert("gl_download".to_string(), PV::Bool(gl_download));
            p.insert(
                "pgm_resolution".to_string(),
                PV::String(format!("{}x{}", W, H)),
            );
            p.insert(
                "multiview_resolution".to_string(),
                PV::String(format!("{}x{}", W, H)),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    flow.elements.push(elem(
        "bg",
        "videotestsrc",
        vec![
            ("pattern", PV::String("smpte".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "bgcaps",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw,format=RGBA,width={},height={},framerate=30/1",
                W, H
            )),
        )],
    ));
    for sink in ["pgmsink", "mvsink"] {
        flow.elements.push(elem(
            sink,
            "fakesink",
            vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
        ));
    }

    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }

    for (from, to) in [
        ("bg:src".to_string(), "bgcaps:sink".to_string()),
        ("bgcaps:src".to_string(), format!("{}:video_in_0", BLOCK_ID)),
        (format!("{}:pgm_out", BLOCK_ID), "pgmsink:sink".to_string()),
        (
            format!("{}:multiview_out", BLOCK_ID),
            "mvsink:sink".to_string(),
        ),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

fn build_manager(gl_download: bool) -> PipelineManager {
    gstreamer::init().unwrap();
    strom::gpu::detect_gpu_capabilities();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    PipelineManager::new(
        &build_flow(gl_download),
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("build GPU pipeline")
}

/// The format pin on each output branch must be reachable and must actually
/// carry `output_format`. Needs the GL plugin installed but no GL context —
/// element creation and linking happen in NULL.
fn assert_output_chain_pins_format(gl_download: bool) {
    let manager = build_manager(gl_download);

    for branch in ["dist", "mv"] {
        let name = format!("{}:capsfilter_{}", BLOCK_ID, branch);
        let cf = manager
            .pipeline()
            .by_name(&name)
            .unwrap_or_else(|| panic!("{} exists", name));

        let sink = cf.static_pad("sink").expect("capsfilter sink pad");
        assert!(
            sink.is_linked(),
            "{}:sink never linked — the {} output branch carries no video",
            name,
            branch
        );

        let caps = cf.property::<gstreamer::Caps>("caps");
        let format = caps
            .structure(0)
            .and_then(|s| s.get::<String>("format").ok());
        assert_eq!(
            format.as_deref(),
            Some(OUTPUT_FORMAT),
            "{} does not pin output_format (caps: {})",
            name,
            caps
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_output_format_links_with_gl_download() {
    gstreamer::init().unwrap();
    require_gl_plugin();
    assert_output_chain_pins_format(true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_output_format_links_without_gl_download() {
    gstreamer::init().unwrap();
    require_gl_plugin();
    assert_output_chain_pins_format(false);
}

/// End to end: the requested format has to be *negotiated*, not merely
/// requested. Needs a working GL context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_output_format_is_negotiated_end_to_end() {
    gstreamer::init().unwrap();
    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
        return;
    }

    let mut manager = build_manager(true);
    manager.start().expect("start GPU pipeline");

    // Count buffers reaching the PGM sink: negotiated caps alone would not
    // prove frames survive the conversion.
    let pgm_buffers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let counter = std::sync::Arc::clone(&pgm_buffers);
        let sink = manager.pipeline().by_name("pgmsink").expect("pgmsink");
        let pad = sink.static_pad("sink").expect("pgmsink sink pad");
        pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gstreamer::PadProbeReturn::Ok
        });
    }

    // Software GL on a cold runner needs time for context creation and shader
    // JIT before the first frame.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline
        && pgm_buffers.load(std::sync::atomic::Ordering::Relaxed) < 3
    {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let mut negotiated = Vec::new();
    for branch in ["dist", "mv"] {
        let name = format!("{}:capsfilter_{}", BLOCK_ID, branch);
        let cf = manager.pipeline().by_name(&name).expect("capsfilter");
        let caps = cf
            .static_pad("src")
            .expect("capsfilter src pad")
            .current_caps();
        negotiated.push((branch, caps));
    }

    let frames = pgm_buffers.load(std::sync::atomic::Ordering::Relaxed);
    let _ = manager.stop();

    assert!(
        frames >= 3,
        "PGM output produced {} buffers — no video reached the block's output",
        frames
    );

    for (branch, caps) in negotiated {
        let caps = caps.unwrap_or_else(|| panic!("capsfilter_{} never negotiated", branch));
        let s = caps.structure(0).expect("caps structure");
        assert_eq!(
            s.get::<String>("format").ok().as_deref(),
            Some(OUTPUT_FORMAT),
            "capsfilter_{} negotiated {} instead of {}",
            branch,
            caps,
            OUTPUT_FORMAT
        );
        assert!(
            !caps
                .features(0)
                .map(|f| f.contains("memory:GLMemory"))
                .unwrap_or(false),
            "capsfilter_{} still carries GL memory with gl_download=true: {}",
            branch,
            caps
        );
    }
}
