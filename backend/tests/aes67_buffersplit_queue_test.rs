//! AES67 Input compacts the 1 ms packets it receives with `audiobuffersplit`.
//! The split pushes from the jitterbuffer's streaming thread, so without a
//! thread boundary after it a downstream that blocks now and then (an encoder,
//! a syncing sink) stalls the receive path. Audio then goes missing before the
//! split, and every gap makes it resync: a WHIP output to an intercom sounded
//! broken until `buffer_duration_ms` was set to 0.
//!
//! The test builds the real block, feeds it 1 ms L24 packets over loopback and
//! puts a syncing sink behind it that blocks every few buffers. It requires all
//! of the audio to come out. Without the queue after the split, about two
//! thirds of it is lost.

pub mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom::blocks::builtin::get_builder;
use strom_types::PropertyValue;

const RATE: u64 = 48_000;
const CHANNELS: u64 = 2;
const BYTES_PER_FRAME: u64 = CHANNELS * 3;

#[test]
fn aes67_input_keeps_all_audio_when_downstream_blocks() {
    if !common::plugins_available(&[
        "filesrc",
        "sdpdemux",
        "rtpbin",
        "udpsrc",
        "udpsink",
        "decodebin",
        "rtpL24depay",
        "rtpL24pay",
        "capssetter",
        "audiobuffersplit",
        "queue",
        "audioconvert",
        "audioresample",
        "audiotestsrc",
        "fakesink",
    ]) {
        return;
    }

    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .expect("free UDP port")
        .port();
    let sdp = format!(
        "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\n\
         s=buffersplit queue test\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 96\r\n\
         a=rtpmap:96 L24/48000/2\r\n\
         a=ptime:1\r\n"
    );

    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    props.insert("SDP".to_string(), PropertyValue::String(sdp));
    props.insert("buffer_duration_ms".to_string(), PropertyValue::Int(10));
    let ctx = common::block::context();
    let built = get_builder("builtin.aes67_input")
        .expect("AES67 Input has a builder")
        .build("aes67", &props, &ctx)
        .expect("AES67 Input builds");

    let pipeline = gst::Pipeline::new();
    for (_, element) in &built.elements {
        pipeline.add(element).expect("add block element");
    }
    for (from, to) in &built.internal_links {
        let src = pipeline
            .by_name(&from.element_id)
            .expect("internal link source is in the pipeline");
        let dst = pipeline
            .by_name(&to.element_id)
            .expect("internal link sink is in the pipeline");
        // sdpdemux's stream pad only exists once the SDP is parsed; the
        // pipeline manager defers that link, and so does this test.
        if let Some(pad) = from.pad_name.as_deref() {
            if src.static_pad(pad).is_none() {
                let dst = dst.downgrade();
                let sink_pad = to.pad_name.clone();
                let wanted = pad.to_string();
                src.connect_pad_added(move |_, new_pad| {
                    if new_pad.name() != wanted.as_str() {
                        return;
                    }
                    let Some(dst) = dst.upgrade() else { return };
                    let sink = dst
                        .static_pad(sink_pad.as_deref().expect("named sink pad"))
                        .expect("deferred link sink pad");
                    new_pad.link(&sink).expect("deferred internal link");
                });
                continue;
            }
        }
        src.link_pads(from.pad_name.as_deref(), &dst, to.pad_name.as_deref())
            .expect("internal AES67 link");
    }
    common::block::run_setups(&ctx);

    let out = pipeline
        .by_name("aes67:audioresample")
        .expect("AES67 Input ends in audioresample");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("fakesink");
    pipeline.add(&sink).expect("add sink");
    out.link(&sink).expect("link block output to sink");

    // Count what leaves the block, and block the streaming thread now and
    // then, as an encoder or a WebRTC sink does.
    let frames = Arc::new(AtomicU64::new(0));
    let buffers = Arc::new(AtomicU64::new(0));
    {
        let frames = frames.clone();
        let buffers = buffers.clone();
        out.static_pad("src")
            .expect("audioresample src pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    frames.fetch_add(buffer.size() as u64 / BYTES_PER_FRAME, Ordering::Relaxed);
                    if buffers.fetch_add(1, Ordering::Relaxed) % 5 == 4 {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }

    let sender = gst::parse::launch(&format!(
        "audiotestsrc is-live=true wave=red-noise samplesperbuffer=48 \
         ! audio/x-raw,format=S24BE,rate={RATE},channels={CHANNELS} \
         ! rtpL24pay pt=96 min-ptime=1000000 max-ptime=1000000 \
         ! udpsink host=127.0.0.1 port={port}"
    ))
    .expect("sender pipeline");

    pipeline
        .set_state(gst::State::Playing)
        .expect("receiver to PLAYING");
    sender
        .set_state(gst::State::Playing)
        .expect("sender to PLAYING");

    // Let the stream settle, then measure over a fixed window.
    std::thread::sleep(Duration::from_secs(1));
    let start_frames = frames.load(Ordering::Relaxed);
    let window = Duration::from_secs(4);
    std::thread::sleep(window);
    let received = frames.load(Ordering::Relaxed) - start_frames;

    let error = common::bus::first_error(&pipeline.bus().expect("bus"), Duration::ZERO);
    sender.set_state(gst::State::Null).expect("sender to NULL");
    pipeline
        .set_state(gst::State::Null)
        .expect("receiver to NULL");
    assert!(error.is_none(), "receiver posted an error: {:?}", error);

    let expected = RATE * window.as_secs();
    assert!(
        received * 100 >= expected * 90,
        "AES67 Input delivered {received} of {expected} frames ({}%) while downstream \
         blocked now and then; audio is lost before the buffer split",
        received * 100 / expected
    );
}
