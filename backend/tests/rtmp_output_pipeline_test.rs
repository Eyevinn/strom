//! Regression tests for how `builtin.rtmp_output` hands out `flvmux` sink pads.
//!
//! `flvmux` writes the FLV header once, from the pads it holds when the first
//! buffer arrives. A pad requested after that is still granted and its link
//! still succeeds, so the tags are written — but the header does not declare the
//! stream, and a player that configures its decoders from the header plays
//! silent video. Measured on GStreamer 1.28.6 with this block's own topology:
//! `TypeFlags=0x05` with both pads present before the first buffer,
//! `TypeFlags=0x01` with the audio pad requested afterwards. Nothing returns an
//! error either way, which is why this needs a test rather than a log line.
//!
//! So every connected input must have its pad before the stream starts, and an
//! unconnected input must have none — an aggregator sink pad that never carries
//! data stops `flvmux` aggregating and nothing reaches the sink at all.
//!
//! These tests drive the real builder, including its element setup hook and its
//! caps probes, which is the mechanism `rtmp_output_test.rs` cannot reach:
//! deleting both probe bodies leaves that suite green.
//!
//! The pipeline is taken to PAUSED and no further. Preroll is enough to
//! negotiate caps and fire the probes, and `rtmp2sink` only opens its connection
//! on the way to PLAYING, so nothing here needs a reachable RTMP server.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use strom::blocks::builtin::rtmp::RtmpOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "rtmp2sink",
    "flvmux",
    "h264parse",
    "aacparse",
    "avenc_aac",
    "identity",
    "videotestsrc",
    "audiotestsrc",
    "x264enc",
    "audioconvert",
    "capsfilter",
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

/// Which of the block's inputs the flow connects, and with what.
#[derive(Clone, Copy, PartialEq)]
enum Feed {
    /// H.264 into the video input, raw PCM into the audio input. The normal case.
    EncodedVideoAndAudio,
    /// H.264 into the video input, nothing on the audio input.
    EncodedVideoOnly,
    /// Raw video, which the block refuses, plus raw PCM the block accepts.
    RawVideoAndAudio,
}

const INSTANCE: &str = "rtmp0";

struct Harness {
    pipeline: gst::Pipeline,
    mux: gst::Element,
    /// Kept alive so the setup hook can still be run by the test.
    ctx: BlockBuildContext,
}

impl Harness {
    /// Run the element setup hooks the pipeline manager would run: after every
    /// block is linked, before the pipeline leaves NULL. Skipping this would
    /// exercise nothing — the hook is where the pads are reserved.
    fn run_element_setups(&self) {
        for setup in self.ctx.take_element_setups() {
            setup(uuid::Uuid::new_v4(), EventBroadcaster::new(16));
        }
    }

    fn mux_pad(&self, name: &str) -> Option<gst::Pad> {
        self.mux.static_pad(name)
    }

    /// The muxer's sink pads, by name. Used to catch an extra pad requested
    /// behind the reservation's back as well as a missing one.
    fn mux_sink_pad_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .mux
            .pads()
            .into_iter()
            .filter(|p| p.direction() == gst::PadDirection::Sink)
            .map(|p| p.name().to_string())
            .collect();
        names.sort();
        names
    }

    /// Poll until `cond` holds, or fail. Bus errors are drained rather than
    /// asserted on: `rtmp2sink` has no server, and what is under test here is
    /// the graph, not the connection.
    fn wait_until(&self, what: &str, cond: impl Fn(&Harness) -> bool) {
        let bus = self.pipeline.bus().expect("pipeline has a bus");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond(self) {
                return;
            }
            let _ = bus.timed_pop(gst::ClockTime::from_mseconds(50));
        }
        panic!("timed out waiting for {}", what);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Build the block for real, add it to a pipeline, and wire up the inputs named
