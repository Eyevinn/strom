//! A WHEP viewer that leaves before its connection comes up must not keep its
//! session forever.
//!
//! `whepserversink` (gst-plugin-webrtc 0.15) ends a viewer's session on DELETE
//! or when the peer connection fails. A viewer that takes its answer and goes
//! away without DELETE, before connecting, does neither: the server has none
//! of its candidates, so ICE never fails. Each such session keeps a webrtcbin,
//! its threads and about 20 file descriptors. In production, viewers that
//! connected and dropped within a second piled these up until the process ran
//! out of file descriptors and GLib aborted it from a webrtcbin thread.
//!
//! The test builds the real block, posts an offer and never connects, and
//! expects the block to end that session within its connect timeout.

use crate::common;

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
fn viewer_that_never_connects_is_ended() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        return;
    }

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("num_audio_tracks".to_string(), PropertyValue::Int(0));
    props.insert("num_video_tracks".to_string(), PropertyValue::Int(1));
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("unconnected-viewer-test".to_string()),
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

    let removed = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let removed = removed.clone();
        by_id[&format!("{}:whepserversink", INSTANCE)].connect(
            "consumer-removed",
            false,
            move |values| {
                let peer_id = values[1].get::<String>().unwrap_or_default();
                removed.lock().unwrap().push(peer_id);
                None
            },
        );
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    let status = post_offer(port, OFFER);
    assert!(
        status.contains(" 201 "),
        "viewer got no answer: {:?}",
        status
    );
    assert_eq!(webrtcbins.lock().unwrap().len(), 1, "one viewer session");

    // The viewer never connects and never sends DELETE. The block's connect
    // timeout is 30 s from the answer; give it some slack.
    let deadline = Instant::now() + Duration::from_secs(45);
    let ended = loop {
        let alive = webrtcbins
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.upgrade().is_some());
        if !alive && !removed.lock().unwrap().is_empty() {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(250));
    };

    let _ = pipeline.set_state(gst::State::Null);
    assert!(
        ended,
        "a viewer that never connected still has its session after 45 s \
         (consumer-removed: {:?})",
        removed.lock().unwrap()
    );
}
