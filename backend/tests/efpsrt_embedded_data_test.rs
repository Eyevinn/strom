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

/// The embed pad must be requested here, but the link to it must be *reported*
/// rather than made.
///
/// An earlier version of this test asserted the src pad's peer directly, and
/// passed while the channel carried nothing: the builder linked the pads itself,
/// and `gst_bin_add` drops any link whose peer is outside the bin, so the
/// pipeline builder's own "add every element, then link" order silently undid
/// it. `efpsrt_data_roundtrip_test` is what actually proves bytes flow; this
/// asserts the shape that lets them.
#[test]
fn output_block_reports_an_embed_pad_link_per_data_track() {
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
        assert!(
            element(&result, &id).is_some(),
            "block should contain a '{}' element",
            id
        );

        let link = result
            .internal_links
            .iter()
            .find(|(from, _)| from.element_id == id && from.pad_name.as_deref() == Some("src"))
            .unwrap_or_else(|| {
                panic!(
                    "'{}' src should be reported as an internal link, got {:?}",
                    id,
                    result
                        .internal_links
                        .iter()
                        .map(|(f, t)| format!("{:?} -> {:?}", f, t))
                        .collect::<Vec<_>>()
                )
            });

        assert_eq!(link.1.element_id, "blk:efpmux");
        let pad_name = link.1.pad_name.as_deref().expect("link names a pad");
        assert!(
            pads.iter().any(|pad| pad.name() == pad_name),
            "'{}' should link to an '{}' pad, but names '{}'",
            id,
            EMBED_TEMPLATE,
            pad_name
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

/// gst-plugin-efp v0.4.0 gives each EFP stream its own `embedded_<stream-id>`
/// src pad, so several data tracks can now be received. Until v0.3.0 the
/// demuxer cached one `embedded` pad for every stream and this block rejected
/// anything above one rather than publish outputs that could never carry a
/// buffer.
#[test]
fn input_block_builds_a_data_output_per_track_beyond_the_first() {
    init();
    require_elements(&["efpdemux", "srtsrc", "identity"]);

    let props = properties(&[
        ("num_video_tracks", 0),
        ("num_audio_tracks", 0),
        ("num_data_tracks", 3),
    ]);
    let result = EfpSrtInputBuilder
        .build("blk", &props, &context())
        .expect("several data tracks must build against a demuxer that fans out per stream");

    for i in 0..3 {
        let id = format!("blk:data_output_{}", i);
        assert!(
            element(&result, &id).is_some(),
            "block should contain a '{}' element, got {:?}",
            id,
            result.elements.iter().map(|(id, _)| id).collect::<Vec<_>>()
        );
    }
}

#[test]
fn input_block_advertises_every_data_output_it_builds() {
    init();

    let props = properties(&[("num_data_tracks", 3)]);
    let pads = EfpSrtInputBuilder
        .get_external_pads(&props)
        .expect("efpsrt_input should report external pads");

    let data_pads = pads
        .outputs
        .iter()
        .filter(|pad| pad.name.starts_with("data_out_"))
        .map(|pad| pad.name.clone())
        .collect::<Vec<_>>();

    assert_eq!(
        data_pads,
        vec![
            "data_out_0".to_string(),
            "data_out_1".to_string(),
            "data_out_2".to_string()
        ],
        "the flow graph must show every data output the block builds"
    );
}

/// Guards the `usize::try_from` conversion. Read with `as usize`, a negative
/// `Int` becomes `usize::MAX` and drives the pad loops, so this asserts the
/// value is discarded and the default applies instead.
#[test]
fn a_negative_track_count_falls_back_to_the_default() {
    init();

    for name in ["num_video_tracks", "num_audio_tracks", "num_data_tracks"] {
        let props: HashMap<String, PropertyValue> =
            [(name.to_string(), PropertyValue::Int(-1))].into();

        assert_eq!(
            strom::blocks::builtin::efpsrt::track_count(&props, name),
            None,
            "a negative {} must be discarded, not wrapped to usize::MAX",
            name
        );
    }
}
