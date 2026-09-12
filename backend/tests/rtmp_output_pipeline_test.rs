//! Pipeline tests for `builtin.rtmp_output`: the half the unit suite cannot reach.
//!
//! `rtmp_output_test.rs` covers the codec decision through `video_plan` and
//! `audio_plan`, which are pure. It does not reach the pad probes that act on
//! those decisions, so deleting both `add_probe` blocks leaves that suite green
//! while the block does nothing at all.
//!
//! **The real sink is deliberately left out of the pipeline.** These tests are
//! about the pad probes: which chain gets built for which caps, and which mux
//! pads get reserved. None of that involves the sink, so `flvmux` is linked to a
//! `fakesink` instead and `rtmp2sink` is built but never added. That keeps the
//! test off the network entirely, which matters: with the real sink in a
//! pipeline reaching PLAYING, GIO resolves proxy settings, and on a host without
//! the `org.gnome.system.proxy` GSettings schema that is a fatal `GLib-GIO-ERROR`
//! rather than a connection failure. Their CI runner is such a host, which this
//! file discovered the hard way. The sink's own configuration is covered by
//! `rtmp_output_test.rs`, which builds it without running it.
//!
//! **No RTMP server is needed, which is why these tests exist.** An earlier
//! draft of the block claimed the opposite and used it to justify the gap. The
//! probes fire on the CAPS event, so both chains are built and linked whether or
//! not anything is listening; `rtmp2sink` posts a connect error on the bus and
//! that is orthogonal to graph construction upstream of it. So these tests point
//! the sink at a closed port on purpose and assert on the graph rather than the
//! socket, tolerating a sink-sourced bus error.
//!
//! What each test pins down:
//!
//! 1. H.264 plus raw audio builds the parser and the full encode chain.
//! 2. H.264 plus AAC parses both and does NOT run a second audio encoder.
//! 3. Raw video is refused without taking the audio side down with it, and
//!    requests no `video` pad. This is the aggregator stall from the other
//!    suite's point 2, proven at runtime rather than at build time.
//! 4. An input that is never fed requests no pad at all, for the same reason.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::rtmp::RtmpOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder, ElementSetupFn};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE: &str = "rtmp0";

/// Elements these tests need beyond core GStreamer.
const REQUIRED: &[&str] = &[
    "rtmp2sink",
    "flvmux",
    "h264parse",
    "aacparse",
    "avenc_aac",
    "identity",
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "x264enc",
    "fakesink",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    gst::init().expect("gst init");
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    eprintln!(
        "skipping: missing GStreamer elements: {}",
        missing.join(", ")
    );
    false
}

/// The location the block is configured with. Nothing ever connects to it: the
/// sink is built to satisfy the builder and then left out of the pipeline.
const DEAD_LOCATION: &str = "rtmp://127.0.0.1:1/live/nothing";

struct Harness {
    pipeline: gst::Pipeline,
    mux: gst::Element,
    /// Held rather than run in `new`, because the real pipeline runs these only
    /// after every block is linked. Running them before the test sources are
    /// attached would report both inputs unconnected and reserve nothing.
    setups: std::cell::RefCell<Vec<ElementSetupFn>>,
}

