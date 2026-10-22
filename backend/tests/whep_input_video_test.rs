//! `builtin.whep_input` video output, end to end over a real WebRTC session.
//!
//! Strom's own `builtin.whep_output` hosts the WHEP endpoint, fed by test
//! sources. A WHEP Input built through its real builder pulls from it on
//! localhost:
//! - with `video_out` linked, decoded frames must arrive there, and the
//!   depayloader must ask for keyframes on loss;
//! - with `video_out` declared but not linked, the video must be discarded
//!   without a decoder ever being created.

pub mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom::blocks::builtin::get_builder;
use strom_types::PropertyValue;

/// Elements this test needs on top of the statically linked WebRTC plugins.
/// `nicesrc` comes from gstreamer1.0-nice, `x264enc` from plugins-ugly and
/// `avdec_h264` from gstreamer1.0-libav, all in the CI package list.
const REQUIRED: &[&str] = &[
    "nicesrc",
    "nicesink",
    "videotestsrc",
    "audiotestsrc",
    "x264enc",
    "avdec_h264",
    "opusenc",
    "opusdec",
    "decodebin",
    "liveadder",
    "tee",
    "appsink",
];

fn available() -> bool {
    common::init_webrtc_plugins();
    strom::gpu::detect_gpu_capabilities();
    common::plugins_available(REQUIRED)
}

/// A WHEP Output fed by live test sources, serving on its own port.
struct Server {
    pipeline: gst::Pipeline,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn start_server(endpoint: &str) -> Server {
    let props: HashMap<String, PropertyValue> = [
        (
            "endpoint_id".to_string(),
            PropertyValue::String(endpoint.to_string()),
        ),
        ("num_audio_tracks".to_string(), PropertyValue::UInt(1)),
        ("num_video_tracks".to_string(), PropertyValue::UInt(1)),
    ]
    .into();
    let ctx = common::block::context();
    let built = get_builder("builtin.whep_output")
        .expect("whep_output builder")
        .build("srv", &props, &ctx)
        .expect("WHEP Output builds");
    let port = ctx
        .take_whep_endpoints()
        .first()
        .expect("WHEP Output registers an endpoint")
        .internal_port;

    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);

    let vsrc = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let vcaps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        )
        .build()
        .unwrap();
    // Feed H.264 encoded here, so WHEP Output passes it through instead of
    // encoding with whichever encoder the host has (vtenc_h264 on macOS,
    // whose sessions fail to negotiate with whepclientsrc). Constrained
    // baseline, no B-frames, frequent keyframes: what a WebRTC receiver takes.
    let venc = gst::ElementFactory::make("x264enc")
        .property_from_str("tune", "zerolatency")
        .property("key-int-max", 30u32)
        .build()
        .unwrap();
    let hcaps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("profile", "constrained-baseline")
                .build(),
        )
        .build()
        .unwrap();
    let asrc = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    pipeline
        .add_many([&vsrc, &vcaps, &venc, &hcaps, &asrc])
        .unwrap();
    gst::Element::link_many([&vsrc, &vcaps, &venc, &hcaps]).unwrap();
    hcaps.link(&by_id["srv:video_queue"]).unwrap();
    asrc.link(&by_id["srv:audio_queue"]).unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("WHEP Output pipeline starts");

    // whepserversink starts its HTTP server only after codec discovery; a
    // client that posts its offer before then is refused and does not retry.
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "WHEP Output never started serving on port {}",
            port
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Server {
        pipeline,
        url: format!("http://127.0.0.1:{}/whep/endpoint", port),
    }
}

/// A WHEP Input pulling from `url`, with `num_video_tracks = 1`.
struct Client {
    pipeline: gst::Pipeline,
    by_id: HashMap<String, gst::Element>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Build the client. `link_video` decides whether `video_out` gets a
/// consumer; the setup hooks run after linking, as the pipeline manager runs
/// them.
fn build_client(
    url: &str,
    implementation: &str,
    link_video: bool,
) -> (Client, Option<gst_app::AppSink>) {
    let props: HashMap<String, PropertyValue> = [
        (
            "whep_endpoint".to_string(),
            PropertyValue::String(url.to_string()),
        ),
        (
            "implementation".to_string(),
            PropertyValue::String(implementation.to_string()),
        ),
        ("num_video_tracks".to_string(), PropertyValue::UInt(1)),
    ]
    .into();
    let builder = get_builder("builtin.whep_input").expect("whep_input builder");
    let pads = builder
        .get_external_pads(&props)
        .expect("WHEP Input has dynamic pads");
    let video_pad = pads
        .outputs
        .iter()
        .find(|p| p.name == "video_out")
        .expect("num_video_tracks = 1 exposes video_out");
    let audio_pad = pads
        .outputs
        .iter()
        .find(|p| p.name == "audio_out")
        .expect("audio_out is always exposed");

    let ctx = common::block::context();
    let built = builder
        .build("cli", &props, &ctx)
        .expect("WHEP Input builds");
    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);

