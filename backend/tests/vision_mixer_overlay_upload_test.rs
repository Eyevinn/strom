//! Regression tests for the multiview overlay's GPU uploads.
//!
//! The overlay timer runs at the multiview framerate. It used to re-push the
//! last overlay frame on every tick, so `glupload_overlay` uploaded an
//! identical full-canvas RGBA frame 30 times a second on the single GL
//! thread. An unchanged overlay now only tells the mixer to keep the frame it
//! has, so uploads follow real overlay changes. VU meters with live audio
//! change on almost every tick (each input's `level` posts on its own
//! phase), so meter and clock redraws are capped at 4 a second; source and
//! tally changes (a cut) still redraw at once.
//!
//! Builds real GPU vision mixer flows through `PipelineManager`, counts the
//! buffers entering `glupload_overlay`, and reads the multiview output.

pub mod common;

use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::vision_mixer::overlay;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glshader", "gltestsrc"];
const MV_W: i32 = 1280;
const MV_H: i32 = 720;
/// Steady-state measurement window.
const WINDOW: Duration = Duration::from_secs(3);

/// The tests share the machine's GL and CPU; run them one at a time so
/// neither starves the other on a small CI runner.
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

fn link(flow: &mut Flow, from: String, to: String) {
    flow.links.push(strom_types::Link { from, to });
}

