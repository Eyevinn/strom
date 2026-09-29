//! The AES67 Input and WHEP Input blocks set `drop-on-latency` on the `rtpbin`
//! behind their jitterbuffers, as a workaround for an upstream rtpjitterbuffer
//! bug.
//!
//! Bug: after a mute gap (no RTP packets), `calculate_packet_spacing` sees the
//! large RTP timestamp jump as packet spacing. The exponential moving average
//! keeps it there across many packets, so when a packet is lost while spacing
//! is corrupted, its lost timer is scheduled proportionally far in the future
//! and audio output stalls for about as long as the mute lasted. With
//! `drop-on-latency=true` the jitterbuffer drops queued packets that exceed its
//! latency, which moves it past the stall.
//!
//! Upstream references (none merged as of 2026-04):
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/570
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/951
//!   - https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/221
//!
//! `rtpbin` defaults to `drop-on-latency=false`, so each test builds the real
//! block and reads the property back off the `rtpbin` it ends up with. Drop the
//! workaround from a block and its test fails.

pub mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use strom::blocks::builtin::get_builder;
use strom::blocks::BlockBuildContext;
use strom_types::PropertyValue;

/// Register the `gst-plugins-rs` elements, then check `required`.
fn webrtc_elements_available(required: &[&str]) -> bool {
    common::init_webrtc_plugins();
    common::plugins_available(required)
}

/// Every `rtpbin` inside `bin`, at any depth.
fn rtpbins(bin: &gst::Bin) -> Vec<gst::Element> {
    bin.iterate_recurse()
        .into_iter()
        .flatten()
        .filter(|e| e.factory().is_some_and(|f| f.name() == "rtpbin"))
        .collect()
}

fn build_whepsrc(drop_on_latency: Option<bool>) -> gst::Bin {
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert(
        "implementation".to_string(),
        PropertyValue::String("whepsrc".to_string()),
    );
    props.insert(
        "whep_endpoint".to_string(),
        PropertyValue::String("http://192.0.2.10/whep".to_string()),
    );
    if let Some(value) = drop_on_latency {
        props.insert("drop_on_latency".to_string(), PropertyValue::Bool(value));
    }

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = get_builder("builtin.whep_input")
        .expect("WHEP Input has a builder")
        .build("whep", &props, &ctx)
        .expect("WHEP Input builds");
    let (_, whepsrc) = built
        .elements
        .into_iter()
        .find(|(id, _)| id.ends_with(":whepsrc"))
        .expect("WHEP Input builds a whepsrc");
    whepsrc.downcast::<gst::Bin>().expect("whepsrc is a bin")
}

/// WHEP Input (`whepsrc`) turns the workaround on by default, and its
/// `drop_on_latency` property turns it off.
#[test]
fn whep_input_whepsrc_sets_drop_on_latency_on_its_rtpbin() {
    if !webrtc_elements_available(&["whepsrc", "rtpbin"]) {
        return;
    }

    for (setting, expected) in [(None, true), (Some(true), true), (Some(false), false)] {
        let whepsrc = build_whepsrc(setting);
        let bins = rtpbins(&whepsrc);
        assert!(
            !bins.is_empty(),
            "whepsrc holds no rtpbin at build time, so the block has nothing to set"
        );
        for rtpbin in bins {
            assert_eq!(
                rtpbin.property::<bool>("drop-on-latency"),
                expected,
                "rtpbin {} inside whepsrc (drop_on_latency property {:?})",
                rtpbin.name(),
                setting
            );
        }
    }
}

/// AES67 Input: `sdpdemux` only creates its `rtpbin` once it has parsed the SDP,
/// so the pipeline has to run for the block's element-added handler to see it.
#[test]
fn aes67_input_sets_drop_on_latency_on_the_sdpdemux_rtpbin() {
    if !webrtc_elements_available(&["filesrc", "sdpdemux", "rtpbin", "udpsrc"]) {
        return;
    }

    // A free local port, so the udpsrc sdpdemux creates can bind. Unicast
    // loopback: nothing is ever sent, the test only needs the rtpbin to exist.
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .expect("free UDP port")
        .port();
    let sdp = format!(
        "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\n\
         s=drop-on-latency test\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 96\r\n\
         a=rtpmap:96 L24/48000/2\r\n"
    );

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("SDP".to_string(), PropertyValue::String(sdp));
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = get_builder("builtin.aes67_input")
        .expect("AES67 Input has a builder")
        .build("aes67", &props, &ctx)
        .expect("AES67 Input builds");

    let pipeline = gst::Pipeline::new();
    let mut sdpdemux = None;
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        if id.ends_with(":sdpdemux") {
            sdpdemux = Some(element.clone());
        }
    }
    for (from, to) in &built.internal_links {
        let src = pipeline
            .by_name(&from.element_id)
            .expect("internal link source is in the pipeline");
        // Links from sdpdemux's sometimes pads can only be made once the SDP is
        // parsed; the pipeline manager defers them. This test only needs the
        // SDP to reach sdpdemux, so they are left out.
        if let Some(pad) = from.pad_name.as_deref() {
            if src.static_pad(pad).is_none() {
                continue;
            }
        }
        let dst = pipeline
            .by_name(&to.element_id)
            .expect("internal link sink is in the pipeline");
        src.link_pads(from.pad_name.as_deref(), &dst, to.pad_name.as_deref())
            .expect("internal AES67 link");
    }
    let sdpdemux = sdpdemux
        .expect("AES67 Input builds an sdpdemux")
        .downcast::<gst::Bin>()
        .expect("sdpdemux is a bin");

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bins = rtpbins(&sdpdemux);
    while bins.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        bins = rtpbins(&sdpdemux);
    }
    let values: Vec<(String, bool)> = bins
        .iter()
        .map(|b| (b.name().to_string(), b.property::<bool>("drop-on-latency")))
        .collect();
    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");

    assert!(
        !values.is_empty(),
        "sdpdemux never created an rtpbin within 10s"
    );
    for (name, value) in values {
        assert!(
            value,
            "rtpbin {name} inside sdpdemux has drop-on-latency=false"
        );
    }
}
