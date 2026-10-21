//! `builtin.whip_output` with video (#839), built through the real builder.
//!
//! - **Connectivity**: the block requests a sink pad on `whipsink` /
//!   `whipclientsink` only for inputs a flow links. A declared video track
//!   left unlinked must not get a pad, or the sink waits for its caps forever
//!   and never negotiates the session, audio included.
//! - **Refusal**: raw video fails the flow with an error from the block's video
//!   input that names `builtin.videoenc`, not with `not-negotiated` from the
//!   source.
//! - **Loopback**: H.264 from the block reaches a real WHIP server
//!   (`whipserversrc`) as a video stream, with the audio beside it; and with
//!   the video track declared but unlinked, the audio still gets through.
//! - **Pipeline manager**: the same through a stored flow, with the block's
//!   computed pads, the manager's linking and setup hooks, and teardown with
//!   nothing left alive; also with both inputs fed from sometimes pads, whose
//!   links are still pending when the setup hook runs.

pub mod common;

use common::bus::BusError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::blocks::builtin::whip::WHIPOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE: &str = "whip";
const TEST_SOURCE: &str = "test_source";

/// Elements the block builds, plus the test sources.
const REQUIRED: &[&str] = &[
    "whipsink",
    "whipclientsink",
    "audioconvert",
    "audioresample",
    "opusenc",
    "rtpopuspay",
    "identity",
    "h264parse",
    "rtph264pay",
    "videotestsrc",
    "audiotestsrc",
    "capsfilter",
];

fn available(extra: &[&str]) -> bool {
    common::init_webrtc_plugins();
    let all: Vec<&str> = REQUIRED.iter().chain(extra.iter()).copied().collect();
    common::plugins_available(&all)
}

fn props(implementation: &str, endpoint: &str, num_video: u64) -> HashMap<String, PropertyValue> {
    let mut p = HashMap::new();
    p.insert(
        "implementation".to_string(),
        PropertyValue::String(implementation.to_string()),
    );
    p.insert(
        "whip_endpoint".to_string(),
        PropertyValue::String(endpoint.to_string()),
    );
    p.insert(
        "num_video_tracks".to_string(),
        PropertyValue::UInt(num_video),
    );
    p
}

/// The block built into a pipeline, with nothing linked to its inputs yet.
struct Built {
    pipeline: gst::Pipeline,
    ctx: BlockBuildContext,
    by_id: HashMap<String, gst::Element>,
}

impl Built {
    fn new(properties: HashMap<String, PropertyValue>) -> Self {
        let ctx = common::block::context();
        let built = WHIPOutputBuilder
            .build(INSTANCE, &properties, &ctx)
            .expect("whip_output builds");
        let pipeline = gst::Pipeline::new();
        let by_id = common::block::install(&pipeline, &built);
        Built {
            pipeline,
            ctx,
            by_id,
        }
    }

    fn element(&self, name: &str) -> &gst::Element {
        let id = format!("{}:{}", INSTANCE, name);
        self.by_id
            .get(&id)
            .unwrap_or_else(|| panic!("block builds {}", id))
    }

    /// Link `chain` (sources first) into the block's element `input`.
    fn feed(&self, chain: &[gst::Element], input: &str) {
        self.pipeline.add_many(chain).expect("add source");
        let mut all: Vec<&gst::Element> = chain.iter().collect();
        all.push(self.element(input));
        gst::Element::link_many(all).expect("link source to the block");
    }

    /// Run the block's setup hooks, as the pipeline manager does after linking.
    fn run_setups(&self) {
        common::block::run_setups(&self.ctx);
    }

    /// The sink's request pads, by name, with whether each is linked.
    fn sink_pads(&self, sink: &str) -> Vec<(String, bool)> {
        let mut pads: Vec<(String, bool)> = self
            .element(sink)
            .sink_pads()
            .iter()
            .map(|p| (p.name().to_string(), p.is_linked()))
            .collect();
        pads.sort();
        pads
    }
}

