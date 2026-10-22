//! FX slots that hold no effect must not render.
//!
//! The GPU vision mixer builds a `glshader` FX slot per input on each side of
//! the tee (`fx_look_{i}`, `fx_take_{i}`) and two after PGM (`fx_pgm`,
//! `fx_pgm_take`). A slot holding the identity fragment used to render every
//! frame anyway: a full-frame copy with a pool buffer, an FBO render and a GL
//! sync per slot per frame. Idle slots now run in `GstBaseTransform`
//! passthrough and hand the input buffer on untouched.
//!
//! These tests count, per slot, output buffers that are the input buffer
//! (passthrough) against new buffers (rendered), and check the PGM picture so
//! that a look, a wipe take and clearing them still show on screen.
//!
//! Runs on software GL in CI (see `common::gl_available`).

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::gst::pipeline::PipelineManager;
use strom_types::effects::{EffectTarget, VideoEffect};
use strom_types::Flow;
use strom_types::PropertyValue as PV;

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glshader", "videotestsrc"];

/// Per-slot counters: output buffers that are the input buffer, and output
/// buffers the slot rendered itself.
#[derive(Default)]
struct SlotCounter {
    last_in: AtomicUsize,
    passed: AtomicU64,
    rendered: AtomicU64,
    /// Buffers passed through after the slot had rendered one.
    passed_after_render: AtomicU64,
}

impl SlotCounter {
    fn reset(&self) {
        self.passed.store(0, Ordering::SeqCst);
        self.rendered.store(0, Ordering::SeqCst);
        self.passed_after_render.store(0, Ordering::SeqCst);
    }
    fn get(&self) -> (u64, u64) {
        (
            self.passed.load(Ordering::SeqCst),
            self.rendered.load(Ordering::SeqCst),
        )
    }
}

/// Test-only probes: the sink side records the incoming buffer, the src side
/// classifies the outgoing one. A passthrough slot pushes the same buffer.
fn attach_counter(elem: &gst::Element) -> Arc<SlotCounter> {
    let counter = Arc::new(SlotCounter::default());
    let c = counter.clone();
    elem.static_pad("sink")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                c.last_in.store(b.as_ptr() as usize, Ordering::SeqCst);
            }
            gst::PadProbeReturn::Ok
        });
    let c = counter.clone();
    elem.static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                if b.as_ptr() as usize == c.last_in.load(Ordering::SeqCst) {
                    c.passed.fetch_add(1, Ordering::SeqCst);
                    if c.rendered.load(Ordering::SeqCst) > 0 {
                        c.passed_after_render.fetch_add(1, Ordering::SeqCst);
                    }
                } else {
                    c.rendered.fetch_add(1, Ordering::SeqCst);
                }
            }
            gst::PadProbeReturn::Ok
        });
    counter
}

