//! End-to-end test for the vision mixer shader FX engine.
//!
//! Builds a real vision mixer flow on the GPU (OpenGL) backend, starts it,
//! and exercises the FX surface: FX slot presence, applying looks (input +
//! master), shader wipe takes and master-FX takes. Runs on software GL
//! (llvmpipe) — skips when the environment cannot actually create a GL
//! context (probed like `shader_validation_test`; merely having the GL
//! plugins installed is not enough, as headless CI runners show).

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::effects::{EffectTarget, VideoEffect};
use strom_types::Flow;
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vmfx";

/// Probe whether this environment can actually render through GL: the GL
/// plugins being installed is not enough — on headless CI runners the
/// elements exist but no GL context can be created, and a GPU pipeline
/// builds, starts and then silently never produces a frame. Same probe as
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vision_mixer_fx_engine_end_to_end() {
    gstreamer::init().unwrap();

    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
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
async fn wipe_between_letterboxed_sources_animates() {
    use gstreamer::prelude::*;
    gstreamer::init().unwrap();

    if !gl_environment_available() {
        eprintln!("SKIP: GL environment unavailable (no context or GL elements missing)");
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

    // Fraction of pixels that are white-ish / red-ish in a frame.
    let fractions_of = |sample: &gstreamer::Sample| -> (f64, f64) {
        let caps = sample.caps().expect("caps");
        let s = caps.structure(0).unwrap();
        let w = s.get::<i32>("width").unwrap() as usize;
        let h = s.get::<i32>("height").unwrap() as usize;
        let format = s.get::<&str>("format").unwrap().to_string();
        let buffer = sample.buffer().expect("buffer");
        let map = buffer.map_readable().expect("map");
        // RGBA or BGRx-ish 4-byte formats: identify R/B channel offsets.
        let (ri, gi, bi) = match format.as_str() {
            "RGBA" | "RGBx" => (0usize, 1usize, 2usize),
            "BGRA" | "BGRx" => (2, 1, 0),
            other => panic!("unexpected PGM format {}", other),
        };
        let mut white = 0u64;
        let mut red = 0u64;
        let total = (w * h) as u64;
        for px in map.chunks_exact(4).take(w * h) {
            let (r, g, b) = (px[ri], px[gi], px[bi]);
            if r > 200 && g > 200 && b > 200 {
                white += 1;
            } else if r > 200 && g < 80 && b < 80 {
                red += 1;
            }
        }
        (white as f64 / total as f64, red as f64 / total as f64)
    };

    // Debug aid: verify the source branches are actually linked.
    for name in ["vmfx:queue_0", "vmfx:queue_1"] {
        let q = manager.pipeline().by_name(name).expect(name);
        let linked = q.static_pad("sink").map(|p| p.is_linked()).unwrap_or(false);
        eprintln!("{} sink linked: {}", name, linked);
    }

    // First frame: a cold software-GL CI runner can take many seconds to
    // produce it (GL context creation + llvmpipe shader JIT) — poll
    // generously instead of a single short pull.
    let first = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(s) = appsink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) {
                break s;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no PGM frame within 30s of start"
            );
        }
    };
    let (w0, r0) = fractions_of(&first);
    assert!(w0 > 0.5, "PGM should start mostly white, got {}", w0);
    assert!(r0 < 0.05, "no red expected before take, got {}", r0);

    // Watch a wipe to completion: pull every PGM frame, record whether any
    // frame showed a substantial amount of BOTH sources (the wipe animated
    // rather than hard-switching), and stop once the picture settles on the
    // incoming source. Watching the whole window instead of sampling at
    // fixed wall-clock offsets keeps the test honest on slow runners, where
    // a single "mid-wipe" sample can land after the wipe already finished.
    let observe_wipe = |incoming_is_red: bool| -> (bool, f64, f64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_both = false;
        let mut last = (0.0, 0.0);
        while std::time::Instant::now() < deadline {
            let Some(s) = appsink.try_pull_sample(gstreamer::ClockTime::from_mseconds(500)) else {
                continue;
            };
            let f = fractions_of(&s);
            if f.0 > 0.10 && f.1 > 0.10 {
                saw_both = true;
            }
            last = f;
            let (incoming, outgoing) = if incoming_is_red {
                (f.1, f.0)
            } else {
                (f.0, f.1)
            };
            // Settled on the incoming source after animating — done. (A
            // hard-switch regression never sets saw_both and runs out the
            // deadline, failing the animation assert below.)
            if saw_both && incoming > 0.5 && outgoing < 0.05 {
                break;
            }
        }
        (saw_both, last.0, last.1)
    };

    // --- classic orientation: 2.40:1 -> 2.34:1 (outgoing does not cover) ---
    manager
        .trigger_transition(BLOCK_ID, 0, 1, "wipe_left", 2000)
        .expect("wipe 0->1");
    // Mirror the API handler: persist the PGM/PVW swap after the take —
    // trigger_transition reads the authoritative bus state from overlay
    // state, so without this the next take would re-run the same pair.
    manager
        .update_vision_mixer_after_take(BLOCK_ID, Some(1), Some(0), 2)
        .expect("after take 0->1");
    let (animated, w_end, r_end) = observe_wipe(true);
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
    manager
        .trigger_transition(BLOCK_ID, 1, 0, "wipe_left", 2000)
        .expect("wipe 1->0");
    manager
        .update_vision_mixer_after_take(BLOCK_ID, Some(0), Some(1), 2)
        .expect("after take 1->0");
    let (animated2, w_end2, r_end2) = observe_wipe(false);
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
