//! Regression tests for `builtin.rtmp_output`, built through the real
//! `RtmpOutputBuilder`.
//!
//! `build_time` checks the elements the block builds without running them;
//! `pipeline` runs the pad probes that act on the codec decision. The codec
//! decision itself (`video_plan`, `audio_plan`) and the location parsing and
//! redaction are pure, and are unit-tested next to them in `rtmp.rs`.

pub mod common;

use std::collections::HashMap;
use strom::blocks::builtin::rtmp::RtmpOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare image.
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

fn plugins_available() -> bool {
    common::plugins_available(REQUIRED)
}

/// Three properties, each of which was wrong in a first draft of the block and
/// each of which is silent when it breaks:
///
/// 1. **The sink is not async.** Block-built elements bypass `add_element`, so
///    nothing sets `async=false` for them. A sink that waits to preroll holds
///    the whole pipeline out of PLAYING, and the flow simply never starts.
/// 2. **Nothing links to `flvmux` at build time.** An aggregator sink pad
///    requested for an input that never carries data means the muxer never
///    aggregates and the sink receives nothing, which looks like a dead server
///    rather than a graph mistake.
/// 3. **The declared properties map onto what the block builds.** A mapping or
///    pad naming an element the block does not build goes nowhere at runtime.
///
/// Plus the location contract as it reaches the sink: trimmed, refused when the
/// sink cannot parse it, and `rtmps://` left strict.
mod build_time {
    use super::*;

    fn build(properties: HashMap<String, PropertyValue>) -> strom::blocks::BlockBuildResult {
        gst::init().expect("gst init");
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        RtmpOutputBuilder
            .build("rtmp0", &properties, &ctx)
            .expect("rtmp_output should build")
    }

