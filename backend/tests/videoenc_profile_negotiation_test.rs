//! What `builtin.videoenc` actually negotiates, per input pixel format.
//!
//! The unit suite in `videoenc/tests.rs` covers `get_codec_caps_string`, which
//! is pure: it proves the capsfilter is built with `profile=high`. It cannot
//! prove that pinning the field changes what the encoder does, and that is the
//! whole of #782 — the profile is pinned on the capsfilter, *after* the parser,
//! so the constraint only reaches the encoder by propagating back upstream
//! through it. Whether that propagation happens is a property of GStreamer, not
//! of this repo's code, so it needs a running pipeline to establish.
//!
//! The assertion that matters is therefore on the **encoder's sink pad**: the
//! raw format the encoder actually accepted. 8-bit 4:2:0 there means the
//! escalation is gone at the source. Asserting only on the coded caps
//! downstream would be circular — the capsfilter puts `profile=high` in them by
//! construction.
//!
//! Reverting the default to `Profile::None` fails every test here that feeds a
//! non-4:2:0 format: measured on GStreamer 1.28.1, `videoconvert` scores
//! RGBA→Y444 as less lossy than RGBA→I420 and hands the encoder Y444, which
//! x264enc encodes as `High 4:4:4 Predictive`.
//!
//! `encoder_preference` is pinned to `software` so the encoder under test is
//! the same everywhere. With the default `auto` the block prefers hardware, and
//! which encoder that is depends on the host — VideoToolbox on macOS, NVENC on
//! an NVIDIA box, x264enc on CI. Those paths are not covered here.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

const INSTANCE: &str = "venc0";

/// Elements these tests need beyond core GStreamer.
const REQUIRED: &[&str] = &[
    "videotestsrc",
    "capsfilter",
    "videoconvert",
    "x264enc",
    "x265enc",
    "h264parse",
    "h265parse",
    "fakesink",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure. x264enc comes
/// from gst-plugins-ugly and x265enc from gst-plugins-bad; the CI workflow
/// installs both.
fn plugins_available() -> bool {
    gst::init().expect("gst init");
    // `VideoEncBuilder::build` reads the process-global video convert mode,
    // which panics until this has run.
    strom::gpu::detect_gpu_capabilities();
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
    eprintln!(
        "skipping: missing GStreamer elements: {}",
        missing.join(", ")
    );
    false
}

/// The 8-bit 4:2:0 raw formats an H.264 / H.265 encoder may pick for a 4:2:0
/// profile. Which one it is depends on the encoder; that it is one of these is
/// the point.
const YUV_420_8BIT: &[&str] = &["I420", "YV12", "NV12", "NV21", "IYUV"];

/// What a run negotiated.
struct Negotiated {
    /// Raw pixel format on the encoder's sink pad.
    raw_format: String,
    /// `profile` field on the block's output capsfilter, if it carries one.
    coded_profile: Option<String>,
}

/// Build the real block, feed it `input_format`, run it to EOS, and report what
/// the pads settled on.
///
/// The block is built through `VideoEncBuilder` and wired with its own declared
/// `internal_links`, so the chain under test is the one that ships rather than
/// an equivalent assembled here.
fn negotiate(codec: &str, input_format: &str, profile: Option<&str>) -> Negotiated {
    let mut props = HashMap::new();
    props.insert(
        "codec".to_string(),
        PropertyValue::String(codec.to_string()),
    );
    // Keep the encoder the same on every host; see the module header.
    props.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".to_string()),
    );
    if let Some(p) = profile {
        props.insert("profile".to_string(), PropertyValue::String(p.to_string()));
    }

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    let built = VideoEncBuilder
        .build(INSTANCE, &props, &ctx)
        .expect("videoenc block builds");

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
        src.link(sink)
            .unwrap_or_else(|e| panic!("internal link failed: {:?}", e));
    }

    let encoder = by_id
        .get(&format!("{}:encoder", INSTANCE))
        .expect("block builds an encoder")
        .clone();
    let capsfilter = by_id
        .get(&format!("{}:capsfilter", INSTANCE))
        .expect("block builds a capsfilter")
        .clone();
    let convert = by_id
        .get(&format!("{}:videoconvert", INSTANCE))
        .expect("block builds a converter")
        .clone();

    // Source: the format under test, pushed at the block's input.
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 10i32)
        .build()
        .expect("videotestsrc");
    let src_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", input_format)
                .field("width", 320i32)
                .field("height", 240i32)
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        )
        .build()
        .expect("source capsfilter");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    pipeline
        .add_many([&src, &src_caps, &sink])
        .expect("add harness elements");
    gst::Element::link_many([&src, &src_caps]).expect("link source");
    src_caps.link(&convert).expect("link source into the block");
    capsfilter.link(&sink).expect("link block to fakesink");

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline goes to PLAYING");

    // Run to EOS rather than sampling caps after a sleep: caps read from a
    // pipeline that never pushed a buffer would not prove the chain works.
    let bus = pipeline.bus().expect("pipeline has a bus");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_eos = false;
    while Instant::now() < deadline && !saw_eos {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Eos(_) => saw_eos = true,
            gst::MessageView::Error(e) => {
                let _ = pipeline.set_state(gst::State::Null);
                panic!(
                    "pipeline error for codec={} format={} profile={:?}: {} ({:?})",
                    codec,
                    input_format,
                    profile,
                    e.error(),
                    e.debug()
                );
            }
            _ => {}
        }
    }

    let raw_format = encoder
        .static_pad("sink")
        .expect("encoder has a sink pad")
        .current_caps()
        .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()));
    let coded_profile = capsfilter
        .static_pad("src")
        .expect("capsfilter has a src pad")
        .current_caps()
        .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("profile").ok()));

    let _ = pipeline.set_state(gst::State::Null);

    assert!(
        saw_eos,
        "pipeline did not reach EOS for codec={} format={} profile={:?}",
        codec, input_format, profile
    );

    Negotiated {
        raw_format: raw_format.expect("encoder sink pad negotiated raw caps"),
        coded_profile,
    }
}

