//! Local Output plays whatever raw audio arrives on the OS default device, and
//! refuses encoded audio with its own reason (#1080).

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

/// The name the test source gets, so its errors can be told from the block's.
const TEST_SOURCE: &str = "test_source";

/// A stand-in for a stereo output device, with the caps macOS's `osxaudiosink`
/// offers for built-in speakers: positioned stereo, or mono. It plays nowhere.
/// CI runners have no sound card, and there `autoaudiosink` falls back to a
/// sink that takes anything, which would let unadapted audio through.
fn stereo_device(name: &str) -> gst::Element {
    let bin = gst::Bin::with_name(name);
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::from_str(
                "audio/x-raw, layout=interleaved, channels=2, channel-mask=(bitmask)0x3; \
                 audio/x-raw, layout=interleaved, channels=1",
            )
            .expect("device caps"),
        )
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

/// Build the block with no device picked, put [`stereo_device`] where the OS
/// default would play, feed it silence through `caps` (and `encoder`, when
/// given), run briefly, and return the pipeline's final state and every error
/// the bus carried.
fn run_block(instance: &str, caps: gst::Caps, encoder: Option<&str>) -> Outcome {
    gst::init().unwrap();
    common::require_elements(REQUIRED);

    let ctx = common::block::context();
    let mut built = LocalOutputBuilder
        .build(instance, &HashMap::new(), &ctx)
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
    sink.1 = stereo_device(&sink_id);
    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install(&pipeline, &built);
    let input = by_id[&format!("{}:input", instance)].clone();
    let convert = by_id[&format!("{}:audioconvert", instance)].clone();

    let src = gst::ElementFactory::make("audiotestsrc")
        .name(TEST_SOURCE)
        .property("is-live", true)
        .property_from_str("wave", "silence")
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
    let out_channels = convert
        .static_pad("src")
        .and_then(|p| p.current_caps())
        .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("channels").ok()));
    let _ = pipeline.set_state(gst::State::Null);
    Outcome {
        state,
        errors,
        matrix_rows,
        out_channels,
    }
}

struct Outcome {
    state: gst::State,
    errors: Vec<BusError>,
    /// Rows of the block's `audioconvert` mix-matrix: its output channels when
    /// the block set one, 0 when it left the converter to decide.
    matrix_rows: usize,
    /// Channels `audioconvert` hands on, as negotiated.
    out_channels: Option<i32>,
}

/// Six unpositioned channels of 32-bit float at 96 kHz, as Local Input hands
/// on a multichannel interface, play on a stereo device: the block converts
/// format and rate, and takes channels 1 and 2 to left and right.
#[test]
fn multichannel_float_audio_plays_on_a_stereo_device() {
    gst::init().unwrap();
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("channels", 6i32)
        .field("channel-mask", gst::Bitmask::new(0))
        .field("rate", 96000i32)
        .build();
    let out = run_block("lo_raw", caps, None);
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
    assert_eq!(out.out_channels, Some(2));
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
    let errors = run_block("lo_opus", caps, Some("opusenc")).errors;

    let from_block: Vec<&BusError> = errors
        .iter()
        .filter(|e| e.source == "lo_opus:input")
        .collect();
    assert_eq!(
        from_block.len(),
        1,
        "expected one error from the block's input, got: {:?}",
        errors
    );
    assert!(
        from_block[0].message.contains("audio/x-opus")
            && from_block[0].message.contains("raw audio"),
        "the refusal must name what arrived and what the block plays: {:?}",
        from_block[0]
    );
    assert!(
        !errors.iter().any(|e| e.source == TEST_SOURCE),
        "the refused stream must be silenced: {:?}",
        errors
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