    // The audio output always has a consumer, as in a real flow.
    let audio_sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .property("async", false)
        .build()
        .unwrap();
    pipeline.add(&audio_sink).unwrap();
    by_id[&format!("cli:{}", audio_pad.internal_element_id)]
        .link_pads(Some(&audio_pad.internal_pad_name), &audio_sink, None)
        .expect("link audio_out");

    let appsink = link_video.then(|| {
        let appsink = gst_app::AppSink::builder()
            .sync(false)
            .max_buffers(1)
            .drop(true)
            .build();
        pipeline.add(&appsink).unwrap();
        by_id[&format!("cli:{}", video_pad.internal_element_id)]
            .link_pads(
                Some(&video_pad.internal_pad_name),
                appsink.upcast_ref::<gst::Element>(),
                None,
            )
            .expect("link video_out");
        appsink
    });

    common::block::run_setups(&ctx);
    pipeline
        .set_state(gst::State::Playing)
        .expect("WHEP Input pipeline starts");
    (Client { pipeline, by_id }, appsink)
}

/// Elements anywhere in the pipeline whose name contains `part`.
fn elements_named(pipeline: &gst::Pipeline, part: &str) -> Vec<gst::Element> {
    pipeline
        .iterate_recurse()
        .into_iter()
        .flatten()
        .filter(|e| e.name().contains(part))
        .collect()
}

fn linked_video_out_receives_decoded_frames(implementation: &str, endpoint: &str) {
    let server = start_server(endpoint);
    let (client, appsink) = build_client(&server.url, implementation, true);
    let appsink = appsink.expect("video_out linked");

    let sample = appsink
        .try_pull_sample(gst::ClockTime::from_seconds(30))
        .unwrap_or_else(|| {
            panic!(
                "no decoded video reached video_out within 30 s ({}): {:?}",
                implementation,
                common::bus::first_error(
                    &client.pipeline.bus().unwrap(),
                    Duration::from_millis(100)
                )
            )
        });
    let caps = sample.caps().expect("sample has caps");
    let name = caps.structure(0).unwrap().name().to_string();
    assert_eq!(
        name, "video/x-raw",
        "video_out must carry decoded video, got {}",
        caps
    );

    let decodebins = elements_named(&client.pipeline, "video_decodebin");
    assert_eq!(
        decodebins.len(),
        1,
        "one video decodebin expected for the one video track"
    );

    // whepsrc hands out RTP, so the depayloader sits in our decodebin and must
    // be set to ask for a keyframe on loss. whepclientsrc may depayload
    // inside itself, where this block does not configure anything.
    if implementation == "whepsrc" {
        let bin = decodebins[0].clone().downcast::<gst::Bin>().unwrap();
        let depays: Vec<gst::Element> = bin
            .iterate_recurse()
            .into_iter()
            .flatten()
            .filter(|e| e.has_property("request-keyframe"))
            .collect();
        assert!(!depays.is_empty(), "no depayloader in the video decodebin");
        for depay in depays {
            assert!(
                depay.property::<bool>("request-keyframe"),
                "{} does not request keyframes on loss",
                depay.name()
            );
        }
    }

    drop(client);
    drop(server);
}

#[test]
fn whepsrc_linked_video_out_receives_decoded_frames() {
    if !available() {
        return;
    }
    linked_video_out_receives_decoded_frames("whepsrc", "whep-in-video-a");
}

#[test]
fn whepclientsrc_linked_video_out_receives_decoded_frames() {
    if !available() {
        return;
    }
    linked_video_out_receives_decoded_frames("whepclientsrc", "whep-in-video-b");
}

/// A declared but unlinked `video_out` keeps today's behaviour: the video
/// track reaches a fakesink and nothing decodes it.
#[test]
fn unlinked_video_out_is_discarded_undecoded() {
    if !available() {
        return;
    }
    let server = start_server("whep-in-video-c");
    let (client, _) = build_client(&server.url, "whepsrc", false);

    // Wait for video to reach the discard sink, which proves the session is
    // up and the video track was routed.
    let deadline = Instant::now() + Duration::from_secs(30);
    let discarded = Arc::new(AtomicU64::new(0));
    let probed = AtomicBool::new(false);
    while discarded.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < deadline,
            "no video reached the discard fakesink within 30 s: {:?}",
            common::bus::first_error(&client.pipeline.bus().unwrap(), Duration::from_millis(100))
        );
        if !probed.load(Ordering::Relaxed) {
            if let Some(sink) = elements_named(&client.pipeline, "video_fakesink")
                .into_iter()
                .next()
            {
                let count = discarded.clone();
                sink.static_pad("sink").unwrap().add_probe(
                    gst::PadProbeType::BUFFER,
                    move |_, _| {
                        count.fetch_add(1, Ordering::Relaxed);
                        gst::PadProbeReturn::Remove
                    },
                );
                probed.store(true, Ordering::Relaxed);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(
        elements_named(&client.pipeline, "video_decodebin").is_empty(),
        "an unlinked video_out must not create a video decoder"
    );
    assert!(
        !client.by_id["cli:video_out_tee_0"]
            .static_pad("sink")
            .unwrap()
            .is_linked(),
        "nothing should feed the unlinked video_out"
    );

    drop(client);
    drop(server);
}
