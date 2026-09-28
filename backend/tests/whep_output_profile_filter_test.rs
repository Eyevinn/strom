//! A WHEP viewer must get pre-encoded H.264 whatever profile and level its
//! offer names.
//!
//! Browsers offer H.264 Baseline first, and the WHEP Output block answers with
//! that payload type whatever profile the encoder produces. webrtcsink puts the
//! offered profile and level on the capsfilter in front of the viewer's
//! payloader, where a High profile stream, or one above the offered level 3.1,
//! fails with `not-negotiated` at its first keyframe unless the block strips
//! them.
//!
//! webrtcsink sets a viewer's streams up in `HashMap` order, so the strip has
//! to hold when the video stream is set up first. A video-only offer makes that
//! the only order.
//!
//! The test builds the real block, feeds it High profile H.264, POSTs a
//! Chrome-shaped video-only offer to the block's internal WHEP endpoint and
//! waits for the viewer's payloader to emit a buffer. No ICE or DTLS is needed:
//! the chain negotiates as soon as the first keyframe reaches the session.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strom::blocks::builtin::get_builder;
use strom::blocks::BlockBuildContext;
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// `x264enc` is in plugins-ugly, `nicesrc` in gstreamer1.0-nice; CI installs
/// both.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "x264enc",
    "h264parse",
    "rtph264pay",
    "capsfilter",
    "webrtcbin",
    "nicesrc",
    "dtlssrtpenc",
    "whepserversink",
];

const INSTANCE: &str = "whep_out";

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    init_gst();
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

/// The WHIP/WHEP elements come from `gst-plugins-rs`, which is linked into the
/// binary and registered at startup. A test binary has to register it itself.
fn init_gst() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("gst init");
        gstwebrtchttp::plugin_register_static().expect("register webrtchttp plugins");
        gstrswebrtc::plugin_register_static().expect("register webrtc plugins");
    });
}

/// A video-only offer shaped like Chrome's: H.264 Baseline 3.1 as the only
/// payload type, which is also the first H.264 entry in a real Chrome offer.
/// The ICE and DTLS values are syntactically valid and never used.
const OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=extmap-allow-mixed\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 7B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=rtpmap:102 H264/90000\r\n\
a=rtcp-fb:102 goog-remb\r\n\
a=rtcp-fb:102 transport-cc\r\n\
a=rtcp-fb:102 ccm fir\r\n\
a=rtcp-fb:102 nack\r\n\
a=rtcp-fb:102 nack pli\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r\n";

/// POST `OFFER` to the block's internal WHEP endpoint and return the status
/// line. Retries until the sink's HTTP server is listening.
fn post_offer(port: u16) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("WHEP endpoint on port {} never listened: {}", port, e),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let request = format!(
        "POST /whep/endpoint HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/sdp\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        port,
        OFFER.len(),
        OFFER
    );
    stream.write_all(request.as_bytes()).expect("send offer");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.lines().next().unwrap_or_default().to_string()
}

/// What one viewer got from the block.
struct Viewer {
    /// Buffers the viewer's payloader pushed.
    payloaded: usize,
    /// The level the encoder produced, from the parsed stream's caps.
    level: Option<String>,
    /// Anything that went wrong along the way.
    problems: Vec<String>,
}

