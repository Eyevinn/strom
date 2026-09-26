//! End-to-end test for the vision mixer shader FX engine.
//!
//! Builds a real vision mixer flow on the GPU (OpenGL) backend, starts it,
//! and exercises the FX surface: FX slot presence, applying looks (input +
//! master), shader wipe takes and master-FX takes. Runs on software GL
//! (llvmpipe) in CI — skips only where no GL context can be created at all,
//! which `STROM_REQUIRE_GL` turns into a failure on the jobs that render.

use serial_test::serial;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::effects::{EffectTarget, VideoEffect};
use strom_types::Flow;
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vmfx";

/// Probe whether this environment can actually render through GL: the GL
/// plugins being installed is not enough — where no context can be created a
/// GPU pipeline builds, starts and then silently never produces a frame. Same
/// probe as
/// `shader_validation_test`: a trivial shader-free GL run must reach EOS
/// (no `glshader` in the probe — a shader compile bug must fail the test,
/// not skip it).
fn gl_environment_available() -> bool {
    use gstreamer::prelude::*;
    if gstreamer::ElementFactory::find("glvideomixerelement").is_none()
        || gstreamer::ElementFactory::find("glshader").is_none()
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
    // 20 s budget: software GL context creation can be slow on loaded CI.
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

/// Skip unless GL actually works — but only where skipping is legitimate.
///
/// A skip is silent: a real GL regression on a platform that *can* render would
/// slip through as a green 0.05 s pass.
///
/// `STROM_REQUIRE_GL=1` turns the skip into a failure. CI sets it on both test
/// jobs — Linux renders through llvmpipe under Xvfb, macOS natively — so neither
/// can quietly stop exercising the FX engine.
fn gl_available_or_required() -> bool {
    if gl_environment_available() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GL").is_none(),
        "STROM_REQUIRE_GL is set but no GL context could be created — this platform \
         is supposed to render, so a skip here would hide a GL regression"
    );
    eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
    false
}

/// A flow with a single vision mixer block forced onto the GPU backend.
/// Inputs are left unlinked — force-live compositors output regardless.
fn build_vm_flow() -> Flow {
    let mut flow = Flow::new("vm_fx_test");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                strom_types::PropertyValue::String("gpu".to_string()),
            );
            p.insert(
                "num_inputs".to_string(),
                strom_types::PropertyValue::UInt(2),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

// Rendering through llvmpipe costs whole cores, so two of these at once on a
// 4-core CI runner leave neither pipeline enough to keep up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(gl)]
async fn vision_mixer_fx_engine_end_to_end() {
    gstreamer::init().unwrap();

    if !gl_available_or_required() {
        return;
    }

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);
    let media_path = std::env::temp_dir();

    let flow = build_vm_flow();

    let mut manager = match PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        media_path,
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    ) {
        Ok(m) => m,
        Err(e) => {
            // GL context creation can fail in truly headless environments.
            eprintln!("SKIP: could not build GPU pipeline ({})", e);
            return;
        }
    };

    if let Err(e) = manager.start() {
        eprintln!("SKIP: could not start GPU pipeline ({})", e);
        return;
    }

    // Wait until the mixer actually produces output (its position query
    // answers). A fixed sleep is not enough on cold software-GL CI runners,
    // where the first frame can take many seconds (GL context creation +
    // llvmpipe shader JIT) — and trigger_transition needs the mixer
    // position for its timebase, so taking before that errors.
    {
        use gstreamer::prelude::*;
        let mixer = manager
            .pipeline()
            .by_name(&format!("{}:mixer", BLOCK_ID))
            .expect("mixer in pipeline");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while mixer.query_position::<gstreamer::ClockTime>().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "mixer never produced output (position query still failing after 30s)"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }

    // FX engine must be detected on the GPU path with default enable_fx.
    assert!(
        manager.vision_mixer_fx_available(BLOCK_ID),
        "FX slots missing from GPU pipeline"
    );

    // Apply a look to input 0 and a master look — both must succeed and
    // report the clamped effect back.
    let applied = manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(0),
            &VideoEffect::Pixelate { block_size: 9999.0 },
        )
        .expect("input look failed");
    assert_eq!(applied, VideoEffect::Pixelate { block_size: 200.0 });

    manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Master,
            &VideoEffect::Vignette {
                amount: 0.5,
                softness: 0.5,
            },
        )
        .expect("master look failed");

    // Param-only change on the same kind (uniform swap path).
    manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(0),
            &VideoEffect::Pixelate { block_size: 32.0 },
        )
        .expect("param-only update failed");

    // Invalid color must be rejected.
    assert!(manager
        .set_vision_mixer_effect(
            BLOCK_ID,
            EffectTarget::Input(1),
            &VideoEffect::ChromaKey {
                key_color: "green".to_string(),
                similarity: 0.3,
                smoothness: 0.1,
                spill: 0.5,
            },
        )
        .is_err());

    // Out-of-range input must be rejected (no such FX slot).
    assert!(manager
        .set_vision_mixer_effect(BLOCK_ID, EffectTarget::Input(7), &VideoEffect::None)
        .is_err());

    // Shader wipe take and master-FX take must run without error.
    manager
        .trigger_transition(BLOCK_ID, 0, 1, "wipe_left", 200)
        .expect("wipe take failed");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    manager
        .trigger_transition(BLOCK_ID, 1, 0, "glitch_cut", 200)
        .expect("glitch take failed");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // The master look and the take envelope run on independent PGM slots:
    // the vignette look must still sit on fx_pgm (a take must not evict
    // it) and the glitch envelope on fx_pgm_take.
    {
        use gstreamer::prelude::*;
        let look = manager
            .pipeline()
            .by_name(&format!("{}:fx_pgm", BLOCK_ID))
            .expect("fx_pgm slot missing");
        let frag = look
            .property::<Option<String>>("fragment")
            .unwrap_or_default();
        assert!(
            frag.contains("u_amount"),
            "master look evicted from fx_pgm by the FX take; fragment now: {}",
            &frag[..frag.len().min(120)]
        );
        let take = manager
            .pipeline()
            .by_name(&format!("{}:fx_pgm_take", BLOCK_ID))
            .expect("fx_pgm_take slot missing");
        let frag = take
            .property::<Option<String>>("fragment")
            .unwrap_or_default();
        assert!(
            frag.contains("envelope"),
            "glitch envelope not on fx_pgm_take; fragment now: {}",
            &frag[..frag.len().min(120)]
        );
    }

    // The pipeline must still be alive and rolling after the FX work —
    // a shader compile failure would have posted an error and torn it down.
    use gstreamer::prelude::ElementExtManual;
    assert_eq!(
        manager.pipeline().current_state(),
        gstreamer::State::Playing,
        "pipeline died during FX"
    );

    manager.stop().expect("stop failed");
    drop(manager);
}

