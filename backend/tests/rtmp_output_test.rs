//! Regression tests for `builtin.rtmp_output`.
//!
//! Three properties, each of which was wrong in a first draft of the block and
//! each of which is silent when it breaks:
//!
//! 1. **The sink is not async.** Block-built elements bypass `add_element`, so
//!    nothing sets `async=false` for them. A sink that waits to preroll holds
//!    the whole pipeline out of PLAYING, and the flow simply never starts.
//! 2. **Nothing links to `flvmux` at build time.** An aggregator sink pad
//!    requested for an input that never carries data means the muxer never
//!    aggregates and the sink receives nothing, which looks like a dead server
//!    rather than a graph mistake.
//! 3. **The declared properties are the ones the block reads.** A property read
//!    but never declared is settable only by hand-editing flow JSON and is
//!    invisible in the UI.
//!
//! The codec decision each pad probe makes is covered directly, through
//! `video_plan` and `audio_plan`, which exist as separate functions so the
//! branch logic can be tested without a pipeline.
//!
//! **What is not covered here, stated rather than implied:** the element
//! construction and pad linking those decisions lead to. A chain that builds
//! the wrong element or links the wrong pad passes this suite — deleting both
//! probe bodies leaves every test below green. `rtmp_output_pipeline_test.rs`
//! covers that half, by driving this builder in a real pipeline; it needs no
//! RTMP server, because the probes fire on the caps event and preroll is enough
//! to produce one.

use std::collections::HashMap;
use strom::blocks::builtin::rtmp::{
    audio_plan, rtmp_missing_message, rtmp_package_hint, video_plan, AudioPlan, RtmpOutputBuilder,
};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Elements these tests need beyond core GStreamer. Missing on a bare image.
const REQUIRED: &[&str] = &[
    "rtmp2sink",
    "flvmux",
    "h264parse",
    "aacparse",
    "avenc_aac",
    "identity",
];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    // Before the factory lookup, not after: ElementFactory::find panics on an
    // uninitialised GStreamer, and this runs before build() would have done it.
    gst::init().expect("gst init");
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

fn build(properties: HashMap<String, PropertyValue>) -> strom::blocks::BlockBuildResult {
    gst::init().expect("gst init");
    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    RtmpOutputBuilder
        .build("rtmp0", &properties, &ctx)
        .expect("rtmp_output should build")
}

fn element<'a>(result: &'a strom::blocks::BlockBuildResult, suffix: &str) -> &'a gst::Element {
    result
        .elements
        .iter()
        .find(|(id, _)| id.ends_with(suffix))
        .map(|(_, e)| e)
        .unwrap_or_else(|| panic!("no element ending in {}", suffix))
}

/// `rtmp2sink` parses `location` and re-serialises it, and that drops the
/// default port on at least GStreamer 1.28, so `rtmp://h:1935/p` reads back as
/// `rtmp://h/p`. Compare through this rather than pinning one version's
/// normalisation: the runtime image is on 1.26 and CI must not depend on which
/// of the two behaviours it has.
fn without_default_port(url: &str) -> String {
    url.replace(":1935/", "/")
}

#[test]
fn the_sink_is_not_async() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let sink = element(&result, ":rtmp_sink");
    assert!(
        !sink.property::<bool>("async"),
        "the RTMP sink is async, so it will wait to preroll and hold the pipeline \
         out of PLAYING. Block-built elements bypass add_element, so this has to \
         be set here"
    );
    assert!(
        sink.property::<bool>("qos"),
        "qos should be on so the sink can report back pressure upstream"
    );
}

#[test]
fn nothing_is_linked_to_the_muxer_at_build_time() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    assert_eq!(
        result.internal_links.len(),
        1,
        "only flvmux -> sink may be linked statically. A mux sink pad requested \
         for an input that never carries data means the muxer never aggregates: \
         {:?}",
        result.internal_links
    );
    let (from, to) = &result.internal_links[0];
    assert!(from.element_id.ends_with(":rtmp_flvmux"));
    assert!(to.element_id.ends_with(":rtmp_sink"));
}

#[test]
fn both_inputs_exist_and_are_identities() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    for suffix in [":rtmp_video_input", ":rtmp_audio_input"] {
        let input = element(&result, suffix);
        assert_eq!(
            input.factory().map(|f| f.name().to_string()).as_deref(),
            Some("identity"),
            "{} should be an identity, so a probe can insert the real chain",
            suffix
        );
    }
}

#[test]
fn the_location_default_is_used_when_absent_or_blank() {
    if !plugins_available() {
        return;
    }
    for properties in [
        HashMap::new(),
        HashMap::from([(
            "location".to_string(),
            PropertyValue::String("   ".to_string()),
        )]),
    ] {
        let result = build(properties);
        assert_eq!(
            without_default_port(&element(&result, ":rtmp_sink").property::<String>("location")),
            without_default_port(strom_types::DEFAULT_RTMP_LOCATION),
            "a blank location should fall back to the default rather than \
             publishing to an empty URL"
        );
    }
}

#[test]
fn the_location_and_sync_properties_reach_the_sink() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::from([
        (
            "location".to_string(),
            PropertyValue::String("rtmp://192.0.2.10:1935/live/key".to_string()),
        ),
        ("sync".to_string(), PropertyValue::Bool(false)),
    ]));
    let sink = element(&result, ":rtmp_sink");
    assert_eq!(
        without_default_port(&sink.property::<String>("location")),
        without_default_port("rtmp://192.0.2.10:1935/live/key")
    );
    assert!(!sink.property::<bool>("sync"));
}