/// A GPU vision mixer fed by live `videotestsrc` inputs (input 0 red, the
/// rest white), with PGM downloaded into an appsink.
fn build_flow(block_id: &str, num_inputs: usize, width: u32, height: u32) -> Flow {
    let mut flow = Flow::new("vm_fx_passthrough");
    let properties = HashMap::from([
        (
            "compositor_preference".to_string(),
            PV::String("gpu".to_string()),
        ),
        ("num_inputs".to_string(), PV::UInt(num_inputs as u64)),
        (
            "pgm_resolution".to_string(),
            PV::String(format!("{}x{}", width, height)),
        ),
        (
            "multiview_resolution".to_string(),
            PV::String("640x360".to_string()),
        ),
        ("gl_download".to_string(), PV::Bool(true)),
    ]);
    let computed_external_pads = strom::blocks::builtin::get_builder("builtin.vision_mixer")
        .and_then(|builder| builder.get_external_pads(&properties));
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads,
    });
    let elem = |id: &str, ty: &str, props: Vec<(&str, PV)>| strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    };
    for i in 0..num_inputs {
        let pattern = if i == 0 { "red" } else { "white" };
        flow.elements.push(elem(
            &format!("src{}", i),
            "videotestsrc",
            vec![
                ("pattern", PV::String(pattern.into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{}", i),
            "capsfilter",
            vec![(
                "caps",
                PV::String(format!(
                    "video/x-raw,width={},height={},framerate=30/1",
                    width, height
                )),
            )],
        ));
        flow.links.push(strom_types::Link {
            from: format!("src{}:src", i),
            to: format!("caps{}:sink", i),
        });
        flow.links.push(strom_types::Link {
            from: format!("caps{}:src", i),
            to: format!("{}:video_in_{}", block_id, i),
        });
    }
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    flow.links.push(strom_types::Link {
        from: format!("{}:pgm_out", block_id),
        to: "pgmsink:sink".to_string(),
    });
    flow
}

struct Harness {
    manager: PipelineManager,
    block_id: &'static str,
    appsink: gstreamer_app::AppSink,
    slots: Vec<(String, Arc<SlotCounter>)>,
    // The pipeline spawns tokio tasks; the test body itself stays blocking.
    _rt_guard: tokio::runtime::EnterGuard<'static>,
}

impl Harness {
    fn start(block_id: &'static str, num_inputs: usize, width: u32, height: u32) -> Self {
        let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
            tokio::runtime::Runtime::new().expect("tokio runtime"),
        ));
        let rt_guard = rt.enter();
        let flow = build_flow(block_id, num_inputs, width, height);
        let mut manager = common::manager::build(&flow)
            // GL was proven to render: fail, do not skip.
            .expect("GPU vision mixer pipeline builds");

        let mut names: Vec<String> = Vec::new();
        for i in 0..num_inputs {
            names.push(format!("fx_look_{}", i));
            names.push(format!("fx_take_{}", i));
        }
        names.push("fx_pgm".to_string());
        names.push("fx_pgm_take".to_string());
        let slots = names
            .into_iter()
            .map(|n| {
                let e = manager
                    .pipeline()
                    .by_name(&format!("{}:{}", block_id, n))
                    .unwrap_or_else(|| panic!("FX slot {} missing", n));
                let c = attach_counter(&e);
                (n, c)
            })
            .collect();

        manager.start().expect("GPU vision mixer pipeline starts");
        let appsink = manager
            .pipeline()
            .by_name("pgmsink")
            .expect("appsink")
            .downcast::<gstreamer_app::AppSink>()
            .expect("appsink type");
        Harness {
            manager,
            block_id,
            appsink,
            slots,
            _rt_guard: rt_guard,
        }
    }

    fn slot(&self, name: &str) -> &SlotCounter {
        &self
            .slots
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no counter for {}", name))
            .1
    }

    fn reset_counters(&self) {
        for (_, c) in &self.slots {
            c.reset();
        }
    }

    /// Fractions of (red, white, gray) pixels in the next PGM frame.
    fn pull_fractions(&self) -> Option<(f64, f64, f64)> {
        let sample = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(500))?;
        let caps = sample.caps()?;
        let s = caps.structure(0)?;
        let w = s.get::<i32>("width").ok()? as usize;
        let h = s.get::<i32>("height").ok()? as usize;
        let (ri, gi, bi) = match s.get::<&str>("format").ok()? {
            "RGBA" | "RGBx" => (0usize, 1usize, 2usize),
            "BGRA" | "BGRx" => (2, 1, 0),
            other => panic!("unexpected PGM format {}", other),
        };
        let buffer = sample.buffer()?;
        let map = buffer.map_readable().ok()?;
        // Flat colour fields: sampling every 16th pixel keeps the scan short
        // (it runs unoptimised under `cargo test`).
        let (mut red, mut white, mut gray, mut total) = (0u64, 0u64, 0u64, 0u64);
        for px in map.as_chunks::<4>().0.iter().take(w * h).step_by(16) {
            let (r, g, b) = (px[ri] as i32, px[gi] as i32, px[bi] as i32);
            if r > 200 && g > 200 && b > 200 {
                white += 1;
            } else if r > 200 && g < 80 && b < 80 {
                red += 1;
            } else if (r - g).abs() < 16 && (g - b).abs() < 16 && r > 30 && r < 200 {
                gray += 1;
            }
            total += 1;
        }
        let t = total.max(1) as f64;
        Some((red as f64 / t, white as f64 / t, gray as f64 / t))
    }

    /// Pull PGM frames until `pred` holds, or panic after `timeout`.
    fn wait_for_pgm(&self, what: &str, timeout: Duration, pred: impl Fn(f64, f64, f64) -> bool) {
        let deadline = Instant::now() + timeout;
        let mut last = (0.0, 0.0, 0.0);
        while Instant::now() < deadline {
            if let Some(f) = self.pull_fractions() {
                last = f;
                if pred(f.0, f.1, f.2) {
                    return;
                }
            }
        }
        panic!(
            "PGM never showed {} within {:?}; last frame red={:.2} white={:.2} gray={:.2}",
            what, timeout, last.0, last.1, last.2
        );
    }

    /// Assert that no FX slot rendered a frame over `window`, while frames
    /// did flow through every one of them.
    fn assert_all_idle(&self, when: &str, window: Duration) {
        self.reset_counters();
        std::thread::sleep(window);
        for (name, c) in &self.slots {
            let (passed, rendered) = c.get();
            assert_eq!(
                rendered, 0,
                "{}: idle FX slot {} rendered {} frames (passed {}) — an identity slot must run in passthrough",
                when, name, rendered, passed
            );
            assert!(
                passed > 0,
                "{}: no frames flowed through FX slot {}",
                when,
                name
            );
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.manager.stop();
    }
}

