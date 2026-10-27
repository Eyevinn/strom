//! Smoke test for the oldest GStreamer Strom supports (see
//! `strom::version::MIN_GSTREAMER_VERSION`).
//!
//! The `check-floor` CI job runs only this test, on Debian 12's GStreamer 1.22.
//! It starts Strom the way `main` does (see [`init_like_main`] for the steps
//! it skips), then drives a few core flows through the HTTP API and asserts
//! each one reaches PLAYING and holds it with no pipeline error. It is not a
//! feature test: it answers "does Strom still start and run ordinary flows on
//! the floor".
//!
//! Every element it needs is in the core, base, good, ugly and libav packages
//! bookworm ships, and each test fails rather than skips when one is missing.

pub mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Element, Flow, FlowId, Link, PropertyValue as PV, StromEvent};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::broadcast::Receiver;
use tower::ServiceExt;

/// Elements each flow uses, directly or inside its blocks.
const AUDIO_MIXER_ELEMENTS: &[&str] = &[
    "audiotestsrc",
    "fakesink",
    "audiomixer",
    "audioconvert",
    "audioresample",
    "audiopanorama",
    "audiowsinclimit",
    "audiocheblimit",
    "volume",
    "level",
    "tee",
    "queue",
    "identity",
    "capsfilter",
];
const VISION_MIXER_ELEMENTS: &[&str] = &[
    "videotestsrc",
    "fakesink",
    "compositor",
    "videoconvert",
    "videoscale",
    "videocrop",
    "capsfilter",
    "tee",
    "queue",
];
const ENCODER_ELEMENTS: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "fakesink",
    "videoconvert",
    "videoscale",
    "capsfilter",
    "x264enc",
    "h264parse",
    "audioconvert",
    "audioresample",
    "avenc_aac",
    "aacparse",
];
const PLAIN_ELEMENTS: &[&str] = &["videotestsrc", "fakesink"];

/// How long a started flow gets to reach PLAYING.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a flow must stay in PLAYING once there. A pipeline stays in
/// PLAYING after an ERROR message, so this window watches the flow's
/// `PipelineError` events too: a caps or allocation failure after the first
/// buffers shows up there.
const HOLD: Duration = Duration::from_secs(2);