#[test]
fn the_muxer_is_streamable() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    assert!(
        element(&result, ":rtmp_flvmux").property::<bool>("streamable"),
        "the non-streamable form rewrites the header at end of file, and a live \
         stream has no end"
    );
}

/// The declared set is exactly `location` and `sync`, both named here.
///
/// The name says "the declared set" rather than "every property the block
/// reads" on purpose: this test cannot enumerate the reads, it checks a list
/// written by hand. The teeth come from composition. The length assertion
/// catches a property declared with no reader, and
/// `the_location_and_sync_properties_reach_the_sink` proves both of these are
/// read, so adding a third read without declaring it fails the length check as
/// soon as anyone declares it, and reaches the UI as a bug otherwise.
#[test]
fn the_declared_property_set_is_exactly_location_and_sync() {
    // No GStreamer needed: this is about the definition, not the pipeline.
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    let declared: Vec<&str> = definition
        .exposed_properties
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for name in ["location", "sync"] {
        assert!(
            declared.contains(&name),
            "the block reads {} but does not declare it, so it is settable only \
             by hand-editing flow JSON and invisible in the UI. Declared: {:?}",
            name,
            declared
        );
    }
    assert_eq!(
        declared.len(),
        2,
        "a declared property with no reader is the mirror of the same defect. \
         Declared: {:?}",
        declared
    );
}

#[test]
fn every_mapping_names_an_element_the_block_builds() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    for property in &definition.exposed_properties {
        let suffix = format!(":{}", property.mapping.element_id);
        assert!(
            result
                .elements
                .iter()
                .any(|(id, _)| id.ends_with(suffix.as_str())),
            "property {} maps to element {}, which the block does not build, so a \
             runtime update would silently go nowhere",
            property.name,
            property.mapping.element_id
        );
    }
}

#[test]
fn every_declared_pad_names_an_element_the_block_builds() {
    if !plugins_available() {
        return;
    }
    let result = build(HashMap::new());
    let definition = &strom::blocks::builtin::rtmp::get_blocks()[0];
    for pad in &definition.external_pads.inputs {
        let suffix = format!(":{}", pad.internal_element_id);
        assert!(
            result
                .elements
                .iter()
                .any(|(id, _)| id.ends_with(suffix.as_str())),
            "pad {} points at element {}, which the block does not build, so the \
             graph would fail to link",
            pad.name,
            pad.internal_element_id
        );
    }
    assert!(
        definition.external_pads.outputs.is_empty(),
        "an output block has no outputs"
    );
}

#[test]
fn the_missing_plugin_message_names_what_to_install() {
    // Runs on a host with the element present, which is why the message is a
    // separate function from the check.
    let message = rtmp_missing_message();
    assert!(message.contains("rtmp2sink"), "{}", message);
    assert!(message.contains(rtmp_package_hint()), "{}", message);
}

// ---------------------------------------------------------------------------
// The codec decision, which is the half of the probe logic that can be tested
// without a pipeline. Every arm is here, because the refusals are what an
// operator sees when a flow is wired wrongly and a wrong message costs more
// than a wrong element: it sends them to fix the wrong block.
// ---------------------------------------------------------------------------

#[test]
fn h264_video_is_accepted() {
    assert_eq!(video_plan("video/x-h264"), Ok(()));
}

#[test]
fn raw_video_is_refused_and_names_the_encoder_block() {
    let message = video_plan("video/x-raw").expect_err("raw video must be refused");
    assert!(
        message.contains("builtin.videoenc"),
        "the refusal must name the block to add, got: {}",
        message
    );
}

#[test]
fn other_video_codecs_are_refused_and_named() {
    let message = video_plan("video/x-vp8").expect_err("VP8 must be refused");
    assert!(
        message.contains("video/x-vp8"),
        "the refusal must name what arrived, got: {}",
        message
    );
}

#[test]
fn raw_audio_is_encoded_in_the_block() {
    assert_eq!(audio_plan("audio/x-raw", 0, 0), Ok(AudioPlan::Encode));
}

#[test]
fn aac_audio_is_parsed_only() {
    assert_eq!(audio_plan("audio/mpeg", 4, 0), Ok(AudioPlan::Parse));
    assert_eq!(audio_plan("audio/mpeg", 2, 0), Ok(AudioPlan::Parse));
}

/// MPEG-1 layer 3 is MP3, which FLV does carry. The block refuses it, and the
/// message must not claim FLV cannot: that would send an operator to change a
/// container setting that is not the problem.
#[test]
fn mp3_is_refused_without_blaming_the_container() {
    let message = audio_plan("audio/mpeg", 1, 3).expect_err("MP3 must be refused");
    assert!(
        message.contains("MP3"),
        "the refusal must name MP3, got: {}",
        message
    );
    assert!(
        !message.contains("FLV cannot"),
        "FLV does carry MP3, so the refusal must not blame the container: {}",
        message
    );
}

/// MPEG-1 layers 1 and 2 are a different case from MP3: `flvmux` accepts only
/// layer 3, so here the container really is the reason.
#[test]
fn mpeg1_layer_two_is_refused_as_a_container_limit() {
    let message = audio_plan("audio/mpeg", 1, 2).expect_err("MPEG-1 layer 2 must be refused");
    assert!(
        message.contains("layer 2"),
        "the refusal must name the layer, got: {}",
        message
    );
}

#[test]
fn other_audio_codecs_are_refused_and_named() {
    let message = audio_plan("audio/x-opus", 0, 0).expect_err("Opus must be refused");
    assert!(
        message.contains("audio/x-opus"),
        "the refusal must name what arrived, got: {}",
        message
    );
}
