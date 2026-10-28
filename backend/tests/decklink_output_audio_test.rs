//! DeckLink Output adapts its audio input to what the card takes (#1068).
//!
//! The block used to map `audio_in` straight onto `decklinkaudiosink`, which
//! takes only 2, 8 or 16 channels at 48 kHz, S16LE or S32LE. Six channels from
//! an ASIO interface, mono, or 44.1 kHz audio failed to negotiate. The block
//! now has an audio input front that picks the card's channel count from the
//! input and pads it channel for channel, then converts rate and format.
//!
//! CI has no DeckLink card. These tests build the real block through
//! `DeckLinkOutputBuilder` and put a stand-in in the sinks' place: a
//! capsfilter carrying the caps the real `decklinkaudiosink` answers without a
//! card (its template), in front of an `appsink`. The real sinks never run
//! here.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use strom::blocks::builtin::decklink::{
    DeckLinkOutputBuilder, AUDIO_CARD_CAPS_ELEMENT, AUDIO_INPUT_ELEMENT,
};
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const ELEMENTS: &[&str] = &[
    "decklinkaudiosink",
    "decklinkvideosink",
    "audioconvert",
    "audioresample",
    "appsrc",
    "appsink",
];

/// Input sample rate: not one the card takes.
const IN_RATE: i32 = 44100;
/// Frames per input buffer (100 ms).
const FRAMES: usize = 4410;
/// Input buffers pushed.
const BUFFERS: u64 = 10;

/// The caps the real `decklinkaudiosink` answers without a card.
fn card_caps() -> gst::Caps {
    let sink = gst::ElementFactory::make("decklinkaudiosink")
        .build()
        .expect("decklinkaudiosink");
    sink.static_pad("sink").unwrap().query_caps(None)
}

/// The value input channel `i` carries throughout.
fn level(i: usize) -> f32 {
    (i as f32 + 1.0) / 32.0
}

struct Run {
    /// The caps the card got.
    caps: Option<gst::Caps>,
    /// One frame from the middle of a later buffer, as floats in [-1, 1].
    frame: Vec<f32>,
    /// The first error on the bus.
    error: Option<common::bus::BusError>,
}

/// Build DeckLink Output (audio only) as `id`, feed it `channels` channels of
/// F32LE at 44.1 kHz with `mask` as their channel-mask (none when `None`),
/// each channel at its own [`level`], and read what reaches the card.
fn run(id: &str, channels: usize, mask: Option<u64>) -> Run {
    let props = HashMap::from([(
        "stream_mode".to_string(),
        PropertyValue::String("audio".into()),
    )]);
    let built = DeckLinkOutputBuilder
        .build(id, &props, &common::block::context())
        .expect("DeckLink Output builds");
    let skip: Vec<String> = [
        "decklinkvideosink",
        "video_filler",
        "video_filler_caps",
        "decklinkaudiosink",
    ]
    .iter()
    .map(|name| format!("{id}:{name}"))
    .collect();
    let skip: Vec<&str> = skip.iter().map(String::as_str).collect();
    let pipeline = gst::Pipeline::new();
    let by_id = common::block::install_except(&pipeline, &built, &skip);

    let card = gst::ElementFactory::make("capsfilter")
        .property("caps", card_caps())
        .build()
        .unwrap();
    let appsink = gstreamer_app::AppSink::builder().sync(false).build();
    pipeline.add_many([&card, appsink.upcast_ref()]).unwrap();
    by_id
        .get(&format!("{id}:{AUDIO_CARD_CAPS_ELEMENT}"))
        .expect("the block builds the card's caps filter")
        .link(&card)
        .expect("the block links to the card's caps");
    card.link(&appsink).unwrap();

    let mut in_caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved")
        .field("rate", IN_RATE)
        .field("channels", channels as i32);
    if let Some(mask) = mask {
        in_caps = in_caps.field("channel-mask", gst::Bitmask::new(mask));
    }
    let appsrc = gstreamer_app::AppSrc::builder()
        .caps(&in_caps.build())
        .format(gst::Format::Time)
        .build();
    pipeline.add(appsrc.upcast_ref::<gst::Element>()).unwrap();
    let input = by_id
        .get(&format!("{id}:{AUDIO_INPUT_ELEMENT}"))
        .expect("the block builds an audio input front");
    appsrc
        .upcast_ref::<gst::Element>()
        .link(input)
        .expect("the source links to the block's audio input");

    let frame: Vec<u8> = (0..channels).flat_map(|i| level(i).to_le_bytes()).collect();
    let data = frame.repeat(FRAMES);
    let duration = gst::ClockTime::from_mseconds(100);
    for n in 0..BUFFERS {
        let mut buffer = gst::Buffer::from_slice(data.clone());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(duration * n);
            buffer.set_duration(duration);
        }
        appsrc.push_buffer(buffer).unwrap();
    }
    appsrc.end_of_stream().unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let mut caps = None;
    let mut frame = Vec::new();
    // A later buffer, past the resampler's start-up.
    for _ in 0..4 {
        let Some(sample) = appsink.try_pull_sample(gst::ClockTime::from_seconds(5)) else {
            break;
        };
        caps = sample.caps().map(|c| c.to_owned());
        let (Some(caps), Some(buffer)) = (caps.as_ref(), sample.buffer()) else {
            continue;
        };
        frame = middle_frame(caps, buffer);
    }
    let error = common::bus::first_error(&pipeline.bus().unwrap(), Duration::from_millis(200));
    pipeline.set_state(gst::State::Null).unwrap();
    Run { caps, frame, error }
}

