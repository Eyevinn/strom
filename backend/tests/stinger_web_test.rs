//! Page stingers against a real `cefsrc`: an HTML Input in stinger mode on
//! the vision mixer's stinger input, with live sources on the mixer's inputs.
//!
//! `cefsrc` is not in the distro GStreamer packages, so these skip wherever
//! it is missing. `STROM_REQUIRE_CEF=1` turns the skip into a failure. On
//! macOS CEF needs a Cocoa run loop on the main thread, which the test
//! harness does not have, so these run on Linux only.
//!
//! Each test takes `STINGER_WEB_TAKES` stingers (default 10), alternating
//! PGM and PVW as every take swaps them, and prints the distribution.
//!
//! CEF initialises once per process and is slow the first time, so every
//! wait polls against a generous deadline, and the tests run serially.

pub mod common;
#[path = "common/stinger.rs"]
pub mod rig;

use base64::Engine;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use serial_test::serial;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::stinger::{
    STINGER_MODE_PROPERTY, WEB_STINGER_CUT_POINT_PROPERTY, WEB_STINGER_DURATION_PROPERTY,
};
use strom_types::{Flow, FlowId, Link, PropertyValue as PV, StromEvent};
use tempfile::NamedTempFile;

const W: usize = 640;
const H: usize = 360;
const FRAME_NS: u64 = 33_333_333;
const CUT_MS: u64 = 500;
const VM: &str = "vm";

/// Budget for the page's first frame. CEF's first initialisation in a
/// process takes several seconds.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(60);