fn element(id: &str, ty: &str, props: &[(&str, PV)]) -> Element {
    Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn block(id: &str, definition: &str, props: &[(&str, PV)]) -> BlockInstance {
    BlockInstance {
        id: id.to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    }
}

fn live_audio(id: &str) -> Element {
    element(id, "audiotestsrc", &[("is-live", PV::Bool(true))])
}

fn live_video(id: &str) -> Element {
    element(id, "videotestsrc", &[("is-live", PV::Bool(true))])
}

fn sink(id: &str) -> Element {
    element(id, "fakesink", &[])
}

fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

/// Two live tones through the audio mixer block (lsp-dsp-rs backend).
fn audio_mixer_flow() -> Flow {
    let mut flow = Flow::new("floor-audio-mixer");
    flow.elements.push(live_audio("tone1"));
    flow.elements.push(live_audio("tone2"));
    flow.elements.push(sink("main"));
    flow.blocks.push(block(
        "mixer",
        "builtin.mixer",
        &[("num_channels", PV::UInt(2))],
    ));
    link(&mut flow, "tone1:src", "mixer:input_1");
    link(&mut flow, "tone2:src", "mixer:input_2");
    link(&mut flow, "mixer:main_out", "main:sink");
    flow
}

/// Two live test patterns through the vision mixer on its CPU compositor, so
/// it needs no GL context.
fn vision_mixer_flow() -> Flow {
    let mut flow = Flow::new("floor-vision-mixer");
    flow.elements.push(live_video("cam1"));
    flow.elements.push(live_video("cam2"));
    flow.elements.push(sink("pgm"));
    flow.elements.push(sink("mv"));
    flow.blocks.push(block(
        "vm",
        "builtin.vision_mixer",
        &[
            ("compositor_preference", PV::String("cpu".to_string())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String("320x180".to_string())),
            ("multiview_resolution", PV::String("320x180".to_string())),
        ],
    ));
    link(&mut flow, "cam1:src", "vm:video_in_0");
    link(&mut flow, "cam2:src", "vm:video_in_1");
    link(&mut flow, "vm:pgm_out", "pgm:sink");
    link(&mut flow, "vm:multiview_out", "mv:sink");
    flow
}

/// Raw video and audio through the format and encoder blocks: H.264 in
/// software, AAC.
fn encoder_flow() -> Flow {
    let mut flow = Flow::new("floor-encoders");
    flow.elements.push(live_video("video"));
    flow.elements.push(live_audio("audio"));
    flow.elements.push(sink("vsink"));
    flow.elements.push(sink("asink"));
    flow.blocks.push(block(
        "vfmt",
        "builtin.videoformat",
        &[("resolution", PV::String("640x360".to_string()))],
    ));
    flow.blocks.push(block(
        "venc",
        "builtin.videoenc",
        &[
            ("codec", PV::String("h264".to_string())),
            ("encoder_preference", PV::String("software".to_string())),
        ],
    ));
    flow.blocks.push(block("afmt", "builtin.audioformat", &[]));
    flow.blocks.push(block(
        "aenc",
        "builtin.audioenc",
        &[("codec", PV::String("aac".to_string()))],
    ));
    link(&mut flow, "video:src", "vfmt:video_in");
    link(&mut flow, "vfmt:video_out", "venc:video_in");
    link(&mut flow, "venc:encoded_out", "vsink:sink");
    link(&mut flow, "audio:src", "afmt:audio_in");
    link(&mut flow, "afmt:audio_out", "aenc:audio_in");
    link(&mut flow, "aenc:encoded_out", "asink:sink");
    flow
}

/// The simplest flow: one live test pattern into a fakesink.
fn plain_flow() -> Flow {
    let mut flow = Flow::new("floor-plain");
    flow.elements.push(live_video("video"));
    flow.elements.push(sink("out"));
    link(&mut flow, "video:src", "out:sink");
    flow
}

/// Start-up as `main` does it, minus the server socket: GStreamer init, the
/// version log, the statically linked plugins, GPU detection and the GLib main
/// loop that dispatches the flow bus watches.
///
/// Skipped: VA-API rank promotion (a private step in `main`; no flow here
/// autoplugs a codec, and the CI container has no VA device), and the ICE and
/// clock-discipline checks (they only log).
fn init_like_main() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        strom::version::log_gstreamer_version();
        common::init_webrtc_plugins();
        strom::gpu::detect_gpu_capabilities();
        strom::gst::start_glib_main_loop();
    });
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<&Flow>,
) -> (StatusCode, serde_json::Value) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(flow) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(flow).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// The flow's state as the API reports it (`gst_state`).
async fn api_state(app: &Router, id: &FlowId) -> serde_json::Value {
    let (status, json) = request(app, "GET", &format!("/api/flows/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "GET flow: {json}");
    json["flow"]["gst_state"].clone()
}

/// The first `PipelineError` the flow has broadcast since `events` subscribed.
fn pipeline_error(events: &mut Receiver<StromEvent>, id: &FlowId) -> Option<String> {
    loop {
        match events.try_recv() {
            Ok(StromEvent::PipelineError {
                flow_id,
                error,
                source,
            }) if flow_id == *id => {
                return Some(format!("{error} (from {})", source.unwrap_or_default()));
            }
            Ok(_) | Err(TryRecvError::Lagged(_)) => continue,
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return None,
        }
    }
}

/// Create and start `flow` through the API, wait for PLAYING, and hold it for
/// [`HOLD`]. `Err` says what went wrong; a pipeline error, at any point from
/// the start on, is reported as `PipelineError: ...`.
///
/// `inject_error` makes a sink of the running pipeline post an element error
/// once it is in PLAYING, the way a streaming failure does; only the test of
/// the check itself sets it.
async fn run_to_playing(flow: Flow, inject_error: bool) -> Result<(), String> {
    init_like_main();
    let test_state = common::state::new();
    let app = strom::create_app_with_state(test_state.app()).await;
    let mut events = test_state.events().subscribe();
    let id = flow.id;
    let version = strom::version::gstreamer_version_string();

    let (status, json) = request(&app, "POST", "/api/flows", Some(&flow)).await;
    if status != StatusCode::CREATED {
        return Err(format!("create failed with {status}: {json}"));
    }
    let (status, json) = request(&app, "POST", &format!("/api/flows/{id}/start"), None).await;
    if status != StatusCode::OK {
        return Err(format!("start failed with {status}: {json}"));
    }

    let result = async {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(e) = pipeline_error(&mut events, &id) {
                return Err(format!("PipelineError on GStreamer {version} while starting: {e}"));
            }
            let state = api_state(&app, &id).await;
            if state == "Playing" {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "did not reach PLAYING within {START_TIMEOUT:?} on GStreamer {version}: state {state}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        if inject_error {
            let pipelines = test_state.pipelines_read().await;
            let pipeline = pipelines.get(&id).expect("running pipeline").pipeline();
            let sink = pipeline
                .iterate_sinks()
                .next()
                .ok()
                .flatten()
                .expect("a sink in the pipeline");
            gst::element_error!(sink, gst::StreamError::Failed, ["injected by the smoke test"]);
        }

        let held_until = Instant::now() + HOLD;
        while Instant::now() < held_until {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Some(e) = pipeline_error(&mut events, &id) {
                return Err(format!("PipelineError on GStreamer {version} in PLAYING: {e}"));
            }
            let state = api_state(&app, &id).await;
            if state != "Playing" {
                return Err(format!("left PLAYING on GStreamer {version}: state {state}"));
            }
        }
        Ok(())
    }
    .await;

    let (status, json) = request(&app, "POST", &format!("/api/flows/{id}/stop"), None).await;
    assert_eq!(status, StatusCode::OK, "stop {}: {json}", flow.name);
    result
}

async fn assert_reaches_playing(flow: Flow, elements: &[&str]) {
    common::require_elements(elements);
    let name = flow.name.clone();
    if let Err(e) = run_to_playing(flow, false).await {
        panic!("{name}: {e}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_endpoint_answers() {
    init_like_main();
    let test_state = common::state::new();
    let app = strom::create_app_with_state(test_state.app()).await;
    let (status, _) = request(&app, "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_mixer_flow_reaches_playing() {
    assert_reaches_playing(audio_mixer_flow(), AUDIO_MIXER_ELEMENTS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_vision_mixer_flow_reaches_playing() {
    assert_reaches_playing(vision_mixer_flow(), VISION_MIXER_ELEMENTS).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoder_flow_reaches_playing() {
    assert_reaches_playing(encoder_flow(), ENCODER_ELEMENTS).await;
}

/// Guards the check itself: an element error on a pipeline already in PLAYING
/// must fail [`run_to_playing`]. A pipeline stays in PLAYING after an ERROR
/// message, so without the `PipelineError` watch this would pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_streaming_error_fails_the_check() {
    common::require_elements(PLAIN_ELEMENTS);
    match run_to_playing(plain_flow(), true).await {
        Err(e) if e.starts_with("PipelineError") => {}
        other => panic!("expected a PipelineError, got {other:?}"),
    }
}
