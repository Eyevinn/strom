//! A viewer's ICE gathering that reports `complete` a second time must not
//! take Strom down.
//!
//! `whepserversink`'s signaller (gst-plugin-webrtc 0.15) hands the SDP answer
//! to the waiting POST on the first `complete` of a viewer's webrtcbin and
//! panics, inside a GObject callback that cannot unwind, on any later one: the
//! process aborts. Seen in the field when viewers connect and drop within a
//! second.
//!
//! The test builds the real block, connects one viewer, and once it has its
//! answer sends `notify::ice-gathering-state` on the viewer's webrtcbin again,
//! which is what the signaller sees when gathering completes a second time.
//! Without the block's guard the process aborts there, which is why this file
//! has a test binary of its own (see `backend/Cargo.toml`). A second viewer
//! must then still get an answer: the signaller's lock is not poisoned.

pub mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use strom::blocks::builtin::get_builder;
use strom::blocks::BlockBuildContext;
use strom::events::EventBroadcaster;
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_webrtc::WebRTCICEGatheringState;

/// `x264enc` is in plugins-ugly, `nicesrc` in gstreamer1.0-nice; CI installs
/// both.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "x264enc",
    "webrtcbin",
    "nicesrc",
    "dtlssrtpenc",
    "whepserversink",
];

const INSTANCE: &str = "whep_out";

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    common::init_static_plugins();
    strom::gpu::detect_gpu_capabilities();
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

/// A video-only H.264 offer shaped like Chrome's. The ICE and DTLS values are
/// syntactically valid and never used: the server gathers its candidates and
/// answers without reaching the viewer.
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
a=rtcp-fb:102 nack\r\n\
a=rtcp-fb:102 nack pli\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

/// POST `offer` to the block's internal WHEP endpoint and return the status
/// line. Retries until the sink's HTTP server is listening.
fn post_offer(port: u16, offer: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("WHEP endpoint on port {} never listened: {}", port, e),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let request = format!(
        "POST /whep/endpoint HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/sdp\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        port,
        offer.len(),
        offer
    );
    stream.write_all(request.as_bytes()).expect("send offer");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.lines().next().unwrap_or_default().to_string()
}

#[test]
fn second_ice_gathering_complete_does_not_abort() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        return;
    }

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_audio_tracks".to_string(), PropertyValue::Int(0));
    props.insert("num_video_tracks".to_string(), PropertyValue::Int(1));
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("repeated-ice-complete-test".to_string()),
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
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .expect("capsfilter");
    pipeline.add_many([&src, &raw]).expect("add source");
    gst::Element::link_many([&src, &raw, &by_id[&format!("{}:video_queue", INSTANCE)]])
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

    let webrtcbins = Arc::new(Mutex::new(Vec::<glib::WeakRef<gst::Element>>::new()));
    {
        let webrtcbins = webrtcbins.clone();
        by_id[&format!("{}:whepserversink", INSTANCE)].connect(
            "consumer-added",
            false,
            move |values| {
                let webrtcbin = values[2].get::<gst::Element>().expect("webrtcbin");
                webrtcbins.lock().unwrap().push(webrtcbin.downgrade());
                None
            },
        );
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    let first = post_offer(port, OFFER);
    assert!(
        first.contains(" 201 "),
        "first viewer got no answer: {:?}",
        first
    );

    let webrtcbin = webrtcbins
        .lock()
        .unwrap()
        .first()
        .and_then(|w| w.upgrade())
        .expect("first viewer's webrtcbin");
    assert_eq!(
        webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state"),
        WebRTCICEGatheringState::Complete,
        "the answer goes out on the first complete"
    );

    // What the signaller sees when gathering completes again. Without the
    // guard this aborts the test binary.
    webrtcbin.notify("ice-gathering-state");
    webrtcbin.notify("ice-gathering-state");

    let second = post_offer(port, OFFER);
    assert!(
        second.contains(" 201 "),
        "a viewer after the repeated complete got no answer: {:?}",
        second
    );

    let _ = pipeline.set_state(gst::State::Null);
}
