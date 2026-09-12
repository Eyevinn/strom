//! Regression test: a DSK input declared premultiplied composites correctly
//! on both vision mixer backends.
//!
//! `compositor` and `glvideomixerelement` blend straight alpha. A premultiplied
//! source (every `cefsrc` HTML graphic) blended as straight is scaled by its
//! alpha twice: 50 % white over grey 64 comes out 96, where the right answer
//! is 160. Grey matters: against black or white, clamping hides the error.
//!
//! CI has no `cefsrc`, so a `videotestsrc` paints the premultiplied encoding
//! of 50 % white, `(128, 128, 128, 128)`, into the real block.

use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const W: usize = 320;
const H: usize = 180;
const NUM_INPUTS: usize = 2;
/// The DSK pad on the dist mixer: after the video inputs.
const DSK_PAD: &str = "sink_2";

/// Premultiplied 50 % white over grey 64 at pad alpha 1.
const CORRECT_FULL: f64 = 160.0;
/// The same at pad alpha 0.5: 255 * 0.25 + 64 * 0.75.
const CORRECT_HALF: f64 = 112.0;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn raw_caps(format: &str) -> PV {
    PV::String(format!(
        "video/x-raw,format={},width={},height={},framerate=30/1",
        format, W, H
    ))
}

/// What feeds DSK 0.
enum DskSource {
    /// A live `videotestsrc` painting premultiplied 50 % white as BGRA.
    TestPattern,
    /// A media player block looping the clip at this path.
    MediaPlayer(std::path::PathBuf),
}

