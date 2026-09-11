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
use gstreamer_app as gst_app;
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

/// A flow for the keyed-alpha measurement: a white program input, and on
/// `dsk_in_0` a graphic whose left half is opaque red and whose right half is
/// fully transparent. `videobox` pads the half-width red source out to full
/// width with `border-alpha=0`.
fn build_keyed_flow() -> Flow {
    let mut flow = Flow::new("vm_gl_keyed_alpha");
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
            // The measurement tap is a plain videoconvert, which cannot take GL
            // memory, so the block has to hand out system memory.
            p.insert("gl_download".to_string(), PV::Bool(true));
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

    let rgba = |w: u32| {
        PV::String(format!(
            "video/x-raw,format=RGBA,width={},height={},framerate=30/1",
            w, H
        ))
    };
    flow.elements.push(elem(
        "bg",
        "videotestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements
        .push(elem("bgcaps", "capsfilter", vec![("caps", rgba(W))]));
    flow.elements.push(elem(
        "gfx",
        "videotestsrc",
        vec![
            ("pattern", PV::String("red".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements
        .push(elem("gfxcaps", "capsfilter", vec![("caps", rgba(W / 2))]));
    flow.elements.push(elem(
        "gfxbox",
        "videobox",
        vec![
            ("right", PV::Int(-((W / 2) as i64))),
            ("border-alpha", PV::Float(0.0)),
        ],
    ));
    flow.elements
        .push(elem("gfxout", "capsfilter", vec![("caps", rgba(W))]));
    // Measure in RGBA whatever the block negotiated. Converting after the
    // mixer cannot restore alpha already flattened upstream, so it cannot mask
    // the failure.
    flow.elements.push(elem("pgmconv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "pgmcaps",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "mvsink",
        "fakesink",
        vec![("sync", PV::Bool(false)), ("async", PV::Bool(false))],
    ));

    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }

    for (from, to) in [
        ("bg:src".to_string(), "bgcaps:sink".to_string()),
        ("bgcaps:src".to_string(), format!("{}:video_in_0", BLOCK_ID)),
        ("gfx:src".to_string(), "gfxcaps:sink".to_string()),
        ("gfxcaps:src".to_string(), "gfxbox:sink".to_string()),
        ("gfxbox:src".to_string(), "gfxout:sink".to_string()),
        ("gfxout:src".to_string(), format!("{}:dsk_in_0", BLOCK_ID)),
        (format!("{}:pgm_out", BLOCK_ID), "pgmconv:sink".to_string()),
        ("pgmconv:src".to_string(), "pgmcaps:sink".to_string()),
        ("pgmcaps:src".to_string(), "pgmsink:sink".to_string()),
        (
            format!("{}:multiview_out", BLOCK_ID),
            "mvsink:sink".to_string(),
        ),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// Fraction of red and of white pixels in a horizontal band of an RGBA frame.
/// Colour fractions rather than brightness: an all-black startup frame is dark
/// the same way a flattened key is, so a luma threshold cannot tell them apart.
fn band_colors(sample: &gstreamer::Sample, x0: usize, x1: usize) -> (f64, f64) {
    let caps = sample.caps().expect("sample caps");
    let info = gstreamer_video::VideoInfo::from_caps(caps).expect("video info");
    assert_eq!(info.format(), gstreamer_video::VideoFormat::Rgba);
    let buffer = sample.buffer().expect("sample buffer");
    let frame =
        gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let (mut red, mut white, mut total) = (0usize, 0usize, 0usize);
    let h = info.height() as usize;
    for y in (h / 4)..(3 * h / 4) {
        let row = &data[y * stride..];
        for x in x0..x1 {
            let o = x * 4;
            let (r, g, b) = (row[o], row[o + 1], row[o + 2]);
            if r > 150 && g < 100 && b < 100 {
                red += 1;
            }
            if r > 200 && g > 200 && b > 200 {
                white += 1;
            }
            total += 1;
        }
    }
    (red as f64 / total as f64, white as f64 / total as f64)
}

/// A keyed DSK graphic must keep its per-pixel alpha with an alpha-less
/// `output_format`. The GL mixer cannot blend in anything but RGBA, so the
/// format pin cannot reach the blend the way it does on `compositor` — this
/// measures that rather than assuming it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_dsk_alpha_survives_nv12_on_gpu() {
    gstreamer::init().unwrap();
    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
        return;
    }
    strom::gpu::detect_gpu_capabilities();

    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let mut manager = PipelineManager::new(
        &build_keyed_flow(),
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("build GPU pipeline");
    manager.start().expect("start GPU pipeline");

    // DSK pads are built hidden (alpha=0) — key the graphic in.
    manager
        .set_dsk_enabled(BLOCK_ID, 0, 2, true)
        .expect("enable DSK 0");

    let sink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink")
        .downcast::<gst_app::AppSink>()
        .expect("appsink type");

    // Pull until the graphic is actually on screen: the pad alpha change lands
    // a frame or two after set_dsk_enabled, and early frames are still black.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (left_red, right_white) = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "DSK graphic never appeared in PGM within 30s"
        );
        let Some(sample) = sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) else {
            continue;
        };
        let w = W as usize;
        let (left_red, _) = band_colors(&sample, w / 8, 3 * w / 8);
        let (_, right_white) = band_colors(&sample, 5 * w / 8, 7 * w / 8);
        if left_red > 0.8 {
            break (left_red, right_white);
        }
    };

    let _ = manager.stop();

    assert!(
        right_white > 0.8,
        "transparent half of the DSK graphic must show the white background, {:.3} white \
         (alpha flattened before blending); opaque half was {:.3} red",
        right_white,
        left_red
    );
}
