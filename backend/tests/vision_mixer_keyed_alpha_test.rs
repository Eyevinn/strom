//! Regression test: an alpha-less `output_format` must not destroy the
//! per-pixel alpha of a DSK (downstream keyer) input.
//!
//! Builds a real vision mixer flow through `PipelineManager` on the CPU
//! (software compositor) backend with `output_format=NV12`, feeds DSK 0 a
//! half-transparent RGBA graphic, and asserts the transparent half still
//! shows the program background. Before the fix the builder inserted an
//! NV12 capsfilter between the DSK input and the compositor, which flattened
//! the alpha and painted the graphic's black RGB over the background.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const W: usize = 320;
const H: usize = 180;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Flow: a white program input on video_in_0, and on dsk_in_0 a graphic whose
/// left half is opaque red and whose right half is fully transparent
/// (videobox pads the 160-wide red source out to 320 with border-alpha=0).
fn build_flow(block_id: &str, output_format: &str) -> Flow {
    let mut flow = Flow::new(format!("vm_keyed_alpha_{}", block_id));
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String("cpu".to_string()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(2));
            p.insert("num_dsk_inputs".to_string(), PV::String("1".to_string()));
            p.insert(
                "output_format".to_string(),
                PV::String(output_format.to_string()),
            );
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
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "bgcaps",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw,width={},height={},framerate=30/1",
                W, H
            )),
        )],
    ));
    flow.elements.push(elem(
        "gfx",
        "videotestsrc",
        vec![
            ("pattern", PV::String("red".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "gfxcaps",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw,format=RGBA,width={},height={},framerate=30/1",
                W / 2,
                H
            )),
        )],
    ));
    // Pad the red source out to full width with a fully transparent border.
    flow.elements.push(elem(
        "gfxbox",
        "videobox",
        vec![
            ("right", PV::Int(-((W / 2) as i64))),
            ("border-alpha", PV::Float(0.0)),
        ],
    ));
    flow.elements.push(elem(
        "gfxout",
        "capsfilter",
        vec![(
            "caps",
            PV::String(format!(
                "video/x-raw,format=RGBA,width={},height={},framerate=30/1",
                W, H
            )),
        )],
    ));
    // Measure in RGBA regardless of what the mixer output negotiated: with
    // output_format=Auto the CPU compositor can settle on exotic formats
    // (GBRA_12LE here). Converting after the mixer cannot restore alpha that
    // was already flattened upstream, so it does not mask the bug.
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
    // Same for the multiview, whose overlay (labels, VU meters, tallies) is a
    // keyed RGBA pad covering the whole canvas.
    flow.elements.push(elem("mvconv", "videoconvert", vec![]));
    flow.elements.push(elem(
        "mvcaps",
        "capsfilter",
        vec![("caps", PV::String("video/x-raw,format=RGBA".into()))],
    ));
    flow.elements.push(elem(
        "mvsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));

    // DSK pads are dynamic (num_dsk_inputs), so the flow needs the computed
    // pad set the API normally fills in before validation.
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }

    for (from, to) in [
        ("bg:src".to_string(), "bgcaps:sink".to_string()),
        ("bgcaps:src".to_string(), format!("{}:video_in_0", block_id)),
        ("gfx:src".to_string(), "gfxcaps:sink".to_string()),
        ("gfxcaps:src".to_string(), "gfxbox:sink".to_string()),
        ("gfxbox:src".to_string(), "gfxout:sink".to_string()),
        ("gfxout:src".to_string(), format!("{}:dsk_in_0", block_id)),
        (format!("{}:pgm_out", block_id), "pgmconv:sink".to_string()),
        ("pgmconv:src".to_string(), "pgmcaps:sink".to_string()),
        ("pgmcaps:src".to_string(), "pgmsink:sink".to_string()),
        (
            format!("{}:multiview_out", block_id),
            "mvconv:sink".to_string(),
        ),
        ("mvconv:src".to_string(), "mvcaps:sink".to_string()),
        ("mvcaps:src".to_string(), "mvsink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// Fraction of red and of white pixels in a horizontal band of an RGBA frame.
///
/// Colour fractions rather than mean luma: an all-black startup frame is dark
/// the way an opaque-keyed frame is dark, so a brightness threshold cannot
/// tell "the graphic is up" from "nothing is composited yet".
fn band_colors(sample: &gstreamer::Sample, x0: usize, x1: usize) -> (f64, f64) {
    use gstreamer_video::{VideoFormat, VideoFrameRef, VideoInfo};
    let caps = sample.caps().expect("caps");
    let info = VideoInfo::from_caps(caps).expect("video info from caps");
    assert_eq!(
        info.format(),
        VideoFormat::Rgba,
        "PGM sample should be RGBA"
    );
    let buffer = sample.buffer().expect("buffer");
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let h = info.height() as usize;
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let (mut red, mut white, mut n) = (0u64, 0u64, 0u64);
    for y in (h / 4)..(3 * h / 4) {
        let row = &data[y * stride..];
        for x in x0..x1 {
            let o = x * 4;
            let (r, g, b) = (row[o], row[o + 1], row[o + 2]);
            if r > 150 && g < 80 && b < 80 {
                red += 1;
            } else if r > 200 && g > 200 && b > 200 {
                white += 1;
            }
            n += 1;
        }
    }
    (red as f64 / n as f64, white as f64 / n as f64)
}

/// What one run of the flow observed.
struct Measured {
    /// Fraction of red pixels in the opaque half of the keyed DSK graphic.
    dsk_left_red: f64,
    /// Fraction of white pixels in the transparent half — the program
    /// background must show through there.
    dsk_right_white: f64,
    /// Fraction of near-white pixels in the multiview frame. The multiview
    /// overlay is a full-canvas keyed RGBA pad; if its alpha is dropped the
    /// multiview is the overlay's own black RGB and this goes to ~0.
    mv_bright: f64,
    /// Pixel format the mixer output actually negotiated, i.e. what
    /// `output_format` delivers to whatever consumes PGM.
    pgm_format: String,
}

/// Fraction of pixels in an RGBA frame that are near-white. The multiview is
/// mostly black canvas, so a mean is a poor signal; what matters is whether
/// the white input's thumbnails and preview survive under the overlay.
fn bright_fraction(sample: &gstreamer::Sample) -> f64 {
    use gstreamer_video::{VideoFrameRef, VideoInfo};
    let caps = sample.caps().expect("caps");
    let info = VideoInfo::from_caps(caps).expect("video info from caps");
    let buffer = sample.buffer().expect("buffer");
    let frame = VideoFrameRef::from_buffer_ref_readable(buffer, &info).expect("map frame");
    let (w, h) = (info.width() as usize, info.height() as usize);
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let mut bright = 0u64;
    for y in 0..h {
        let row = &data[y * stride..];
        for x in 0..w {
            let o = x * 4;
            if row[o] > 200 && row[o + 1] > 200 && row[o + 2] > 200 {
                bright += 1;
            }
        }
    }
    bright as f64 / (w * h) as f64
}

fn run_flow(block_id: &str, output_format: &str) -> Measured {
    gstreamer::init().unwrap();
    // The builder picks its videoconvert factory from the detected GPU
    // capabilities; without this the first lookup panics.
    strom::gpu::detect_gpu_capabilities();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    let flow = build_flow(block_id, output_format);
    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    )
    .expect("build CPU pipeline");

    // Count overlay frames so the multiview is only measured once the overlay
    // is actually being composited — before its first push the multiview looks
    // healthy whether or not its alpha survives.
    let overlay_frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let counter = std::sync::Arc::clone(&overlay_frames);
        manager
            .pipeline()
            .by_name(&format!("{}:appsrc_overlay", block_id))
            .expect("overlay appsrc in pipeline")
            .static_pad("src")
            .expect("overlay appsrc src pad")
            .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                gstreamer::PadProbeReturn::Ok
            });
    }

    manager.start().expect("start CPU pipeline");

    // DSK pads are built hidden (alpha=0) — key the graphic in.
    manager
        .set_dsk_enabled(block_id, 0, 2, true)
        .expect("enable DSK 0");

    let sink = |name: &str| {
        manager
            .pipeline()
            .by_name(name)
            .unwrap_or_else(|| panic!("{} in pipeline", name))
            .downcast::<gstreamer_app::AppSink>()
            .expect("appsink type")
    };
    let pgm_sink = sink("pgmsink");
    let mv_sink = sink("mvsink");

    // Pull PGM frames until the keyed graphic is actually on screen (the pad
    // alpha change takes effect a frame or two after set_dsk_enabled), then
    // measure that frame.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let (dsk_left_red, dsk_right_white) = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "DSK graphic never appeared in PGM within 20s"
        );
        let Some(sample) = pgm_sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500))
        else {
            continue;
        };
        let (left_red, _) = band_colors(&sample, W / 8, 3 * W / 8);
        let (_, right_white) = band_colors(&sample, 5 * W / 8, 7 * W / 8);
        // Wait for the graphic's opaque half to actually be red: early frames
        // are still black everywhere, and a keyed-opaque failure is black too.
        if left_red > 0.8 {
            break (left_red, right_white);
        }
    };

    // Wait for the overlay renderer to push (it starts after the appsrc
    // reaches PLAYING), then drop the frames composited before it appeared.
    let mv_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while overlay_frames.load(std::sync::atomic::Ordering::Relaxed) < 3 {
        assert!(
            std::time::Instant::now() < mv_deadline,
            "overlay renderer never pushed a frame within 20s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Watch the multiview until a frame shows picture, keeping the best
    // reading. When the overlay's alpha is flattened, the overlay's own black
    // covers the whole canvas and picture never appears.
    let mut mv_bright: f64 = 0.0;
    let watch_until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < watch_until {
        let Some(sample) = mv_sink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) else {
            continue;
        };
        mv_bright = mv_bright.max(bright_fraction(&sample));
        if mv_bright > 0.05 {
            break;
        }
    }

    // What the mixer output negotiated, read at the block's own output
    // capsfilter rather than at the test's RGBA measurement tap.
    let pgm_format = manager
        .pipeline()
        .by_name(&format!("{}:capsfilter_dist", block_id))
        .expect("capsfilter_dist in pipeline")
        .static_pad("src")
        .expect("capsfilter_dist src pad")
        .current_caps()
        .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()))
        .expect("negotiated PGM format");

    manager.stop().expect("stop");
    drop(manager);
    Measured {
        dsk_left_red,
        dsk_right_white,
        mv_bright,
        pgm_format,
    }
}