impl Drop for Built {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn make(factory: &str) -> gst::Element {
    gst::ElementFactory::make(factory)
        .build()
        .unwrap_or_else(|e| panic!("{}: {}", factory, e))
}

fn audio_source() -> Vec<gst::Element> {
    vec![gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .expect("audiotestsrc")]
}

fn raw_video_source() -> Vec<gst::Element> {
    let src = gst::ElementFactory::make("videotestsrc")
        .name(TEST_SOURCE)
        .property("is-live", true)
        .property("num-buffers", 60i32)
        .build()
        .expect("videotestsrc");
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    vec![src, filter]
}

/// videotestsrc -> x264enc, set up the way builtin.videoenc sets it up for
/// WebRTC: no B-frames, low latency, a keyframe every second.
fn h264_source() -> Vec<gst::Element> {
    let src = gst::ElementFactory::make("videotestsrc")
        .name(TEST_SOURCE)
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    let enc = gst::ElementFactory::make("x264enc")
        .property("bframes", 0u32)
        .property("key-int-max", 25u32)
        .property_from_str("tune", "zerolatency")
        .property_from_str("speed-preset", "ultrafast")
        .build()
        .expect("x264enc");
    vec![src, filter, enc]
}

// --- Connectivity ---------------------------------------------------------

#[test]
fn whipsink_audio_only_flow_is_unchanged() {
    if !available(&[]) {
        return;
    }
    let built = Built::new(props("whipsink", "http://192.0.2.1/whip", 0));
    built.feed(&audio_source(), "audioconvert");
    built.run_setups();

    assert_eq!(
        built.sink_pads("whipsink"),
        vec![("sink_0".to_string(), true)]
    );
    assert_eq!(built.element("rtpopuspay").property::<u32>("pt"), 96);
}

#[test]
fn whipsink_requests_no_pad_for_an_unlinked_video_track() {
    if !available(&[]) {
        return;
    }
    let built = Built::new(props("whipsink", "http://192.0.2.1/whip", 1));
    built.feed(&audio_source(), "audioconvert");
    built.run_setups();

    assert_eq!(
        built.sink_pads("whipsink"),
        vec![("sink_0".to_string(), true)],
        "only the linked audio track may have a whipsink pad"
    );
    assert!(!built
        .element("rtph264pay")
        .static_pad("src")
        .unwrap()
        .is_linked());
}

#[test]
fn whipsink_links_a_connected_video_track_with_its_own_payload_type() {
    if !available(&[]) {
        return;
    }
    let built = Built::new(props("whipsink", "http://192.0.2.1/whip", 1));
    built.feed(&audio_source(), "audioconvert");
    built.feed(&[make("videotestsrc")], "video_input");
    built.run_setups();

    assert_eq!(
        built.sink_pads("whipsink"),
        vec![("sink_0".to_string(), true), ("sink_1".to_string(), true)]
    );
    let audio_pt = built.element("rtpopuspay").property::<u32>("pt");
    let video_pt = built.element("rtph264pay").property::<u32>("pt");
    assert_eq!(audio_pt, 96);
    assert_ne!(audio_pt, video_pt, "each stream needs its own payload type");
    let peer = built
        .element("rtph264pay")
        .static_pad("src")
        .unwrap()
        .peer()
        .expect("video payloader is linked");
    assert_eq!(peer.name(), "sink_1");
}

#[test]
fn whipclientsink_requests_no_pad_for_an_unlinked_video_track() {
    if !available(&[]) {
        return;
    }
    let built = Built::new(props("whipclientsink", "http://192.0.2.1/whip", 1));
    built.feed(&audio_source(), "audioconvert");
    built.run_setups();

    assert_eq!(
        built.sink_pads("whipclientsink"),
        vec![("audio_0".to_string(), true)],
        "only the linked audio track may have a whipclientsink pad"
    );
    assert!(
        built
            .element("whipclientsink")
            .property::<gst::Caps>("video-caps")
            .is_empty(),
        "with no video linked, whipclientsink must offer no video codecs"
    );
}

#[test]
fn whipclientsink_links_a_connected_video_track() {
    if !available(&[]) {
        return;
    }
    let built = Built::new(props("whipclientsink", "http://192.0.2.1/whip", 1));
    built.feed(&audio_source(), "audioconvert");
    built.feed(&[make("videotestsrc")], "video_input");
    built.run_setups();

    assert_eq!(
        built.sink_pads("whipclientsink"),
        vec![("audio_0".to_string(), true), ("video_0".to_string(), true)]
    );
    assert!(
        !built
            .element("whipclientsink")
            .property::<gst::Caps>("video-caps")
            .is_empty(),
        "with video linked, whipclientsink must offer a video codec"
    );
}

// --- Refusal --------------------------------------------------------------

fn errors_after_raw_video(implementation: &str) -> Vec<BusError> {
    let built = Built::new(props(implementation, "http://192.0.2.1/whip", 1));
    built.feed(&raw_video_source(), "video_input");
    built.run_setups();
    let bus = built.pipeline.bus().expect("bus");
    built
        .pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");
    common::bus::collect_errors(&bus, Duration::from_secs(4))
}

fn assert_refused_by_block(errors: &[BusError]) {
    let input = format!("{}:video_input", INSTANCE);
    let from_block: Vec<&BusError> = errors.iter().filter(|e| e.source == input).collect();
    assert_eq!(
        from_block.len(),
        1,
        "expected exactly one error from {}, got: {:?}",
        input,
        errors
    );
    assert!(
        from_block[0].message.contains("builtin.videoenc"),
        "the refusal must name builtin.videoenc: {:?}",
        from_block[0]
    );
    assert!(
        !errors.iter().any(|e| e.source == TEST_SOURCE),
        "the refused stream must not come back upstream as not-negotiated: {:?}",
        errors
    );
}

#[test]
fn whipsink_refuses_raw_video_by_failing_the_flow() {
    if !available(&[]) {
        return;
    }
    assert_refused_by_block(&errors_after_raw_video("whipsink"));
}

#[test]
fn whipclientsink_refuses_raw_video_by_failing_the_flow() {
    if !available(&[]) {
        return;
    }
    assert_refused_by_block(&errors_after_raw_video("whipclientsink"));
}

// --- Loopback -------------------------------------------------------------

/// A standalone `whipserversrc` on a free local port, counting the audio and
/// video buffers it receives.
struct Server {
    pipeline: gst::Pipeline,
    endpoint: String,
    video_buffers: Arc<AtomicUsize>,
    audio_buffers: Arc<AtomicUsize>,
}

impl Server {
    fn start() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let pipeline = gst::Pipeline::new();
        let src = make("whipserversrc");
        src.set_property("stun-server", None::<&str>);
        src.property::<gst::glib::Object>("signaller")
            .set_property("host-addr", format!("http://127.0.0.1:{}", port));
        src.set_property("video-codecs", gst::Array::new(["H264"]));
        src.set_property("audio-codecs", gst::Array::new(["OPUS"]));
        pipeline.add(&src).expect("add whipserversrc");

