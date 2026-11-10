//! WHEP Output keeps running when its H.264 or H.265 input changes size and
//! level mid-stream.
//!
//! An HLS source that switches variant (640x360p25 to 1080p50) makes the
//! Video Encoder pick a higher H.264 level (3 to 4.2). webrtcsink refuses a
//! level rise on its input ("Incompatible H264 levels", then "Renegotiation is
//! not supported"), the encoder gets `not-negotiated` and the flow stops. The
//! block removes the level from its input caps so the change goes through.
//! For H.265 webrtcsink has no level rule at all and refuses any change of
//! level or tier; the block removes both.
//!
//! The test builds the real Video Encoder (x264enc, x265enc) and the real WHEP Output,
//! pushes the switch and checks that the flow posts no error and that 1080p
//! frames reach the sink. It fails if the block stops removing the level:
//! the flow then posts `Internal data stream error` (not-negotiated) at the
//! switch. webrtcsink checks its input caps whether or not a viewer is
//! connected, so no viewer is needed; one without ICE and DTLS stalls after
//! its first packets anyway, which would hide what happens at the switch.

pub mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use strom::blocks::builtin::get_builder;
use strom_types::PropertyValue;

/// `x264enc` is in plugins-ugly, `x265enc` in plugins-bad, `nicesrc` in
/// gstreamer1.0-nice; CI installs all three.
const REQUIRED: &[&str] = &["appsrc", "webrtcbin", "nicesrc", "whepserversink"];

fn level(caps: &gst::CapsRef) -> String {
    caps.structure(0)
        .and_then(|s| s.get::<String>("level").ok())
        .unwrap_or_default()
}

#[test]
fn whep_output_follows_an_h264_level_rise_from_a_size_change() {
    size_change("h264", "software", Some("x264enc"), "h264parse", true);
}

#[test]
fn whep_output_follows_an_h265_level_rise_from_a_size_change() {
    size_change("h265", "software", Some("x265enc"), "h265parse", true);
}

/// H.265 Main 10 from 10-bit input: the session starts from the caps
/// h265parse completes (profile, 4:2:0, 10-bit) and follows the switch.
#[test]
fn whep_output_follows_a_size_change_h265_main_10() {
    size_change_in(
        "h265",
        "software",
        Some("x265enc"),
        "h265parse",
        true,
        Some("main-10"),
        "I420_10LE",
    );
}

// The encoder the block picks on its default preference: a hardware one where
// the host has it (VideoToolbox on macOS, NVENC, ...), else the software one.

#[test]
fn whep_output_starts_and_follows_a_size_change_h264_default_encoder() {
    size_change("h264", "auto", None, "h264parse", false);
}

#[test]
fn whep_output_starts_and_follows_a_size_change_h265_default_encoder() {
    size_change("h265", "auto", None, "h265parse", false);
}

/// vp9parse sends caps without `chroma-format` and the bit depths, then adds
/// them once it has parsed the first frame. That is a refinement, not a
/// change: the flow has to start.
#[test]
fn whep_output_starts_and_follows_a_size_change_vp9() {
    size_change("vp9", "auto", None, "vp9parse", false);
}

/// av1parse refines its caps the same way as vp9parse.
#[test]
fn whep_output_starts_and_follows_a_size_change_av1() {
    if common::skip_known_ci_failure(
        &["linux"],
        "whepserversink posts \"There is no codec present that can handle the stream's type\" \
         on the 360p to 1080p switch with SVT-AV1 2.3 on the Linux runner",
    ) {
        return;
    }
    // The Video Encoder's software AV1 encoders, either will do.
    if common::missing_elements(&["svtav1enc", "av1enc"]).len() == 2
        && !common::require_or_skip(&["svtav1enc or av1enc"])
    {
        return;
    }
    size_change("av1", "auto", None, "av1parse", false);
}

/// Encode 640x360p25 then 1080p50 with the Video Encoder for `codec` on
/// `preference` into WHEP Output, and check that the flow starts and follows
/// the switch. `encoder` pins the encoder the block must pick;
/// `level_must_change` asserts the switch raised the H.264/H.265 level.
fn size_change(
    codec: &str,
    preference: &str,
    encoder: Option<&str>,
    parser: &str,
    level_must_change: bool,
) {
    size_change_in(
        codec,
        preference,
        encoder,
        parser,
        level_must_change,
        None,
        "I420",
    );
}