fn desaturate() -> VideoEffect {
    VideoEffect::ColorCorrect {
        brightness: 0.0,
        contrast: 1.0,
        saturation: 0.0,
        hue: 0.0,
        gamma: 1.0,
        temperature: 0.0,
        tint: 0.0,
    }
}

/// Idle slots render nothing; a look and a wipe take still show on PGM from
/// the first frames after being set, and the slots go idle again once
/// cleared.
#[test]
fn idle_fx_slots_pass_buffers_through_and_effects_still_apply() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let h = Harness::start("vmfxpt", 2, 640, 360);
    let settle = Duration::from_secs(30);
    // Long enough for frames to reach every slot on software GL in CI.
    let window = Duration::from_secs(2);

    h.wait_for_pgm("the red input 0", settle, |r, _, _| r > 0.9);
    h.assert_all_idle("after start", window);

    // --- LOOK on the PGM input: desaturate turns red into gray. ---
    h.reset_counters();
    h.manager
        .set_vision_mixer_effect(h.block_id, EffectTarget::Input(0), &desaturate())
        .expect("look on input 0");
    h.wait_for_pgm("the input 0 look", Duration::from_secs(5), |r, _, g| {
        r < 0.05 && g > 0.9
    });
    let look = h.slot("fx_look_0");
    assert!(look.get().1 > 0, "fx_look_0 never rendered its look");
    // Buffers that reached the slot before the look was set may pass through
    // untouched; once it has rendered one, it renders every later one.
    let passed_after = look.passed_after_render.load(Ordering::SeqCst);
    assert_eq!(
        passed_after, 0,
        "fx_look_0 passed {} buffers through after it started rendering the look",
        passed_after
    );
    h.manager
        .set_vision_mixer_effect(h.block_id, EffectTarget::Input(0), &VideoEffect::None)
        .expect("clear look on input 0");
    h.wait_for_pgm(
        "input 0 without its look",
        Duration::from_secs(5),
        |r, _, _| r > 0.9,
    );
    h.assert_all_idle("after clearing the input look", window);

    // --- MASTER look: same, on fx_pgm. ---
    h.manager
        .set_vision_mixer_effect(h.block_id, EffectTarget::Master, &desaturate())
        .expect("master look");
    h.wait_for_pgm("the master look", Duration::from_secs(5), |r, _, g| {
        r < 0.05 && g > 0.9
    });
    assert!(h.slot("fx_pgm").get().1 > 0, "fx_pgm never rendered");
    h.manager
        .set_vision_mixer_effect(h.block_id, EffectTarget::Master, &VideoEffect::None)
        .expect("clear master look");
    h.wait_for_pgm(
        "PGM without the master look",
        Duration::from_secs(5),
        |r, _, _| r > 0.9,
    );
    h.assert_all_idle("after clearing the master look", window);

    // --- TAKE: a shader wipe from red (0) to white (1) must animate. ---
    h.reset_counters();
    h.manager
        .trigger_transition(h.block_id, Some(0), Some(1), "wipe_left", 1500)
        .expect("wipe take");
    h.manager
        .update_vision_mixer_after_take(h.block_id, Some(1), Some(0), 2)
        .expect("after wipe take");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_both = false;
    let mut ended = false;
    while Instant::now() < deadline {
        let Some((r, w, _)) = h.pull_fractions() else {
            continue;
        };
        if r > 0.1 && w > 0.1 {
            saw_both = true;
        }
        if saw_both && w > 0.9 && r < 0.05 {
            ended = true;
            break;
        }
    }
    assert!(
        saw_both,
        "the wipe never showed both sources (no animation)"
    );
    assert!(ended, "the wipe never settled on the white input");
    let take_rendered = h.slot("fx_take_0").get().1 + h.slot("fx_take_1").get().1;
    assert!(take_rendered > 0, "no TAKE slot rendered the wipe");

    // The next take resets the TAKE slots to neutral: all idle again.
    h.manager
        .trigger_transition(h.block_id, Some(1), Some(0), "cut", 0)
        .expect("cut back");
    h.manager
        .update_vision_mixer_after_take(h.block_id, Some(0), Some(1), 2)
        .expect("after cut");
    h.wait_for_pgm(
        "the red input after the cut",
        Duration::from_secs(5),
        |r, _, _| r > 0.9,
    );
    h.assert_all_idle("after the take that follows a wipe", window);

    // --- Master-FX take: the envelope renders on fx_pgm_take. ---
    h.reset_counters();
    h.manager
        .trigger_transition(h.block_id, Some(0), Some(1), "glitch_cut", 600)
        .expect("glitch take");
    h.manager
        .update_vision_mixer_after_take(h.block_id, Some(1), Some(0), 2)
        .expect("after glitch take");
    std::thread::sleep(Duration::from_millis(900));
    assert!(
        h.slot("fx_pgm_take").get().1 > 0,
        "fx_pgm_take never rendered the master-FX envelope"
    );
    h.wait_for_pgm(
        "the white input after the glitch take",
        Duration::from_secs(5),
        |_, w, _| w > 0.9,
    );
    // Once the envelope has run out the slot goes back to passthrough by
    // itself, without waiting for the next take. It switches when buffer PTS
    // passes the take's end, and on a loaded runner the pipeline's running
    // time trails the wall clock, so wait for the slot to stop rendering
    // instead of sleeping a fixed time. Without the end-of-take switch it
    // never stops, and this times out.
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let before = h.slot("fx_pgm_take").get().1;
        std::thread::sleep(Duration::from_millis(300));
        if h.slot("fx_pgm_take").get().1 == before {
            break;
        }
        assert!(
            Instant::now() < stop_deadline,
            "fx_pgm_take kept rendering 10 s after the master-FX take ran out"
        );
    }
    h.assert_all_idle("after the master-FX take ran out", window);

    assert_eq!(
        h.manager.pipeline().current_state(),
        gst::State::Playing,
        "pipeline died during FX"
    );
}

