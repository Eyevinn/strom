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
/// The mixer's latency in the test flow, and how late every input arrives
/// within it.
const MIXER_LATENCY_MS: u64 = 300;
const INPUT_DELAY_MS: u64 = 200;
/// The PiP background (input 1) is blue; every other input is black.
const BG_INPUT: usize = 1;

/// The underlay tests share the machine's GL and CPU; run them one at a
/// time so neither starves the other on a small CI runner.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            // Real mixer latency: a border whose frame only arrives after a
            // take would be missing from PGM for this long.
            p.insert("latency".to_string(), PV::UInt(MIXER_LATENCY_MS));
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
                (
                    "pattern",
                    PV::String(if i == BG_INPUT { "blue" } else { "black" }.into()),
                ),
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
        // Every input arrives INPUT_DELAY_MS after its timestamp, like an
        // SRT or WHIP input with real latency: the mixer then composes each
        // output frame that long after its time.
        flow.elements.push(elem(
            &format!("delay{i}"),
            "queue",
            vec![("min-threshold-time", PV::UInt(INPUT_DELAY_MS * 1_000_000))],
        ));
        flow.links.push(strom_types::Link {
            from: format!("caps{i}:src"),
            to: format!("delay{i}:sink"),
        });
        flow.links.push(strom_types::Link {
            from: format!("delay{i}:src"),
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

fn is_blue((r, g, b): (u8, u8, u8)) -> bool {
    b > 200 && r < 60 && g < 60
}

/// RGB near the top-left corner of PGM: the PiP background when a PiP is
/// on PGM, far from the zone and its border.
fn corner_rgb(sample: &gstreamer::Sample) -> (u8, u8, u8) {
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
    let o = (20 * w + 20) * 4;
    (map[o + ri], map[o + 1], map[o + bi])
}

fn is_black((r, g, b): (u8, u8, u8)) -> bool {
    r < 40 && g < 40 && b < 40
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

/// The two mixers' sink pads fed by an underlay source.
fn underlay_pads(manager: &PipelineManager, block_id: &str) -> Vec<gstreamer::Pad> {
    let prefix = format!("{block_id}:underlay_");
    ["mixer", "mv_comp"]
        .iter()
        .flat_map(|name| {
            manager
                .pipeline()
                .by_name(&format!("{block_id}:{name}"))
                .unwrap_or_else(|| panic!("{name} in pipeline"))
                .sink_pads()
        })
        .filter(|pad| {
            pad.peer()
                .and_then(|p| p.parent_element())
                .is_some_and(|e| e.name().starts_with(&prefix))
        })
        .collect()
}

/// Underlay pads holding a frame. The mixers prepare (map) every pad that
/// holds one on every output frame — `glmixer` maps it for GL even when the
/// pad is fully transparent — and skip a pad that holds none.
fn underlay_pads_holding_a_frame(pads: &[gstreamer::Pad]) -> usize {
    pads.iter()
        .filter(|pad| {
            // SAFETY: every pad here is a sink pad of a videoaggregator
            // (glvideomixerelement or compositor), so a
            // GstVideoAggregatorPad; the call only reads it under its lock.
            unsafe {
                gstreamer_video::ffi::gst_video_aggregator_pad_has_current_buffer(
                    pad.as_ptr() as *mut gstreamer_video::ffi::GstVideoAggregatorPad
                ) != 0
            }
        })
        .count()
}

/// Pull PGM frames for `window` and return the underlay pads holding a frame
/// per second, summed over the PGM frames seen: the underlay frame maps per
/// second on the PGM mixer, and on the multiview mixer at the same rate.
fn underlay_maps_per_sec(
    appsink: &gstreamer_app::AppSink,
    pads: &[gstreamer::Pad],
    window: Duration,
) -> f64 {
    let t0 = Instant::now();
    let mut held = 0usize;
    while t0.elapsed() < window {
        pull(appsink);
        held += underlay_pads_holding_a_frame(pads);
    }
    held as f64 / t0.elapsed().as_secs_f64()
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
    run("vmunderlay_gpu", "gpu", None);
}

/// The same flow with the PGM mixer rendering below real time (held about
/// 60 ms per output frame, as a loaded or software-GL host does): it falls
/// further behind the clock every second. A border configured while it is
/// behind must still be on the cut frame that reveals it, and a colour change
/// must still reach PGM. A restarted underlay frame stamped with the clock's
/// running time lies in such a mixer's future and shows up seconds late.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn borders_keep_up_with_a_mixer_behind_the_clock_gpu() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    run(
        "vmunderlay_gpu_slow",
        "gpu",
        Some(Duration::from_millis(60)),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_underlays_are_not_repushed_cpu() {
    common::require_elements(&["compositor", "videotestsrc"]);
    // The CPU mixer's converters ask for the detected GPU mode, which
    // panics if nothing has probed for it — `main` does this at startup.
    strom::gpu::detect_gpu_capabilities();
    run("vmunderlay_cpu", "cpu", None);
}

fn run(block_id: &str, backend: &str, mixer_frame_delay: Option<Duration>) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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

    // Test-only: hold every PGM output frame to put the mixer behind the clock.
    if let Some(delay) = mixer_frame_delay {
        manager
            .pipeline()
            .by_name(&format!("{block_id}:mixer"))
            .expect("mixer in pipeline")
            .static_pad("src")
            .expect("mixer src pad")
            .add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
                std::thread::sleep(delay);
                gstreamer::PadProbeReturn::Ok
            });
    }
    manager.start().expect("start GPU vision mixer pipeline");
    let appsink = manager
        .pipeline()
        .by_name("pgmsink")
        .unwrap()
        .downcast::<gstreamer_app::AppSink>()
        .unwrap();
    pull(&appsink);
    let pads = underlay_pads(&manager, block_id);
    assert_eq!(pads.len(), expected, "mixer pads fed by underlays");

    // No border anywhere: no underlay pad should hold a frame. Each one does
    // hold its single start-up frame until the mixer's output has passed the
    // frame's end (200 ms of running time). A mixer that renders slower than
    // real time (software or virtualized GL on a CI runner) takes seconds of
    // wall-clock time to get there, so wait for it. A pad that keeps its frame
    // never gets there and fails below.
    let settle_deadline = Instant::now() + Duration::from_secs(10);
    while underlay_pads_holding_a_frame(&pads) > 0 && Instant::now() < settle_deadline {
        pull(&appsink);
    }
    let maps_off = underlay_maps_per_sec(&appsink, &pads, Duration::from_secs(1));
    eprintln!("{backend}: borders off: underlay frame maps {maps_off:.0}/s");

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
    // The zone's PGM border is configured but hidden: its pad holds a
    // frame and the configuration gave it the zone's color.
    std::thread::sleep(Duration::from_millis(1000));
    manager
        .trigger_transition(block_id, 0, 0, "cut", 0)
        .expect("take the PiP");
    // The first PGM frame showing the PiP (blue background) already shows
    // the border. Had the border's frame only been pushed on the cut, it
    // would be missing for the mixer's latency.
    let deadline = Instant::now() + Duration::from_secs(10);
    let cut_frame = loop {
        let sample = pull(&appsink);
        if is_blue(corner_rgb(&sample)) {
            break sample;
        }
        assert!(Instant::now() < deadline, "the cut never reached PGM");
    };
    let rgb = border_rgb(&cut_frame);
    eprintln!("{backend}: border on the cut frame: {rgb:?}");
    assert!(
        is_red(rgb),
        "the configured border is missing from the cut frame: {rgb:?}"
    );
    std::thread::sleep(Duration::from_millis(500));

    // Steady state: no border changes.
    const WINDOW: Duration = Duration::from_secs(3);
    let uploads_before = uploads.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let mut frames = 0u32;
    let mut last = None;
    while t0.elapsed() < WINDOW {
        last = Some(pull(&appsink));
        frames += 1;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let window_uploads = uploads.load(Ordering::Relaxed) - uploads_before;
    let held_on = underlay_pads_holding_a_frame(&pads);
    let maps_on = underlay_maps_per_sec(&appsink, &pads, Duration::from_secs(1));
    eprintln!(
        "{backend}: one bordered zone on PGM: {held_on} underlay pads hold a frame, underlay frame maps {maps_on:.0}/s"
    );
    eprintln!(
        "{backend}: steady state over {:.2}s with {} underlays: underlay frames {} ({:.1}/s), PGM frames {} ({:.1}/s), threads {:?} (before build {:?})",
        elapsed,
        underlays,
        window_uploads,
        window_uploads as f64 / elapsed,
        frames,
        frames as f64 / elapsed,
        thread_count(),
        threads_before_build,
    );

    // The border is still on screen.
    let rgb = border_rgb(&last.expect("PGM frames in the window"));
    assert!(is_red(rgb), "zone border lost: {rgb:?}");
    // With no zone configured no underlay pad holds a frame, so the mixers
    // never map one.
    assert_eq!(
        maps_off, 0.0,
        "underlay pads hold a frame with no zone configured"
    );
    // One bordered source: its border on PGM, on the PVW big display and on
    // its multiview PiP tile — the only underlays with a configured border.
    assert_eq!(
        held_on, 3,
        "{held_on} underlay pads hold a frame with one bordered source"
    );
    // Frames keep coming (a loose bound: CI GL is slow, and slower still with
    // the mixer held on purpose).
    let min_fps = if mixer_frame_delay.is_some() {
        3.0
    } else {
        10.0
    };
    assert!(
        frames as f64 / elapsed > min_fps,
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
    let frames_until_back = wait_for_border(&appsink, "the green zone border back", is_green);
    eprintln!("{backend}: border back after a 300 ms fade: {frames_until_back} PGM frame(s)");

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
    // Cyan: the restarted source pushes its frame and is held.
    pip_config("#00FFFF");
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