fn takes() -> usize {
    std::env::var("STINGER_WEB_TAKES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
}

fn cefsrc_available() -> bool {
    gst::init().unwrap();
    let required = strom_types::env::var_opt("STROM_REQUIRE_CEF").is_some();
    if !cfg!(target_os = "linux") {
        assert!(
            !required,
            "STROM_REQUIRE_CEF is set, but these tests run on Linux only"
        );
        eprintln!("SKIP: cefsrc tests run on Linux only");
        return false;
    }
    if gst::ElementFactory::find("cefsrc").is_some() {
        return true;
    }
    assert!(
        !required,
        "STROM_REQUIRE_CEF is set but the cefsrc element is not installed (check GST_PLUGIN_PATH)"
    );
    eprintln!("SKIP: cefsrc not installed (set STROM_REQUIRE_CEF=1 to fail instead)");
    false
}

fn data_url(html: &str) -> String {
    format!(
        "data:text/html;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(html)
    )
}

/// Transparent at rest. For one second after every `hashchange` it covers
/// the frame in yellow from its first animation frame, with a hole in the
/// bottom-right corner that shows the program and a blue marker top left.
fn covering_page() -> String {
    data_url(
        "<!doctype html><html><body style=\"margin:0;background:transparent;overflow:hidden\">\
        <canvas id=\"c\" width=\"640\" height=\"360\"></canvas>\
        <script>\
        const x = document.getElementById('c').getContext('2d');\
        let t0 = 0;\
        function draw() {\
          x.clearRect(0, 0, 640, 360);\
          if (performance.now() - t0 >= 1000) return;\
          x.fillStyle = 'rgb(255,255,0)'; x.fillRect(0, 0, 640, 360);\
          x.clearRect(600, 320, 40, 40);\
          x.fillStyle = 'rgb(0,0,255)'; x.fillRect(0, 0, 80, 45);\
          requestAnimationFrame(draw);\
        }\
        addEventListener('hashchange', () => { t0 = performance.now(); requestAnimationFrame(draw); });\
        </script></body></html>",
    )
}

/// Opens on 300 ms that change no pixel (its panel moves in from off
/// frame), then covers the frame with a hole showing the program. A marker
/// top left carries the page's own elapsed ms: R = ms >> 4,
/// G = (ms & 15) * 16, B = 255.
fn late_painting_page() -> String {
    data_url(
        "<!doctype html><html><body style=\"margin:0;background:transparent;overflow:hidden\">\
        <div id=\"panel\" style=\"position:absolute;left:0;top:0;width:640px;height:360px;background:rgb(255,255,0);\
          clip-path:polygon(0 0,100% 0,100% 320px,600px 320px,600px 100%,0 100%);transform:translateX(-200%)\"></div>\
        <div id=\"mk\" style=\"position:absolute;left:0;top:0;width:80px;height:45px;visibility:hidden\"></div>\
        <script>\
        const panel = document.getElementById('panel'), mk = document.getElementById('mk');\
        let t0 = 0;\
        function frame(now) {\
          const e = now - t0;\
          if (e >= 1300) { panel.style.transform = 'translateX(-200%)'; mk.style.visibility = 'hidden'; return; }\
          if (e < 300) {\
            panel.style.transform = `translateX(${-200 + 50 * e / 300}%)`;\
          } else {\
            panel.style.transform = 'translateX(0%)';\
            const ms = Math.floor(e);\
            mk.style.background = `rgb(${(ms >> 4) & 255},${(ms & 15) * 16},255)`;\
            mk.style.visibility = 'visible';\
          }\
          requestAnimationFrame(frame);\
        }\
        addEventListener('hashchange', () => { t0 = performance.now(); requestAnimationFrame(frame); });\
        </script></body></html>",
    )
}

fn block(id: &str, definition: &str, props: &[(&str, PV)]) -> strom_types::BlockInstance {
    strom_types::BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

fn element(id: &str, element_type: &str, props: &[(&str, PV)]) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: element_type.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn link(from: &str, to: &str) -> Link {
    Link {
        from: from.to_string(),
        to: to.to_string(),
    }
}

struct Running {
    state: AppState,
    flow_id: FlowId,
    tap: gst_app::AppSink,
    _storage: NamedTempFile,
    _blocks: NamedTempFile,
}

/// Red on input 0 and green on input 1 (live), the page on the stinger
/// input, and every program frame in order on an RGBA appsink.
async fn start(name: &str, backend: &str, url: &str, duration_ms: u64, page_fps: u64) -> Running {
    if std::env::var("STINGER_TEST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("STINGER_TEST_LOG").unwrap())
            .with_test_writer()
            .try_init();
    }
    strom::gpu::detect_gpu_capabilities();
    let mut flow = Flow::new(name);
    for (id, colour) in [("red", 0xffff0000u64), ("green", 0xff00ff00u64)] {
        flow.elements.push(element(
            id,
            "videotestsrc",
            &[
                ("pattern", PV::String("solid-color".into())),
                ("foreground-color", PV::UInt(colour)),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(element(
            &format!("{id}_caps"),
            "capsfilter",
            &[(
                "caps",
                PV::String(format!("video/x-raw,width={W},height={H},framerate=30/1")),
            )],
        ));
        flow.links
            .push(link(&format!("{id}:src"), &format!("{id}_caps:sink")));
    }
    flow.blocks.push(block(
        "web",
        strom::blocks::builtin::html_input::BLOCK_ID,
        &[
            ("url", PV::String(url.to_string())),
            ("width", PV::UInt(W as u64)),
            ("height", PV::UInt(H as u64)),
            ("framerate", PV::UInt(page_fps)),
            (STINGER_MODE_PROPERTY, PV::Bool(true)),
            (WEB_STINGER_DURATION_PROPERTY, PV::UInt(duration_ms)),
            (WEB_STINGER_CUT_POINT_PROPERTY, PV::UInt(CUT_MS)),
        ],
    ));
    flow.blocks.push(block(
        VM,
        "builtin.vision_mixer",
        &[
            ("compositor_preference", PV::String(backend.into())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String(format!("{W}x{H}"))),
            ("multiview_resolution", PV::String(format!("{W}x{H}"))),
            ("pgm_framerate", PV::String("30/1".into())),
            ("enable_stinger", PV::Bool(true)),
        ],
    ));
    flow.elements
        .push(element("pgm_convert", "videoconvert", &[]));
    flow.elements.push(element(
        "pgm_tap",
        "appsink",
        &[
            ("caps", PV::String("video/x-raw,format=RGBA".into())),
            ("sync", PV::Bool(false)),
            // Every frame, in order: dropping would renumber what is counted.
            ("max-buffers", PV::UInt(400)),
            ("drop", PV::Bool(false)),
        ],
    ));
    flow.links.push(link("red_caps:src", "vm:video_in_0"));
    flow.links.push(link("green_caps:src", "vm:video_in_1"));
    flow.links.push(link("web:video_out", "vm:stinger_in"));
    flow.links.push(link("vm:pgm_out", "pgm_convert:sink"));
    flow.links.push(link("pgm_convert:src", "pgm_tap:sink"));

    let storage = NamedTempFile::new().unwrap();
    let blocks = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage.path()),
        blocks.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
        false,
        false,
    );
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");
    state.start_flow(&flow_id).await.expect("start_flow");
    let tap = {
        let pipelines = state.pipelines_read().await;
        pipelines
            .get(&flow_id)
            .and_then(|m| m.pipeline().by_name("pgm_tap"))
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .expect("pgm_tap")
    };
    let running = Running {
        state,
        flow_id,
        tap,
        _storage: storage,
        _blocks: blocks,
    };
    // Let CEF start and the page load before the first take.
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    loop {
        let s = running
            .state
            .stinger_state(&running.flow_id, VM)
            .await
            .unwrap();
        if s.ready && s.problem.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the stinger page never became ready: {:?}",
            s.problem
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    running
}

/// One program frame: its timestamp, the marker pixel and the hole pixel.
struct Frame {
    pts: u64,
    marker: [u8; 4],
    hole: [u8; 4],
}

fn rgba(sample: &gst::Sample, x: usize, y: usize) -> [u8; 4] {
    let info = gstreamer_video::VideoInfo::from_caps(sample.caps().unwrap()).unwrap();
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    let offset = info.offset()[0] + y * info.stride()[0] as usize + x * 4;
    map[offset..offset + 4].try_into().unwrap()
}

fn program(hole: [u8; 4]) -> Option<usize> {
    match hole {
        [r, g, b, _] if r > 200 && g < 60 && b < 60 => Some(0),
        [r, g, b, _] if r < 60 && g > 200 && b < 60 => Some(1),
        _ => None,
    }
}

/// Take a stinger and return the program frames from the take to the first
/// one whose source changed, then wait for the take to finish.
async fn take(r: &Running) -> (Vec<Frame>, strom_types::stinger::StingerTakeReport) {
    while r
        .tap
        .try_pull_sample(gst::ClockTime::from_mseconds(1))
        .is_some()
    {}
    let mut events = r.state.events().subscribe();
    let response = r
        .state
        .stinger_take(&r.flow_id, VM, None, None)
        .await
        .expect("stinger take");
    let tap = r.tap.clone();
    let frames = tokio::task::spawn_blocking(move || {
        let mut frames = Vec::new();
        let mut was = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let Some(sample) = tap.try_pull_sample(gst::ClockTime::from_mseconds(500)) else {
                continue;
            };
            let frame = Frame {
                pts: sample.buffer().unwrap().pts().unwrap().nseconds(),
                marker: rgba(&sample, 20, 11),
                hole: rgba(&sample, 630, 350),
            };
            let now = program(frame.hole);
            frames.push(frame);
            if let Some(now) = now {
                if was.is_some_and(|w| w != now) {
                    break;
                }
                was = Some(now);
            }
        }
        frames
    })
    .await
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let report = loop {
        match tokio::time::timeout_at(deadline.into(), events.recv()).await {
            Ok(Ok(StromEvent::StingerCompleted { report, .. }))
                if report.take_id == response.take_id =>
            {
                break report
            }
            Ok(Ok(StromEvent::StingerFailed { reason, .. })) => panic!("take failed: {reason}"),
            Ok(_) => continue,
            Err(_) => panic!("the stinger never completed"),
        }
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    (frames, report)
}

fn histogram<T: Ord + Copy + std::fmt::Debug>(values: &[T]) -> String {
    let mut counts = std::collections::BTreeMap::new();
    for v in values {
        *counts.entry(*v).or_insert(0usize) += 1;
    }
    counts
        .iter()
        .map(|(v, n)| format!("{v:?}: {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The program changes on the frame the cut point names, counted from the
/// page's first frame on air: 500 ms at 30 fps is 15 frames.
async fn cuts_on_the_frame_its_cut_point_names(backend: &str, page_fps: u64) {
    const EXPECTED: i64 = 15;
    let r = start(
        &format!("web_stinger_cut_{backend}_{page_fps}"),
        backend,
        &covering_page(),
        1000,
        page_fps,
    )
    .await;
    let blue = |p: [u8; 4]| p[2] > 200 && p[0] < 60 && p[1] < 60;
    let mut landed = Vec::new();
    let mut reports = Vec::new();
    for _ in 0..takes() {
        let (frames, report) = take(&r).await;
        let first = frames.iter().position(|f| blue(f.marker));
        let cut = frames.len().checked_sub(1);
        let n = first.zip(cut).map(|(f, c)| {
            let n = c as i64 - f as i64;
            // Frames are pulled in order, so a gap in timestamps would mean
            // a dropped frame, which would miscount.
            assert_eq!(
                ((frames[c].pts - frames[f].pts + FRAME_NS / 2) / FRAME_NS) as i64,
                n,
                "program frames were dropped"
            );
            eprintln!(
                "  page first on air at {} ns, cut at {} ns",
                frames[f].pts, frames[c].pts
            );
            n
        });
        landed.push(n);
        reports.push(report);
    }
    r.state.stop_flow(&r.flow_id).await.expect("stop_flow");
    eprintln!(
        "{backend}: frames from the page's first frame on air to the cut, per take: {landed:?} \
         ({})",
        histogram(&landed)
    );
    for rep in &reports {
        eprintln!(
            "  take {}: on air {:.0} ms after the take, {}/{} frames, {} late, worst margin {:?} ms",
            rep.take_id,
            rep.take_to_air_ms,
            rep.frames_arrived,
            rep.frames_expected,
            rep.frames_late,
            rep.worst_margin_ms.map(|m| m.round())
        );
    }
    assert!(
        reports
            .iter()
            .all(|rep| rep.frames_arrived >= rep.frames_expected && rep.frames_late == 0),
        "every page frame of a take must reach the mixer, in time"
    );
    assert!(
        landed.iter().all(|n| n.is_some_and(|n| n == EXPECTED)),
        "every take must cut {EXPECTED} frames after the page's first frame on air; got {landed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn cpu_page_stinger_cuts_on_the_frame_its_cut_point_names() {
    if !cefsrc_available() {
        return;
    }
    cuts_on_the_frame_its_cut_point_names("cpu", 30).await;
}

/// A 60 fps page on a 30 fps mixer paints at two phases half an output frame
/// apart, so about half its takes start in the second half of an output
/// frame. A page frame goes on air in the output frame it starts in, so a
/// take planned from the nearest output frame would cut a frame late on
/// those.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn cpu_page_stinger_faster_than_the_mixer_cuts_on_the_frame_its_cut_point_names() {
    if !cefsrc_available() {
        return;
    }
    cuts_on_the_frame_its_cut_point_names("cpu", 60).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn gpu_page_stinger_cuts_on_the_frame_its_cut_point_names() {
    if !cefsrc_available() || !common::gl_available(rig::GL_ELEMENTS) {
        return;
    }
    cuts_on_the_frame_its_cut_point_names("gpu", 30).await;
}

/// A take whose request is dropped while it waits for the page (the client
/// went away) still finishes and frees the mixer: the next take runs, and so
/// does an ordinary cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_dropped_take_request_still_frees_the_mixer() {
    if !cefsrc_available() {
        return;
    }
    let r = start("web_stinger_dropped", "cpu", &covering_page(), 1000, 30).await;
    let mut events = r.state.events().subscribe();
    let dropped = tokio::time::timeout(
        Duration::from_millis(20),
        r.state.stinger_take(&r.flow_id, VM, None, None),
    )
    .await;
    assert!(
        dropped.is_err(),
        "the take must still be waiting for the page at 20 ms"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline.into(), events.recv()).await {
            Ok(Ok(StromEvent::StingerCompleted { .. })) => break,
            Ok(Ok(StromEvent::StingerFailed { reason, .. })) => panic!("take failed: {reason}"),
            Ok(_) => continue,
            Err(_) => panic!("the dropped take never finished"),
        }
    }
    let (frames, _) = take(&r).await;
    assert!(!frames.is_empty(), "the next take must run");
    let s = r.state.stinger_state(&r.flow_id, VM).await.unwrap();
    assert!(!s.running);
    r.state.stop_flow(&r.flow_id).await.expect("stop_flow");
}

/// A page whose animation opens without changing a pixel paints its first
/// frame 300 ms in. Anchored on that frame the cut would land 300 ms late,
/// so the take is timed from the trigger instead. The page stamps its own
/// elapsed time, so the frame the program changes on says where by the
/// page's clock the cut landed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_late_painting_page_is_timed_from_the_take() {
    if !cefsrc_available() {
        return;
    }
    const TOLERANCE_MS: i64 = 67;
    let r = start("web_stinger_late", "cpu", &late_painting_page(), 1300, 30).await;
    let page_ms = |p: [u8; 4]| {
        let [red, g, b, _] = p;
        (b == 255 && g % 16 == 0).then(|| ((red as i64) << 4) + (g as i64) / 16)
    };
    let mut landed = Vec::new();
    for _ in 0..takes() {
        let (frames, _) = take(&r).await;
        landed.push(frames.last().and_then(|f| page_ms(f.marker)));
    }
    r.state.stop_flow(&r.flow_id).await.expect("stop_flow");
    let off: Vec<Option<i64>> = landed
        .iter()
        .map(|l| l.map(|ms| ms - CUT_MS as i64))
        .collect();
    eprintln!(
        "late page: cut landed at page ms {landed:?}; off by {off:?} ms ({})",
        histogram(&off)
    );
    assert!(
        off.iter()
            .all(|o| o.is_some_and(|o| o.abs() <= TOLERANCE_MS)),
        "every take must cut within {TOLERANCE_MS} ms of {CUT_MS} ms by the page's clock; \
         off by {off:?}"
    );
}
