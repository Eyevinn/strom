//! DeckLink Output adapts its video input to the configured mode (#1127).
//!
//! The block used to map `video_in` straight onto `decklinkvideosink`. Behind
//! a GPU Vision Mixer the linker added a `gldownload`, but nothing converted
//! the mixer's RGBA to the card's UYVY, so the link never negotiated. The
//! block now has an input front (`gst::download_front`) and a
//! `videorate → videoscale → videoconvert` chain in front of the sink.
//!
//! CI has no DeckLink card, and `decklinkvideosink` fails to start without
//! one. These tests build the real block through `DeckLinkOutputBuilder` and
//! put a stand-in in the sink's place: a capsfilter carrying the caps the real
//! `decklinkvideosink` answers for the mode (it answers them without a card),
//! in front of an `appsink`. The real sink never runs here.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use strom::blocks::builtin::decklink::{DeckLinkOutputBuilder, VIDEO_INPUT_ELEMENT};
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const ELEMENTS: &[&str] = &[
    "decklinkvideosink",
    "videotestsrc",
    "videoconvert",
    "videoscale",
    "videorate",
    "appsink",
];

/// The caps the real `decklinkvideosink` takes for `mode`, in 8-bit YUV.
fn card_caps(mode: &str) -> gst::Caps {
    let sink = gst::ElementFactory::make("decklinkvideosink")
        .property_from_str("mode", mode)
        .property_from_str("video-format", "8bit-yuv")
        .build()
        .expect("decklinkvideosink");
    sink.static_pad("sink").unwrap().query_caps(None)
}

/// The block for `mode`, in `pipeline`, with the stand-in in place of its
/// sink. Returns the block's video input pad, the stand-in's appsink, and the
/// real `decklinkvideosink`, left out of the pipeline: the block reads the
/// card's caps from it, so the caller keeps it alive.
fn add_block(
    pipeline: &gst::Pipeline,
    id: &str,
    mode: &str,
) -> (gst::Pad, gstreamer_app::AppSink, gst::Element) {
    let props = HashMap::from([
        (
            "stream_mode".to_string(),
            PropertyValue::String("video".into()),
        ),
        ("mode".to_string(), PropertyValue::String(mode.into())),
        (
            "video_format".to_string(),
            PropertyValue::String("8bit-yuv".into()),
        ),
    ]);
    let built = DeckLinkOutputBuilder
        .build(id, &props, &common::block::context())
        .expect("DeckLink Output builds");
    let sink_id = format!("{id}:decklinkvideosink");
    let by_id = common::block::install_except(pipeline, &built, &[sink_id.as_str()]);

    let card = gst::ElementFactory::make("capsfilter")
        .property("caps", card_caps(mode))
        .build()
        .unwrap();
    let appsink = gstreamer_app::AppSink::builder()
        .sync(false)
        .max_buffers(1)
        .drop(true)
        .build();
    pipeline.add_many([&card, appsink.upcast_ref()]).unwrap();
    by_id[&format!("{id}:ratecaps")]
        .link(&card)
        .expect("the block links to the card's caps");
    card.link(&appsink).unwrap();

    let input = by_id[&format!("{id}:{VIDEO_INPUT_ELEMENT}")]
        .static_pad("sink")
        .unwrap();
    let card_sink = built
        .elements
        .iter()
        .find(|(element_id, _)| *element_id == sink_id)
        .map(|(_, element)| element.clone())
        .expect("the block builds decklinkvideosink");
    (input, appsink, card_sink)
}

/// Link `caps` from `source` into `input`.
fn feed(pipeline: &gst::Pipeline, source: &str, caps: &str, input: &gst::Pad) {
    let src = gst::ElementFactory::make(source)
        .property("is-live", true)
        .build()
        .unwrap();
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps.parse::<gst::Caps>().unwrap())
        .build()
        .unwrap();
    pipeline.add_many([&src, &filter]).unwrap();
    src.link(&filter).unwrap();
    filter
        .static_pad("src")
        .unwrap()
        .link(input)
        .expect("the source links to the block's video input");
}

