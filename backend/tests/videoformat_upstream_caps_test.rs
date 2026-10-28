//! `builtin.videoformat` tells a producer that can choose what to deliver.
//!
//! Issues #1156 and #1157: since #1117 the block's input answered a CAPS
//! query with raw video at any size and in any format. A `videotestsrc` with
//! no caps of its own then fixated to its defaults, 320x240 in `A444_16LE`,
//! the first format it lists, instead of the block's size. `videoscale`
//! pillarboxed the 4:3 frame into a 16:9 output, and in `A444_16LE` (as in
//! every YUV format deeper than 8 bits, on GStreamer 1.28.8) it fills the
//! borders pink. In a Vision Mixer flow, every `videotestsrc ! Video Format`
//! input showed as 4:3 with pink bars.
//!
//! The block now offers what its output takes first: its size, in the order
//! of the formats downstream takes. These build the real block through
//! `VideoFormatBuilder`, link a caps-less `videotestsrc` into it and check
//! what the source settles on.

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use strom::blocks::builtin::videoformat::VideoFormatBuilder;
use strom::blocks::BlockBuilder;
use strom_types::PropertyValue;

const INSTANCE: &str = "vf";

const REQUIRED: &[&str] = &[
    "videotestsrc",
    "identity",
    "capsfilter",
    "videoscale",
    "videoconvert",
    "fakesink",
];

/// Build the block, set to `resolution`, into `pipeline` and return its
/// declared input and output pads.
fn add_block(pipeline: &gst::Pipeline, resolution: &str) -> (gst::Pad, gst::Pad) {
    let props = HashMap::from([(
        "resolution".to_string(),
        PropertyValue::String(resolution.into()),
    )]);
    let built = VideoFormatBuilder
        .build(INSTANCE, &props, &common::block::context())
        .expect("videoformat block builds");
    let by_id = common::block::install(pipeline, &built);
    let definition = strom::blocks::builtin::videoformat::get_blocks()
        .into_iter()
        .next()
        .expect("videoformat definition");
    let pad = |p: &strom_types::block::ExternalPad| {
        by_id[&format!("{}:{}", INSTANCE, p.internal_element_id)]
            .static_pad(&p.internal_pad_name)
            .expect("block pad")
    };
    (
        pad(&definition.external_pads.inputs[0]),
        pad(&definition.external_pads.outputs[0]),
    )
}

/// `videotestsrc ! [videoformat block] ! [downstream caps] ! fakesink`, run
/// until the first frames have passed. Returns the caps the source settled
/// on.
fn source_caps(resolution: &str, downstream: Option<&str>) -> gst::Caps {
    gst::init().unwrap();
    common::require_elements(REQUIRED);
    let pipeline = gst::Pipeline::new();
    let (block_in, block_out) = add_block(&pipeline, resolution);
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 3)
        .build()
        .unwrap();
    let filter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            downstream
                .map(|c| c.parse::<gst::Caps>().unwrap())
                .unwrap_or_else(gst::Caps::new_any),
        )
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline.add_many([&src, &filter, &sink]).unwrap();
    src.static_pad("src")
        .unwrap()
        .link(&block_in)
        .expect("the source links into the block");
    block_out
        .link(&filter.static_pad("sink").unwrap())
        .expect("the block links downstream");
    filter.link(&sink).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("the flow ends within 10s");
    if let gst::MessageView::Error(e) = msg.view() {
        panic!("the flow failed: {} ({:?})", e.error(), e.debug());
    }
    let caps = src
        .static_pad("src")
        .unwrap()
        .current_caps()
        .expect("the source's caps");
    pipeline.set_state(gst::State::Null).unwrap();
    caps
}

/// A caps-less source delivers the block's size, so nothing is scaled and no
/// border is added.
#[test]
fn a_source_that_can_choose_delivers_the_block_size() {
    for (resolution, size) in [("640x360", (640, 360)), ("1920x1080", (1920, 1080))] {
        let caps = source_caps(resolution, None);
        let info = gst_video::VideoInfo::from_caps(&caps).expect("raw video caps");
        assert_eq!(
            (info.width(), info.height()),
            size,
            "videotestsrc settled on {caps} in front of Video Format {resolution}; \
             the block must offer its own size first, or the source fixates to \
             320x240 and is pillarboxed (#1157)"
        );
    }
}

/// The source follows the format downstream takes, not its own first format
/// (`A444_16LE`, which `videoscale` borders in pink).
#[test]
fn a_source_that_can_choose_delivers_the_format_downstream_takes() {
    let caps = source_caps("640x360", Some("video/x-raw,format=NV12"));
    let info = gst_video::VideoInfo::from_caps(&caps).expect("raw video caps");
    assert_eq!(
        info.format(),
        gst_video::VideoFormat::Nv12,
        "videotestsrc settled on {caps}, with NV12 taken downstream (#1156)"
    );
}