/// Measurement aid, not a guard: FX slot renders per second and process CPU
/// for an idle GPU vision mixer. Run with
/// `STROM_FX_MEASURE_INPUTS=6 cargo test --test vision_mixer_fx_passthrough_test -- --ignored --nocapture`.
#[cfg(unix)]
#[test]
#[ignore]
fn measure_idle_fx_slot_renders() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let inputs = std::env::var("STROM_FX_MEASURE_INPUTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6usize);
    let secs = std::env::var("STROM_FX_MEASURE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5u64);
    let h = Harness::start("vmfxmeasure", inputs, 1920, 1080);
    h.wait_for_pgm("the red input 0", Duration::from_secs(30), |r, _, _| {
        r > 0.9
    });
    std::thread::sleep(Duration::from_secs(1));

    let cpu = || {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        t(ru.ru_utime) + t(ru.ru_stime)
    };
    h.reset_counters();
    let (c0, t0) = (cpu(), Instant::now());
    // Keep draining PGM so the appsink is not the bottleneck.
    while t0.elapsed() < Duration::from_secs(secs) {
        let _ = h
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(100));
    }
    let (c1, el) = (cpu(), t0.elapsed().as_secs_f64());
    let mut total_rendered = 0;
    let mut total_passed = 0;
    for (name, c) in &h.slots {
        let (p, r) = c.get();
        total_rendered += r;
        total_passed += p;
        eprintln!(
            "  {:<14} rendered {:>6.1}/s  passed {:>6.1}/s",
            name,
            r as f64 / el,
            p as f64 / el
        );
    }
    eprintln!(
        "MEASURE inputs={} 1920x1080@30 window={:.1}s: FX renders {:.1}/s, passthrough {:.1}/s, process CPU {:.1}% of one core",
        inputs,
        el,
        total_rendered as f64 / el,
        total_passed as f64 / el,
        (c1 - c0) / el * 100.0
    );
}