/// The caps of the first frame the stand-in receives, or the first error.
fn first_frame(pipeline: &gst::Pipeline, appsink: &gstreamer_app::AppSink) -> gst::Caps {
    pipeline.set_state(gst::State::Playing).unwrap();
    let sample = appsink.try_pull_sample(gst::ClockTime::from_seconds(10));
    let error = common::bus::first_error(&pipeline.bus().unwrap(), Duration::ZERO);
    pipeline.set_state(gst::State::Null).unwrap();
    let sample = sample.unwrap_or_else(|| panic!("no frame reached the card's caps: {error:?}"));
    sample.caps().expect("sample caps").to_owned()
}

fn assert_card_frame(caps: &gst::Caps) {
    let s = caps.structure(0).unwrap();
    assert_eq!(
        (
            s.get::<&str>("format").ok(),
            s.get::<i32>("width").ok(),
            s.get::<i32>("height").ok(),
            s.get::<gst::Fraction>("framerate").ok(),
        ),
        (
            Some("UYVY"),
            Some(1920),
            Some(1080),
            Some(gst::Fraction::new(25, 1))
        ),
        "the card gets {caps}"
    );
}

/// The factories between the block's input front and its first converter,
/// `videorate`.
fn spliced(pipeline: &gst::Pipeline, id: &str) -> Vec<String> {
    let front = format!("{id}:{VIDEO_INPUT_ELEMENT}");
    let first = pipeline.by_name(&format!("{id}:videorate")).unwrap();
    let mut factories = Vec::new();
    let mut next = first
        .static_pad("sink")
        .and_then(|p| p.peer())
        .and_then(|p| p.parent_element());
    while let Some(element) = next {
        if element.name() == front {
            return factories;
        }
        factories.insert(
            0,
            element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default(),
        );
        assert!(factories.len() < 5, "the input has a loop: {factories:?}");
        next = element
            .static_pad("sink")
            .and_then(|p| p.peer())
            .and_then(|p| p.parent_element());
    }
    panic!("videorate is not fed from the input front: {factories:?}");
}

#[test]
fn system_memory_is_converted_scaled_and_retimed_to_the_mode() {
    gst::init().unwrap();
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let (input, appsink, _card_sink) = add_block(&pipeline, "dl_sys", "1080p25");
    feed(
        &pipeline,
        "videotestsrc",
        "video/x-raw,format=I420,width=1280,height=720,framerate=30/1",
        &input,
    );
    let caps = first_frame(&pipeline, &appsink);
    assert_card_frame(&caps);
    assert!(
        spliced(&pipeline, "dl_sys").is_empty(),
        "system memory needs no download"
    );
}

#[test]
#[serial_test::serial(gl)]
fn gl_memory_is_downloaded_by_the_input_front() {
    gst::init().unwrap();
    let mut required = ELEMENTS.to_vec();
    required.extend(["gltestsrc", "gldownload"]);
    if !common::gl_available(&required) {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let (input, appsink, _card_sink) = add_block(&pipeline, "dl_gl", "1080p25");
    feed(
        &pipeline,
        "gltestsrc",
        "video/x-raw(memory:GLMemory),format=RGBA,width=640,height=360,framerate=30/1",
        &input,
    );
    let caps = first_frame(&pipeline, &appsink);
    assert_card_frame(&caps);
    assert_eq!(
        spliced(&pipeline, "dl_gl"),
        ["gldownload", "capsfilter"],
        "the block's input front must download the GL memory, pinned to system memory"
    );
}

#[test]
fn progressive_video_into_an_interlaced_mode_fails_with_a_message() {
    gst::init().unwrap();
    if !common::plugins_available(ELEMENTS) {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let (input, _appsink, _card_sink) = add_block(&pipeline, "dl_interlace", "1080i50");
    feed(
        &pipeline,
        "videotestsrc",
        "video/x-raw,format=UYVY,width=1920,height=1080,framerate=25/1,interlace-mode=progressive",
        &input,
    );
    pipeline.set_state(gst::State::Playing).unwrap();
    let error = common::bus::first_error(&pipeline.bus().unwrap(), Duration::from_secs(10));
    pipeline.set_state(gst::State::Null).unwrap();
    let error = error.expect("an error for progressive video into 1080i50");
    assert!(
        error.message.contains("progressive") && error.message.contains("1080i50"),
        "the first error must name the input's interlacing and the mode, got {error:?}"
    );
}