/// Build the WHEP Output block with one video track, feed it `profile` H.264
/// at `width`x`height` and `fps`, and connect one video-only viewer.
fn viewer_payloaded_buffers(profile: &str, width: i32, height: i32, fps: i32) -> Viewer {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_audio_tracks".to_string(), PropertyValue::Int(0));
    props.insert("num_video_tracks".to_string(), PropertyValue::Int(1));
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("profile-filter-test".to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = get_builder("builtin.whep_output")
        .expect("WHEP Output builder")
        .build(INSTANCE, &props, &ctx)
        .expect("WHEP Output builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
    for (from, to) in &built.internal_links {
        let src = &by_id[&from.element_id];
        let sink = &by_id[&to.element_id];
        match (&from.pad_name, &to.pad_name) {
            (Some(src_pad), Some(sink_pad)) => src
                .link_pads(Some(src_pad.as_str()), sink, Some(sink_pad.as_str()))
                .expect("internal pad link"),
            _ => src.link(sink).expect("internal element link"),
        }
    }

    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .expect("videotestsrc");
    let raw = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", width)
                .field("height", height)
                .field("framerate", gst::Fraction::new(fps, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    let enc = gst::ElementFactory::make("x264enc")
        // No speed preset: `ultrafast` turns off the High profile tools and
        // x264 then labels the stream Constrained Baseline.
        .property_from_str("tune", "zerolatency")
        .property("key-int-max", 15u32)
        .build()
        .expect("x264enc");
    let encoded = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("profile", profile)
                .build(),
        )
        .build()
        .expect("capsfilter");
    let parse = gst::ElementFactory::make("h264parse")
        .build()
        .expect("h264parse");
    pipeline
        .add_many([&src, &raw, &enc, &encoded, &parse])
        .expect("add source");
    let block_input = &by_id[&format!("{}:video_queue", INSTANCE)];
    gst::Element::link_many([&src, &raw, &enc, &encoded, &parse, block_input])
        .expect("link source to block");

    let flow_id = strom_types::flow::FlowId::new_v4();
    let events = EventBroadcaster::with_capacity(16);
    for setup in ctx.take_element_setups() {
        setup(flow_id, events.clone());
    }
    let port = ctx
        .take_whep_endpoints()
        .first()
        .expect("block registers its WHEP endpoint")
        .internal_port;

    // Count what the viewer's payloader emits. A buffer out of the payloader
    // means the chain in front of it negotiated.
    let payloaded = Arc::new(AtomicUsize::new(0));
    let problems = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = &by_id[&format!("{}:whepserversink", INSTANCE)];
    {
        let payloaded = payloaded.clone();
        sink.connect("payloader-setup", false, move |values| {
            let consumer_id = values[1].get::<String>().unwrap_or_default();
            let payloader = values[3].get::<gst::Element>().expect("payloader");
            if consumer_id != "discovery" {
                let payloaded = payloaded.clone();
                payloader
                    .static_pad("src")
                    .expect("payloader src pad")
                    .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                        payloaded.fetch_add(1, Ordering::Relaxed);
                        gst::PadProbeReturn::Ok
                    });
            }
            Some(false.to_value())
        });
    }
    {
        let problems = problems.clone();
        sink.connect("consumer-removed", false, move |values| {
            let consumer_id = values[1].get::<String>().unwrap_or_default();
            problems
                .lock()
                .unwrap()
                .push(format!("consumer {} removed", consumer_id));
            None
        });
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    let status = post_offer(port);
    if !status.contains(" 201 ") {
        problems
            .lock()
            .unwrap()
            .push(format!("offer answered with {:?}", status));
    }

    // Keyframes come every 15 frames, so the first one reaches the viewer's
    // session within about half a second of it starting.
    let bus = pipeline.bus().expect("pipeline bus");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && payloaded.load(Ordering::Relaxed) == 0 {
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Error],
        ) {
            if let gst::MessageView::Error(err) = msg.view() {
                problems.lock().unwrap().push(format!(
                    "error from {}: {}",
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error()
                ));
            }
        }
    }

    let level = parse
        .static_pad("src")
        .and_then(|pad| pad.current_caps())
        .and_then(|caps| {
            caps.structure(0)
                .and_then(|s| s.get::<String>("level").ok())
        });
    let _ = pipeline.set_state(gst::State::Null);
    let problems = problems.lock().unwrap().clone();
    Viewer {
        payloaded: payloaded.load(Ordering::Relaxed),
        level,
        problems,
    }
}

#[test]
fn baseline_offer_gets_high_profile_video() {
    if !plugins_available() {
        return;
    }
    let viewer = viewer_payloaded_buffers("high", 320, 240, 30);
    assert!(
        viewer.payloaded > 0,
        "a viewer offering H.264 Baseline got no High profile video \
         (payloaded {} buffers; {:?})",
        viewer.payloaded,
        viewer.problems
    );
}

/// 720p at 60 fps is level 3.2, one step above the 3.1 every Chrome H.264
/// payload type offers.
#[test]
fn level_3_1_offer_gets_720p60_video() {
    if !plugins_available() {
        return;
    }
    let viewer = viewer_payloaded_buffers("high", 1280, 720, 60);
    assert_eq!(
        viewer.level.as_deref(),
        Some("3.2"),
        "the encoder has to produce a level above the offered 3.1 for this \
         test to mean anything ({:?})",
        viewer.problems
    );
    assert!(
        viewer.payloaded > 0,
        "a viewer offering H.264 level 3.1 got no level 3.2 video \
         (payloaded {} buffers; {:?})",
        viewer.payloaded,
        viewer.problems
    );
}
