//! Regression test: an unchanged multiview overlay must not be uploaded to
//! the GPU every frame, while a PGM/PVW switch still reaches the next
//! multiview frame.
//!
//! The overlay timer runs at the multiview framerate. It used to re-push the
//! last overlay frame on every tick, so `glupload_overlay` uploaded an
//! identical full-canvas RGBA frame 30 times a second on the single GL
//! thread. An unchanged overlay now only tells the mixer to keep the frame it
//! has, so uploads follow real overlay changes (the clock, once a second).
//!
//! Builds a real GPU vision mixer flow through `PipelineManager`, counts the
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
const BLOCK_ID: &str = "vmoverlayupload";
const MV_W: i32 = 1280;
const MV_H: i32 = 720;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn build_flow() -> Flow {
    let mut flow = Flow::new("vm_overlay_upload");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PV::String("gpu".into()),
            );
            p.insert("num_inputs".to_string(), PV::UInt(2));
            p.insert("pgm_resolution".to_string(), PV::String("1280x720".into()));
            p.insert(
                "multiview_resolution".to_string(),
                PV::String(format!("{MV_W}x{MV_H}")),
            );
            p.insert("initial_pgm_input".to_string(), PV::UInt(0));
            p.insert("initial_pvw_input".to_string(), PV::UInt(1));
            // Download so the appsinks can map pixels.
            p.insert("gl_download".to_string(), PV::Bool(true));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    let caps = "video/x-raw,width=640,height=360,framerate=30/1";
    for i in 0..2 {
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
            to: format!("{BLOCK_ID}:video_in_{i}"),
        });
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
    flow.links.push(strom_types::Link {
        from: format!("{BLOCK_ID}:multiview_out"),
        to: "mvsink:sink".to_string(),
    });
    flow.links.push(strom_types::Link {
        from: format!("{BLOCK_ID}:pgm_out"),
        to: "pgmsink:sink".to_string(),
    });
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
fn thumbnail_tally(sample: &gstreamer::Sample, i: usize) -> Tally {
    let state = overlay::get_overlay_state(BLOCK_ID).expect("overlay state registered");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_overlay_is_not_reuploaded_every_frame() {
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let main_loop = gstreamer::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };
    let registry_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let mut manager = PipelineManager::new(
        &build_flow(),
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
        .by_name(&format!("{BLOCK_ID}:glupload_overlay"))
        .expect("glupload_overlay in pipeline");
    // Frames entering the upload.
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
    // Everything that feeds the mixer's overlay pad: frames and GAPs. The
    // live mixer waits for this pad until its deadline when it has nothing.
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

    // Overlay up: input 0 on PGM, input 1 on PVW.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = pull(&appsink);
        if thumbnail_tally(&s, 0) == Tally::Pgm && thumbnail_tally(&s, 1) == Tally::Pvw {
            break;
        }
        assert!(Instant::now() < deadline, "overlay tally never appeared");
    }
    std::thread::sleep(Duration::from_millis(500));

    // Steady state: nothing changes but the clock.
    const WINDOW: Duration = Duration::from_secs(3);
    let uploads_before = uploads.load(Ordering::Relaxed);
    let fed_before = fed.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let mut frames = 0u32;
    let mut last = None;
    while t0.elapsed() < WINDOW {
        last = Some(pull(&appsink));
        frames += 1;
    }
    let elapsed = t0.elapsed().as_secs_f64();
    let window_uploads = uploads.load(Ordering::Relaxed) - uploads_before;
    let window_fed = fed.load(Ordering::Relaxed) - fed_before;
    eprintln!(
        "steady state over {:.2}s: overlay uploads {} ({:.1}/s), overlay pad fed {:.1}/s, multiview frames {} ({:.1}/s)",
        elapsed,
        window_uploads,
        window_uploads as f64 / elapsed,
        window_fed as f64 / elapsed,
        frames,
        frames as f64 / elapsed,
    );

    // The overlay is still on screen after a stretch with no uploads.
    let last = last.expect("multiview frames in the window");
    assert_eq!(thumbnail_tally(&last, 0), Tally::Pgm, "PGM tally lost");
    assert_eq!(thumbnail_tally(&last, 1), Tally::Pvw, "PVW tally lost");
    // The multiview keeps running (a loose bound: CI GL is slow).
    assert!(
        frames as f64 / elapsed > 10.0,
        "multiview stalled: {frames} frames in {elapsed:.2}s"
    );
    // The overlay pad keeps getting something (a frame or a GAP) about
    // every frame, so the mixer does not wait for it. Loose for slow CI.
    assert!(
        window_fed as f64 / elapsed > 10.0,
        "overlay pad fed only {window_fed} times in {elapsed:.2}s"
    );
    // Only real changes upload: the clock ticks once a second. Re-pushing
    // the unchanged frame every tick would be ~90 here.
    assert!(
        window_uploads <= 3 * WINDOW.as_secs() + 1,
        "unchanged overlay uploaded {window_uploads} times in {elapsed:.2}s"
    );

    // A cut still reaches the next multiview frame. Same two calls as the
    // take endpoint (`AppState::trigger_transition`).
    let (_, old_pgm, new_pgm, _) = manager
        .trigger_transition(BLOCK_ID, 0, 1, "cut", 0)
        .expect("cut");
    manager
        .update_vision_mixer_after_take(BLOCK_ID, new_pgm, old_pgm, 2)
        .expect("multiview update after the cut");
    let mut frames_until_switch = 0;
    loop {
        let s = pull(&appsink);
        frames_until_switch += 1;
        if thumbnail_tally(&s, 1) == Tally::Pgm && thumbnail_tally(&s, 0) == Tally::Pvw {
            break;
        }
        assert!(
            frames_until_switch < 30,
            "the cut never reached the multiview overlay: thumbnails show {:?} / {:?}",
            thumbnail_tally(&s, 0),
            thumbnail_tally(&s, 1)
        );
    }
    eprintln!("cut visible after {frames_until_switch} multiview frame(s)");
    // max-buffers=1 drop=true appsink may hold one frame from before the cut,
    // and the mixer's latency is one more; leave room for a slow CI runner.
    assert!(
        frames_until_switch <= 6,
        "the cut took {frames_until_switch} multiview frames to show"
    );

    manager.stop().expect("stop");
    drop(manager);
    main_loop.quit();
    main_loop_thread.join().expect("main loop thread");
}