    fn element<'a>(result: &'a strom::blocks::BlockBuildResult, suffix: &str) -> &'a gst::Element {
        result
            .elements
            .iter()
            .find(|(id, _)| id.ends_with(suffix))
            .map(|(_, e)| e)
            .unwrap_or_else(|| panic!("no element ending in {}", suffix))
    }

    /// `rtmp2sink` parses `location` and re-serialises it, and that drops the
    /// default port on at least GStreamer 1.28, so `rtmp://h:1935/p` reads back as
    /// `rtmp://h/p`. Compare through this rather than pinning one version's
    /// normalisation: the runtime image is on 1.26 and CI must not depend on which
    /// of the two behaviours it has.
    fn without_default_port(url: &str) -> String {
        url.replace(":1935/", "/")
    }

    #[test]
    fn the_sink_is_not_async() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        let sink = element(&result, ":rtmp_sink");
        assert!(
            !sink.property::<bool>("async"),
            "the RTMP sink is async, so it will wait to preroll and hold the pipeline \
             out of PLAYING. Block-built elements bypass add_element, so this has to \
             be set here"
        );
        assert!(
            sink.property::<bool>("qos"),
            "qos should be on so the sink can report back pressure upstream"
        );
    }

    #[test]
    fn nothing_is_linked_to_the_muxer_at_build_time() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        assert_eq!(
            result.internal_links.len(),
            1,
            "only flvmux -> sink may be linked statically. A mux sink pad requested \
             for an input that never carries data means the muxer never aggregates: \
             {:?}",
            result.internal_links
        );
        let (from, to) = &result.internal_links[0];
        assert!(from.element_id.ends_with(":rtmp_flvmux"));
        assert!(to.element_id.ends_with(":rtmp_sink"));
    }

    #[test]
    fn both_inputs_exist_and_are_identities() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        for suffix in [":rtmp_video_input", ":rtmp_audio_input"] {
            let input = element(&result, suffix);
            assert_eq!(
                input.factory().map(|f| f.name().to_string()).as_deref(),
                Some("identity"),
                "{} should be an identity, so a probe can insert the real chain",
                suffix
            );
        }
    }

    #[test]
    fn the_location_default_is_used_when_absent_or_blank() {
        if !plugins_available() {
            return;
        }
        for properties in [
            HashMap::new(),
            HashMap::from([(
                "location".to_string(),
                PropertyValue::String("   ".to_string()),
            )]),
        ] {
            let result = build(properties);
            assert_eq!(
                without_default_port(
                    &element(&result, ":rtmp_sink").property::<String>("location")
                ),
                without_default_port(strom_types::DEFAULT_RTMP_LOCATION),
                "a blank location should fall back to the default rather than \
                 publishing to an empty URL"
            );
        }
    }

    #[test]
    fn the_location_and_sync_properties_reach_the_sink() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::from([
            (
                // A non-default port on purpose: 1935 is the one `rtmp2sink`
                // normalises away, so using it here would mean the suite never
                // asserts that a port reaches the sink at all.
                "location".to_string(),
                PropertyValue::String("rtmp://192.0.2.10:1936/live/key".to_string()),
            ),
            ("sync".to_string(), PropertyValue::Bool(false)),
        ]));
        let sink = element(&result, ":rtmp_sink");
        assert_eq!(
            sink.property::<String>("location"),
            "rtmp://192.0.2.10:1936/live/key",
            "a non-default port must survive verbatim into the sink"
        );
        assert!(!sink.property::<bool>("sync"));
    }

    #[test]
    fn the_muxer_is_streamable() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        assert!(
            element(&result, ":rtmp_flvmux").property::<bool>("streamable"),
            "the non-streamable form rewrites the header at end of file, and a live \
             stream has no end"
        );
    }

    #[test]
    fn every_mapping_names_an_element_the_block_builds() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
        for property in &definition.exposed_properties {
            let suffix = format!(":{}", property.mapping.element_id);
            assert!(
                result
                    .elements
                    .iter()
                    .any(|(id, _)| id.ends_with(suffix.as_str())),
                "property {} maps to element {}, which the block does not build, so a \
                 runtime update would silently go nowhere",
                property.name,
                property.mapping.element_id
            );
        }
    }

    #[test]
    fn every_declared_pad_names_an_element_the_block_builds() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::new());
        let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
        for pad in &definition.external_pads.inputs {
            let suffix = format!(":{}", pad.internal_element_id);
            assert!(
                result
                    .elements
                    .iter()
                    .any(|(id, _)| id.ends_with(suffix.as_str())),
                "pad {} points at element {}, which the block does not build, so the \
                 graph would fail to link",
                pad.name,
                pad.internal_element_id
            );
        }
        assert!(
            definition.external_pads.outputs.is_empty(),
            "an output block has no outputs"
        );
    }

    #[test]
    fn the_trimmed_location_is_what_reaches_the_sink() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::from([(
            "location".to_string(),
            PropertyValue::String("  rtmps://192.0.2.10/live/key  ".to_string()),
        )]));
        let sink = element(&result, ":rtmp_sink");
        assert_eq!(
            sink.property::<String>("location"),
            "rtmps://192.0.2.10/live/key",
            "an untrimmed location would reach rtmp2sink as rtmp:/ with the scheme fallen back"
        );
    }

    /// The block must refuse rather than build an output whose sink silently holds a
    /// degenerate location. Verified against the real element, not a prediction.
    #[test]
    fn a_location_the_sink_cannot_parse_is_refused_at_build_time() {
        if !plugins_available() {
            return;
        }
        gst::init().expect("gst init");
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let props = HashMap::from([(
            "location".to_string(),
            PropertyValue::String("rtmp://user:p/ss@192.0.2.10/live/key".to_string()),
        )]);
        let message = match RtmpOutputBuilder.build("rtmp0", &props, &ctx) {
            Ok(_) => panic!("a location rtmp2sink reduces to rtmp:/ must not build"),
            Err(e) => e.to_string(),
        };
        assert!(
            !message.contains("p/ss"),
            "the refusal leaks the password: {}",
            message
        );
        assert!(
            message.contains("%2F"),
            "should hint at percent-encoding: {}",
            message
        );
    }

    #[test]
    fn an_rtmps_location_reaches_the_sink_and_sets_its_scheme() {
        if !plugins_available() {
            return;
        }
        let result = build(HashMap::from([(
            "location".to_string(),
            PropertyValue::String("rtmps://192.0.2.10/live/key".to_string()),
        )]));
        let sink = element(&result, ":rtmp_sink");
        assert_eq!(
            sink.property::<String>("location"),
            "rtmps://192.0.2.10/live/key"
        );
        // The sink's own scheme enum, set from the URL rather than by us. Compared
        // as an integer because the generated Rust enum is not re-exported here;
        // `gst-inspect-1.0 rtmp2sink` documents 0 = rtmp, 1 = rtmps.
        let scheme = sink.property_value("scheme");
        assert_eq!(
            scheme
                .transform::<i32>()
                .ok()
                .and_then(|v| v.get::<i32>().ok()),
            Some(1),
            "an rtmps:// location must leave the sink's scheme on rtmps, got {:?}",
            scheme
        );
        // Certificate validation must stay strict. This block exposes no way to
        // weaken it, so a flow gets the sink's secure default: validate-all, which
        // is 0x7f. If that ever reads as anything else, someone has added a switch.
        let flags = sink.property_value("tls-validation-flags");
        assert_eq!(
            flags
                .transform::<u32>()
                .ok()
                .and_then(|v| v.get::<u32>().ok()),
            Some(0x7f),
            "tls-validation-flags should still be validate-all, got {:?}",
            flags
        );
    }
}

