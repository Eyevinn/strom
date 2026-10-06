//! Regression test: a zone-border underlay whose colour does not change must
//! not be uploaded to the GPU over and over, while a border colour change
//! still reaches PGM.
//!
//! Each underlay pad is fed by a 16x16 solid-colour `videotestsrc`. It used
//! to stream at 5 fps for the life of the flow, so every underlay cost five
//! GL uploads a second on the single GL thread even though its colour only
//! changes on a border edit. The source now pushes one frame, the mixer pad
//! repeats it, and a colour change pushes one new frame.
//!
//! Builds a real vision mixer flow through `PipelineManager` on both
//! backends, puts a bordered zone on PGM, counts the frames every underlay
//! source pushes (on the GPU backend each is a `glupload`), and reads the
//! border colour on PGM through a colour change and takes away and back.

pub mod common;

use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::vision_mixer::{NormRect, PipTransforms, Zone, ZoneBorder};
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glshader", "gltestsrc"];
const NUM_INPUTS: usize = 4;
const NUM_PIPS: usize = 2;
const PGM_W: usize = 1280;
const PGM_H: usize = 720;
/// The zone sits in the middle quarter of PGM; its border is this many PGM
/// pixels wide, drawn outward.
const BORDER_W: f32 = 32.0;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn build_flow(block_id: &str, backend: &str) -> Flow {
    let mut flow = Flow::new("vm_underlay_upload");
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String(backend.into()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(NUM_INPUTS as u64));
            p.insert("num_pips".to_string(), PV::String(NUM_PIPS.to_string()));
            p.insert(
                "pgm_resolution".to_string(),
                PV::String(format!("{PGM_W}x{PGM_H}")),
            );
            p.insert(
                "multiview_resolution".to_string(),
                PV::String("640x360".into()),
            );
            // Download PGM so the appsink can map pixels.
            p.insert("gl_download".to_string(), PV::Bool(true));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let caps = "video/x-raw,width=640,height=360,framerate=30/1";
    for i in 0..NUM_INPUTS {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![
                ("pattern", PV::String("black".into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![("caps", PV::String(caps.into()))],
        ));
        flow.links.push(strom_types::Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(strom_types::Link {
            from: format!("caps{i}:src"),
            to: format!("{block_id}:video_in_{i}"),
        });
    }
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
    flow.elements
        .push(elem("mvsink", "fakesink", vec![("sync", PV::Bool(false))]));
    flow.links.push(strom_types::Link {
        from: format!("{block_id}:pgm_out"),
        to: "pgmcaps:sink".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: "pgmcaps:src".to_string(),
        to: "pgmsink:sink".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: format!("{block_id}:multiview_out"),
        to: "mvsink:sink".to_string(),
    });
    flow
}

/// PiP 0: black background (input 1), input 0 in a bordered zone in the
/// middle quarter of the canvas.
fn bordered_zone(color: &str) -> Vec<Zone> {
    vec![Zone {
        rect: Some(NormRect {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        }),
        capacity: None,
        sources: vec![0],
        border: Some(ZoneBorder {
            color: color.to_string(),
            width: BORDER_W,
        }),
    }]
}

/// RGB in the middle of the zone's left border on PGM.
fn border_rgb(sample: &gstreamer::Sample) -> (u8, u8, u8) {
    let caps = sample.caps().expect("caps");
    let s = caps.structure(0).unwrap();
    let w = s.get::<i32>("width").unwrap() as usize;
    let format = s.get::<&str>("format").unwrap().to_string();
    let (ri, bi) = match format.as_str() {
        "RGBA" | "RGBx" => (0, 2),
        "BGRA" | "BGRx" => (2, 0),
        other => panic!("unexpected PGM format {other}"),
    };
    let buffer = sample.buffer().expect("buffer");
    let map = buffer.map_readable().expect("map");
    let x = PGM_W / 4 - (BORDER_W / 2.0) as usize;
    let y = PGM_H / 2;
    let o = (y * w + x) * 4;
    (map[o + ri], map[o + 1], map[o + bi])
}

fn is_red((r, g, b): (u8, u8, u8)) -> bool {
    r > 200 && g < 60 && b < 60
}

fn is_green((r, g, b): (u8, u8, u8)) -> bool {
    g > 200 && r < 60 && b < 60
}

fn is_yellow((r, g, b): (u8, u8, u8)) -> bool {
    r > 200 && g > 200 && b < 60
}

fn is_black((r, g, b): (u8, u8, u8)) -> bool {
    r < 40 && g < 40 && b < 40
}

/// Process CPU time (user + system).
fn cpu_time() -> Duration {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage writes a full rusage into the pointer it is given.
    let ru = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr());
        ru.assume_init()
    };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Threads in this process, for the log only.
fn thread_count() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task").ok().map(|d| d.count())
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .count()
                .saturating_sub(1),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn pull(appsink: &gstreamer_app::AppSink) -> gstreamer::Sample {
    appsink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
        .expect("PGM frame within 5 s")
}

/// Pull PGM frames until the border satisfies `done`; returns how many
/// frames that took.
fn wait_for_border(
    appsink: &gstreamer_app::AppSink,
    what: &str,
    done: impl Fn((u8, u8, u8)) -> bool,
) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut frames = 0;
    loop {
        let rgb = border_rgb(&pull(appsink));
        frames += 1;
        if done(rgb) {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "PGM never showed {what}, border is {rgb:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_underlays_are_not_reuploaded_gpu() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    run("vmunderlay_gpu", "gpu");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_underlays_are_not_repushed_cpu() {
    common::require_elements(&["compositor", "videotestsrc"]);
    // The CPU mixer's converters ask for the detected GPU mode, which
    // panics if nothing has probed for it — `main` does this at startup.
    strom::gpu::detect_gpu_capabilities();
    run("vmunderlay_cpu", "cpu");
}

fn run(block_id: &str, backend: &str) {
    let main_loop = gstreamer::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };
    let registry_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let threads_before_build = thread_count();
    let mut manager = PipelineManager::new(
        &build_flow(block_id, backend),
        EventBroadcaster::with_capacity(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("build GPU vision mixer pipeline");

    // Count the frames every underlay source pushes (into its glupload on
    // the GPU backend).
    let uploads = Arc::new(AtomicU64::new(0));
    let mut underlays = 0;
    let prefix = format!("{block_id}:underlay_");
    for el in manager.pipeline().iterate_recurse().into_iter().flatten() {
        let name = el.name();
        if name.starts_with(&prefix) && name.ends_with("_caps") {
            underlays += 1;
            let uploads = Arc::clone(&uploads);
            el.static_pad("src").unwrap().add_probe(
                gstreamer::PadProbeType::BUFFER,
                move |_, _| {
                    uploads.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                },
            );
        }
    }
    let expected = NUM_INPUTS * (2 + NUM_PIPS);
    assert_eq!(underlays, expected, "underlay sources in the pipeline");

    manager.start().expect("start GPU vision mixer pipeline");
    let appsink = manager
        .pipeline()
        .by_name("pgmsink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pull(&appsink);

    // Bordered zone on PGM.
    manager
        .apply_vision_mixer_pip_config(
            block_id,
            0,
            Some(1),
            bordered_zone("#FF0000"),
            PipTransforms::new(),
        )
        .expect("PiP config");
    manager
        .select_vision_mixer_pip_for_preview(block_id, 0)
        .expect("PiP to PVW");
    manager
        .trigger_transition(block_id, 0, 0, "cut", 0)
        .expect("take the PiP");
    wait_for_border(&appsink, "a red zone border", is_red);
    std::thread::sleep(Duration::from_millis(500));

    // Steady state: no border changes.
    const WINDOW: Duration = Duration::from_secs(3);
    let uploads_before = uploads.load(Ordering::Relaxed);
    let cpu_before = cpu_time();
    let t0 = Instant::now();
    let mut frames = 0u32;
    let mut last = None;
    while t0.elapsed() < WINDOW {
        last = Some(pull(&appsink));
        frames += 1;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let cpu = (cpu_time() - cpu_before).as_secs_f64();
    let window_uploads = uploads.load(Ordering::Relaxed) - uploads_before;
    eprintln!(
        "{backend}: steady state over {:.2}s with {} underlays: underlay frames {} ({:.1}/s), PGM frames {} ({:.1}/s), process CPU {:.0}%, threads {:?} (before build {:?})",
        elapsed,
        underlays,
        window_uploads,
        window_uploads as f64 / elapsed,
        frames,
        frames as f64 / elapsed,
        cpu / elapsed * 100.0,
        thread_count(),
        threads_before_build,
    );

    // The border is still on screen.
    let rgb = border_rgb(&last.expect("PGM frames in the window"));
    assert!(is_red(rgb), "zone border lost: {rgb:?}");
    assert!(
        frames as f64 / elapsed > 20.0,
        "PGM stalled: {frames} frames in {elapsed:.2}s"
    );
    // An unchanged underlay is not uploaded again. Streaming at 5 fps
    // would be 5 per underlay per second here.
    assert_eq!(
        window_uploads, 0,
        "static underlays pushed {window_uploads} frames in {elapsed:.2}s"
    );

    // A border colour change still reaches PGM.
    let changed_at = Instant::now();
    manager
        .apply_vision_mixer_pip_config(
            block_id,
            0,
            Some(1),
            bordered_zone("#00FF00"),
            PipTransforms::new(),
        )
        .expect("PiP config");
    let frames_until_green = wait_for_border(&appsink, "a green zone border", is_green);
    eprintln!(
        "{backend}: border colour change visible after {} PGM frame(s), {} ms",
        frames_until_green,
        changed_at.elapsed().as_millis()
    );

    // Take away from the PiP (the border goes) and fade back to it (the
    // border returns in its new colour).
    manager
        .trigger_transition(block_id, 0, 0, "cut", 0)
        .expect("take the input");
    wait_for_border(&appsink, "the zone border gone", is_black);
    manager
        .trigger_transition(block_id, 0, 0, "fade", 300)
        .expect("take the PiP back");
    wait_for_border(&appsink, "the green zone border back", is_green);

    // A colour change must not block its caller while the underlay source
    // is stuck pushing downstream (here: held by a blocking probe, as it
    // can be inside the mixer's sink pad). Stopping that source waits for
    // its streaming thread, so a synchronous restart would hang the API
    // call until the source is released.
    let held_pad = manager
        .pipeline()
        .by_name(&format!("{block_id}:underlay_dist_0_caps"))
        .expect("PGM underlay of input 0")
        .static_pad("src")
        .unwrap();
    let held = Arc::new(AtomicBool::new(false));
    let probe = {
        let held = Arc::clone(&held);
        held_pad
            .add_probe(
                gstreamer::PadProbeType::BLOCK | gstreamer::PadProbeType::BUFFER,
                move |_, _| {
                    held.store(true, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                },
            )
            .expect("blocking probe")
    };
    let pip_config = |color: &str| {
        manager
            .apply_vision_mixer_pip_config(
                block_id,
                0,
                Some(1),
                bordered_zone(color),
                PipTransforms::new(),
            )
            .expect("PiP config");
    };
    // Blue: the restarted source pushes its frame and is held.
    pip_config("#0000FF");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !held.load(Ordering::Relaxed) {
        assert!(
            Instant::now() < deadline,
            "the underlay frame was never held"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Two more changes while it is held: both must return at once, and the
    // last one wins.
    let returned_in = std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let pip_config = &pip_config;
        scope.spawn(move || {
            let t0 = Instant::now();
            pip_config("#FF00FF");
            pip_config("#FFFF00");
            let _ = tx.send(t0.elapsed());
        });
        let returned = rx.recv_timeout(Duration::from_secs(3)).ok();
        // Release the source whatever happened, so a hung call can finish
        // and the scope can join.
        held_pad.remove_probe(probe);
        returned
    });
    let returned_in = returned_in.expect("a colour change blocked on a held underlay source");
    eprintln!(
        "{backend}: two colour changes with the source held returned in {} ms",
        returned_in.as_millis()
    );
    assert!(
        returned_in < Duration::from_secs(1),
        "colour changes took {returned_in:?} with the source held"
    );
    wait_for_border(&appsink, "the latest (yellow) zone border", is_yellow);

    manager.stop().expect("stop");
    drop(manager);
    main_loop.quit();
    main_loop_thread.join().expect("main loop thread");
}
