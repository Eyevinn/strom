//! A measurement harness for the stinger preroll contract. Not a guard.
//!
//! Nothing here asserts a timing figure — a latency assertion would be flaky on
//! a shared machine and would fail for reasons that have nothing to do with
//! this code. It exists so the numbers behind the design can be reproduced, and
//! it is `#[ignore]`d so it never runs in CI.
//!
//! ```text
//! cargo test --test stinger_preroll_bench -- --ignored --nocapture
//! ```
//!
//! What it measures: how long after "go" a stinger clip's first frame reaches
//! the main pipeline. Every other transition Strom has is pure property
//! animation — `TransitionController` ramps compositor pad properties and
//! nothing has to stay in sync with a clock. A stinger is the first that must
//! stay in sync with something playing, so a slow or jittery start shows up as
//! a late or hitching transition.
//!
//! Probed at the media player's `video_out` identity in the MAIN pipeline —
//! past the internal `uridecodebin` and past the appsink->appsrc bridge, so it
//! is the frame a mixer pad would actually see.
//!
//! Method, which matters more than the numbers:
//!
//! * `MediaPlayerState::stop()` is `pause()` + `seek(0)`; it does not tear the
//!   internal pipeline down, so it is **not** a cold path. Measuring "stop then
//!   play" against "preroll then play" measures the same thing twice.
//! * A **quiescence control** runs first: after each arming action, the harness
//!   waits with no `play()` and confirms no buffers arrive. Without it the
//!   trials time "when is the next buffer", which is not the same question.
//! * Three regimes, genuinely different:
//!   `pipeline start` (`manager.start()` to the first buffer, what an operator
//!   waits for when a flow comes up), `prerolled` (`pause()` + `seek(0)`,
//!   settle, `play()`), and `clip switch` (`goto()` another entry, then
//!   `play()` — an operator picking a different stinger and firing at once).
//! * A source block whose output pad is left unlinked returns not-linked and no
//!   data flows, so a `fakesink` is attached before starting.
//!
//! Measured on macOS, 2026-09-02, FFV1/Matroska, median of 5 (one frame at
//! 30 fps = 33.3 ms):
//!
//!   resolution   pipeline start   prerolled   clip switch
//!   320x180              3.8 ms      0.5 ms        4.4 ms
//!   1920x1080            9.5 ms      0.4 ms       14.1 ms
//!   3840x2160           33.1 ms      0.5 ms       28.4 ms  (max 30.9)
//!
//! The prerolled path is flat in resolution — the frame is already decoded and
//! waiting, so `play()` is only a state change. Clip switch scales roughly with
//! pixel count and reaches the one-frame budget at 4K.
//!
//! Cadence, same run: over 45 frames the offset moved 0.3 ms -> 2.9 ms, max
//! deviation 5.1 ms — 0.15 of a frame. A plain wall-clock timer is therefore
//! accurate enough for a stinger's cut point.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom::blocks::builtin::mediaplayer::{
    MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY,
};
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue};
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "mp_timing";
const W: u32 = 1920;
const H: u32 = 1080;
const CLIP_FRAMES: usize = 60; // 2 s at 30 fps — longer than any stinger
const FRAME_DUR_NS: u64 = 33_333_333;
const TRIALS: usize = 5;
const CADENCE_FRAMES: usize = 45; // 1.5 s at 30 fps

/// One BGRA frame: opaque green left, fully transparent right.
fn frame(index: usize) -> gst::Buffer {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for y in 0..H {
        for x in 0..W {
            let i = ((y * W + x) * 4) as usize;
            if x < W / 2 {
                data[i + 1] = 255;
                data[i + 3] = 255;
            }
        }
    }
    let mut buf = gst::Buffer::from_mut_slice(data);
    {
        let b = buf.get_mut().expect("fresh buffer is writable");
        b.set_pts(gst::ClockTime::from_nseconds(index as u64 * FRAME_DUR_NS));
        b.set_duration(gst::ClockTime::from_nseconds(FRAME_DUR_NS));
    }
    buf
}

/// Lossless BGRA clip as FFV1 in Matroska (the alpha-carrying pair CI has).
fn write_clip(path: &std::path::Path) -> Result<(), String> {
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=fg ! avenc_ffv1 ! matroskamux ! filesink location={}",
        path.display()
    ))
    .map_err(|e| format!("parse: {e}"))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_string())?;

    let appsrc = pipeline
        .by_name("fg")
        .ok_or("no appsrc")?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| "not an appsrc".to_string())?;
    appsrc.set_caps(Some(
        &gst::Caps::builder("video/x-raw")
            .field("format", "BGRA")
            .field("width", W as i32)
            .field("height", H as i32)
            .field("framerate", gst::Fraction::new(30, 1))
            .build(),
    ));
    appsrc.set_format(gst::Format::Time);
    appsrc.set_is_live(false);

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("encode PLAYING: {e}"))?;
    for i in 0..CLIP_FRAMES {
        appsrc
            .push_buffer(frame(i))
            .map_err(|e| format!("push: {e:?}"))?;
    }
    let _ = appsrc.end_of_stream();
    let bus = pipeline.bus().expect("bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg {
        Some(m) if matches!(m.view(), gst::MessageView::Eos(_)) => Ok(()),
        Some(m) => Err(format!("encode failed: {:?}", m.view())),
        None => Err("encode timed out".to_string()),
    }
}