        let video_buffers = Arc::new(AtomicUsize::new(0));
        let audio_buffers = Arc::new(AtomicUsize::new(0));
        let pipeline_weak = pipeline.downgrade();
        let video_counter = video_buffers.clone();
        let audio_counter = audio_buffers.clone();
        src.connect_pad_added(move |_, pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .property("async", false)
                .build()
                .expect("fakesink");
            pipeline.add(&sink).expect("add fakesink");
            sink.sync_state_with_parent().expect("sync fakesink");
            pad.link(&sink.static_pad("sink").unwrap())
                .expect("link server pad");
            let counter = if pad.name().starts_with("video") {
                video_counter.clone()
            } else {
                audio_counter.clone()
            };
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        });

        pipeline
            .set_state(gst::State::Playing)
            .expect("server pipeline plays");
        Server {
            pipeline,
            endpoint: format!("http://127.0.0.1:{}/whip/endpoint", port),
            video_buffers,
            audio_buffers,
        }
    }

    fn received(&self) -> (usize, usize) {
        (
            self.audio_buffers.load(Ordering::Relaxed),
            self.video_buffers.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Publish from the block to a real WHIP server, with one video track
/// declared, and return `(audio, video)` buffers the server received. Video is
/// linked only when `link_video` is set. Stops early once enough has arrived.
fn loopback(implementation: &str, link_video: bool) -> (usize, usize) {
    let server = Server::start();
    let built = Built::new(props(implementation, &server.endpoint, 1));
    built.feed(&audio_source(), "audioconvert");
    if link_video {
        built.feed(&h264_source(), "video_input");
    }
    built.run_setups();
    let bus = built.pipeline.bus().expect("bus");
    built
        .pipeline
        .set_state(gst::State::Playing)
        .expect("publisher plays");

    let enough = |(audio, video): (usize, usize)| audio >= 10 && (!link_video || video >= 10);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !enough(server.received()) {
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Error],
        ) {
            if let gst::MessageView::Error(e) = msg.view() {
                panic!(
                    "publisher error from {:?}: {} ({:?})",
                    msg.src().map(|s| s.name()),
                    e.error(),
                    e.debug()
                );
            }
        }
    }
    server.received()
}