/// A GPU vision mixer with `num_inputs` black live video inputs, input 0
/// on PGM and input 1 on PVW. With `live_audio`, every input and PGM audio
/// get live ticks, so the VU meters change all the time.
fn build_flow(block_id: &str, num_inputs: usize, live_audio: bool) -> Flow {
    let mut flow = Flow::new("vm_overlay_upload");
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String("gpu".into()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(num_inputs as u64));
            p.insert("pgm_resolution".to_string(), PV::String("640x360".into()));
            p.insert(
                "multiview_resolution".to_string(),
                PV::String(format!("{MV_W}x{MV_H}")),
            );
            p.insert("initial_pgm_input".to_string(), PV::UInt(0));
            p.insert("initial_pvw_input".to_string(), PV::UInt(1));
            p.insert("show_vu_meters".to_string(), PV::Bool(true));
            // Download so the appsinks can map pixels.
            p.insert("gl_download".to_string(), PV::Bool(true));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let caps = "video/x-raw,width=320,height=180,framerate=30/1";
    for i in 0..num_inputs {
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
        link(&mut flow, format!("src{i}:src"), format!("caps{i}:sink"));
        link(
            &mut flow,
            format!("caps{i}:src"),
            format!("{block_id}:video_in_{i}"),
        );
    }
    if live_audio {
        let audio_pads = (0..num_inputs)
            .map(|i| format!("audio_in_{i}"))
            .chain(std::iter::once("pgm_audio_in".to_string()));
        for (i, pad) in audio_pads.enumerate() {
            flow.elements.push(elem(
                &format!("asrc{i}"),
                "audiotestsrc",
                vec![
                    // Quiet ticks with a loud marker tick about every
                    // 300 ms, each source on its own period: a meter
                    // window either catches a marker or not, so levels
                    // change all the time.
                    ("wave", PV::String("ticks".into())),
                    (
                        "tick-interval",
                        PV::UInt(70_000_000 + 10_000_000 * i as u64),
                    ),
                    ("volume", PV::Float(0.1)),
                    ("marker-tick-period", PV::UInt(4)),
                    ("marker-tick-volume", PV::Float(1.0)),
                    // Different buffer sizes put each source's `level`
                    // messages on a different phase, as with real inputs.
                    ("samplesperbuffer", PV::Int(1024 + 1500 * i as i64)),
                    ("is-live", PV::Bool(true)),
                ],
            ));
            link(
                &mut flow,
                format!("asrc{i}:src"),
                format!("{block_id}:{pad}"),
            );
        }
    }
    flow.elements.push(elem(
        "mvsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    flow.elements
        .push(elem("pgmsink", "fakesink", vec![("sync", PV::Bool(false))]));
    link(
        &mut flow,
        format!("{block_id}:multiview_out"),
        "mvsink:sink".to_string(),
    );
    link(
        &mut flow,
        format!("{block_id}:pgm_out"),
        "pgmsink:sink".to_string(),
    );
    flow
}

/// Tally colour of a multiview thumbnail border.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Tally {
    Pgm,
    Pvw,
    Other,
}

/// Read the tally colour on the top edge of input `i`'s thumbnail slot.
fn thumbnail_tally(sample: &gstreamer::Sample, block_id: &str, i: usize) -> Tally {
    let state = overlay::get_overlay_state(block_id).expect("overlay state registered");
    let r = state.layout.thumbnail_slot_rects[i];
    let caps = sample.caps().expect("caps");
    let s = caps.structure(0).unwrap();
    let w = s.get::<i32>("width").unwrap() as usize;
    let format = s.get::<&str>("format").unwrap().to_string();
    let (ri, bi) = match format.as_str() {
        "RGBA" | "RGBx" => (0, 2),
        "BGRA" | "BGRx" => (2, 0),
        other => panic!("unexpected multiview format {other}"),
    };
    let buffer = sample.buffer().expect("buffer");
    let map = buffer.map_readable().expect("map");
    let x = (r.x + r.w / 2.0) as usize;
    let y0 = r.y.round() as usize;
    // The stroke straddles the rect edge; look at a few rows around it.
    for y in y0.saturating_sub(1)..=y0 + 1 {
        let o = (y * w + x) * 4;
        let px = &map[o..o + 4];
        let (red, green, blue) = (px[ri], px[1], px[bi]);
        if red > 180 && green < 80 && blue < 80 {
            return Tally::Pgm;
        }
        if green > 180 && red < 80 && blue < 80 {
            return Tally::Pvw;
        }
    }
    Tally::Other
}

fn pull(appsink: &gstreamer_app::AppSink) -> gstreamer::Sample {
    appsink
        .try_pull_sample(gstreamer::ClockTime::from_seconds(5))
        .expect("multiview frame within 5 s")
}

/// A running vision mixer flow with counters on the overlay upload.
struct Running {
    block_id: &'static str,
    num_inputs: usize,
    manager: PipelineManager,
    appsink: gstreamer_app::AppSink,
    /// Frames entering `glupload_overlay`.
    uploads: Arc<AtomicU64>,
    /// Everything that feeds the mixer's overlay pad: frames and GAPs. The
    /// live mixer waits for this pad until its deadline when it has nothing.
    fed: Arc<AtomicU64>,
    main_loop: gstreamer::glib::MainLoop,
    main_loop_thread: std::thread::JoinHandle<()>,
    _registry_file: NamedTempFile,
}

/// What a steady-state window measured.
struct Window {
    elapsed: f64,
    uploads: u64,
    fed: u64,
    frames: u32,
    last: gstreamer::Sample,
}

impl Running {
    fn start(block_id: &'static str, num_inputs: usize, live_audio: bool) -> Running {
        let main_loop = gstreamer::glib::MainLoop::new(None, false);
        let main_loop_thread = {
            let ml = main_loop.clone();
            std::thread::spawn(move || ml.run())
        };
        let registry_file = NamedTempFile::new().unwrap();
        let registry = BlockRegistry::new(registry_file.path());
        let mut manager = PipelineManager::new(
            &build_flow(block_id, num_inputs, live_audio),
            EventBroadcaster::with_capacity(10),
            &registry,
            vec![],
            "all".to_string(),
            None,
            std::env::temp_dir(),
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        )
        .expect("build GPU vision mixer pipeline");

        let glupload_overlay = manager
            .pipeline()
            .by_name(&format!("{block_id}:glupload_overlay"))
            .expect("glupload_overlay in pipeline");
        let uploads = Arc::new(AtomicU64::new(0));
        {
            let uploads = Arc::clone(&uploads);
            glupload_overlay.static_pad("sink").unwrap().add_probe(
                gstreamer::PadProbeType::BUFFER,
                move |_, _| {
                    uploads.fetch_add(1, Ordering::Relaxed);
                    gstreamer::PadProbeReturn::Ok
                },
            );
        }
        let fed = Arc::new(AtomicU64::new(0));
        {
            let fed = Arc::clone(&fed);
            glupload_overlay.static_pad("src").unwrap().add_probe(
                gstreamer::PadProbeType::BUFFER | gstreamer::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    match &info.data {
                        Some(gstreamer::PadProbeData::Buffer(_)) => {
                            fed.fetch_add(1, Ordering::Relaxed);
                        }
                        Some(gstreamer::PadProbeData::Event(ev))
                            if ev.type_() == gstreamer::EventType::Gap =>
                        {
                            fed.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                    gstreamer::PadProbeReturn::Ok
                },
            );
        }
        drop(glupload_overlay);
        manager.start().expect("start GPU vision mixer pipeline");
        let appsink = manager
            .pipeline()
            .by_name("mvsink")
            .unwrap()
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();
        let running = Running {
            block_id,
            num_inputs,
            manager,
            appsink,
            uploads,
            fed,
            main_loop,
            main_loop_thread,
            _registry_file: registry_file,
        };

        // Overlay up: input 0 on PGM, input 1 on PVW.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = pull(&running.appsink);
            if running.tally(&s, 0) == Tally::Pgm && running.tally(&s, 1) == Tally::Pvw {
                break;
            }
            assert!(Instant::now() < deadline, "overlay tally never appeared");
        }
        std::thread::sleep(Duration::from_millis(500));
        running
    }

    fn tally(&self, sample: &gstreamer::Sample, i: usize) -> Tally {
        thumbnail_tally(sample, self.block_id, i)
    }

    /// Pull multiview frames for [`WINDOW`] and count what happened.
    fn measure(&self, what: &str) -> Window {
        let uploads_before = self.uploads.load(Ordering::Relaxed);
        let fed_before = self.fed.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let mut frames = 0u32;
        let mut last = None;
        while t0.elapsed() < WINDOW {
            last = Some(pull(&self.appsink));
            frames += 1;
        }
        let w = Window {
            elapsed: t0.elapsed().as_secs_f64(),
            uploads: self.uploads.load(Ordering::Relaxed) - uploads_before,
            fed: self.fed.load(Ordering::Relaxed) - fed_before,
            frames,
            last: last.expect("multiview frames in the window"),
        };
        eprintln!(
            "{what}: over {:.2}s overlay uploads {} ({:.1}/s), overlay pad fed {:.1}/s, multiview frames {} ({:.1}/s)",
            w.elapsed,
            w.uploads,
            w.uploads as f64 / w.elapsed,
            w.fed as f64 / w.elapsed,
            w.frames,
            w.frames as f64 / w.elapsed,
        );
        w
    }

    /// Cut PVW (input 1) to PGM and return how many multiview frames it
    /// took for the tallies to follow. Same two calls as the take endpoint
    /// (`AppState::trigger_transition`).
    fn cut_frames(&self) -> u32 {
        let (_, old_pgm, new_pgm, _) = self
            .manager
            .trigger_transition(self.block_id, 0, 1, "cut", 0)
            .expect("cut");
        self.manager
            .update_vision_mixer_after_take(self.block_id, new_pgm, old_pgm, self.num_inputs)
            .expect("multiview update after the cut");
        let mut frames = 0;
        loop {
            let s = pull(&self.appsink);
            frames += 1;
            if self.tally(&s, 1) == Tally::Pgm && self.tally(&s, 0) == Tally::Pvw {
                return frames;
            }
            assert!(
                frames < 30,
                "the cut never reached the multiview overlay: thumbnails show {:?} / {:?}",
                self.tally(&s, 0),
                self.tally(&s, 1)
            );
        }
    }

    fn stop(mut self) {
        self.manager.stop().expect("stop");
        drop(self.manager);
        self.main_loop.quit();
        self.main_loop_thread.join().expect("main loop thread");
    }
}

/// Stall guards shared by both tests: loose bounds, CI GL is slow.
fn assert_keeps_running(w: &Window) {
    // The multiview keeps running.
    assert!(
        w.frames as f64 / w.elapsed > 10.0,
        "multiview stalled: {} frames in {:.2}s",
        w.frames,
        w.elapsed
    );
    // The overlay pad keeps getting something (a frame or a GAP) about
    // every frame, so the mixer does not wait for it.
    assert!(
        w.fed as f64 / w.elapsed > 10.0,
        "overlay pad fed only {} times in {:.2}s",
        w.fed,
        w.elapsed
    );
}

/// A cut reaches the multiview overlay at once. The max-buffers=1 drop=true
/// appsink may hold one frame from before the cut, and the mixer's latency
/// is one more; leave room for a slow CI runner.
fn assert_cut_is_prompt(frames: u32) {
    eprintln!("cut visible after {frames} multiview frame(s)");
    assert!(
        frames <= 6,
        "the cut took {frames} multiview frames to show"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_overlay_is_not_reuploaded_every_frame() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // No audio linked: nothing changes but the clock.
    let running = Running::start("vmoverlay_static", 2, false);
    let w = running.measure("static overlay");

    // The overlay is still on screen after a stretch with no uploads.
    assert_eq!(running.tally(&w.last, 0), Tally::Pgm, "PGM tally lost");
    assert_eq!(running.tally(&w.last, 1), Tally::Pvw, "PVW tally lost");
    assert_keeps_running(&w);
    // Only real changes upload: the clock ticks once a second. Re-pushing
    // the unchanged frame every tick would be ~90 here.
    assert!(
        w.uploads <= 3 * WINDOW.as_secs() + 1,
        "unchanged overlay uploaded {} times in {:.2}s",
        w.uploads,
        w.elapsed
    );

    assert_cut_is_prompt(running.cut_frames());
    running.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn meter_redraws_are_capped_but_cuts_are_not() {
    if !common::gl_available(GL_ELEMENTS) || !common::plugins_available(&["audiotestsrc", "level"])
    {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Live audio on three inputs and PGM: some meter changes on almost
    // every overlay tick.
    let running = Running::start("vmoverlay_meters", 3, true);
    let w = running.measure("live VU meters");

    assert_keeps_running(&w);
    let per_sec = w.uploads as f64 / w.elapsed;
    // The meters are really moving: more redraws than the clock alone.
    assert!(
        per_sec > 2.0,
        "meters never changed the overlay: {per_sec:.1} uploads/s"
    );
    // Meter and clock redraws are capped at 4 a second (a little margin
    // for the window edges).
    assert!(
        per_sec <= 4.5,
        "meter changes redrew the overlay {per_sec:.1} times a second"
    );

    // A cut is a source change: drawn at once, not held back by the cap.
    assert_cut_is_prompt(running.cut_frames());
    running.stop();
}