/// Flow: grey 64 on `video_in_0` (the initial program), the premultiplied
/// source on `dsk_in_0`, and program out converted to RGBA into an appsink.
/// `alpha_mode: None` leaves the property unset, as existing flows have it.
fn build_flow(backend: &str, block_id: &str, alpha_mode: Option<&str>, dsk: DskSource) -> Flow {
    let mut flow = Flow::new(format!("vm_premul_{}", block_id));
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String(backend.to_string()),
            );
            // The measurement tap is a plain videoconvert, which cannot take
            // GL memory.
            p.insert("gl_download".to_string(), PV::Bool(backend == "gpu"));
            p.insert("num_inputs".to_string(), PV::UInt(NUM_INPUTS as u64));
            p.insert("num_dsk_inputs".to_string(), PV::String("1".to_string()));
            if let Some(mode) = alpha_mode {
                p.insert("dsk_0_alpha_mode".to_string(), PV::String(mode.to_string()));
            }
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
            ("pattern", PV::String("solid-color".into())),
            ("foreground-color", PV::UInt(0xff40_4040)),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "bgcaps",
        "capsfilter",
        vec![("caps", raw_caps("BGRA"))],
    ));

    let dsk_out = match dsk {
        DskSource::TestPattern => {
            flow.elements.push(elem(
                "gfx",
                "videotestsrc",
                vec![
                    ("pattern", PV::String("solid-color".into())),
                    // ARGB 0x80808080: every BGRA byte is 128.
                    ("foreground-color", PV::UInt(0x8080_8080)),
                    ("is-live", PV::Bool(true)),
                ],
            ));
            flow.elements.push(elem(
                "gfxcaps",
                "capsfilter",
                vec![("caps", raw_caps("BGRA"))],
            ));
            flow.links.push(strom_types::Link {
                from: "gfx:src".to_string(),
                to: "gfxcaps:sink".to_string(),
            });
            "gfxcaps:src".to_string()
        }
        DskSource::MediaPlayer(path) => {
            flow.blocks.push(strom_types::BlockInstance {
                id: "clip".to_string(),
                block_definition_id: "builtin.media_player".to_string(),
                name: None,
                properties: {
                    let mut p = HashMap::new();
                    p.insert("decode".to_string(), PV::Bool(true));
                    p.insert("loop_playlist".to_string(), PV::Bool(true));
                    p.insert(
                        "playlist".to_string(),
                        PV::String(serde_json::to_string(&[path]).unwrap()),
                    );
                    p
                },
                position: strom_types::block::Position { x: 0.0, y: 0.0 },
                runtime_data: None,
                computed_external_pads: None,
            });
            "clip:video_out".to_string()
        }
    };

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
        (dsk_out, format!("{}:dsk_in_0", block_id)),
        (format!("{}:pgm_out", block_id), "pgmconv:sink".to_string()),
        ("pgmconv:src".to_string(), "pgmcaps:sink".to_string()),
        ("pgmcaps:src".to_string(), "pgmsink:sink".to_string()),
    ] {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

/// Mean red channel over the centre quarter of an RGBA frame. The scene is
/// grey, so red stands for all three.
fn centre_red(sample: &gstreamer::Sample) -> f64 {
    use gstreamer_video::{VideoFormat, VideoFrameRef, VideoInfo};
    let info = VideoInfo::from_caps(sample.caps().expect("caps")).expect("video info");
    assert_eq!(
        info.format(),
        VideoFormat::Rgba,
        "PGM sample should be RGBA"
    );
    let frame = VideoFrameRef::from_buffer_ref_readable(sample.buffer().expect("buffer"), &info)
        .expect("map frame");
    let stride = info.stride()[0] as usize;
    let data = frame.plane_data(0).expect("plane 0");
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (H / 4)..(3 * H / 4) {
        for x in (W / 4)..(3 * W / 4) {
            sum += data[y * stride + x * 4] as u64;
            n += 1;
        }
    }
    sum as f64 / n as f64
}

struct Running {
    manager: PipelineManager,
    block_id: String,
    sink: gstreamer_app::AppSink,
}

impl Running {
    fn start(backend: &str, block_id: &str, alpha_mode: Option<&str>, dsk: DskSource) -> Self {
        gstreamer::init().unwrap();
        // The CPU builder picks its videoconvert factory from the detected
        // GPU capabilities; without this the first lookup panics.
        strom::gpu::detect_gpu_capabilities();

        let temp_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(temp_file.path());
        let flow = build_flow(backend, block_id, alpha_mode, dsk);
        let mut manager = PipelineManager::new(
            &flow,
            EventBroadcaster::new(10),
            &registry,
            vec![],
            "all".to_string(),
            None,
            std::env::temp_dir(),
            std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .unwrap_or_else(|e| panic!("build {} pipeline: {}", backend, e));
        manager
            .start()
            .unwrap_or_else(|e| panic!("start {} pipeline: {}", backend, e));

        let sink = manager
            .pipeline()
            .by_name("pgmsink")
            .expect("pgmsink in pipeline")
            .downcast::<gstreamer_app::AppSink>()
            .expect("appsink type");
        Running {
            manager,
            block_id: block_id.to_string(),
            sink,
        }
    }

    fn mixer_id(&self) -> String {
        format!("{}:mixer", self.block_id)
    }

    /// Pull frames until three in a row read within `tol` of `expected`, or
    /// until the deadline. Returns the last reading either way, so a failure
    /// reports what was actually on air.
    fn settle_on(&self, expected: f64, tol: f64) -> f64 {
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut last, mut streak) = (f64::NAN, 0);
        while Instant::now() < deadline && streak < 3 {
            let Some(sample) = self
                .sink
                .try_pull_sample(gstreamer::ClockTime::from_mseconds(500))
            else {
                continue;
            };
            last = centre_red(&sample);
            streak = if (last - expected).abs() <= tol {
                streak + 1
            } else {
                0
            };
        }
        last
    }

    fn set_pad_alpha(&self, alpha: f64) {
        self.manager
            .update_pad_property(&self.mixer_id(), DSK_PAD, "alpha", &PV::Float(alpha))
            .expect("set DSK pad alpha");
    }

    fn stop(mut self) {
        self.manager.stop().expect("stop");
    }
}

fn assert_near(what: &str, got: f64, expected: f64, tol: f64) {
    assert!(
        (got - expected).abs() <= tol,
        "{what}: got {got:.1}, expected {expected:.0} +/- {tol:.0} \
         (premultiplied blended as straight reads 96 at full alpha, 80 at half)"
    );
}

/// Full and half pad alpha on one running flow, against the correct results
/// `(full, half)` for this source.
fn assert_correct_at_full_and_half_alpha(
    backend: &str,
    block_id: &str,
    dsk: DskSource,
    (full_expected, half_expected): (f64, f64),
) {
    const TOL: f64 = 3.0;
    let run = Running::start(backend, block_id, Some("premultiplied"), dsk);
    run.manager
        .set_dsk_enabled(block_id, 0, NUM_INPUTS, true)
        .expect("enable DSK 0");

    let full = run.settle_on(full_expected, TOL);
    eprintln!("{backend} {block_id}: pad alpha 1.0 -> {full:.1}");
    assert_near("pad alpha 1.0", full, full_expected, TOL);

    // Through the property API, as PATCH on the pad does.
    run.set_pad_alpha(0.5);
    let half = run.settle_on(half_expected, TOL);
    eprintln!("{backend} {block_id}: pad alpha 0.5 -> {half:.1}");
    assert_near("pad alpha 0.5", half, half_expected, TOL);

    run.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_premultiplied_dsk_composites_correctly() {
    assert_correct_at_full_and_half_alpha(
        "cpu",
        "vmp_cpu",
        DskSource::TestPattern,
        (CORRECT_FULL, CORRECT_HALF),
    );
}

/// The declaration is what changes the result: the same source with no alpha
/// mode set still composites as straight. Guards the default, so existing
/// flows with straight graphics are not unpremultiplied behind their back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_default_mode_leaves_the_source_alone() {
    let run = Running::start("cpu", "vmp_cpu_default", None, DskSource::TestPattern);
    run.manager
        .set_dsk_enabled("vmp_cpu_default", 0, NUM_INPUTS, true)
        .expect("enable DSK 0");
    let got = run.settle_on(96.0, 3.0);
    eprintln!("cpu default mode: pad alpha 1.0 -> {got:.1}");
    assert!(
        (got - 96.0).abs() <= 3.0,
        "with no alpha mode the source should blend as-is (96), got {got:.1}"
    );
    run.stop();
}

/// Write a premultiplied 50 % white clip, encoded as A420 so the decoded
/// frames are YUV with alpha and have to be converted before unpremultiplying.
fn write_premultiplied_clip(path: &std::path::Path) -> Result<(), String> {
    for factory in ["avenc_ffv1", "matroskamux"] {
        if gstreamer::ElementFactory::find(factory).is_none() {
            return Err(format!("{factory} missing"));
        }
    }
    let pipeline = gstreamer::parse::launch(&format!(
        "videotestsrc num-buffers=30 pattern=solid-color foreground-color=0x80808080 \
         ! video/x-raw,format=BGRA,width={W},height={H},framerate=30/1 \
         ! videoconvert ! video/x-raw,format=A420 ! avenc_ffv1 ! matroskamux \
         ! filesink location={}",
        path.display()
    ))
    .map_err(|e| e.to_string())?;
    pipeline
        .set_state(gstreamer::State::Playing)
        .map_err(|e| e.to_string())?;
    let bus = pipeline.bus().expect("bus");
    let msg = bus.timed_pop_filtered(
        gstreamer::ClockTime::from_seconds(30),
        &[gstreamer::MessageType::Eos, gstreamer::MessageType::Error],
    );
    let _ = pipeline.set_state(gstreamer::State::Null);
    match msg.as_ref().map(|m| m.view()) {
        Some(gstreamer::MessageView::Eos(_)) => Ok(()),
        other => Err(format!("clip encode did not finish: {other:?}")),
    }
}

/// A premultiplied clip through the media player: the decoder hands out A420,
/// which the unpremultiply element cannot take directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_premultiplied_clip_through_media_player() {
    gstreamer::init().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("premultiplied.mkv");
    // gst-libav and matroska are in CI's package list; failing here rather
    // than skipping keeps the test from passing green without running.
    write_premultiplied_clip(&clip).expect("write premultiplied clip");

    // videoconvert's RGB -> A420 -> RGB round trip turns the clip's 128 into
    // 126, which unpremultiplies to 251: 157.5 at full alpha, 110.4 at half.
    // Blended as straight it would read 95.
    assert_correct_at_full_and_half_alpha(
        "cpu",
        "vmp_clip",
        DskSource::MediaPlayer(clip),
        (157.5, 110.4),
    );
}

