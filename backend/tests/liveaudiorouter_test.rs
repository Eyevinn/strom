//! Specification tests for `builtin.liveaudiorouter`.
//!
//! The capability-parity test is the guard for #661: the new block must expose
//! the same property set and pad shape as `builtin.audiorouter`, and differ only
//! in being able to change its crosspoints on a running flow.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use strom::blocks::builtin::{audiorouter, liveaudiorouter};
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::{BlockDefinition, MediaType, PropertyValue};

fn definition(blocks: Vec<BlockDefinition>, id: &str) -> BlockDefinition {
    blocks
        .into_iter()
        .find(|b| b.id == id)
        .unwrap_or_else(|| panic!("no block definition with id {id}"))
}

fn property_names(def: &BlockDefinition) -> HashSet<String> {
    def.exposed_properties
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

/// Pad shape as the graph editor sees it: name, label and media type.
fn pad_shape(pads: &[strom_types::ExternalPad]) -> Vec<(String, Option<String>, MediaType)> {
    pads.iter()
        .map(|p| (p.name.clone(), p.label.clone(), p.media_type))
        .collect()
}

fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

// ============================================================================
// Capability parity — the guard the maintainer asked for on #661
// ============================================================================

#[test]
fn liveaudiorouter_exposes_the_same_property_set_as_audiorouter() {
    let old = definition(audiorouter::get_blocks(), "builtin.audiorouter");
    let new = definition(liveaudiorouter::get_blocks(), "builtin.liveaudiorouter");

    let old_names = property_names(&old);
    let new_names = property_names(&new);

    let missing: Vec<_> = old_names.difference(&new_names).cloned().collect();
    assert!(
        missing.is_empty(),
        "builtin.liveaudiorouter is missing properties that builtin.audiorouter has: {missing:?}"
    );

    let extra: Vec<_> = new_names.difference(&old_names).cloned().collect();
    assert!(
        extra.is_empty(),
        "builtin.liveaudiorouter exposes properties builtin.audiorouter does not: {extra:?}"
    );
}

#[test]
fn liveaudiorouter_property_types_and_defaults_match_audiorouter() {
    let old = definition(audiorouter::get_blocks(), "builtin.audiorouter");
    let new = definition(liveaudiorouter::get_blocks(), "builtin.liveaudiorouter");

    for old_prop in &old.exposed_properties {
        let new_prop = new
            .exposed_properties
            .iter()
            .find(|p| p.name == old_prop.name)
            .unwrap_or_else(|| panic!("liveaudiorouter has no property {}", old_prop.name));

        // PropertyType and PropertyValue do not implement PartialEq, so compare
        // their Debug representations rather than deriving it in strom-types.
        assert_eq!(
            format!("{:?}", old_prop.property_type),
            format!("{:?}", new_prop.property_type),
            "property {} has a different type on liveaudiorouter",
            old_prop.name
        );
        assert_eq!(
            format!("{:?}", old_prop.default_value),
            format!("{:?}", new_prop.default_value),
            "property {} has a different default on liveaudiorouter",
            old_prop.name
        );
    }
}

#[test]
fn liveaudiorouter_declares_routing_matrix_live_and_audiorouter_does_not() {
    let old = definition(audiorouter::get_blocks(), "builtin.audiorouter");
    let new = definition(liveaudiorouter::get_blocks(), "builtin.liveaudiorouter");

    let old_live = old
        .exposed_properties
        .iter()
        .find(|p| p.name == "routing_matrix")
        .expect("audiorouter routing_matrix")
        .live;
    let new_live = new
        .exposed_properties
        .iter()
        .find(|p| p.name == "routing_matrix")
        .expect("liveaudiorouter routing_matrix")
        .live;

    assert!(
        !old_live,
        "builtin.audiorouter must keep routing_matrix live: false — it is not modified by #661"
    );
    assert!(
        new_live,
        "builtin.liveaudiorouter must declare routing_matrix live: true — that is the whole point"
    );
}

#[test]
fn liveaudiorouter_pad_shape_matches_audiorouter_for_the_same_configuration() {
    let old_builder = audiorouter::AudioRouterBuilder;
    let new_builder = liveaudiorouter::LiveAudioRouterBuilder;

    for (num_inputs, num_outputs) in [(1u64, 1u64), (2, 2), (3, 4), (8, 8)] {
        let p = props(&[
            ("num_inputs", PropertyValue::UInt(num_inputs)),
            ("num_outputs", PropertyValue::UInt(num_outputs)),
        ]);

        let old_pads = old_builder.get_external_pads(&p).expect("audiorouter pads");
        let new_pads = new_builder
            .get_external_pads(&p)
            .expect("liveaudiorouter pads");

        assert_eq!(
            pad_shape(&old_pads.inputs),
            pad_shape(&new_pads.inputs),
            "input pad shape differs for {num_inputs} inputs"
        );
        assert_eq!(
            pad_shape(&old_pads.outputs),
            pad_shape(&new_pads.outputs),
            "output pad shape differs for {num_outputs} outputs"
        );
    }
}

#[test]
fn liveaudiorouter_definition_pad_shape_matches_audiorouter() {
    let old = definition(audiorouter::get_blocks(), "builtin.audiorouter");
    let new = definition(liveaudiorouter::get_blocks(), "builtin.liveaudiorouter");

    assert_eq!(
        pad_shape(&old.external_pads.inputs),
        pad_shape(&new.external_pads.inputs)
    );
    assert_eq!(
        pad_shape(&old.external_pads.outputs),
        pad_shape(&new.external_pads.outputs)
    );
}

// ============================================================================
// Fan-in — the capability most easily lost when the routing model is rewritten
// ============================================================================

#[test]
fn two_input_channels_routed_to_one_output_channel_are_summed() {
    let p = props(&[
        ("num_inputs", PropertyValue::UInt(2)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(2)),
        ("input_1_channels", PropertyValue::UInt(2)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"],"i1c0":["o0c0"]}"#.to_string()),
        ),
    ]);

    let matrix = liveaudiorouter::matrix_from_properties(&p);

    assert_eq!(matrix.len(), 2, "one row per output channel");
    assert_eq!(matrix[0].len(), 4, "one column per input channel");

    // Global input channel order is i0c0, i0c1, i1c0, i1c1.
    assert_eq!(
        matrix[0],
        vec![1.0, 0.0, 1.0, 0.0],
        "output 0 channel 0 must sum input 0 channel 0 and input 1 channel 0"
    );
    assert_eq!(
        matrix[1],
        vec![0.0, 0.0, 0.0, 0.0],
        "output 0 channel 1 is unrouted and must be silent"
    );
}

#[test]
fn one_input_channel_routed_to_several_outputs_fans_out() {
    let p = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(2)),
        ("input_0_channels", PropertyValue::UInt(2)),
        ("output_0_channels", PropertyValue::UInt(1)),
        ("output_1_channels", PropertyValue::UInt(1)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c1":["o0c0","o1c0"]}"#.to_string()),
        ),
    ]);

    let matrix = liveaudiorouter::matrix_from_properties(&p);

    assert_eq!(matrix.len(), 2);
    assert_eq!(matrix[0], vec![0.0, 1.0]);
    assert_eq!(matrix[1], vec![0.0, 1.0]);
}