fn build_flow(clips: &[std::path::PathBuf]) -> Flow {
    let mut flow = Flow::new("stinger_timing");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            // decode=true: a stinger needs raw frames with alpha, not a
            // passthrough elementary stream.
            p.insert("decode".to_string(), PropertyValue::Bool(true));
            p.insert("sync".to_string(), PropertyValue::Bool(true));
            p.insert(
                "playlist".to_string(),
                PropertyValue::String(
                    serde_json::to_string(
                        &clips
                            .iter()
                            .map(|c| c.display().to_string())
                            .collect::<Vec<_>>(),
                    )
                    .unwrap(),
                ),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

/// One trial. Returns time from `play()` to the first buffer at `video_out`.
fn measure(
    player: &Arc<MediaPlayerState>,
    first: &Arc<AtomicU64>,
    epoch: Instant,
    arm: impl FnOnce(&Arc<MediaPlayerState>),
) -> Option<Duration> {
    arm(player);
    // Let the requested state settle before timing anything.
    std::thread::sleep(Duration::from_millis(600));

    first.store(0, Ordering::SeqCst);
    let t0 = epoch.elapsed().as_nanos() as u64;
    player.play().ok()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let v = first.load(Ordering::SeqCst);
        if v != 0 {
            return Some(Duration::from_nanos(v.saturating_sub(t0)));
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn report(label: &str, mut samples: Vec<Duration>) {
    if samples.is_empty() {
        eprintln!("{label}: no frame within timeout");
        return;
    }
    samples.sort();
    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    eprintln!(
        "{label}: min {:.1} ms | median {:.1} ms | max {:.1} ms  (n={})  all: {}",
        ms(&samples[0]),
        ms(&samples[samples.len() / 2]),
        ms(&samples[samples.len() - 1]),
        samples.len(),
        samples
            .iter()
            .map(|d| format!("{:.1}", ms(d)))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement harness, not a test; see the module docs to run it"]
async fn stinger_clip_start_latency() {
    gst::init().expect("gstreamer init");
    for name in ["avenc_ffv1", "matroskamux"] {
        if gst::ElementFactory::find(name).is_none() {
            panic!("{name} missing — CI installs gst-libav and plugins-good");
        }
    }

    let dir = std::env::temp_dir();
    let clips: Vec<std::path::PathBuf> = (0..2)
        .map(|i| {
            dir.join(format!(
                "strom_stinger_timing_{}_{i}.mkv",
                std::process::id()
            ))
        })
        .collect();
    for c in &clips {
        write_clip(c).expect("write clip");
    }

    let flow = build_flow(&clips);
    let flow_id = flow.id;

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        dir.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .expect("build pipeline");
    // A source block whose output pad is left unlinked returns not-linked and
    // no data flows, so give video_out somewhere to go before starting.
    {
        let pipeline = manager.pipeline();
        let sink = gst::ElementFactory::make("fakesink")
            .name("timing_sink")
            .property("sync", false)
            .property("async", false)
            .build()
            .expect("fakesink");
        pipeline.add(&sink).expect("add fakesink");
        let video_out = pipeline
            .by_name(&format!("{}:video_out", BLOCK_ID))
            .expect("video_out identity in pipeline");
        video_out.link(&sink).expect("link video_out -> fakesink");
    }

    // Timestamp the first buffer out of the block after each arming. The
    // closure captures no GStreamer references (see CLAUDE.md) and does one
    // CAS, so it stays cheap even though BUFFER probes are hot. Installed
    // before start() so the pipeline-start figure is measurable.
    let epoch = Instant::now();
    let first = Arc::new(AtomicU64::new(0));
    // Arrival stamp of each of the first CADENCE_FRAMES buffers, for the
    // cut-point analysis below. Lock-free so the BUFFER probe stays cheap.
    let slots: Arc<Vec<AtomicU64>> =
        Arc::new((0..CADENCE_FRAMES).map(|_| AtomicU64::new(0)).collect());
    let idx = Arc::new(AtomicUsize::new(usize::MAX));
    {
        let video_out = manager
            .pipeline()
            .by_name(&format!("{}:video_out", BLOCK_ID))
            .expect("video_out identity in pipeline");
        let pad = video_out.static_pad("src").expect("video_out src pad");
        let f = Arc::clone(&first);
        let sl = Arc::clone(&slots);
        let ix = Arc::clone(&idx);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let now = epoch.elapsed().as_nanos() as u64;
            let _ = f.compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst);
            // usize::MAX means "not collecting"; wrapping_add(1) arms at 0.
            let i = ix.load(Ordering::SeqCst);
            if i != usize::MAX {
                let n = ix.fetch_add(1, Ordering::SeqCst);
                if n < sl.len() {
                    sl[n].store(now, Ordering::SeqCst);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    let t_start = epoch.elapsed().as_nanos() as u64;
    manager.start().expect("start pipeline");
    let start_latency = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let v = first.load(Ordering::SeqCst);
            if v != 0 {
                break Some(Duration::from_nanos(v.saturating_sub(t_start)));
            }
            if Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    };

    let player = MEDIA_PLAYER_REGISTRY
        .get(&MediaPlayerKey {
            flow_id,
            block_id: BLOCK_ID.to_string(),
        })
        .expect("media player registered");

    eprintln!(
        "player state after start: {:?}, file: {:?}, duration: {:?}",
        player.state(),
        player.current_file(),
        player.duration()
    );

    // Control. The whole measurement assumes stop()/pause() actually halt the
    // flow into the main pipeline, so that the next buffer after play() is a
    // fresh one. If buffers keep arriving with no play() call, the trials below
    // are timing "when is the next buffer" and mean nothing.
    for (label, stop_it) in [("stop()", true), ("pause()", false)] {
        if stop_it {
            let _ = player.stop();
        } else {
            let _ = player.pause();
        }
        std::thread::sleep(Duration::from_millis(600));
        first.store(0, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        eprintln!(
            "control: 300 ms after {label} with no play() -> buffers still arriving: {}",
            first.load(Ordering::SeqCst) != 0
        );
    }

    // Warm-up: the first play of a process pays one-off costs no later
    // stinger would.
    let _ = measure(&player, &first, epoch, |p| {
        let _ = p.pause();
        let _ = p.seek(0);
    });

    let mut prerolled = Vec::new();
    let mut switched = Vec::new();
    for i in 0..TRIALS {
        if let Some(d) = measure(&player, &first, epoch, |p| {
            let _ = p.pause();
            let _ = p.seek(0);
        }) {
            prerolled.push(d);
        }
        if let Some(d) = measure(&player, &first, epoch, |p| {
            let _ = p.pause();
            let _ = p.goto(i % 2);
        }) {
            switched.push(d);
        }
    }

    match start_latency {
        Some(d) => eprintln!(
            "pipeline start (start() -> first buffer): {:.1} ms",
            d.as_secs_f64() * 1000.0
        ),
        None => eprintln!("pipeline start: no buffer within 20 s"),
    }
    // --- Cut point: does a wall-clock timer land on the right frame? ---
    //
    // The underlying transition has to fire when the clip covers frame. The
    // cheap implementation is a timer set to cut_point_ms after play(). That is
    // only sound if wall-clock time since play() predicts which frame is
    // actually on the mixer pad. Compare each frame's arrival against the
    // nominal 30 fps cadence to find out.
    {
        let _ = player.pause();
        let _ = player.seek(0);
        std::thread::sleep(Duration::from_millis(600));
        for slot in slots.iter() {
            slot.store(0, Ordering::SeqCst);
        }
        idx.store(0, Ordering::SeqCst);
        let t0 = epoch.elapsed().as_nanos() as u64;
        player.play().expect("play for cadence");
        std::thread::sleep(Duration::from_millis(
            (CADENCE_FRAMES as u64 * 1000 / 30) + 400,
        ));
        idx.store(usize::MAX, Ordering::SeqCst);

        let arrivals: Vec<u64> = slots
            .iter()
            .map(|s| s.load(Ordering::SeqCst))
            .take_while(|v| *v != 0)
            .collect();
        if arrivals.len() < 10 {
            eprintln!("cadence: only {} frames captured, skipping", arrivals.len());
        } else {
            let mut drift: Vec<f64> = Vec::new();
            for (i, a) in arrivals.iter().enumerate() {
                let actual = (a.saturating_sub(t0)) as f64 / 1e6;
                let nominal = i as f64 * 1000.0 / 30.0;
                drift.push(actual - nominal);
            }
            let first_d = drift[0];
            let last_d = *drift.last().unwrap();
            let max_dev = drift
                .iter()
                .map(|d| (d - first_d).abs())
                .fold(0.0_f64, f64::max);
            eprintln!(
                "cadence over {} frames: offset at frame 0 = {:.1} ms, at last frame = {:.1} ms, \
max deviation from the frame-0 offset = {:.1} ms ({:.2} frames)",
                arrivals.len(),
                first_d,
                last_d,
                max_dev,
                max_dev / (1000.0 / 30.0)
            );
        }
    }

    report("prerolled   (pause+seek0 -> play)", prerolled.clone());
    report("clip switch (pause+goto   -> play)", switched.clone());
    eprintln!("one frame at 30 fps = 33.3 ms");

    let _ = manager.stop();
    for c in &clips {
        let _ = std::fs::remove_file(c);
    }

    assert!(
        !prerolled.is_empty() && !switched.is_empty(),
        "expected both regimes to produce a frame; prerolled={} switched={}",
        prerolled.len(),
        switched.len()
    );
}