const LOOPBACK_ELEMENTS: &[&str] = &["whipserversrc", "x264enc", "fakesink"];

#[test]
fn whipsink_publishes_h264_to_a_whip_server() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    let (audio, video) = loopback("whipsink", true);
    assert!(video >= 10, "the server received {video} video buffers");
    assert!(audio >= 10, "the server received {audio} audio buffers");
}

#[test]
fn whipclientsink_publishes_h264_to_a_whip_server() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    let (audio, video) = loopback("whipclientsink", true);
    assert!(video >= 10, "the server received {video} video buffers");
    assert!(audio >= 10, "the server received {audio} audio buffers");
}

/// The failure the connectivity guard prevents: a video track declared and
/// left unlinked must not hold the session back. Audio has to get through.
#[test]
fn whipsink_publishes_audio_with_a_video_track_left_unlinked() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    let (audio, _) = loopback("whipsink", false);
    assert!(audio >= 10, "the server received {audio} audio buffers");
}

#[test]
fn whipclientsink_publishes_audio_with_a_video_track_left_unlinked() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    let (audio, _) = loopback("whipclientsink", false);
    assert!(audio >= 10, "the server received {audio} audio buffers");
}

// --- Through the pipeline manager ----------------------------------------

/// How the flow's sources reach the block's inputs.
#[derive(Clone, Copy, Debug)]
enum Upstream {
    /// Straight from the encoder and the audio source: linked at construction.
    Direct,
    /// Through `parsebin` (video) and `decodebin` (audio), whose src pads are
    /// sometimes pads. Construction cannot link them, so both links wait in
    /// `pending_links` while the block's setup hook runs, and are made only
    /// when data flows.
    SometimesPads,
}

/// A flow as the API stores it: raw elements encode H.264 and feed the block's
/// `video_in`, a raw audio source feeds `audio_in`.
fn flow_into_whip_output(
    implementation: &str,
    endpoint: &str,
    upstream: Upstream,
) -> strom_types::Flow {
    use strom_types::{Element, Flow, Link};

    let element = |id: &str, kind: &str, props: &[(&str, PropertyValue)]| Element {
        id: id.to_string(),
        element_type: kind.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    };

    let mut flow = Flow::new("whip_output_video");
    flow.elements.push(element(
        "vsrc",
        "videotestsrc",
        &[("is-live", PropertyValue::Bool(true))],
    ));
    // 4:2:0, or x264enc picks Y444 and High 4:4:4, which the block refuses.
    flow.elements.push(element(
        "vcaps",
        "capsfilter",
        &[(
            "caps",
            PropertyValue::String("video/x-raw,format=I420".to_string()),
        )],
    ));
    flow.elements.push(element(
        "venc",
        "x264enc",
        &[
            ("bframes", PropertyValue::UInt(0)),
            ("key-int-max", PropertyValue::UInt(25)),
            ("tune", PropertyValue::String("zerolatency".to_string())),
            (
                "speed-preset",
                PropertyValue::String("ultrafast".to_string()),
            ),
        ],
    ));
    flow.elements.push(element(
        "asrc",
        "audiotestsrc",
        &[("is-live", PropertyValue::Bool(true))],
    ));

    if let Upstream::SometimesPads = upstream {
        flow.elements.push(element("vparse", "parsebin", &[]));
        flow.elements.push(element("adec", "decodebin", &[]));
    }

    let properties = props(implementation, endpoint, 1);
    flow.blocks.push(strom_types::BlockInstance {
        id: "out".to_string(),
        block_definition_id: "builtin.whip_output".to_string(),
        name: None,
        properties: properties.clone(),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: WHIPOutputBuilder.get_external_pads(&properties),
    });

    let mut links = vec![("vsrc:src", "vcaps:sink"), ("vcaps:src", "venc:sink")];
    match upstream {
        Upstream::Direct => {
            links.push(("venc:src", "out:video_in"));
            links.push(("asrc:src", "out:audio_in"));
        }
        Upstream::SometimesPads => {
            links.push(("venc:src", "vparse:sink"));
            links.push(("vparse:src", "out:video_in"));
            links.push(("asrc:src", "adec:sink"));
            links.push(("adec:src", "out:audio_in"));
        }
    }
    for (from, to) in links {
        flow.links.push(Link {
            from: from.to_string(),
            to: to.to_string(),
        });
    }
    flow
}