/// Assert the encoder took 8-bit 4:2:0 and emitted the expected profile.
fn assert_420_8bit(n: &Negotiated, input_format: &str, expected_profile: &str) {
    assert!(
        YUV_420_8BIT.contains(&n.raw_format.as_str()),
        "from {} the encoder should take 8-bit 4:2:0, but negotiated {} — \
         the profile pin is not reaching the encoder",
        input_format,
        n.raw_format
    );
    assert_eq!(
        n.coded_profile.as_deref(),
        Some(expected_profile),
        "from {} the coded output should be {}",
        input_format,
        expected_profile
    );
}

/// RGBA is not a hypothetical: the vision mixer pins `format=RGBA` on its
/// output on both the CPU and GPU paths, so this is what the encoder is handed
/// in an ordinary flow. Unpinned, `videoconvert` prefers RGBA→Y444 over
/// RGBA→I420 and the output is 4:4:4.
#[test]
fn rgba_input_encodes_as_8bit_420_by_default() {
    if !plugins_available() {
        return;
    }
    let n = negotiate("h264", "RGBA", None);
    assert_420_8bit(&n, "RGBA", "high");
}

/// 4:2:2 in. Unpinned this came out `High 4:4:4 Predictive` — the chain does
/// not merely carry the input's sampling through, it widens it.
#[test]
fn yuv422_input_encodes_as_8bit_420_by_default() {
    if !plugins_available() {
        return;
    }
    for format in ["UYVY", "Y42B"] {
        let n = negotiate("h264", format, None);
        assert_420_8bit(&n, format, "high");
    }
}

/// 10-bit in. Unpinned, `I420_10LE` came out `High 10` and `P010_10LE` came out
/// `High 4:4:4 Predictive`.
#[test]
fn ten_bit_input_encodes_as_8bit_420_by_default() {
    if !plugins_available() {
        return;
    }
    for format in ["I420_10LE", "P010_10LE"] {
        let n = negotiate("h264", format, None);
        assert_420_8bit(&n, format, "high");
    }
}

/// A 4:2:0 source was always fine, which is why the bug was easy to miss. It
/// has to stay fine.
#[test]
fn yuv420_input_is_unchanged_by_the_default() {
    if !plugins_available() {
        return;
    }
    let n = negotiate("h264", "I420", None);
    assert_420_8bit(&n, "I420", "high");
}

/// The default has to resolve per codec. "high" is not an H.265 profile name:
/// pinning it on an h265 capsfilter fails to negotiate outright, so a single
/// codec-blind default would have traded a bad stream for a dead pipeline.
#[test]
fn h265_default_resolves_to_main_not_high() {
    if !plugins_available() {
        return;
    }
    let n = negotiate("h265", "RGBA", None);
    assert_420_8bit(&n, "RGBA", "main");
}

/// The escape hatch survives: `profile=none` still omits the field and lets the
/// encoder negotiate freely, which is what anyone deliberately encoding 4:4:4
/// or 10-bit needs.
#[test]
fn profile_none_leaves_negotiation_free() {
    if !plugins_available() {
        return;
    }
    let n = negotiate("h264", "RGBA", Some("none"));
    assert!(
        !YUV_420_8BIT.contains(&n.raw_format.as_str()),
        "with profile=none nothing should constrain the encoder's input, but it \
         negotiated {} — if videoconvert's scoring changed upstream this test \
         is reporting that, not a regression in the block",
        n.raw_format
    );
}

/// An explicit profile still wins over the default.
#[test]
fn explicit_profile_overrides_the_default() {
    if !plugins_available() {
        return;
    }
    let n = negotiate("h264", "I420", Some("constrained-baseline"));
    assert_eq!(n.coded_profile.as_deref(), Some("constrained-baseline"));
}