/// The pad probes: the half the other tests cannot reach.
///
/// The unit tests in `rtmp.rs` cover the codec decision through `video_plan`
/// and `audio_plan`, which are pure, and `build_time` only builds the block. Neither
/// reaches the pad probes that act on those decisions, so deleting both
/// `add_probe` blocks leaves them green while the block does nothing at all.
///
/// **The real sink is deliberately left out of the pipeline.** These tests are
/// about the pad probes: which chain gets built for which caps, and which mux
/// pads get reserved. None of that involves the sink, so `flvmux` is linked to a
/// `fakesink` instead and `rtmp2sink` is built but never added. That keeps the
/// test off the network entirely, which matters: with the real sink in a
/// pipeline reaching PLAYING, GIO resolves proxy settings, and on a host without
/// the `org.gnome.system.proxy` GSettings schema that is a fatal `GLib-GIO-ERROR`
/// rather than a connection failure. Their CI runner is such a host, which this
/// file discovered the hard way. The sink's own configuration is covered by
/// `build_time`, which builds it without running it.
///
/// **No RTMP server is needed, which is why these tests exist.** An earlier
/// draft of the block claimed the opposite and used it to justify the gap. The
/// probes fire on the CAPS event, so both chains are built and linked whether or
/// not anything is listening; `rtmp2sink` posts a connect error on the bus and
/// that is orthogonal to graph construction upstream of it. So these tests point
/// the sink at a closed port on purpose and assert on the graph rather than the
/// socket, tolerating a sink-sourced bus error.
///
/// What each test pins down:
///
/// 1. H.264 plus raw audio builds the parser and the full encode chain.
/// 2. H.264 plus AAC parses both and does NOT run a second audio encoder.
/// 3. Raw video is refused without taking the audio side down with it, and
///    requests no `video` pad. This is the aggregator stall from
///    `build_time`'s point 2, proven at runtime rather than at build time. The
///    refusal is the only error on the bus, posted by the block and naming
///    `builtin.videoenc`.
/// 4. An input that is never fed requests no pad at all, for the same reason.
mod pipeline {
    use super::*;
    use std::time::{Duration, Instant};
    use strom::blocks::ElementSetupFn;
    use strom::events::EventBroadcaster;

    const INSTANCE: &str = "rtmp0";

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

            let ctx = common::block::context();
            let built = RtmpOutputBuilder
                .build(INSTANCE, &props, &ctx)
                .expect("rtmp_output block builds");

            let sink_id = format!("{}:rtmp_sink", INSTANCE);
            let pipeline = gst::Pipeline::new();
            // Everything but the sink; see the header for why it stays out.
            let by_id = common::block::install_except(&pipeline, &built, &[&sink_id]);

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
                        // 8-bit 4:2:0, so x264enc picks High. Left open, it follows
                        // videotestsrc's first format into a profile the block refuses.
                        .field("format", "I420")
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

        /// Feed H.264 in the High 4:4:4 profile, which `x264enc` picks for a Y444
        /// input: the case #783 is about, and one `flvmux` accepts without a word.
        fn feed_h264_444(&self) {
            let src = gst::ElementFactory::make("videotestsrc")
                .property("num-buffers", 30i32)
                .property("is-live", true)
                .build()
                .expect("videotestsrc");
            let caps = gst::ElementFactory::make("capsfilter")
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", "Y444")
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

        /// Every error the bus carries over the next few seconds, as
        /// (posting element's name, message).
        fn collect_errors(&self, bus: &gst::Bus) -> Vec<(String, String)> {
            let mut errors = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Some(msg) = bus.timed_pop_filtered(
                    gst::ClockTime::from_mseconds(100),
                    &[gst::MessageType::Error],
                ) {
                    if let gst::MessageView::Error(err) = msg.view() {
                        let source = msg.src().map(|s| s.name().to_string()).unwrap_or_default();
                        errors.push((source, err.error().to_string()));
                    }
                }
            }
            errors
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
            let events = EventBroadcaster::with_capacity(16);
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
        let bus = h.pipeline.bus().expect("pipeline bus");
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

        // The refusal fails the flow with the block's reason (#840). Before, the
        // video input's src pad was left unlinked and the only error was the
        // source's `Internal data stream error`, which says nothing about why.
        let video_input = format!("{}:rtmp_video_input", INSTANCE);
        let errors = h.collect_errors(&bus);
        assert_eq!(
            errors.len(),
            1,
            "expected only the block's refusal on the bus, got: {:?}",
            errors
        );
        assert_eq!(errors[0].0, video_input, "{:?}", errors);
        assert!(errors[0].1.contains("builtin.videoenc"), "{:?}", errors);
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

    /// #783: H.264 that RTMP receivers refuse is refused here, by name, instead of
    /// publishing and being rejected by the platform with nothing in Strom saying so.
    #[test]
    fn h264_in_a_profile_rtmp_receivers_refuse_fails_the_flow_naming_the_profile() {
        if !plugins_available() {
            return;
        }
        let h = Harness::new();
        h.feed_h264_444();
        h.feed_raw_audio();
        h.finish_linking();
        let bus = h.pipeline.bus().expect("pipeline bus");
        h.start();

        h.wait_until("the audio chain", |h| h.has_child("aacparse"));
        let errors = h.collect_errors(&bus);

        assert!(
            !h.has_child("h264parse"),
            "a refused profile must not be linked into the muxer"
        );
        assert_eq!(
            errors.len(),
            1,
            "expected only the block's refusal on the bus, got: {:?}",
            errors
        );
        assert_eq!(
            errors[0].0,
            format!("{}:rtmp_video_input", INSTANCE),
            "{:?}",
            errors
        );
        assert!(
            errors[0].1.contains("high-4:4:4"),
            "the refusal must name the profile that arrived: {:?}",
            errors
        );
    }
}
