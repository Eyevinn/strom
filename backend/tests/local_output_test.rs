//! Local Output plays raw audio on a local device with the channel count it is
//! set to, and refuses what it cannot play with its own reason (#1080).
//!
//! CI runners have no sound card, and there `autoaudiosink` falls back to a
//! sink that takes anything, which would let unadapted audio through. So each
//! test puts a stand-in device, with the caps a real one offers, where the OS
//! default would play.

pub mod common;

use common::bus::BusError;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use strom::blocks::builtin::local_output::LocalOutputBuilder;
use strom::blocks::{BlockBuildError, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const REQUIRED: &[&str] = &[
    "audiotestsrc",
    "audioconvert",
    "audioresample",
    "autoaudiosink",
    "capsfilter",
    "opusenc",
];

/// Widest output a test reads back peaks for.
const MAX_TEST_CHANNELS: usize = 16;

/// The name the test source gets, so its errors can be told from the block's.
const TEST_SOURCE: &str = "test_source";

/// What macOS's `osxaudiosink` offers for built-in speakers. S16 only, so the
/// test can read the samples that reach the device.
const SPEAKERS: &str = "audio/x-raw, format=S16LE, layout=interleaved, channels=2, \
                        channel-mask=(bitmask)0x3; \
                        audio/x-raw, format=S16LE, layout=interleaved, channels=1";

/// An ASIO interface with 16 outputs: any count up to 16, no layout.
const ASIO_16: &str = "audio/x-raw, format=S16LE, layout=interleaved, channels=(int)[ 1, 16 ]";

/// A stand-in output device that takes `caps` and plays nowhere.
fn device(name: &str, caps: &str) -> gst::Element {
    let bin = gst::Bin::with_name(name);
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", gst::Caps::from_str(caps).expect("device caps"))
        .build()
        .expect("capsfilter");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", true)
        .build()
        .expect("fakesink");
    bin.add_many([&filter, &sink]).expect("add device parts");
    filter.link(&sink).expect("link device parts");
    let pad = gst::GhostPad::with_target(&filter.static_pad("sink").expect("sink pad"))
        .expect("ghost pad");
    bin.add_pad(&pad).expect("add ghost pad");
    bin.upcast()
}

/// Build the block with `channels` (default when `None`) and no device picked,
/// put a [`device`] taking `device_caps` where the OS default would play, feed
/// it silence through `caps` (and `encoder`, when given), and run briefly.
fn run_block(
    instance: &str,
    channels: Option<&str>,
    device_caps: &str,
    caps: gst::Caps,
    encoder: Option<&str>,
) -> Outcome {
    gst::init().unwrap();
    common::require_elements(REQUIRED);

    let ctx = common::block::context();
    let props: HashMap<String, PropertyValue> = channels
        .map(|c| {
            (
                "audio_channels".to_string(),
                PropertyValue::String(c.into()),
            )
        })
        .into_iter()
        .collect();
    let mut built = LocalOutputBuilder
        .build(instance, &props, &ctx)
        .expect("block builds");
    let sink_id = format!("{}:sink", instance);
    let sink = built
        .elements
        .iter_mut()
        .find(|(id, _)| *id == sink_id)
        .expect("block builds a sink");
    assert_eq!(
        sink.1.factory().map(|f| f.name().to_string()).as_deref(),
        Some("autoaudiosink"),
        "with no device picked the block plays on the OS default"
    );
    sink.1 = device(&sink_id, device_caps);
    let device_pad = sink.1.static_pad("sink").expect("device pad");
    // A real device sink fails on a gap that comes before any caps
    // ("Sink not negotiated before gap event"); the stand-in does not, so
    // count them here.
    let early_gaps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let early_gaps_probe = early_gaps.clone();
    device_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        if let Some(gst::PadProbeData::Event(e)) = info.data.as_ref() {
            if e.type_() == gst::EventType::Gap && pad.current_caps().is_none() {
                early_gaps_probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        gst::PadProbeReturn::Ok
    });
    // Loudest sample per output channel that reaches the device. A BUFFER
    // probe, so atomics only, sized before the run: no lock, no allocation.
    let peaks: std::sync::Arc<Vec<std::sync::atomic::AtomicI32>> = std::sync::Arc::new(
        (0..MAX_TEST_CHANNELS)
            .map(|_| std::sync::atomic::AtomicI32::new(0))
            .collect(),
    );
    let channels_seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let channels_probe = channels_seen.clone();
    device_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        if let Some(gst::PadProbeData::Event(e)) = info.data.as_ref() {
            if let gst::EventView::Caps(c) = e.view() {
                let channels = c
                    .caps()
                    .structure(0)
                    .and_then(|s| s.get::<i32>("channels").ok())
                    .unwrap_or(0) as usize;
                channels_probe.store(channels, std::sync::atomic::Ordering::SeqCst);
            }
        }
        gst::PadProbeReturn::Ok
    });
    let peaks_probe = peaks.clone();
    let channels_buffer = channels_seen.clone();
    device_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        let channels = channels_buffer
            .load(std::sync::atomic::Ordering::Relaxed)
            .clamp(1, MAX_TEST_CHANNELS);
        if let Some(buffer) = info.buffer() {
            if let Ok(map) = buffer.map_readable() {
                for (i, frame) in map.as_chunks::<2>().0.iter().enumerate() {
                    let v = (i16::from_le_bytes(*frame) as i32).abs();
                    peaks_probe[i % channels].fetch_max(v, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);
    let input = by_id[&format!("{}:input", instance)].clone();
    let convert = by_id[&format!("{}:audioconvert", instance)].clone();

    let src = gst::ElementFactory::make("audiotestsrc")
        .name(TEST_SOURCE)
        .property("is-live", true)
        .property("volume", 0.3f64)
        .build()
        .expect("audiotestsrc");
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .expect("capsfilter");
    let mut chain = vec![src, filter];
    if let Some(factory) = encoder {
        chain.push(gst::ElementFactory::make(factory).build().expect(factory));
    }
    pipeline.add_many(&chain).expect("add source");
    chain.push(input);
    gst::Element::link_many(&chain).expect("link source to block");

    common::block::run_setups(&ctx);

    let bus = pipeline.bus().expect("pipeline bus");
    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");
    let errors = common::bus::collect_errors(&bus, Duration::from_secs(2));
    let (_, state, _) = pipeline.state(gst::ClockTime::from_seconds(1));
    let matrix_rows = convert.property::<gst::Array>("mix-matrix").len();
    let at_device = device_pad.current_caps().and_then(|c| {
        let s = c.structure(0)?;
        Some((
            s.get::<i32>("channels").ok()?,
            s.get::<gst::Bitmask>("channel-mask").ok().map(|m| m.0),
        ))
    });
    let _ = pipeline.set_state(gst::State::Null);
    let seen = channels_seen
        .load(std::sync::atomic::Ordering::SeqCst)
        .min(MAX_TEST_CHANNELS);
    let peaks = peaks[..seen]
        .iter()
        .map(|p| p.load(std::sync::atomic::Ordering::SeqCst))
        .collect();
    let early_gaps = early_gaps.load(std::sync::atomic::Ordering::SeqCst);
    Outcome {
        state,
        errors,
        matrix_rows,
        at_device,
        peaks,
        early_gaps,
    }
}

struct Outcome {
    state: gst::State,
    errors: Vec<BusError>,
    /// Rows of the block's `audioconvert` mix-matrix: its output channels when
    /// the block set one, 0 when it left the converter to decide.
    matrix_rows: usize,
    /// Channels and channel-mask the device negotiated.
    at_device: Option<(i32, Option<u64>)>,
    /// Loudest sample on each output channel at the device.
    peaks: Vec<i32>,
    /// Gap events that reached the device before any caps did.
    early_gaps: usize,
}

/// Six unpositioned channels of 32-bit float at 96 kHz, as Local Input hands
/// on from a multichannel interface.
fn six_unpositioned() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("channels", 6i32)
        .field("channel-mask", gst::Bitmask::new(0))
        .field("rate", 96000i32)
        .build()
}

/// The refusal `instance`'s input posted, asserting it is the only one and
/// that the source did not follow it with an error of its own.
fn the_refusal(instance: &str, out: &Outcome) -> String {
    let errors = &out.errors;
    assert_eq!(
        out.early_gaps, 0,
        "a refused stream's gaps must not reach the device, or a real sink posts \
         its own error before ours"
    );
    let input = format!("{}:input", instance);
    let from_block: Vec<&BusError> = errors.iter().filter(|e| e.source == input).collect();
    assert_eq!(
        from_block.len(),
        1,
        "expected one error from the block's input, got: {:?}",
        errors
    );
    assert!(
        !errors.iter().any(|e| e.source == TEST_SOURCE),
        "the refused stream must be silenced: {:?}",
        errors
    );
    from_block[0].message.clone()
}

/// By default the device gets stereo: six unpositioned channels play 1 left
/// and 2 right on built-in speakers, converted from F32 at 96 kHz.
#[test]
fn by_default_multichannel_audio_plays_1_and_2_in_stereo() {
    gst::init().unwrap();
    let out = run_block("lo_raw", None, SPEAKERS, six_unpositioned(), None);
    assert!(
        out.errors.is_empty(),
        "raw audio must play: {:?}",
        out.errors
    );
    assert_eq!(out.state, gst::State::Playing);
    assert_eq!(
        out.matrix_rows, 2,
        "six unpositioned channels have no layout to mix down by; the block must \
         pick channels 1 and 2 itself, or a real device refuses them"
    );
    assert_eq!(out.at_device, Some((2, Some(0x3))));
}

/// Mono plays on both sides of a stereo device, not only the left.
#[test]
fn mono_plays_on_both_sides() {
    gst::init().unwrap();
    let mono = gst::Caps::builder("audio/x-raw")
        .field("channels", 1i32)
        .field("rate", 48000i32)
        .build();
    let out = run_block("lo_mono", None, SPEAKERS, mono, None);
    assert!(out.errors.is_empty(), "mono must play: {:?}", out.errors);
    assert_eq!(out.at_device, Some((2, Some(0x3))));
    assert!(
        out.peaks.len() == 2 && out.peaks.iter().all(|p| *p > 1000),
        "both channels must carry the mono signal: peaks {:?}",
        out.peaks
    );
}

/// Set to 8, an interface with 16 outputs gets 8 independent channels: inputs
/// 1-6 on outputs 1-6, and 7-8 silent.
#[test]
fn eight_channels_reach_an_interface_as_eight_independent_outputs() {
    gst::init().unwrap();
    let out = run_block("lo_eight", Some("8"), ASIO_16, six_unpositioned(), None);
    assert!(
        out.errors.is_empty(),
        "raw audio must play: {:?}",
        out.errors
    );
    assert_eq!(out.state, gst::State::Playing);
    assert_eq!(
        out.matrix_rows, 8,
        "one row per output, channel for channel"
    );
    assert_eq!(out.at_device, Some((8, Some(0))));
}

/// Set to 8, built-in speakers cannot play it. The block says so, and names
/// what the device takes.
#[test]
fn a_device_that_cannot_take_the_channels_is_refused_with_what_it_takes() {
    gst::init().unwrap();
    let out = run_block("lo_toomany", Some("8"), SPEAKERS, six_unpositioned(), None);
    let reason = the_refusal("lo_toomany", &out);
    assert!(reason.contains("8 channels"), "{}", reason);
    assert!(reason.contains("audio_channels"), "{}", reason);
}

/// More input channels than GStreamer's channel mixer takes are refused by
/// the block, instead of reaching `audioconvert`, which asserts on them.
#[test]
fn more_than_64_input_channels_are_refused() {
    gst::init().unwrap();
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("channels", 65i32)
        .field("channel-mask", gst::Bitmask::new(0))
        .field("rate", 48000i32)
        .build();
    let out = run_block("lo_wide", None, SPEAKERS, caps, None);
    let reason = the_refusal("lo_wide", &out);
    assert!(
        reason.contains("at most 64") && reason.contains("65"),
        "{}",
        reason
    );
}

/// Encoded audio cannot be played. The block says so from its input, and the
/// source does not follow with a not-negotiated error of its own.
#[test]
fn encoded_audio_is_refused_with_the_blocks_reason() {
    gst::init().unwrap();
    let caps = gst::Caps::builder("audio/x-raw")
        .field("channels", 2i32)
        .field("rate", 48000i32)
        .build();
    let out = run_block("lo_opus", None, SPEAKERS, caps, Some("opusenc"));
    let reason = the_refusal("lo_opus", &out);
    assert!(
        reason.contains("audio/x-opus") && reason.contains("raw audio"),
        "the refusal must name what arrived and what the block plays: {}",
        reason
    );
}

/// A device id that discovery does not know fails the build with a message
/// that says how to fix it, instead of playing somewhere else.
#[test]
fn an_unknown_device_fails_the_build() {
    gst::init().unwrap();
    let ctx = common::block::context();
    let props = HashMap::from([(
        "audio_device".to_string(),
        PropertyValue::String("dev-gone".to_string()),
    )]);
    match LocalOutputBuilder.build("lo_gone", &props, &ctx) {
        Err(BlockBuildError::InvalidConfiguration(msg)) => {
            assert!(msg.contains("dev-gone"), "{}", msg);
            assert!(msg.contains("/api/discovery/devices"), "{}", msg);
        }
        Err(other) => panic!("expected InvalidConfiguration, got {:?}", other),
        Ok(_) => panic!("an unknown device must not build"),
    }
}
