//! End-to-end test for EFP's embedded-data channel: bytes pushed at an
//! `efpsrt_output` block's `data_in_0` come back out of an `efpsrt_input`
//! block's `data_out_0` over a real SRT connection.
//!
//! #700 added the channel and #691 asked for this. Everything shipped so far
//! asserts graph structure — pads requested, elements created, links made —
//! which does not show that a byte survives the trip. This drives the real
//! block builders and reads the far end.
//!
//! It also pins the two things a caller has to get right and cannot discover
//! from the block's shape: embedded data is addressed to a *media* stream by
//! `stream-id`, and it only leaves the muxer on a frame of that stream.

#![cfg(feature = "efp")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom::blocks::builtin::efpsrt::EfpSrtOutputBuilder;
use strom::blocks::builtin::efpsrt_input::EfpSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use strom_types::PropertyValue;

/// Elements this test needs beyond core GStreamer. All are in
/// `gstreamer1.0-plugins-base`/`-good`/`-bad`, which CI installs.
const REQUIRED: &[&str] = &[
    "efpmux",
    "efpdemux",
    "srtsink",
    "srtsrc",
    "identity",
    "appsrc",
    "appsink",
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "opusenc",
    "opusparse",
];

/// The stream ID `efpmux` assigns the block's first media track.
///
/// IDs are allocated from 1 as sink pads are requested, and this flow has one
/// audio track and no video, so audio is stream 1. Embedded data addressed
/// anywhere else has no frame to ride out on and is dropped by the muxer.
const MEDIA_STREAM_ID: i32 = 1;

const DATA_TYPE: i32 = 7;
const PAYLOAD: &[u8] = b"c2pa-manifest-bytes";

fn init() {
    let _ = gst::init();
    let _ = gst_plugin_efp::plugin_register_static();
}

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
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

fn srt_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("no free UDP port for the SRT listener")
}

fn element<'a>(result: &'a BlockBuildResult, id: &str) -> &'a gst::Element {
    result
        .elements
        .iter()
        .find(|(element_id, _)| element_id == id)
        .map(|(_, element)| element)
        .unwrap_or_else(|| {
            panic!(
                "block should contain '{}', got {:?}",
                id,
                result.elements.iter().map(|(id, _)| id).collect::<Vec<_>>()
            )
        })
}