impl Harness {
    /// Build the real block, put it in a pipeline, and apply the internal links
    /// exactly as the pipeline manager does. Without those links `flvmux:src`
    /// never reaches the sink.
    fn new() -> Self {
        let mut props = HashMap::new();
        props.insert(
            "location".to_string(),
            PropertyValue::String(DEAD_LOCATION.to_string()),
        );
        // sync=false so a dead sink cannot pace the graph while we wait.
        props.insert("sync".to_string(), PropertyValue::Bool(false));

        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let built = RtmpOutputBuilder
            .build(INSTANCE, &props, &ctx)
            .expect("rtmp_output block builds");

        let sink_id = format!("{}:rtmp_sink", INSTANCE);
        let pipeline = gst::Pipeline::new();
        let mut by_id: HashMap<String, gst::Element> = HashMap::new();
        for (id, element) in &built.elements {
            // Everything but the sink; see the header for why it stays out.
            if id == &sink_id {
                continue;
            }
            pipeline.add(element).expect("add block element");
            by_id.insert(id.clone(), element.clone());
        }
        for (from, to) in &built.internal_links {
            // The only declared link is flvmux:src to the sink, which is not here.
            if from.element_id == sink_id || to.element_id == sink_id {
                continue;
            }
            let src = by_id
                .get(&from.element_id)
                .unwrap_or_else(|| panic!("internal link source {} missing", from.element_id));
            let sink = by_id
                .get(&to.element_id)
                .unwrap_or_else(|| panic!("internal link target {} missing", to.element_id));
            match (&from.pad_name, &to.pad_name) {
                (Some(src_pad), Some(sink_pad)) => {
                    let src_pad = src
                        .static_pad(src_pad)
                        .unwrap_or_else(|| panic!("{} has no pad {}", from.element_id, src_pad));
                    let sink_pad = sink
                        .static_pad(sink_pad)
                        .unwrap_or_else(|| panic!("{} has no pad {}", to.element_id, sink_pad));
                    src_pad
                        .link(&sink_pad)
                        .unwrap_or_else(|e| panic!("internal pad link failed: {:?}", e));
                }
                _ => src
                    .link(sink)
                    .unwrap_or_else(|e| panic!("internal element link failed: {:?}", e)),
            }
        }

        let mux = by_id
            .get(&format!("{}:rtmp_flvmux", INSTANCE))
            .expect("block builds a flvmux")
            .clone();

        // Stand in for the sink so flvmux has somewhere to push. async=false for
        // the same reason the block sets it on the real sink: a sink waiting to
        // preroll would hold the pipeline out of PLAYING.
        let fake = gst::ElementFactory::make("fakesink")
            .name("standin_for_rtmp_sink")
            .property("async", false)
            .property("sync", false)
            .build()
            .expect("fakesink");
        pipeline.add(&fake).expect("add fakesink");
        mux.link(&fake).expect("link flvmux to the standin");

        Harness {
            pipeline,
            mux,
            setups: std::cell::RefCell::new(ctx.take_element_setups()),
        }
    }

    fn input(&self, suffix: &str) -> gst::Element {
        self.pipeline
            .by_name(&format!("{}:rtmp_{}", INSTANCE, suffix))
            .unwrap_or_else(|| panic!("block builds a {}", suffix))
    }

