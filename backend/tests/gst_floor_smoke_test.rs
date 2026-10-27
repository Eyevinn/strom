//! Smoke test for the oldest GStreamer Strom supports (see
//! `strom::version::MIN_GSTREAMER_VERSION`).
//!
//! The `check-floor` CI job runs only this test, on Debian 12's GStreamer 1.22.
//! It starts Strom the way `main` does (GStreamer init, the statically linked
//! plugins, GPU detection), then drives a few core flows through the HTTP API
//! and asserts each one reaches and holds PLAYING. It is not a feature test:
//! it answers "does Strom still start and run ordinary flows on the floor".
//!
//! Every element it needs is in the core, base, good, ugly and libav packages
//! bookworm ships, and it fails rather than skips when one is missing.

pub mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Element, Flow, FlowId, Link, PropertyValue as PV};
use tower::ServiceExt;

/// Elements the flows below build, directly or inside the blocks.
const REQUIRED: &[&str] = &[
    "audiotestsrc",
    "videotestsrc",
    "fakesink",
    "capsfilter",
    "audioconvert",
    "audioresample",
    "videoconvert",
    "videoscale",
    "compositor",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
];

/// How long a started flow gets to reach PLAYING.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a flow must stay in PLAYING once there. A caps or allocation
/// failure after the first buffers would show up as an error in this window.
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

/// Start-up as `main` does it, minus the server socket.
fn init_like_main() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        strom::version::log_gstreamer_version();
        strom::gst::register_static_plugins();
        strom::gpu::detect_gpu_capabilities();
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

/// The flow's state as the API reports it, and the pipeline's own current
/// GStreamer state.
async fn states(
    app: &Router,
    state: &strom::state::AppState,
    id: &FlowId,
) -> (serde_json::Value, Option<gst::State>) {
    let (status, json) = request(app, "GET", &format!("/api/flows/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "GET flow: {json}");
    let current = state
        .pipelines_read()
        .await
        .get(id)
        .map(|m| m.pipeline().current_state());
    (json["flow"]["gst_state"].clone(), current)
}

async fn assert_reaches_playing(flow: Flow) {
    init_like_main();
    let test_state = common::state::new();
    let app = strom::create_app_with_state(test_state.app()).await;
    let id = flow.id;
    let name = flow.name.clone();

    let (status, json) = request(&app, "POST", "/api/flows", Some(&flow)).await;
    assert_eq!(status, StatusCode::CREATED, "create {name}: {json}");

    let (status, json) = request(&app, "POST", &format!("/api/flows/{id}/start"), None).await;
    assert_eq!(status, StatusCode::OK, "start {name}: {json}");

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let (api, current) = states(&app, &test_state, &id).await;
        if api == "Playing" && current == Some(gst::State::Playing) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{name} did not reach PLAYING within {START_TIMEOUT:?} on GStreamer {}: \
             API state {api}, pipeline state {current:?}",
            gst::version_string()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let held_until = Instant::now() + HOLD;
    while Instant::now() < held_until {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (api, current) = states(&app, &test_state, &id).await;
        assert!(
            api == "Playing" && current == Some(gst::State::Playing),
            "{name} left PLAYING on GStreamer {}: API state {api}, pipeline state {current:?}",
            gst::version_string()
        );
    }

    let (status, json) = request(&app, "POST", &format!("/api/flows/{id}/stop"), None).await;
    assert_eq!(status, StatusCode::OK, "stop {name}: {json}");
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
    common::require_elements(REQUIRED);
    assert_reaches_playing(audio_mixer_flow()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_vision_mixer_flow_reaches_playing() {
    common::require_elements(REQUIRED);
    assert_reaches_playing(vision_mixer_flow()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encoder_flow_reaches_playing() {
    common::require_elements(REQUIRED);
    assert_reaches_playing(encoder_flow()).await;
}