/// With an alpha-less `output_format`, a keyed DSK graphic and the multiview
/// overlay must keep their per-pixel alpha — and PGM must still come out in
/// the requested format.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_pads_keep_alpha_with_alpha_less_output_format() {
    let m = run_flow("vmk_nv12", "NV12");
    eprintln!(
        "NV12 output_format: dsk left red {:.3}, dsk right white {:.3}, mv bright {:.3}, \
         pgm format {}",
        m.dsk_left_red, m.dsk_right_white, m.mv_bright, m.pgm_format
    );
    assert_eq!(
        m.pgm_format, "NV12",
        "output_format must still pin the mixer output"
    );
    assert!(
        m.dsk_left_red > 0.8,
        "opaque half of the DSK graphic should be red, {:.3} red",
        m.dsk_left_red
    );
    assert!(
        m.dsk_right_white > 0.8,
        "transparent half of the DSK graphic must show the white background, {:.3} white \
         (alpha flattened before blending)",
        m.dsk_right_white
    );
    assert!(
        m.mv_bright > 0.05,
        "multiview shows no picture ({:.3} of pixels bright) — the overlay's \
         transparent areas were composited opaquely",
        m.mv_bright
    );
}

/// Baseline: the same flow with `output_format=Auto`, which was never broken.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keyed_pads_keep_alpha_with_auto_output_format() {
    let m = run_flow("vmk_auto", "");
    eprintln!(
        "auto output_format: dsk left red {:.3}, dsk right white {:.3}, mv bright {:.3}, \
         pgm format {}",
        m.dsk_left_red, m.dsk_right_white, m.mv_bright, m.pgm_format
    );
    assert!(
        m.dsk_left_red > 0.8,
        "opaque half should be red, {:.3} red",
        m.dsk_left_red
    );
    assert!(
        m.dsk_right_white > 0.8,
        "transparent half must show the white background, {:.3} white",
        m.dsk_right_white
    );
    assert!(
        m.mv_bright > 0.05,
        "multiview shows no picture ({:.3} of pixels bright)",
        m.mv_bright
    );
}
