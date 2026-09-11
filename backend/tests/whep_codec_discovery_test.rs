//! A WHEP output that asks `whepserversink` for a codec it does not get must
//! say so.
//!
//! `whepserversink` proves out each codec in a throwaway pipeline of its own
//! before offering it, and drops the ones that fail. That pipeline's bus sync
//! handler returns `Drop` for every message, no property lists the survivors,
//! and the sink still reaches `PLAYING` on whatever is left - so a lost codec
//! is invisible from the flow pipeline. On macOS this is not hypothetical:
//! `vtenc_h264` emits `profile=baseline` where the discovery pipeline's
//! `codec-parser-caps` filter demands `constrained-baseline`, H.264 drops out,
//! and Chrome viewers fall back to VP9 encoded in software per viewer.
//!
//! That specific failure needs Apple's VideoToolbox encoder, so it cannot be
//! reproduced on CI. Removing `h264parse` from the registry fails H.264
//! discovery the same way and on every platform: `request-encoded-filter` has
//! already fired for the codec by the time `build_parser` errors, so the block
//! sees a codec that was attempted and never negotiated, which is exactly the
//! shape of the macOS failure.
//!
//! Reverting the report leaves the block reporting `Ok` here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use strom::blocks::builtin::whep::WHEPOutputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

/// Elements this test needs beyond core GStreamer.
const REQUIRED: &[&str] = &["videotestsrc", "whepserversink", "h264parse"];

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

/// Fail H.264 discovery without needing the platform encoder that fails it for
/// real. Process-global and irreversible, which is why this file holds one
/// test: `whepserversink` builds its codec list once per process.
fn break_h264_discovery() {
    let registry = gst::Registry::get();
    let factory = gst::ElementFactory::find("h264parse").expect("checked above");
    registry.remove_feature(factory.upcast_ref::<gst::PluginFeature>());
}

#[test]
fn a_codec_that_fails_discovery_is_reported_as_degraded() {
    gst::init().unwrap();
    gstrswebrtc::plugin_register_static().unwrap();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements are missing");
        return;
    }
    break_h264_discovery();

    let mut properties = HashMap::new();
    properties.insert("mode".to_string(), PropertyValue::String("av".to_string()));
    properties.insert(
        "endpoint_id".to_string(),
        PropertyValue::String("codec-report-test".to_string()),
    );

    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let built = WHEPOutputBuilder
        .build("whep_out", &properties, &ctx)
        .expect("WHEP output builds");
    let diagnostics = ctx.take_block_diagnostics();
    assert_eq!(
        diagnostics.len(),
        1,
        "the WHEP output block should register exactly one diagnostic"
    );
    let report = &diagnostics[0];
    assert_eq!(report.block_id(), "whep_out");

    let pipeline = gst::Pipeline::new();
    let by_id: HashMap<String, gst::Element> = built.elements.iter().cloned().collect();
    for (_, element) in &built.elements {
        pipeline.add(element).unwrap();
    }
    for (from, to) in &built.internal_links {
        let src = &by_id[&from.element_id];
        let sink = &by_id[&to.element_id];
        src.link_pads(from.pad_name.as_deref(), sink, to.pad_name.as_deref())
            .unwrap_or_else(|e| panic!("linking {:?} -> {:?}: {e}", from, to));
    }

    // Raw video into the block's video input, which is what makes it ask for
    // all four codecs rather than pinning the one already on the wire.
    let src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .build()
        .unwrap();
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", 320i32)
                .field("height", 180i32)
                .field("framerate", gst::Fraction::new(15, 1))
                .build(),
        )
        .build()
        .unwrap();
    pipeline.add_many([&src, &caps]).unwrap();
    src.link(&caps).unwrap();
    caps.link(&by_id["whep_out:video_queue"]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // The report holds off until discovery has settled, so poll rather than
    // sampling once.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut verdict = None;
    while Instant::now() < deadline {
        if let Some(detail) = report.degraded() {
            verdict = Some(detail);
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    pipeline.set_state(gst::State::Null).unwrap();

    let verdict = verdict.expect(
        "H.264 discovery cannot succeed without h264parse, so the block should report degraded",
    );
    assert!(
        verdict.contains("H.264"),
        "the report should name the codec that was lost, got: {verdict}"
    );
    assert!(
        !verdict.contains("VP9") && !verdict.contains("AV1") && !verdict.contains("H.265"),
        "only H.264 discovery was broken, got: {verdict}"
    );
}