// ============================================================================
// The live half — a routing change on a running pipeline
// ============================================================================

fn missing_elements(names: &[&str]) -> Vec<String> {
    gst::init().unwrap();
    names
        .iter()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .map(|n| n.to_string())
        .collect()
}

/// Skip only when the plugin genuinely is not installed. CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1`, which turns a skip into a failure so this
/// test cannot pass green without executing.
fn require_elements(names: &[&str]) -> bool {
    let missing = missing_elements(names);
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    eprintln!(
        "skipping: missing GStreamer elements: {}",
        missing.join(", ")
    );
    false
}

struct Harness {
    pipeline: gst::Pipeline,
    elements: HashMap<String, gst::Element>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Build the block through its real builder and assemble the returned elements
/// and internal links into a pipeline, the way the pipeline manager does.
fn assemble(instance: &str, properties: &HashMap<String, PropertyValue>) -> Harness {
    gst::init().unwrap();
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = liveaudiorouter::LiveAudioRouterBuilder
        .build(instance, properties, &ctx)
        .expect("liveaudiorouter build");

    let pipeline = gst::Pipeline::new();
    let mut elements: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &result.elements {
        pipeline.add(element).expect("add element");
        elements.insert(id.clone(), element.clone());
    }

    for (from, to) in &result.internal_links {
        let src = elements
            .get(&from.element_id)
            .unwrap_or_else(|| panic!("link source {} not in elements", from.element_id));
        let dst = elements
            .get(&to.element_id)
            .unwrap_or_else(|| panic!("link sink {} not in elements", to.element_id));

        match (&from.pad_name, &to.pad_name) {
            (Some(src_pad), Some(dst_pad)) => {
                let sp = resolve_pad(src, src_pad);
                let dp = resolve_pad(dst, dst_pad);
                sp.link(&dp).unwrap_or_else(|e| {
                    panic!(
                        "link {}:{} -> {}:{}: {e:?}",
                        from.element_id, src_pad, to.element_id, dst_pad
                    )
                });
            }
            _ => src.link(dst).expect("element link"),
        }
    }

    Harness { pipeline, elements }
}

/// Request pads are not reachable through `static_pad`, and pads requested at
/// build time are already present, so look in `pads()` before requesting.
fn resolve_pad(element: &gst::Element, name: &str) -> gst::Pad {
    if let Some(pad) = element.static_pad(name) {
        return pad;
    }
    if let Some(pad) = element.pads().into_iter().find(|p| p.name() == name) {
        return pad;
    }
    element
        .request_pad_simple(name)
        .unwrap_or_else(|| panic!("element {} has no pad {name}", element.name()))
}

/// Highest per-channel peak seen on `level` messages within `timeout`.
fn observe_peaks(pipeline: &gst::Pipeline, channels: usize, timeout: Duration) -> Vec<f64> {
    let bus = pipeline.bus().expect("pipeline bus");
    let mut peaks = vec![f64::NEG_INFINITY; channels];
    let start = Instant::now();

    while start.elapsed() < timeout {
        let remaining = timeout.saturating_sub(start.elapsed());
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(remaining.as_millis() as u64))
        else {
            break;
        };
        if let gst::MessageView::Element(element_msg) = msg.view() {
            let Some(s) = element_msg.structure() else {
                continue;
            };
            if s.name() != "level" {
                continue;
            }
            let Ok(values) = s.get::<gst::Array>("peak") else {
                continue;
            };
            for (i, v) in values.as_slice().iter().enumerate() {
                if i < peaks.len() {
                    if let Ok(db) = v.get::<f64>() {
                        peaks[i] = peaks[i].max(db);
                    }
                }
            }
        }
    }