/// The real path: expansion with the computed pads, the pipeline manager's
/// linking and setup hooks, and teardown. Video must reach the server, and
/// nothing the block built may outlive the flow.
fn publishes_through_the_manager(implementation: &str, upstream: Upstream) {
    let server = Server::start();
    let flow = flow_into_whip_output(implementation, &server.endpoint, upstream);
    let mut manager = common::manager::build(&flow).expect("PipelineManager builds");
    manager.start().expect("flow starts");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && {
        let (audio, video) = server.received();
        audio < 10 || video < 10
    } {
        std::thread::sleep(Duration::from_millis(100));
    }
    let (audio, video) = server.received();

    let pipeline_weak = manager.pipeline_weak();
    let element_weak_refs = manager.element_weak_refs();
    manager.stop().expect("flow stops");
    drop(manager);

    assert!(
        video >= 10 && audio >= 10,
        "the WHIP server received {} video and {} audio buffers from the flow \
         ({}, {:?}) in 20 s",
        video,
        audio,
        implementation,
        upstream
    );
    assert!(
        pipeline_weak.upgrade().is_none(),
        "pipeline still alive after drop: a closure holds a strong reference"
    );
    let leaked: Vec<_> = element_weak_refs
        .iter()
        .filter_map(|(name, weak)| weak.upgrade().map(|_| name.clone()))
        .collect();
    assert!(leaked.is_empty(), "elements alive after drop: {:?}", leaked);
}

/// The pipeline manager spawns tokio tasks, so it needs a runtime, but the
/// WebRTC elements block on their own runtime while they stop, which tokio
/// refuses on a worker thread. A blocking-pool thread has both.
async fn on_blocking_thread(implementation: &'static str, upstream: Upstream) {
    if let Err(e) =
        tokio::task::spawn_blocking(move || publishes_through_the_manager(implementation, upstream))
            .await
    {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whipsink_flow_publishes_video_and_tears_down() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    on_blocking_thread("whipsink", Upstream::Direct).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whipclientsink_flow_publishes_video_and_tears_down() {
    if !available(LOOPBACK_ELEMENTS) {
        return;
    }
    on_blocking_thread("whipclientsink", Upstream::Direct).await;
}

/// A link into the block from a sometimes pad is still pending when the setup
/// hook decides which inputs get a sink pad. It must count as connected, or
/// the track gets no pad and the late link delivers into nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whipsink_flow_publishes_through_sometimes_pads() {
    if !available(&[LOOPBACK_ELEMENTS, &["parsebin", "decodebin"]].concat()) {
        return;
    }
    on_blocking_thread("whipsink", Upstream::SometimesPads).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whipclientsink_flow_publishes_through_sometimes_pads() {
    if !available(&[LOOPBACK_ELEMENTS, &["parsebin", "decodebin"]].concat()) {
        return;
    }
    on_blocking_thread("whipclientsink", Upstream::SometimesPads).await;
}
