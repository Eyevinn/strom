//! Regression test: the MPEG-TS/SRT output must not hand `srtsink` a
//! `streamheader`.
//!
//! `mpegtsmux` advertises the PAT/PMT it wrote first as `streamheader`, and
//! `srtsink` replays those buffers to every caller that connects afterwards.
//! The header is a snapshot taken when the muxer produced its first output —
//! but this block links its video and audio chains to the muxer only once caps
//! arrive, so that first PMT can list video alone, and later caps changes bump
//! the table version again. The header never moves.
//!
//! A caller connecting later therefore receives a program definition that
//! disagrees with the data behind it: its demuxer builds a program from the
//! header, sees the real tables a few packets on, and tears that program down
//! again — pushing EOS into the parser autoplugged for the pad it removes. With
//! under one frame buffered that parser's EOS error is fatal, and it aborts the
//! *receiving* pipeline's transition to PLAYING. That is how a WHEP output ends
//! up answering 502 for a whole production run.
//!
//! Measured against a sender that had been up four weeks: three separate
//! connects each replayed a byte-identical video-only PMT ahead of a live PMT
//! carrying video and audio.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use strom::blocks::builtin::mpegtssrt::MpegTsSrtOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &[
    "mpegtsmux",
    "srtsink",
    "x264enc",
    "h264parse",
    "avenc_aac",
    "aacparse",
    "videotestsrc",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "capsfilter",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

/// A free UDP port for the SRT listener. Binding one and dropping it races with
/// anything else on the host, but the socket is never used — srtsink only has
/// to reach PLAYING so the caps travel.
fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

/// Every caps `srtsink` was handed while the muxer ran.
///
/// Sampling `current_caps()` once is not enough: mpegtsmux sends caps before it
/// has written any tables and sends them again, with the `streamheader`
/// attached, once it has. Only the whole sequence answers the question.
#[derive(Default)]
struct CapsSeen {
    all: Vec<gst::Caps>,
}

impl CapsSeen {
    fn with_streamheader(&self) -> Option<&gst::Caps> {
        self.all
            .iter()
            .find(|c| c.structure(0).is_some_and(|s| s.has_field("streamheader")))
    }
}

/// Build the block with one video and one audio track, feed both, and collect
/// every caps event that reached `srtsink` — which is what it would replay.
fn caps_reaching_srtsink() -> CapsSeen {
    let instance_id = "tsout";

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_video_tracks".to_string(), PropertyValue::UInt(1));
    props.insert("num_audio_tracks".to_string(), PropertyValue::UInt(1));
    // Listener that nothing ever connects to: the test only reads the caps
    // srtsink was handed, never the socket. wait_for_connection=false keeps it
    // from blocking in render while we wait for those caps.
    props.insert(
        "srt_uri".to_string(),
        PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", srt_port())),
    );
    props.insert(
        "wait_for_connection".to_string(),
        PropertyValue::Bool(false),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = MpegTsSrtOutputBuilder
        .build(instance_id, &props, &ctx)
        .expect("mpegts/srt output block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }

    // The pipeline builder applies these; without them mpegtsmux:src never
    // reaches srtsink and the muxer stalls with not-linked.
    for (from, to) in &built.internal_links {
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

    let video_src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 60i32)
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let video_caps = gst::ElementFactory::make("capsfilter")
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
    let video_enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", 10u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .expect("x264enc");

    pipeline
        .add_many([&video_src, &video_caps, &video_enc])
        .unwrap();
    gst::Element::link_many([&video_src, &video_caps, &video_enc]).unwrap();
    video_enc
        .link(
            by_id
                .get(&format!("{}:video_input", instance_id))
                .expect("block exposes video_input"),
        )
        .expect("link video into the block");

    let audio_src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", 120i32)
        .property("is-live", true)
        .build()
        .expect("audiotestsrc");
    let audio_conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let audio_resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let audio_enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .expect("avenc_aac");

    pipeline
        .add_many([&audio_src, &audio_conv, &audio_resample, &audio_enc])
        .unwrap();
    gst::Element::link_many([&audio_src, &audio_conv, &audio_resample, &audio_enc]).unwrap();
    audio_enc
        .link(
            by_id
                .get(&format!("{}:audio_input_0", instance_id))
                .expect("block exposes audio_input_0"),
        )
        .expect("link audio into the block");

    for setup in ctx.take_element_setups() {
        setup(
            uuid::Uuid::new_v4(),
            strom::events::EventBroadcaster::new(16),
        );
    }

    let srtsink = by_id
        .get(&format!("{}:srtsink", instance_id))
        .expect("block exposes srtsink");
    let sink_pad = srtsink.static_pad("sink").expect("srtsink has a sink pad");

    let seen: Arc<Mutex<CapsSeen>> = Arc::default();
    let recorder = seen.clone();
    sink_pad
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
                if let gst::EventView::Caps(caps_event) = event.view() {
                    recorder
                        .lock()
                        .unwrap()
                        .all
                        .push(caps_event.caps().to_owned());
                }
            }
            gst::PadProbeReturn::Ok
        })
        .expect("probe attaches to the srtsink sink pad");

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    // Collect until EOS — the test sources are finite, so this is ~3 s. What
    // matters is not stopping before the muxer writes its first tables, since
    // that is when it attaches the streamheader and a shorter run would let the
    // test pass on a broken build. The 10 s is a deadline, not a run length.
    let bus = pipeline.bus().expect("pipeline has a bus");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut error = None;
    while std::time::Instant::now() < deadline {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    // Name the element: srt_port() races with the host, so a
                    // failure to bind the listener must not read as the
                    // regression this test guards.
                    let src = err
                        .src()
                        .map(|o| o.name().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    error = Some(format!("{}: {} ({:?})", src, err.error(), err.debug()));
                    break;
                }
                gst::MessageView::Eos(_) => break,
                _ => {}
            }
        }
    }

    let _ = pipeline.set_state(gst::State::Null);

    if let Some(e) = error {
        panic!("pipeline errored while the muxer ran: {}", e);
    }

    let seen = Arc::try_unwrap(seen)
        .map(|m| m.into_inner().unwrap())
        .unwrap_or_else(|arc| CapsSeen {
            all: arc.lock().unwrap().all.clone(),
        });
    assert!(
        !seen.all.is_empty(),
        "srtsink never received caps — the muxer produced no output, so the test proved nothing"
    );
    seen
}

/// The guard. Reverting the strip puts `streamheader` back in these caps, and
/// `srtsink` starts replaying a frozen PAT/PMT to every later caller.
#[test]
fn srtsink_receives_no_streamheader() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let seen = caps_reaching_srtsink();

    assert!(
        seen.with_streamheader().is_none(),
        "srtsink was handed a streamheader in one of {} caps event(s): {}. It \
         replays that frozen PAT/PMT to every caller that connects later, so \
         their demuxer sees a program definition that disagrees with the live \
         tables and tears down the program it just built.",
        seen.all.len(),
        seen.with_streamheader().unwrap()
    );
}