    peaks
}

/// The live half of #661: changing `routing_matrix` on a running pipeline must
/// move audio from one output channel to another without a flow restart.
#[test]
fn a_routing_matrix_change_moves_audio_between_output_channels_while_playing() {
    if !require_elements(&[
        "audiomixmatrix",
        "interleave",
        "deinterleave",
        "audiotestsrc",
        "audioconvert",
        "level",
        "fakesink",
    ]) {
        return;
    }

    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(2)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);

    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("samplesperbuffer", 240_i32)
        .property_from_str("wave", "sine")
        .build()
        .expect("audiotestsrc");
    let convert = gst::ElementFactory::make("audioconvert")
        .build()
        .expect("audioconvert");
    let in_caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("channels", 2i32)
                .field("rate", 48000i32)
                .build(),
        )
        .build()
        .expect("capsfilter");
    let level = gst::ElementFactory::make("level")
        .property("post-messages", true)
        .property("interval", 50_000_000u64)
        .build()
        .expect("level");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");

    h.pipeline
        .add_many([&src, &convert, &in_caps, &level, &sink])
        .expect("add test elements");
    gst::Element::link_many([&src, &convert, &in_caps]).expect("link source chain");
    gst::Element::link_many([&level, &sink]).expect("link sink chain");

    let identity_in = h.elements.get("live:identity_in_0").expect("identity_in_0");
    let queue_out = h.elements.get("live:queue_out_0").expect("queue_out_0");
    in_caps
        .link(identity_in)
        .expect("link capsfilter to block input");
    queue_out.link(&level).expect("link block output to level");

    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let before = observe_peaks(&h.pipeline, 2, Duration::from_secs(3));
    assert!(
        before[0] > before[1] + 20.0,
        "expected audio on output channel 0 before the change, got peaks {before:?}"
    );

    let matrix_element = h.elements.get("live:matrix").expect("matrix element");
    let applied = liveaudiorouter::try_apply_live_matrix(
        matrix_element,
        "live:matrix",
        "routing_matrix",
        &PropertyValue::String(r#"{"i0c0":["o0c1"]}"#.to_string()),
    );
    assert!(applied, "try_apply_live_matrix must claim routing_matrix");

    let after = observe_peaks(&h.pipeline, 2, Duration::from_secs(3));
    assert!(
        after[1] > after[0] + 20.0,
        "routing change did not move audio to output channel 1 on the running pipeline: \
         before {before:?}, after {after:?}"
    );
}