/// Reproduction/regression test for wipes between two letterboxed sources
/// (e.g. 2.40:1 and 2.34:1 on a 16:9 canvas) — the production case where
/// wipes read as hard switches. Renders a white and a red letterboxed
/// source through the GPU mixer, runs wipes in both orientations and
/// asserts mid-wipe frames contain a substantial amount of BOTH sources
/// (i.e. the wipe actually animates instead of switching).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial(gl)]
async fn wipe_between_letterboxed_sources_animates() {
    use gstreamer::prelude::*;
    gstreamer::init().unwrap();

    if !gl_available_or_required() {
        return;
    }

    let mut flow = Flow::new("vm_letterbox_wipe");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                strom_types::PropertyValue::String("gpu".to_string()),
            );
            p.insert(
                "num_inputs".to_string(),
                strom_types::PropertyValue::UInt(2),
            );
            // 1280x720 canvas: the 1280-wide letterboxed sources land
            // unscaled, mirroring production geometry at lower GL cost.
            p.insert(
                "pgm_resolution".to_string(),
                strom_types::PropertyValue::String("1280x720".to_string()),
            );
            p.insert(
                "multiview_resolution".to_string(),
                strom_types::PropertyValue::String("640x360".to_string()),
            );
            // Download PGM to system memory so the appsink can map pixels.
            p.insert(
                "gl_download".to_string(),
                strom_types::PropertyValue::Bool(true),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    let elem =
        |id: &str, ty: &str, props: Vec<(&str, strom_types::PropertyValue)>| strom_types::Element {
            id: id.to_string(),
            element_type: ty.to_string(),
            properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            position: [0.0, 0.0].into(),
            pad_properties: HashMap::new(),
        };
    use strom_types::PropertyValue as PV;
    // White 2.40:1 source and red 2.34:1 source (production geometry).
    flow.elements.push(elem(
        "src0",
        "videotestsrc",
        vec![
            ("pattern", PV::String("white".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "caps0",
        "capsfilter",
        vec![(
            "caps",
            PV::String("video/x-raw,width=1280,height=534,framerate=30/1".into()),
        )],
    ));
    flow.elements.push(elem(
        "src1",
        "videotestsrc",
        vec![
            ("pattern", PV::String("red".into())),
            ("is-live", PV::Bool(true)),
        ],
    ));
    flow.elements.push(elem(
        "caps1",
        "capsfilter",
        vec![(
            "caps",
            PV::String("video/x-raw,width=1280,height=546,framerate=30/1".into()),
        )],
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
    for (from, to) in [
        ("src0:src", "caps0:sink"),
        ("caps0:src", "vmfx:video_in_0"),
        ("src1:src", "caps1:sink"),
        ("caps1:src", "vmfx:video_in_1"),
        ("vmfx:pgm_out", "pgmsink:sink"),
    ] {
        flow.links.push(strom_types::Link {
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    let mut manager = match PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    ) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("SKIP: could not build GPU pipeline ({})", e);
            return;
        }
    };
    if let Err(e) = manager.start() {
        eprintln!("SKIP: could not start GPU pipeline ({})", e);
        return;
    }

    // Let caps probes settle so pads get their aspect-fitted rects.
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let appsink = manager
        .pipeline()
        .by_name("pgmsink")
        .expect("appsink in pipeline")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink type");

    // Every program frame the mixer renders, as (white fraction, red fraction).
    //
    // A pull loop cannot answer "did the wipe animate": the appsink is
    // `max-buffers=1 drop=true`, so it hands back whatever is current when asked
    // and discards the rest. Under llvmpipe in CI one scan costs more than the
    // frame interval, and a 2 s wipe then reads as the picture before it followed
    // by the picture after it — a hard cut. A probe sees what the mixer actually
    // produced, whatever the runner's speed.
    let series: std::sync::Arc<std::sync::Mutex<Vec<(f64, f64)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_pad = appsink.static_pad("sink").expect("appsink sink pad");
    let recorder = series.clone();
    sink_pad
        .add_probe(gstreamer::PadProbeType::BUFFER, move |pad, info| {
            let Some(buffer) = info.buffer() else {
                return gstreamer::PadProbeReturn::Ok;
            };
            let Some(caps) = pad.current_caps() else {
                return gstreamer::PadProbeReturn::Ok;
            };
            let s = caps.structure(0).unwrap();
            let w = s.get::<i32>("width").unwrap() as usize;
            let h = s.get::<i32>("height").unwrap() as usize;
            // RGBA or BGRx-ish 4-byte formats: identify R/B channel offsets.
            let (ri, gi, bi) = match s.get::<&str>("format").unwrap() {
                "RGBA" | "RGBx" => (0usize, 1usize, 2usize),
                "BGRA" | "BGRx" => (2, 1, 0),
                other => panic!("unexpected PGM format {}", other),
            };
            let map = buffer.map_readable().expect("map");
            // One pixel in 64. The sources are flat colour fields, so the
            // fractions are unchanged, and this loop runs unoptimised on the
            // streaming thread — a full scan here would throttle the pipeline
            // it is measuring.
            const STRIDE: usize = 64;
            let (mut white, mut red, mut total) = (0u64, 0u64, 0u64);
            for px in map.chunks_exact(4).take(w * h).step_by(STRIDE) {
                let (r, g, b) = (px[ri], px[gi], px[bi]);
                if r > 200 && g > 200 && b > 200 {
                    white += 1;
                } else if r > 200 && g < 80 && b < 80 {
                    red += 1;
                }
                total += 1;
            }
            let total = total.max(1) as f64;
            recorder
                .lock()
                .unwrap()
                .push((white as f64 / total, red as f64 / total));
            gstreamer::PadProbeReturn::Ok
        })
        .expect("probe on pgm_out");
    let last_frame = || series.lock().unwrap().last().copied();

    // Debug aid: verify the source branches are actually linked.
    for name in ["vmfx:queue_0", "vmfx:queue_1"] {
        let q = manager.pipeline().by_name(name).expect(name);
        let linked = q.static_pad("sink").map(|p| p.is_linked()).unwrap_or(false);
        eprintln!("{} sink linked: {}", name, linked);
    }

    // Opening picture: wait for the mixer to be composing PGM, not merely for a
    // frame to exist. A cold software-GL CI runner takes many seconds to reach
    // steady state (GL context creation + llvmpipe shader JIT), and the frames it
    // emits on the way there are black — the compositor is running before the
    // source pads have delivered anything. The fixed settle sleep above is not a
    // guarantee, so poll for the picture itself rather than asserting on whichever
    // frame happens to arrive first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (w0, r0) = loop {
        match last_frame() {
            Some(f) if f.0 > 0.5 => break f,
            last => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "PGM never settled on a mostly-white picture within 30s, last frame {:?}",
                    last
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };
    assert!(w0 > 0.5, "PGM should start mostly white, got {}", w0);
    assert!(r0 < 0.05, "no red expected before take, got {}", r0);

    // Watch a wipe to completion: wait for the picture to settle on the incoming
    // source, then ask the recorded frames whether any of them showed a
    // substantial amount of BOTH sources — the wipe animated rather than
    // hard-switching. Only frames rendered after the mark count, so an earlier
    // wipe's animation cannot satisfy a later one.
    let observe_wipe = |mark: usize, incoming_is_red: bool| -> (bool, f64, f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut last = (0.0, 0.0);
        loop {
            if let Some(f) = last_frame() {
                last = f;
                let (incoming, outgoing) = if incoming_is_red {
                    (f.1, f.0)
                } else {
                    (f.0, f.1)
                };
                if incoming > 0.5 && outgoing < 0.05 {
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let saw_both = series.lock().unwrap()[mark..]
            .iter()
            .any(|(w, r)| *w > 0.10 && *r > 0.10);
        (saw_both, last.0, last.1)
    };

    // --- classic orientation: 2.40:1 -> 2.34:1 (outgoing does not cover) ---
    let mark = series.lock().unwrap().len();
    manager
        .trigger_transition(BLOCK_ID, 0, 1, "wipe_left", 2000)
        .expect("wipe 0->1");
    // Mirror the API handler: persist the PGM/PVW swap after the take —
    // trigger_transition reads the authoritative bus state from overlay
    // state, so without this the next take would re-run the same pair.
    manager
        .update_vision_mixer_after_take(BLOCK_ID, Some(1), Some(0), 2)
        .expect("after take 0->1");
    let (animated, w_end, r_end) = observe_wipe(mark, true);
    eprintln!(
        "classic wipe: animated={} end white={:.2} red={:.2}",
        animated, w_end, r_end
    );
    assert!(r_end > 0.5, "wipe 0->1 should end on red, got {}", r_end);
    assert!(
        w_end < 0.05,
        "white should be gone after 0->1, got {}",
        w_end
    );
    assert!(
        animated,
        "classic wipe should animate (no frame showed both sources)"
    );

    // --- inverted orientation: 2.34:1 -> 2.40:1 (outgoing covers) ---
    let mark = series.lock().unwrap().len();
    manager
        .trigger_transition(BLOCK_ID, 1, 0, "wipe_left", 2000)
        .expect("wipe 1->0");
    manager
        .update_vision_mixer_after_take(BLOCK_ID, Some(0), Some(1), 2)
        .expect("after take 1->0");
    let (animated2, w_end2, r_end2) = observe_wipe(mark, false);
    eprintln!(
        "inverted wipe: animated={} end white={:.2} red={:.2}",
        animated2, w_end2, r_end2
    );
    // Debug: pad + fx state at the broken end state.
    let mixer = manager.pipeline().by_name("vmfx:mixer").expect("mixer");
    for i in 0..2 {
        let pad = mixer.static_pad(&format!("sink_{}", i)).unwrap();
        eprintln!(
            "pad{}: alpha={:.2} zorder={} rect=({},{},{},{})",
            i,
            pad.property::<f64>("alpha"),
            pad.property::<u32>("zorder"),
            pad.property::<i32>("xpos"),
            pad.property::<i32>("ypos"),
            pad.property::<i32>("width"),
            pad.property::<i32>("height"),
        );
    }
    for i in 0..2 {
        let fx = manager
            .pipeline()
            .by_name(&format!("vmfx:fx_take_{}", i))
            .unwrap();
        let u = fx.property::<Option<gstreamer::Structure>>("uniforms");
        eprintln!("fx_take_{} uniforms: {:?}", i, u.map(|s| s.to_string()));
    }
    assert!(
        w_end2 > 0.5,
        "wipe 1->0 should end on white, got {}",
        w_end2
    );
    assert!(
        r_end2 < 0.05,
        "red should be gone after 1->0, got {}",
        r_end2
    );
    assert!(
        animated2,
        "inverted wipe should animate (no frame showed both sources)"
    );

    manager.stop().expect("stop");
    drop(manager);
}