/// The frame in the middle of `buffer`, as floats.
fn middle_frame(caps: &gst::Caps, buffer: &gst::BufferRef) -> Vec<f32> {
    let s = caps.structure(0).unwrap();
    let channels = s.get::<i32>("channels").unwrap() as usize;
    let format = s.get::<&str>("format").unwrap();
    let width = match format {
        "S16LE" => 2,
        "S32LE" => 4,
        other => panic!("the card takes S16LE or S32LE, got {other}"),
    };
    let map = buffer.map_readable().unwrap();
    let frames = map.len() / (width * channels);
    let start = (frames / 2) * width * channels;
    (0..channels)
        .map(|c| {
            let at = start + c * width;
            if width == 2 {
                i16::from_le_bytes([map[at], map[at + 1]]) as f32 / 32768.0
            } else {
                i32::from_le_bytes([map[at], map[at + 1], map[at + 2], map[at + 3]]) as f32
                    / 2_147_483_648.0
            }
        })
        .collect()
}

fn card_got(run: &Run) -> (i32, i32) {
    let caps = run
        .caps
        .as_ref()
        .unwrap_or_else(|| panic!("no audio reached the card's caps: {:?}", run.error));
    let s = caps.structure(0).unwrap();
    (
        s.get::<i32>("channels").unwrap(),
        s.get::<i32>("rate").unwrap(),
    )
}

fn assert_near(got: f32, want: f32, what: &str, frame: &[f32]) {
    assert!(
        (got - want).abs() < 0.005,
        "{what}: got {got}, want {want} (frame {frame:?})"
    );
}

#[test]
fn six_channels_reach_the_card_as_eight_channel_for_channel() {
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    let run = run("dl_six", 6, Some(0));
    assert_eq!(
        card_got(&run),
        (8, 48000),
        "the card gets 8 channels at 48 kHz"
    );
    assert!(run.error.is_none(), "{:?}", run.error);
    for i in 0..6 {
        assert_near(
            run.frame[i],
            level(i),
            &format!("card channel {}", i + 1),
            &run.frame,
        );
    }
    for i in 6..8 {
        assert_near(
            run.frame[i],
            0.0,
            &format!("card channel {}", i + 1),
            &run.frame,
        );
    }
}

#[test]
fn a_surround_layout_is_not_reordered() {
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    // 5.1 in GStreamer's order: SDI channels are numbered, so channel N stays N.
    let run = run("dl_51", 6, Some(0x3f));
    assert_eq!(card_got(&run), (8, 48000));
    for i in 0..6 {
        assert_near(
            run.frame[i],
            level(i),
            &format!("card channel {}", i + 1),
            &run.frame,
        );
    }
}

#[test]
fn mono_plays_on_both_channels_of_a_stereo_pair() {
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    let run = run("dl_mono", 1, None);
    assert_eq!(card_got(&run), (2, 48000));
    assert!(
        run.frame[0] > 0.01 && (run.frame[0] - run.frame[1]).abs() < 0.005,
        "mono must play on both channels: {:?}",
        run.frame
    );
}

#[test]
fn more_channels_than_the_card_takes_fail_with_a_message() {
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    let run = run("dl_wide", 24, Some(0));
    let error = run
        .error
        .expect("an error for 24 channels into a card that takes at most 16");
    assert_eq!(
        error.source,
        format!("dl_wide:{AUDIO_INPUT_ELEMENT}"),
        "the block's input posts the first error: {error:?}"
    );
    assert!(
        error.message.contains("24 audio channels") && error.message.contains("2, 8 or 16"),
        "the error names what arrived and what the card takes: {error:?}"
    );
}