/// Add a block's elements to `pipeline` and apply the links the builder asked
/// for, so the graph under test is the one the block produced rather than one
/// reconstructed here.
fn install(pipeline: &gst::Pipeline, result: &BlockBuildResult) {
    let by_id: HashMap<&str, &gst::Element> = result
        .elements
        .iter()
        .map(|(id, e)| (id.as_str(), e))
        .collect();

    for (_, e) in &result.elements {
        pipeline.add(e).expect("element joins the pipeline");
    }

    for (from, to) in &result.internal_links {
        let src = by_id[from.element_id.as_str()];
        let dst = by_id[to.element_id.as_str()];
        match (&from.pad_name, &to.pad_name) {
            (None, None) => src.link(dst).expect("elements link"),
            (f, t) => {
                let src_pad = match f {
                    Some(name) => src.static_pad(name).expect("named src pad exists"),
                    None => src.static_pad("src").expect("src pad exists"),
                };
                let dst_pad = match t {
                    Some(name) => dst.static_pad(name).expect("named sink pad exists"),
                    None => dst.static_pad("sink").expect("sink pad exists"),
                };
                src_pad.link(&dst_pad).expect("pads link");
            }
        }
    }
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn drain_errors(pipeline: &gst::Pipeline, label: &str) {
    let bus = pipeline.bus().expect("pipeline has a bus");
    while let Some(msg) = bus.pop() {
        if let gst::MessageView::Error(err) = msg.view() {
            panic!(
                "{} pipeline error: {} ({:?})",
                label,
                err.error(),
                err.debug()
            );
        }
    }
}

/// Bytes pushed at `data_in_0` come out of `data_out_0` on the far side of an
/// SRT hop, carrying the data type and stream ID they were addressed with.
#[test]
fn embedded_data_survives_the_trip_between_the_blocks() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let port = srt_port();
    let ctx = || BlockBuildContext::new(Vec::new(), "all".to_string());

    // --- sender -----------------------------------------------------------
    // wait_for_connection=false: the receiver connects a moment later, and a
    // blocking sink would stall the pushes below before it does.
    let out = EfpSrtOutputBuilder
        .build(
            "tx",
            &props(&[
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
                ("num_data_tracks", PropertyValue::UInt(1)),
                (
                    "srt_uri",
                    PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", port)),
                ),
                ("wait_for_connection", PropertyValue::Bool(false)),
            ]),
            &ctx(),
        )
        .expect("efpsrt_output builds");

    let tx = gst::Pipeline::with_name("tx");
    install(&tx, &out);

    let audio_src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property_from_str("wave", "silence")
        .build()
        .expect("audiotestsrc");
    tx.add(&audio_src).unwrap();
    audio_src
        .link(element(&out, "tx:audio_input_0"))
        .expect("audio source feeds the block's audio input");

    let data_caps = gst::Caps::builder("application/x-efp-embedded")
        .field("data-type", DATA_TYPE)
        .field("stream-id", MEDIA_STREAM_ID)
        .build();
    let data_src = gst::ElementFactory::make("appsrc")
        .property("caps", &data_caps)
        .property("format", gst::Format::Time)
        .property("is-live", true)
        .build()
        .expect("appsrc");
    tx.add(&data_src).unwrap();
    data_src
        .link(element(&out, "tx:data_input_0"))
        .expect("data source feeds the block's data input");
    let data_src = data_src.dynamic_cast::<gst_app::AppSrc>().unwrap();

    // --- receiver ---------------------------------------------------------
    let inp = EfpSrtInputBuilder
        .build(
            "rx",
            &props(&[
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(1)),
                ("num_data_tracks", PropertyValue::UInt(1)),
                (
                    "srt_uri",
                    PropertyValue::String(format!("srt://127.0.0.1:{}?mode=caller", port)),
                ),
                ("decode", PropertyValue::Bool(false)),
            ]),
            &ctx(),
        )
        .expect("efpsrt_input builds");

    let rx = gst::Pipeline::with_name("rx");
    install(&rx, &inp);

    // Read the far end of the block rather than the demuxer directly, so the
    // block's own linking is part of what is under test.
    /// `(buffer bytes, the pad's caps when it was pushed)`.
    type Delivered = (Vec<u8>, Option<gst::Caps>);

    let received: Arc<Mutex<Vec<Delivered>>> = Arc::new(Mutex::new(Vec::new()));
    let received_probe = Arc::clone(&received);
    let data_out = element(&inp, "rx:data_output_0");
    data_out
        .static_pad("src")
        .expect("data_output_0 has a src pad")
        .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                if let Ok(map) = buffer.map_readable() {
                    received_probe
                        .lock()
                        .unwrap()
                        .push((map.as_slice().to_vec(), pad.current_caps()));
                }
            }
            gst::PadProbeReturn::Ok
        });

    // The block leaves its outputs unlinked; without a sink the push fails as
    // not-linked before the probe above ever runs.
    let audio_sink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();
    let data_sink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();
    rx.add_many([&audio_sink, &data_sink]).unwrap();
    element(&inp, "rx:audio_output_0")
        .link(&audio_sink)
        .expect("audio output feeds a sink");
    data_out.link(&data_sink).expect("data output feeds a sink");

    // --- run --------------------------------------------------------------
    tx.set_state(gst::State::Playing).expect("tx plays");
    rx.set_state(gst::State::Playing).expect("rx plays");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_pusher = Arc::clone(&stop);
    let pusher = std::thread::spawn(move || {
        // Embedded data only leaves the muxer on a media frame of its stream,
        // and the audio sink pad is not requested until the audio caps probe
        // fires, so push repeatedly rather than once and hope for the ordering.
        for i in 0..200u64 {
            if stop_pusher.load(Ordering::SeqCst) {
                break;
            }
            let mut buffer = gst::Buffer::from_slice(PAYLOAD);
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(i * 20));
            if data_src.push_buffer(buffer).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        drain_errors(&tx, "tx");
        drain_errors(&rx, "rx");
        if !received.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    stop.store(true, Ordering::SeqCst);
    let _ = pusher.join();
    rx.set_state(gst::State::Null).unwrap();
    tx.set_state(gst::State::Null).unwrap();

    let got = received.lock().unwrap().clone();
    assert!(
        !got.is_empty(),
        "no embedded data reached data_out_0 within 20s; the channel does not carry bytes \
         end to end"
    );

    let (bytes, caps) = &got[0];
    assert_eq!(
        bytes, PAYLOAD,
        "the bytes that arrived are not the bytes that were sent"
    );

    let caps = caps
        .as_ref()
        .expect("the data output pad should carry caps");
    let s = caps.structure(0).expect("caps has a structure");
    assert_eq!(s.name(), "application/x-efp-embedded");
    assert_eq!(
        s.get::<i32>("data-type").expect("caps carry data-type"),
        DATA_TYPE
    );
    // The field gst-plugin-efp v0.4.0 added. Without it a receiver cannot say
    // which media stream the data describes, which is the whole point of the
    // channel for provenance.
    assert_eq!(
        s.get::<i32>("stream-id").expect("caps carry stream-id"),
        MEDIA_STREAM_ID
    );
}

/// `data_stream_ids` pins each data track to a sender stream, so `data_out_0`
/// means the same thing on every run.
///
/// Two media streams each carry their own embedded data, and the list is
/// deliberately reversed — track 0 is pinned to stream 2 — so arrival order
/// cannot produce this result by luck.
#[test]
fn data_stream_ids_pins_each_track_to_its_sender_stream() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }

    let port = srt_port();
    let ctx = || BlockBuildContext::new(Vec::new(), "all".to_string());

    // Two media tracks, so efpmux allocates streams 1 and 2. Which audio track
    // gets which ID depends on the order their caps probes fire, and the
    // assertion below deliberately does not care: it is about how the receiver
    // routes streams to outputs, not about how the sender numbers them.
    let out = EfpSrtOutputBuilder
        .build(
            "tx",
            &props(&[
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(2)),
                ("num_data_tracks", PropertyValue::UInt(2)),
                (
                    "srt_uri",
                    PropertyValue::String(format!("srt://127.0.0.1:{}?mode=listener", port)),
                ),
                ("wait_for_connection", PropertyValue::Bool(false)),
            ]),
            &ctx(),
        )
        .expect("efpsrt_output builds");

    let tx = gst::Pipeline::with_name("tx2");
    install(&tx, &out);

    for i in 0..2 {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property_from_str("wave", "silence")
            .build()
            .unwrap();
        tx.add(&src).unwrap();
        src.link(element(&out, &format!("tx:audio_input_{}", i)))
            .expect("audio source feeds the block");
    }

    let mut data_srcs = Vec::new();
    for (i, stream_id) in [1i32, 2i32].into_iter().enumerate() {
        let caps = gst::Caps::builder("application/x-efp-embedded")
            .field("data-type", DATA_TYPE)
            .field("stream-id", stream_id)
            .build();
        let src = gst::ElementFactory::make("appsrc")
            .property("caps", &caps)
            .property("format", gst::Format::Time)
            .property("is-live", true)
            .build()
            .unwrap();
        tx.add(&src).unwrap();
        src.link(element(&out, &format!("tx:data_input_{}", i)))
            .expect("data source feeds the block");
        data_srcs.push((src.dynamic_cast::<gst_app::AppSrc>().unwrap(), stream_id));
    }

    // Reversed on purpose: data_out_0 must carry stream 2, data_out_1 stream 1.
    let inp = EfpSrtInputBuilder
        .build(
            "rx",
            &props(&[
                ("num_video_tracks", PropertyValue::UInt(0)),
                ("num_audio_tracks", PropertyValue::UInt(2)),
                ("num_data_tracks", PropertyValue::UInt(2)),
                ("data_stream_ids", PropertyValue::String("2,1".to_string())),
                (
                    "srt_uri",
                    PropertyValue::String(format!("srt://127.0.0.1:{}?mode=caller", port)),
                ),
                ("decode", PropertyValue::Bool(false)),
            ]),
            &ctx(),
        )
        .expect("efpsrt_input builds");

    let rx = gst::Pipeline::with_name("rx2");
    install(&rx, &inp);

    let seen: Arc<Mutex<Vec<(usize, i32)>>> = Arc::new(Mutex::new(Vec::new()));
    for track in 0..2usize {
        let out_element = element(&inp, &format!("rx:data_output_{}", track));
        let seen_probe = Arc::clone(&seen);
        out_element.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::BUFFER,
            move |pad, _info| {
                if let Some(caps) = pad.current_caps() {
                    if let Some(s) = caps.structure(0) {
                        if let Ok(id) = s.get::<i32>("stream-id") {
                            let mut seen = seen_probe.lock().unwrap();
                            if !seen.iter().any(|(t, _)| *t == track) {
                                seen.push((track, id));
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .unwrap();
        rx.add(&sink).unwrap();
        out_element.link(&sink).expect("data output feeds a sink");
    }
    for id in ["rx:audio_output_0", "rx:audio_output_1"] {
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .property("sync", false)
            .build()
            .unwrap();
        rx.add(&sink).unwrap();
        element(&inp, id)
            .link(&sink)
            .expect("media output feeds a sink");
    }

    tx.set_state(gst::State::Playing).expect("tx plays");
    rx.set_state(gst::State::Playing).expect("rx plays");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_pusher = Arc::clone(&stop);
    let pusher = std::thread::spawn(move || {
        for i in 0..200u64 {
            if stop_pusher.load(Ordering::SeqCst) {
                break;
            }
            for (src, stream_id) in &data_srcs {
                let mut b =
                    gst::Buffer::from_slice(format!("data-for-stream-{stream_id}").into_bytes());
                b.get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_mseconds(i * 20));
                if src.push_buffer(b).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        drain_errors(&tx, "tx");
        drain_errors(&rx, "rx");
        if seen.lock().unwrap().len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    stop.store(true, Ordering::SeqCst);
    let _ = pusher.join();
    rx.set_state(gst::State::Null).unwrap();
    tx.set_state(gst::State::Null).unwrap();

    let mut got = seen.lock().unwrap().clone();
    got.sort();
    assert_eq!(
        got,
        vec![(0usize, 2i32), (1usize, 1i32)],
        "data_stream_ids=\"2,1\" must put stream 2 on data_out_0 and stream 1 on data_out_1"
    );
}