    /// Feed H.264 into the video input.
    fn feed_h264(&self) {
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 30i32)
            .property("is-live", true)
            .build()
            .expect("videotestsrc");
        let caps = gst::ElementFactory::make("capsfilter")
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
        let enc = gst::ElementFactory::make("x264enc")
            .property("key-int-max", 10u32)
            .property_from_str("tune", "zerolatency")
            .build()
            .expect("x264enc");
        self.pipeline
            .add_many([&src, &caps, &enc])
            .expect("add video source");
        gst::Element::link_many([&src, &caps, &enc]).expect("link video source");
        enc.link(&self.input("video_input")).expect("link to block");
    }

    /// Feed raw video into the video input, which the block must refuse.
    fn feed_raw_video(&self) {
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 30i32)
            .property("is-live", true)
            .build()
            .expect("videotestsrc");
        let caps = gst::ElementFactory::make("capsfilter")
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
        self.pipeline
            .add_many([&src, &caps])
            .expect("add raw video source");
        gst::Element::link_many([&src, &caps]).expect("link raw video source");
        caps.link(&self.input("video_input"))
            .expect("link to block");
    }

    /// Feed raw audio into the audio input.
    fn feed_raw_audio(&self) {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 30i32)
            .property("is-live", true)
            .build()
            .expect("audiotestsrc");
        let conv = gst::ElementFactory::make("audioconvert")
            .build()
            .expect("audioconvert");
        self.pipeline
            .add_many([&src, &conv])
            .expect("add audio source");
        gst::Element::link_many([&src, &conv]).expect("link audio source");
        conv.link(&self.input("audio_input"))
            .expect("link to block");
    }

    /// Feed already-encoded AAC into the audio input.
    fn feed_aac(&self) {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 30i32)
            .property("is-live", true)
            .build()
            .expect("audiotestsrc");
        let conv = gst::ElementFactory::make("audioconvert")
            .build()
            .expect("audioconvert");
        let resample = gst::ElementFactory::make("audioresample")
            .build()
            .expect("audioresample");
        let enc = gst::ElementFactory::make("avenc_aac")
            .build()
            .expect("avenc_aac");
        self.pipeline
            .add_many([&src, &conv, &resample, &enc])
            .expect("add aac source");
        gst::Element::link_many([&src, &conv, &resample, &enc]).expect("link aac source");
        enc.link(&self.input("audio_input")).expect("link to block");
    }

    /// Run the element-setup hooks: the window after every block is linked and
    /// before the pipeline leaves NULL. The block reserves its `flvmux` pads
    /// here, so a harness that skipped this would exercise the late-pad fallback
    /// rather than the path that ships.
    fn finish_linking(&self) {
        let flow_id = strom_types::flow::FlowId::new_v4();
        let events = EventBroadcaster::new(16);
        for setup in self.setups.borrow_mut().drain(..) {
            setup(flow_id, events.clone());
        }
    }

    fn start(&self) {
        self.pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline goes to PLAYING");
    }

    fn has_child(&self, suffix: &str) -> bool {
        self.pipeline
            .by_name(&format!("{}:rtmp_{}", INSTANCE, suffix))
            .is_some()
    }

    /// The names of the sink pads currently requested on `flvmux`.
    fn mux_pads(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .mux
            .sink_pads()
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        names.sort();
        names
    }

    /// Poll until the condition holds or the deadline passes. Polling rather
    /// than sleeping a fixed time: the chains appeared about 50 ms after PLAYING
    /// when this was measured, and a fixed sleep is how these tests go flaky.
    fn wait_until(&self, what: &str, mut cond: impl FnMut(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {}. children: h264parse={} aacparse={} encoder={}, mux pads={:?}",
            what,
            self.has_child("h264parse"),
            self.has_child("aacparse"),
            self.has_child("audio_encoder"),
            self.mux_pads()
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[test]
fn h264_and_raw_audio_build_the_parser_and_the_encode_chain() {
    if !plugins_available() {
        return;
    }
    let h = Harness::new();
    h.feed_h264();
    h.feed_raw_audio();
    h.finish_linking();

    // Before PLAYING, and this is the assertion that matters: the muxer has not
    // written the FLV header yet, so a pad present here is one the header will
    // declare. Measured on 1.28.6 with this block's topology, requesting the pads
    // from the caps probes instead gave `hasAudio=False` in the header while 128
    // audio tags sat in the body.
    assert_eq!(
        h.mux_pads(),
        vec!["audio".to_string(), "video".to_string()],
        "both pads must be reserved before the pipeline starts"
    );

    h.start();

    h.wait_until("both chains", |h| {
        h.has_child("h264parse") && h.has_child("aacparse")
    });
    assert!(
        h.has_child("audio_encoder"),
        "raw audio must be encoded inside the block, so avenc_aac has to exist"
    );
    assert!(h.has_child("audio_convert") && h.has_child("audio_resample"));
    assert_eq!(
        h.mux_pads(),
        vec!["audio".to_string(), "video".to_string()],
        "both pads must be reserved by the setup hook, not requested from the \
         probes: flvmux writes the FLV header at its first aggregation, so a pad \
         added later is never declared in it"
    );
}

#[test]
fn aac_input_is_parsed_without_running_a_second_encoder() {
    if !plugins_available() {
        return;
    }
    let h = Harness::new();
    h.feed_h264();
    h.feed_aac();
    h.finish_linking();
    h.start();

    h.wait_until("both chains", |h| {
        h.has_child("h264parse") && h.has_child("aacparse")
    });
    assert!(
        !h.has_child("audio_encoder"),
        "AAC arrives encoded, so encoding it again would be a second lossy pass"
    );
    assert_eq!(h.mux_pads().len(), 2);
}

#[test]
fn raw_video_is_refused_without_taking_the_audio_side_down() {
    if !plugins_available() {
        return;
    }
    let h = Harness::new();
    h.feed_raw_video();
    h.feed_raw_audio();
    h.finish_linking();
    h.start();

    // The audio side must come up on its own.
    h.wait_until("the audio chain", |h| h.has_child("aacparse"));

    assert!(
        !h.has_child("h264parse"),
        "raw video is refused, so no video parser should be built"
    );
    // The video input IS connected, so the setup hook reserves a video pad before
    // the codec is known. The refusal has to hand it back, or the pad sits there
    // never carrying data and stops flvmux aggregating, which would take the
    // audio down too.
    h.wait_until("the refused video pad to be released", |h| {
        h.mux_pads() == vec!["audio".to_string()]
    });
}

#[test]
fn an_input_that_is_never_fed_requests_no_pad() {
    if !plugins_available() {
        return;
    }
    let h = Harness::new();
    h.feed_h264();
    // Audio deliberately not fed, so its input stays unconnected.
    h.finish_linking();
    h.start();

    h.wait_until("the video chain", |h| h.has_child("h264parse"));

    assert!(!h.has_child("aacparse") && !h.has_child("audio_encoder"));
    assert_eq!(
        h.mux_pads(),
        vec!["video".to_string()],
        "an unconnected audio input must get no pad at all, which is what lets a \
         video-only flow work: an aggregator pad that never carries data stops \
         the muxer aggregating"
    );
}