/// Probe whether this environment can actually render through GL: the plugins
/// being installed is not enough — on headless runners the elements exist but
/// no context can be created. Same probe as `vision_mixer_fx_test`.
fn gl_environment_available() -> bool {
    gstreamer::init().unwrap();
    if gstreamer::ElementFactory::find("glvideomixerelement").is_none()
        || gstreamer::ElementFactory::find("gltestsrc").is_none()
    {
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

/// GL corrects the blend on the pad rather than converting the source, and
/// the correction depends on a blend constant that has to track pad alpha.
/// Half alpha is the case that catches a constant left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_premultiplied_dsk_composites_correctly() {
    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
        return;
    }
    assert_correct_at_full_and_half_alpha(
        "gpu",
        "vmp_gpu",
        DskSource::TestPattern,
        (CORRECT_FULL, CORRECT_HALF),
    );
}

/// Fade to black drives pad alpha from a control binding, frame by frame,
/// not through `set_property` from Strom code. The blend constant has to
/// follow that too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpu_blend_constant_follows_fade_to_black() {
    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
        return;
    }
    let block_id = "vmp_gpu_ftb";
    let run = Running::start(
        "gpu",
        block_id,
        Some("premultiplied"),
        DskSource::TestPattern,
    );
    run.manager
        .set_dsk_enabled(block_id, 0, NUM_INPUTS, true)
        .expect("enable DSK 0");
    let full = run.settle_on(CORRECT_FULL, 3.0);
    assert_near("pad alpha 1.0 before the fade", full, CORRECT_FULL, 3.0);

    let pad = run
        .manager
        .pipeline()
        .by_name(&run.mixer_id())
        .expect("mixer")
        .sink_pads()
        .into_iter()
        .find(|p| p.name() == DSK_PAD)
        .expect("DSK pad");
    assert_eq!(pad.property::<f64>("blend-constant-color-alpha"), 1.0);

    // During the fade the only writer of `alpha` is the control binding.
    assert!(run.manager.fade_to_black(block_id, 300).expect("FTB on"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while pad.property::<f64>("alpha") > 0.0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        pad.property::<f64>("alpha"),
        0.0,
        "fade never reached black"
    );
    assert_eq!(
        pad.property::<f64>("blend-constant-color-alpha"),
        0.0,
        "blend constant did not follow the animated pad alpha"
    );

    run.stop();
}
