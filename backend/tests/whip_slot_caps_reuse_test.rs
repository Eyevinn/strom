//! Regression test for a WHIP Input slot being reused by a publisher whose
//! audio format differs from the previous one.
//!
//! A slot's chain (`appsrc_audio_<slot>` → decodebin → convert → tee) is built
//! once and stays in the running pipeline while sessions come and go, and caps
//! travel with every sample pushed into the appsrc. Consumers past the tee have
//! already negotiated: a seat's recorder answers a second audio format with
//! `not-negotiated`, which propagates back, stops the appsrc's streaming thread
//! and kills the seat's audio for good.
//!
//! Each test runs two sessions through one real slot chain into the recorder's
//! shape (aac → mp4mux). Both must reach the muxer, and the tee must see a
//! single caps event — the guard is that the format past the slot never
//! changes, which is what keeps a committed consumer from refusing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use strom::blocks::builtin::whip::build_whipserversrc;
use strom::blocks::BlockBuildContext;
use strom_types::PropertyValue;

/// Elements this test needs. `whipserversrc` is deliberately not among them:
/// the slot chain is plain core GStreamer, so the test runs on a CI image
/// without `gst-plugins-rs`.
const REQUIRED: &[&str] = &[
    "appsrc",
    "appsink",
    "decodebin",
    "audioconvert",
    "audioresample",
    "capsfilter",
    "tee",
    "queue",
    "audiotestsrc",
    "opusenc",
    "opusdec",
    "identity",
    "avenc_aac",
    "mp4mux",
    "fakesink",
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
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

/// Buffers each simulated publisher sends. `audiotestsrc` at its default
/// 1024-sample blocksize gives ~1 s of 48 kHz audio.
const SESSION_BUFFERS: i32 = 50;

/// How much of a session must reach the muxer to count as "the seat has audio".
/// Well under `SESSION_BUFFERS` so encoder latency and resampler regrouping do
/// not make this flaky, and well above the two or three buffers that squeeze
/// through before a refusing muxer stops the branch.
const MIN_BUFFERS: usize = 30;

/// One publisher's worth of media, produced off to the side so the test can
/// feed it into the slot the way the session bridge does.
struct Session {
    caps: gst::Caps,
    buffers: Vec<gst::Buffer>,
}

/// Encode a second of tone with the given encoder at the given rate and channel
/// count, and collect the encoded buffers.
fn encode_session(encoder: &str, rate: i32, channels: i32) -> Session {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", SESSION_BUFFERS)
        .property_from_str("wave", "sine")
        .build()
        .expect("audiotestsrc");
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let resample = gst::ElementFactory::make("audioresample").build().unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("rate", rate)
                .field("channels", channels)
                .build(),
        )
        .build()
        .unwrap();
    let enc = gst::ElementFactory::make(encoder)
        .build()
        .unwrap_or_else(|_| panic!("{}", encoder));
    let sink = gst_app::AppSink::builder().sync(false).build();

    pipeline
        .add_many([&src, &convert, &resample, &caps, &enc, sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([&src, &convert, &resample, &caps, &enc, sink.upcast_ref()]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut buffers = Vec::new();
    let mut sample_caps = None;
    while let Ok(sample) = sink.pull_sample() {
        if sample_caps.is_none() {
            sample_caps = sample.caps().map(|c| c.to_owned());
        }
        if let Some(buffer) = sample.buffer() {
            buffers.push(buffer.copy());
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    assert!(
        !buffers.is_empty(),
        "{} encoded nothing at {} Hz / {} channel(s)",
        encoder,
        rate,
        channels
    );
    Session {
        caps: sample_caps.expect("the encoded stream has caps"),
        buffers,
    }
}

/// A session that arrives already decoded. `decodebin` forwards raw audio
/// untouched, so this reaches the slot's convert/resample stage directly.
fn raw_session(rate: i32, channels: i32) -> Session {
    encode_session("identity", rate, channels)
}

/// Push a session's buffers into the slot appsrc the way the WHIP session
/// bridge does: a fresh sample carrying only the buffer and that session's
/// caps, with PTS rebased onto the main pipeline's running time.
fn push_session(appsrc: &gst_app::AppSrc, session: &Session, pts_offset: gst::ClockTime) {
    for buffer in &session.buffers {
        let mut buffer = buffer.copy();
        {
            let buf = buffer.get_mut().unwrap();
            let pts = buf.pts().unwrap_or(gst::ClockTime::ZERO) + pts_offset;
            buf.set_pts(pts);
            buf.set_dts(None);
        }
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .caps(&session.caps)
            .build();
        appsrc
            .push_sample(&sample)
            .expect("slot appsrc accepts the sample");
    }
}

/// Run `first` then `second` through one WHIP Input slot and assert the slot
/// carried both without ever changing the format it presents downstream.
fn reuse_slot(first: &Session, second: &Session) {
    let instance_id = "whip_in";
    let mut props: HashMap<String, PropertyValue> = HashMap::new();
    // Audio only: the slot's video chain would sit unfed and has nothing to do
    // with the caps change under test.
    props.insert(
        "mode".to_string(),
        PropertyValue::String("audio".to_string()),
    );
    props.insert("max_sessions".to_string(), PropertyValue::Int(1));
    props.insert("decode".to_string(), PropertyValue::Bool(true));
    props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("slot-reuse".to_string()),
    );

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = build_whipserversrc(instance_id, &props, &ctx).expect("WHIP Input block builds");

    let pipeline = gst::Pipeline::new();
    let mut by_id: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &built.elements {
        pipeline.add(element).expect("add block element");
        by_id.insert(id.clone(), element.clone());
    }
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

    let appsrc: gst_app::AppSrc = by_id
        .get(&format!("{}:appsrc_audio_0", instance_id))
        .expect("slot exposes appsrc_audio_0")
        .clone()
        .downcast()
        .expect("appsrc_audio_0 is an appsrc");
    let tee = by_id
        .get(&format!("{}:audio_out_tee_0", instance_id))
        .expect("slot exposes audio_out_tee_0")
        .clone();

    // The recorder's shape: an encoder and a muxer that commit to the first
    // caps they see and refuse to renegotiate afterwards.
    let queue = gst::ElementFactory::make("queue").build().unwrap();
    let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let enc = gst::ElementFactory::make("avenc_aac")
        .build()
        .expect("avenc_aac");
    let mux = gst::ElementFactory::make("mp4mux").build().expect("mp4mux");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline
        .add_many([&queue, &convert, &enc, &mux, &sink])
        .unwrap();
    gst::Element::link_many([&queue, &convert, &enc, &mux, &sink]).unwrap();

    let tee_src = tee.request_pad_simple("src_%u").expect("tee src pad");
    tee_src
        .link(&queue.static_pad("sink").unwrap())
        .expect("link tee into the recorder branch");

    // What the consumer actually sees. Two vantage points, because they answer
    // different questions: the tee's src pad is the slot boundary — the format
    // the slot presents — while the muxer's sink pad is the only place that
    // proves the committed consumer actually accepted the second session.
    let seen_caps: Arc<Mutex<Vec<gst::Caps>>> = Arc::new(Mutex::new(Vec::new()));
    let at_tee = Arc::new(AtomicUsize::new(0));
    {
        let seen_caps = seen_caps.clone();
        let at_tee = at_tee.clone();
        tee_src.add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::BUFFER,
            move |_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Caps(caps) = event.view() {
                            seen_caps.lock().unwrap().push(caps.caps().to_owned());
                        }
                    }
                    Some(gst::PadProbeData::Buffer(_)) => {
                        at_tee.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    let at_mux = Arc::new(AtomicUsize::new(0));
    {
        let at_mux = at_mux.clone();
        // The muxer's sink pad is a request pad, already claimed by link_many,
        // so reach it through the encoder rather than requesting a second one.
        enc.static_pad("src")
            .and_then(|pad| pad.peer())
            .expect("the muxer's sink pad is linked to the encoder")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                at_mux.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
    }

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    let bus = pipeline.bus().expect("pipeline has a bus");
    let check_bus = |phase: &str| {
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(err) = msg.view() {
                panic!(
                    "{}: pipeline error from {:?}: {} ({:?})",
                    phase,
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
        }
    };

    // Wait until `counter` stops growing, so the next measurement attributes
    // every new buffer to the session that was pushed after it.
    let settle = |counter: &AtomicUsize, phase: &str| -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = counter.load(Ordering::Relaxed);
        let mut stable_since = Instant::now();
        while Instant::now() < deadline {
            check_bus(phase);
            std::thread::sleep(Duration::from_millis(50));
            let now = counter.load(Ordering::Relaxed);
            if now != last {
                last = now;
                stable_since = Instant::now();
            } else if last > 0 && stable_since.elapsed() >= Duration::from_millis(300) {
                break;
            }
        }
        check_bus(phase);
        last
    };

    // First publisher: let its audio drain all the way into the muxer, so the
    // muxer has committed to this format before the slot is reused.
    push_session(&appsrc, first, gst::ClockTime::ZERO);
    let first_at_mux = settle(&at_mux, "first session");
    assert!(
        first_at_mux >= MIN_BUFFERS,
        "the first session's audio never reached the muxer \
         ({} of {} buffers, {} reached the slot boundary)",
        first_at_mux,
        SESSION_BUFFERS,
        at_tee.load(Ordering::Relaxed)
    );
    let first_at_tee = at_tee.load(Ordering::Relaxed);

    // Second publisher on the same slot, in a different audio format, starting
    // where the first left off. Without the slot's capsfilter this is where the
    // muxer refuses: the buffers still reach the tee, and stop there.
    push_session(&appsrc, second, gst::ClockTime::from_seconds(2));
    let second_at_mux = settle(&at_mux, "second session").saturating_sub(first_at_mux);
    let second_at_tee = at_tee.load(Ordering::Relaxed).saturating_sub(first_at_tee);

    let caps = seen_caps.lock().unwrap().clone();
    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        second_at_mux >= MIN_BUFFERS,
        "the slot did not survive being reused in a different audio format: \
         only {} of the second session's buffers reached the muxer \
         ({} of them reached the slot boundary, so the slot itself was fed). \
         The consumer saw these caps: {:?}",
        second_at_mux,
        second_at_tee,
        caps
    );
    // Why the above holds: downstream was never told the format changed.
    assert_eq!(
        caps.len(),
        1,
        "the slot must present one audio format for the life of the flow, \
         but the consumer saw {} caps events: {:?}",
        caps.len(),
        caps
    );
}

#[test]
fn slot_accepts_a_second_session_with_a_different_channel_count() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }
    reuse_slot(
        &encode_session("opusenc", 48000, 1),
        &encode_session("opusenc", 48000, 2),
    );
}

/// Rate as well as channels. `whipserversrc` negotiates OPUS only and
/// `opusdec` always outputs 48 kHz, so a live WHIP slot is not handed a rate
/// change today — this drives the boundary with raw sessions instead, so the
/// pinning is not resting on the codec's output rate happening to be fixed.
#[test]
fn slot_normalises_a_second_session_at_a_different_sample_rate() {
    gst::init().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }
    reuse_slot(&raw_session(48000, 2), &raw_session(16000, 1));
}