/// by `feed`. Stops short of running the setup hooks so a test can assert on
/// what exists before and after them.
fn harness(feed: Feed) -> Harness {
    gst::init().expect("gst init");

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    // Port 1 is closed. Nothing here reaches PLAYING, so the sink never dials it.
    props.insert(
        "location".to_string(),
        PropertyValue::String("rtmp://127.0.0.1:1/live/test".to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = RtmpOutputBuilder
        .build(INSTANCE, &props, &ctx)
        .expect("rtmp_output block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }

    for (from, to) in &built.internal_links {
        let src = by_id
            .get(&from.element_id)
            .unwrap_or_else(|| panic!("link source {} was not built", from.element_id));
        let dst = by_id
            .get(&to.element_id)
            .unwrap_or_else(|| panic!("link target {} was not built", to.element_id));
        let src_pad = src
            .static_pad(from.pad_name.as_deref().unwrap_or("src"))
            .expect("link source pad");
        let dst_pad = dst
            .static_pad(to.pad_name.as_deref().unwrap_or("sink"))
            .expect("link target pad");
        src_pad.link(&dst_pad).expect("internal link");
    }

    let video_in = by_id
        .get(&format!("{}:rtmp_video_input", INSTANCE))
        .expect("block exposes rtmp_video_input")
        .clone();
    let audio_in = by_id
        .get(&format!("{}:rtmp_audio_input", INSTANCE))
        .expect("block exposes rtmp_audio_input")
        .clone();
    let mux = by_id
        .get(&format!("{}:rtmp_flvmux", INSTANCE))
        .expect("block exposes rtmp_flvmux")
        .clone();

    // Video feed. Short and deterministic: 320x240 at 30fps is enough to
    // negotiate caps, which is all the probes need.
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 30i32)
        .build()
        .expect("videotestsrc");
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    pipeline.add_many([&src, &caps]).unwrap();
    src.link(&caps).unwrap();

    match feed {
        Feed::EncodedVideoAndAudio | Feed::EncodedVideoOnly => {
            let enc = gst::ElementFactory::make("x264enc")
                .property("key-int-max", 10u32)
                .property_from_str("tune", "zerolatency")
                .build()
                .expect("x264enc");
            pipeline.add(&enc).unwrap();
            caps.link(&enc).unwrap();
            enc.link(&video_in).expect("link video into the block");
        }
        Feed::RawVideoAndAudio => {
            caps.link(&video_in).expect("link raw video into the block");
        }
    }

    if feed != Feed::EncodedVideoOnly {
        let asrc = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 50i32)
            .build()
            .expect("audiotestsrc");
        let conv = gst::ElementFactory::make("audioconvert")
            .build()
            .expect("audioconvert");
        pipeline.add_many([&asrc, &conv]).unwrap();
        asrc.link(&conv).unwrap();
        conv.link(&audio_in).expect("link audio into the block");
    }

    Harness { pipeline, mux, ctx }
}

/// The element the given mux sink pad's data comes from, by factory name.
fn peer_factory(pad: &gst::Pad) -> String {
    pad.peer()
        .and_then(|p| p.parent_element())
        .and_then(|e| e.factory())
        .map(|f| f.name().to_string())
        .unwrap_or_else(|| "<unlinked>".to_string())
}

#[test]
fn both_mux_pads_are_reserved_before_any_data_flows() {
    if !plugins_available() {
        return;
    }
    let h = harness(Feed::EncodedVideoAndAudio);

    assert!(
        h.mux_sink_pad_names().is_empty(),
        "no mux sink pad may exist at build time, or an unconnected input would \
         stall the muxer: {:?}",
        h.mux_sink_pad_names()
    );

    h.run_element_setups();

    assert_eq!(
        h.mux_sink_pad_names(),
        vec!["audio".to_string(), "video".to_string()],
        "both connected inputs must hold an flvmux pad before the pipeline \
         starts. Requesting from the caps probes instead loses the FLV header: \
         the pad is granted, the tags are written, and the header says the \
         stream is not there"
    );
}

#[test]
fn an_unconnected_input_reserves_no_mux_pad() {
    if !plugins_available() {
        return;
    }
    let h = harness(Feed::EncodedVideoOnly);
    h.run_element_setups();

    assert_eq!(
        h.mux_sink_pad_names(),
        vec!["video".to_string()],
        "an input the flow never connected must get no flvmux pad: an aggregator \
         sink pad that never carries data stops the muxer aggregating and the \
         sink receives nothing"
    );
}

#[test]
fn the_chains_link_to_the_pads_reserved_before_the_stream_started() {
    if !plugins_available() {
        return;
    }
    let h = harness(Feed::EncodedVideoAndAudio);
    h.run_element_setups();

    let video_pad = h.mux_pad("video").expect("video pad reserved");
    let audio_pad = h.mux_pad("audio").expect("audio pad reserved");

    h.pipeline
        .set_state(gst::State::Paused)
        .expect("pipeline reaches PAUSED");

    h.wait_until("both chains to link to the muxer", |_| {
        video_pad.is_linked() && audio_pad.is_linked()
    });

    // The identity of the pads matters as much as the count: a probe that
    // requested its own pad would leave these two linked to nothing and add a
    // third the FLV header never described.
    assert_eq!(
        h.mux_sink_pad_names(),
        vec!["audio".to_string(), "video".to_string()],
        "the probes must link the reserved pads, not request their own"
    );
    assert_eq!(
        peer_factory(&video_pad),
        "h264parse",
        "the video chain must feed the reserved video pad"
    );
    assert_eq!(
        peer_factory(&audio_pad),
        "aacparse",
        "the encoded audio chain must feed the reserved audio pad"
    );
}

#[test]
fn a_refused_codec_hands_its_pad_back() {
    if !plugins_available() {
        return;
    }
    let h = harness(Feed::RawVideoAndAudio);
    h.run_element_setups();

    let audio_pad = h.mux_pad("audio").expect("audio pad reserved");
    assert!(
        h.mux_pad("video").is_some(),
        "connectivity is all the setup hook knows, so the video pad is reserved \
         before its codec is known"
    );

    h.pipeline
        .set_state(gst::State::Paused)
        .expect("pipeline reaches PAUSED");

    // Raw video is refused with a message naming builtin.videoenc. The audio
    // side must survive that, which it can only do if the refused pad goes back:
    // flvmux would otherwise wait forever for data on it.
    h.wait_until("the audio chain to link to the muxer", |_| {
        audio_pad.is_linked()
    });
    h.wait_until("the refused video pad to be released", |h| {
        h.mux_pad("video").is_none()
    });

    assert_eq!(
        h.mux_sink_pad_names(),
        vec!["audio".to_string()],
        "a refused stream must leave the muxer holding only the pad that has data"
    );
}
