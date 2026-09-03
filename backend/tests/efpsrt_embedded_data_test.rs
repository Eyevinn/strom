//! Regression test for #691: EFP's embedded-data channel was unreachable from a flow.
//!
//! `efpmux` declares an `embed_%u` request sink pad and `efpdemux` a matching
//! `embedded` src pad, but `builtin.efpsrt_output` / `builtin.efpsrt_input` only
//! ever built video and audio pads, so neither end of that channel could be
//! addressed.
//!
//! These tests drive the real block builders rather than reconstructing the
//! topology inline. Without the fix `num_data_tracks` is ignored: no
//! `data_input_*` / `data_output_*` element is created, no external data pad is
//! reported and `efpmux` carries no `embed_%u` pad, so every assertion below
//! fails.

#![cfg(feature = "efp")]

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::builtin::efpsrt::EfpSrtOutputBuilder;
use strom::blocks::builtin::efpsrt_input::EfpSrtInputBuilder;
use strom::blocks::{BlockBuildContext, BlockBuildResult, BlockBuilder};
use strom_types::{MediaType, PropertyValue};

const EMBED_TEMPLATE: &str = "embed_%u";

fn init() {
    let _ = gst::init();
    let _ = gst_plugin_efp::plugin_register_static();
}

/// Fail loudly rather than skip: a test that silently skips on a missing element
/// passes green and guards nothing.
fn require_elements(names: &[&str]) {
    for name in names {
        assert!(
            gst::ElementFactory::find(name).is_some(),
            "GStreamer element '{}' is not available; the CI image needs the package that provides it",
            name
        );
    }
}

fn context() -> BlockBuildContext {
    BlockBuildContext::new(Vec::new(), "all".to_string())
}

fn properties(pairs: &[(&str, u64)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), PropertyValue::UInt(*v)))
        .collect()
}

fn element<'a>(result: &'a BlockBuildResult, id: &str) -> Option<&'a gst::Element> {
    result
        .elements
        .iter()
        .find(|(element_id, _)| element_id == id)
        .map(|(_, element)| element)
}

fn embed_pads(mux: &gst::Element) -> Vec<gst::Pad> {
    mux.pads()
        .into_iter()
        .filter(|pad| {
            pad.pad_template()
                .map(|templ| templ.name_template() == EMBED_TEMPLATE)
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn output_block_exposes_a_data_input_per_data_track() {
    init();

    let props = properties(&[("num_data_tracks", 2)]);
    let pads = EfpSrtOutputBuilder
        .get_external_pads(&props)
        .expect("efpsrt_output should report external pads");

    for i in 0..2 {
        let name = format!("data_in_{}", i);
        let pad = pads
            .inputs
            .iter()
            .find(|pad| pad.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "expected external input pad '{}', got {:?}",
                    name,
                    pads.inputs.iter().map(|p| &p.name).collect::<Vec<_>>()
                )
            });

        assert_eq!(
            pad.media_type,
            MediaType::Generic,
            "embedded-data pads carry neither audio nor video"
        );
        assert_eq!(pad.internal_element_id, format!("data_input_{}", i));
        assert_eq!(pad.internal_pad_name, "sink");
    }
}

#[test]
fn output_block_requests_and_links_an_embed_pad_per_data_track() {
    init();
    require_elements(&["efpmux", "srtsink", "identity"]);

    let props = properties(&[
        ("num_video_tracks", 0),
        ("num_audio_tracks", 0),
        ("num_data_tracks", 2),
    ]);
    let result = EfpSrtOutputBuilder
        .build("blk", &props, &context())
        .expect("efpsrt_output should build");

    let mux = element(&result, "blk:efpmux").expect("block should contain an efpmux element");
    let pads = embed_pads(mux);
    assert_eq!(
        pads.len(),
        2,
        "expected one '{}' pad per data track, got {:?}",
        EMBED_TEMPLATE,
        pads.iter()
            .map(|p| p.name().to_string())
            .collect::<Vec<_>>()
    );

    for i in 0..2 {
        let id = format!("blk:data_input_{}", i);
        let identity = element(&result, &id)
            .unwrap_or_else(|| panic!("block should contain a '{}' element", id));

        let src = identity
            .static_pad("src")
            .unwrap_or_else(|| panic!("'{}' should have a src pad", id));
        let peer = src
            .peer()
            .unwrap_or_else(|| panic!("'{}' src pad should be linked to efpmux", id));

        assert!(
            pads.iter().any(|pad| pad == &peer),
            "'{}' should link to an '{}' pad, but links to '{}'",
            id,
            EMBED_TEMPLATE,
            peer.name()
        );
    }
}

#[test]
fn output_block_requests_no_embed_pad_by_default() {
    init();
    require_elements(&["efpmux", "srtsink", "identity"]);

    let result = EfpSrtOutputBuilder
        .build("blk", &HashMap::new(), &context())
        .expect("efpsrt_output should build with default properties");

    let mux = element(&result, "blk:efpmux").expect("block should contain an efpmux element");
    assert!(
        embed_pads(mux).is_empty(),
        "num_data_tracks defaults to 0, so an existing flow must be unchanged"
    );
    assert!(
        element(&result, "blk:data_input_0").is_none(),
        "num_data_tracks defaults to 0, so no data input element must be created"
    );
}

#[test]
fn input_block_exposes_a_data_output_per_data_track() {
    init();

    let props = properties(&[("num_data_tracks", 1)]);
    let pads = EfpSrtInputBuilder
        .get_external_pads(&props)
        .expect("efpsrt_input should report external pads");

    let pad = pads
        .outputs
        .iter()
        .find(|pad| pad.name == "data_out_0")
        .unwrap_or_else(|| {
            panic!(
                "expected external output pad 'data_out_0', got {:?}",
                pads.outputs.iter().map(|p| &p.name).collect::<Vec<_>>()
            )
        });

    assert_eq!(pad.media_type, MediaType::Generic);
    assert_eq!(pad.internal_element_id, "data_output_0");
    assert_eq!(pad.internal_pad_name, "src");
}

#[test]
fn input_block_builds_a_data_output_element_per_data_track() {
    init();
    require_elements(&["efpdemux", "srtsrc", "identity"]);

    let props = properties(&[
        ("num_video_tracks", 0),
        ("num_audio_tracks", 0),
        ("num_data_tracks", 1),
    ]);
    let result = EfpSrtInputBuilder
        .build("blk", &props, &context())
        .expect("efpsrt_input should build");

    assert!(
        element(&result, "blk:data_output_0").is_some(),
        "block should contain a 'blk:data_output_0' element, got {:?}",
        result.elements.iter().map(|(id, _)| id).collect::<Vec<_>>()
    );
}

#[test]
fn input_block_builds_no_data_output_by_default() {
    init();
    require_elements(&["efpdemux", "srtsrc", "identity"]);

    let result = EfpSrtInputBuilder
        .build("blk", &HashMap::new(), &context())
        .expect("efpsrt_input should build with default properties");

    assert!(
        element(&result, "blk:data_output_0").is_none(),
        "num_data_tracks defaults to 0, so no data output element must be created"
    );
}