/// [`size_change`] with the Video Encoder's `profile` and the raw input
/// `format`.
fn size_change_in(
    codec: &str,
    preference: &str,
    encoder: Option<&str>,
    parser: &str,
    level_must_change: bool,
    profile: Option<&str>,
    format: &'static str,
) {
    common::init_webrtc_plugins();
    let mut required = REQUIRED.to_vec();
    required.push(parser);
    required.extend(encoder);
    if !common::plugins_available(&required) {
        return;
    }

    let ctx = common::block::context();
    let mut enc_props = HashMap::new();
    enc_props.insert("codec".to_string(), PropertyValue::String(codec.into()));
    enc_props.insert(
        "encoder_preference".to_string(),
        PropertyValue::String(preference.into()),
    );
    enc_props.insert("keyframe_interval".to_string(), PropertyValue::UInt(10));
    if let Some(profile) = profile {
        enc_props.insert("profile".to_string(), PropertyValue::String(profile.into()));
    }
    let enc = get_builder("builtin.videoenc")
        .expect("Video Encoder builder")
        .build("enc", &enc_props, &ctx)
        .expect("Video Encoder builds");

    let mut whep_props = HashMap::new();
    whep_props.insert("num_audio_tracks".to_string(), PropertyValue::Int(0));
    whep_props.insert("num_video_tracks".to_string(), PropertyValue::Int(1));
    whep_props.insert(
        "endpoint_id".to_string(),
        PropertyValue::String(format!("level-change-test-{codec}-{preference}")),
    );
    let whep = get_builder("builtin.whep_output")
        .expect("WHEP Output builder")
        .build("whep", &whep_props, &ctx)
        .expect("WHEP Output builds");

    let pipeline = gst::Pipeline::new();
    let enc_els = common::block::install(&pipeline, &enc);
    let whep_els = common::block::install(&pipeline, &whep);
    let picked = enc_els["enc:encoder"].factory().unwrap().name().to_string();
    eprintln!("{codec} ({preference}): Video Encoder picked {picked}");
    if let Some(encoder) = encoder {
        assert_eq!(picked, encoder, "{preference} {codec} encoder");
    }

    let appsrc = gst_app::AppSrc::builder()
        .format(gst::Format::Time)
        .is_live(true)
        .block(true)
        .max_buffers(4)
        .build();
    pipeline.add(&appsrc).unwrap();
    appsrc.link(&enc_els["enc:input"]).unwrap();
    enc_els["enc:capsfilter"]
        .link(&whep_els["whep:video_queue"])
        .expect("link encoder to WHEP Output");
    common::block::run_setups(&ctx);

    // Levels the encoder produced, to show the test exercises a level rise.
    let levels = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let levels = levels.clone();
        enc_els["enc:capsfilter"]
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    if let gst::EventView::Caps(c) = event.view() {
                        levels.lock().unwrap().push(level(c.caps()));
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }

    // 1080p frames that reached the sink.
    let sink = &whep_els["whep:whepserversink"];
    let hd_frames = Arc::new(AtomicUsize::new(0));
    {
        let hd_frames = hd_frames.clone();
        sink.static_pad("video_0")
            .expect("whepserversink video_0 pad")
            .add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
                if pad
                    .current_caps()
                    .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("width").ok()))
                    == Some(1920)
                {
                    hd_frames.fetch_add(1, Ordering::Relaxed);
                }
                gst::PadProbeReturn::Ok
            });
    }

    pipeline.set_state(gst::State::Playing).expect("PLAYING");
    let bus = pipeline.bus().unwrap();
    // Push 2 s of 640x360p25, then 2 s of 1080p50, paced to the clock by the
    // live appsrc and the sink's clocksync. A push blocks while the appsrc is
    // full, so on a failed flow the pusher only returns once the pipeline is
    // stopped below.
    let pusher = {
        let appsrc = appsrc.clone();
        std::thread::spawn(move || {
            let mut pts = gst::ClockTime::ZERO;
            for (width, height, fps, frames) in [(640, 360, 25, 50), (1920, 1080, 50, 100)] {
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", format)
                    .field("width", width)
                    .field("height", height)
                    .field("framerate", gst::Fraction::new(fps, 1))
                    .build();
                // 4:2:0: 1.5 samples per pixel, of 2 bytes above 8 bits.
                let bytes_per_sample = if format.ends_with("LE") { 2 } else { 1 };
                let size = (width * height * 3 / 2 * bytes_per_sample) as usize;
                let duration = gst::ClockTime::SECOND / fps as u64;
                for i in 0..frames {
                    let mut buffer = gst::Buffer::from_mut_slice(vec![(i * 7 % 255) as u8; size]);
                    {
                        let b = buffer.get_mut().unwrap();
                        b.set_pts(pts);
                        b.set_duration(duration);
                    }
                    pts += duration;
                    let sample = gst::Sample::builder().caps(&caps).buffer(&buffer).build();
                    if appsrc.push_sample(&sample).is_err() {
                        return;
                    }
                }
            }
            // Drain the encoder: a software AV1 encoder holds back over a
            // second of frames for its lookahead.
            let _ = appsrc.end_of_stream();
        })
    };

    let mut errors: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut settle: Option<Instant> = None;
    while errors.is_empty() && Instant::now() < deadline {
        if pusher.is_finished() {
            // Let the last frames through the encoder: a software AV1 encoder
            // holds back many frames and runs slower than real time at 1080p.
            let until = *settle.get_or_insert_with(|| Instant::now() + Duration::from_secs(15));
            if Instant::now() >= until || hd_frames.load(Ordering::Relaxed) > 50 {
                break;
            }
        }
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) {
            if let gst::MessageView::Error(err) = msg.view() {
                errors.push(format!(
                    "{}: {}",
                    msg.src().map(|s| s.name().to_string()).unwrap_or_default(),
                    err.error()
                ));
            }
        }
    }

    let sink_caps = sink
        .static_pad("video_0")
        .and_then(|p| p.current_caps())
        .map(|c| c.to_string())
        .unwrap_or_default();
    let _ = pipeline.set_state(gst::State::Null);
    pusher.join().unwrap();

    let levels = levels.lock().unwrap().clone();
    if level_must_change {
        assert!(
            levels.first().is_some_and(|l| !l.is_empty()) && levels.first() != levels.last(),
            "the encoder has to change the level for this test to mean anything: {levels:?}"
        );
    }
    eprintln!("{codec} levels at the encoder output: {levels:?}");
    assert!(
        errors.is_empty(),
        "the flow failed on the switch: {errors:?}"
    );
    assert!(
        sink_caps.contains("width=(int)1920"),
        "WHEP Output's sink did not take the 1080p caps: {sink_caps}"
    );
    if let Some(profile) = profile {
        assert!(
            sink_caps.contains(&format!("profile=(string){profile}")),
            "WHEP Output's sink lost the {profile} profile: {sink_caps}"
        );
    }
    let hd_frames = hd_frames.load(Ordering::Relaxed);
    assert!(
        hd_frames > 50,
        "only {hd_frames} 1080p frames reached WHEP Output's sink"
    );
}
