//! Specification tests for `builtin.liveaudiorouter`.
//!
//! The capability-parity test is the guard for #661: the new block must expose
//! the same property set and pad shape as `builtin.audiorouter`, and differ only
//! in being able to change its crosspoints on a running flow.

use gstreamer as gst;
use gstreamer::glib;
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

    // The new block is a superset, not a clone: it adds the fade time that
    // its live crosspoints need. Any *other* addition is still a failure, so
    // this stays a guard rather than becoming a rubber stamp.
    let allowed_additions: HashSet<String> = [
        // The fade its live crosspoints need...
        "crosspoint_fade_ms".to_string(),
        // ...and the output-bus settings, named as the mixer block names them.
        "force_live".to_string(),
        "latency".to_string(),
        "min_upstream_latency".to_string(),
        "output_buffer_duration".to_string(),
    ]
    .into();
    let extra: Vec<_> = new_names
        .difference(&old_names)
        .filter(|n| !allowed_additions.contains(*n))
        .cloned()
        .collect();
    assert!(
        extra.is_empty(),
        "builtin.liveaudiorouter exposes undocumented extra properties: {extra:?}"
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
// Real-audio harness
//
// Every test below runs audio through the block's own builder output. The
// routing model is not re-implemented here: what is asserted is what a `level`
// element measures on the block's output pads.
// ============================================================================

/// Elements the block needs. CI installs gstreamer1.0-plugins-{base,good,bad},
/// which covers all of them, so a missing element is a real failure and not a
/// reason to skip — a skipped test guards nothing.
const REQUIRED_ELEMENTS: &[&str] = &[
    "volume",
    "audiomixer",
    "deinterleave",
    "tee",
    "capssetter",
    "queue",
    "capsfilter",
    "audiointerleave",
    "valve",
    "audiotestsrc",
    "audioconvert",
    "level",
    "fakesink",
];

fn require_elements() {
    gst::init().unwrap();
    let missing: Vec<&str> = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|n| gst::ElementFactory::find(n).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "missing GStreamer elements {missing:?} — install gstreamer1.0-plugins-{{base,good,bad}}"
    );
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

/// Request pads created at build time are already present, so look in `pads()`
/// before asking for a new one.
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

/// Build the block through its real builder and assemble the result into a
/// pipeline the way the pipeline manager does.
fn assemble(instance: &str, properties: &HashMap<String, PropertyValue>) -> Harness {
    require_elements();
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

/// Feed one input pad of the block with `tones`, one sine per channel.
/// Distinct amplitudes let a level reading name which input channel arrived.
fn feed(h: &Harness, instance: &str, input: usize, tones: &[(f64, f64)]) {
    let mut mono_sources = Vec::new();
    for (freq, volume) in tones {
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("freq", *freq)
            .property("volume", *volume)
            .build()
            .expect("audiotestsrc");
        let mono = gst::ElementFactory::make("capsfilter")
            .property("caps", raw_caps(1))
            .build()
            .expect("capsfilter");
        h.pipeline.add_many([&src, &mono]).expect("add source");
        src.link(&mono).expect("link source");
        mono_sources.push(mono);
    }

    let target = h
        .elements
        .get(&format!("{instance}:identity_in_{input}"))
        .unwrap_or_else(|| panic!("no identity_in_{input}"));

    if mono_sources.len() == 1 {
        mono_sources[0].link(target).expect("link input");
        return;
    }

    // Several channels: interleave them into one stream first. This is the
    // test's own plumbing, not the block's.
    let il = gst::ElementFactory::make("audiointerleave")
        .property("channel-positions-from-input", false)
        .build()
        .expect("audiointerleave");
    let cf = gst::ElementFactory::make("capsfilter")
        .property("caps", raw_caps(mono_sources.len()))
        .build()
        .expect("capsfilter");
    h.pipeline.add_many([&il, &cf]).expect("add interleave");
    for (i, mono) in mono_sources.iter().enumerate() {
        let pad = il
            .request_pad_simple(&format!("sink_{i}"))
            .expect("interleave sink pad");
        mono.static_pad("src").unwrap().link(&pad).expect("link");
    }
    il.link(&cf).expect("link interleave");
    cf.link(target).expect("link input");
}

fn raw_caps(channels: usize) -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("channels", channels as i32)
        .field("rate", 48000i32)
        .field("format", "F32LE")
        .build()
}

/// Attach a `level` to one output pad of the block.
fn tap(h: &Harness, instance: &str, output: usize) {
    let level = gst::ElementFactory::make("level")
        .property("post-messages", true)
        .property("interval", 50_000_000u64)
        .build()
        .expect("level");
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .expect("fakesink");
    h.pipeline.add_many([&level, &sink]).expect("add tap");
    level.link(&sink).expect("link tap");
    h.elements
        .get(&format!("{instance}:queue_out_{output}"))
        .unwrap_or_else(|| panic!("no queue_out_{output}"))
        .link(&level)
        .expect("link output to level");
}

/// Highest per-channel peak seen on `level` messages within `timeout`,
/// rounded to 0.1 dB. Any pipeline error fails the test rather than showing
/// up later as unexplained silence.
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
        match msg.view() {
            gst::MessageView::Error(e) => panic!(
                "pipeline error from {:?}: {} ({:?})",
                e.src().map(|s| s.path_string()),
                e.error(),
                e.debug()
            ),
            gst::MessageView::Element(element_msg) => {
                let Some(s) = element_msg.structure() else {
                    continue;
                };
                if s.name() != "level" {
                    continue;
                }
                // `level` posts `peak` as a GValueArray, not a GstValueArray.
                let Ok(values) = s.get::<glib::ValueArray>("peak") else {
                    continue;
                };
                for (i, v) in values.iter().enumerate() {
                    if i < peaks.len() {
                        if let Ok(db) = v.get::<f64>() {
                            peaks[i] = peaks[i].max(db);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    peaks.iter().map(|p| (p * 10.0).round() / 10.0).collect()
}

/// A channel that carries no routed audio.
fn is_silent(db: f64) -> bool {
    db < -100.0
}

fn amplitude_db(amplitude: f64) -> f64 {
    ((20.0 * amplitude.log10()) * 10.0).round() / 10.0
}

// ============================================================================
// Fan-in and fan-out, measured on real audio
// ============================================================================

#[test]
fn two_input_channels_routed_to_one_output_channel_are_summed() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(2)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("input_1_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"],"i1c0":["o0c0"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.4)]);
    feed(&h, "live", 1, &[(1300.0, 0.3)]);
    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let peaks = observe_peaks(&h.pipeline, 2, Duration::from_secs(3));

    // Both inputs land on channel 0, so it must read louder than either alone.
    assert!(
        peaks[0] > amplitude_db(0.4) + 1.0,
        "output channel 0 must carry the sum of both inputs, got {peaks:?} \
         (loudest single input is {} dB)",
        amplitude_db(0.4)
    );
    assert!(
        is_silent(peaks[1]),
        "output channel 1 is unrouted and must be silent, got {peaks:?}"
    );
}

#[test]
fn one_input_channel_routed_to_several_outputs_fans_out() {
    let properties = props(&[
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

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5), (1300.0, 0.25)]);
    tap(&h, "live", 0);
    tap(&h, "live", 1);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    // Both taps post on the same bus; each output is mono, so channel 0 of
    // every message is the routed channel. Input channel 1 is at 0.25.
    let peaks = observe_peaks(&h.pipeline, 1, Duration::from_secs(3));
    assert!(
        (peaks[0] - amplitude_db(0.25)).abs() < 1.0,
        "both outputs must carry input 0 channel 1 ({} dB), got {peaks:?}",
        amplitude_db(0.25)
    );
}

#[test]
fn a_crosspoint_gain_below_unity_attenuates_rather_than_switching() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            // Same source into two output channels at different gains.
            PropertyValue::String(r#"{"i0c0":{"o0c0":1.0,"o0c1":0.25}}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5)]);
    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let peaks = observe_peaks(&h.pipeline, 2, Duration::from_secs(3));
    let expected_drop = -amplitude_db(0.25); // 0.25 gain = 12 dB down
    assert!(
        (peaks[0] - peaks[1] - expected_drop).abs() < 1.5,
        "the 0.25 crosspoint must sit about {expected_drop:.1} dB below the unity one, got {peaks:?}"
    );
}

// ============================================================================
// The live half — a routing change on a running pipeline
// ============================================================================

#[test]
fn a_routing_change_moves_audio_between_output_channels_while_playing() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5)]);
    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let before = observe_peaks(&h.pipeline, 2, Duration::from_secs(2));
    assert!(
        !is_silent(before[0]) && is_silent(before[1]),
        "expected audio on output channel 0 only before the change, got {before:?}"
    );

    // Exactly what the pipeline manager does on a live routing write: resolve
    // a gain for every crosspoint element, then write it. Going through the
    // same function keeps the test and the live path from drifting apart.
    let ids: Vec<&str> = h.elements.keys().map(String::as_str).collect();
    let targets = liveaudiorouter::crosspoint_targets("live", r#"{"i0c0":["o0c1"]}"#, ids);
    assert_eq!(
        targets.len(),
        2,
        "one crosspoint per output channel: {targets:?}"
    );
    for (element_id, gain) in &targets {
        h.elements[*element_id].set_property("volume", *gain);
    }

    // Let the old buffers drain before measuring the new state.
    let _ = observe_peaks(&h.pipeline, 2, Duration::from_millis(500));
    let after = observe_peaks(&h.pipeline, 2, Duration::from_secs(2));
    assert!(
        is_silent(after[0]) && !is_silent(after[1]),
        "the routing change did not move audio to output channel 1 on the running \
         pipeline: before {before:?}, after {after:?}"
    );
}

#[test]
fn closing_every_crosspoint_silences_the_output_without_a_silence_source() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0","o0c1"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5)]);
    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");
    assert!(
        observe_peaks(&h.pipeline, 2, Duration::from_secs(2))
            .iter()
            .all(|p| !is_silent(*p)),
        "both channels should carry audio to start with"
    );

    let ids: Vec<&str> = h.elements.keys().map(String::as_str).collect();
    for (element_id, gain) in liveaudiorouter::crosspoint_targets("live", "{}", ids) {
        assert_eq!(gain, 0.0, "an empty routing closes every crosspoint");
        h.elements[element_id].set_property("volume", gain);
    }

    let _ = observe_peaks(&h.pipeline, 2, Duration::from_millis(500));
    let after = observe_peaks(&h.pipeline, 2, Duration::from_secs(2));
    assert!(
        after.iter().all(|p| is_silent(*p)),
        "an all-closed routing must be silent, got {after:?}"
    );
}

// ============================================================================
// Robustness — the two failures a collectpads interleave cannot survive
// ============================================================================

#[test]
fn an_unconnected_input_does_not_stall_the_router() {
    // Three inputs configured, only the first one connected. A router that
    // collects one buffer per pad before producing output would deadlock here
    // and the output would stay silent forever.
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(3)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("input_1_channels", PropertyValue::UInt(2)),
        ("input_2_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(1)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5)]);
    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let peaks = observe_peaks(&h.pipeline, 1, Duration::from_secs(3));
    assert!(
        !is_silent(peaks[0]),
        "input 0 must reach the output even though inputs 1 and 2 are unconnected, got {peaks:?}"
    );
}

#[test]
fn a_source_that_stops_does_not_stall_the_other_inputs() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(2)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("input_1_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            PropertyValue::String(r#"{"i0c0":["o0c0"],"i1c0":["o0c1"]}"#.to_string()),
        ),
    ]);

    let h = assemble("live", &properties);
    feed(&h, "live", 0, &[(440.0, 0.5)]);

    // Input 1 goes through a valve we can close mid-stream.
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("is-live", true)
        .property("freq", 1300.0)
        .property("volume", 0.5)
        .build()
        .expect("audiotestsrc");
    let mono = gst::ElementFactory::make("capsfilter")
        .property("caps", raw_caps(1))
        .build()
        .expect("capsfilter");
    let valve = gst::ElementFactory::make("valve")
        .build()
        .expect("valve (gstreamer1.0-plugins-base)");
    h.pipeline
        .add_many([&src, &mono, &valve])
        .expect("add source");
    gst::Element::link_many([&src, &mono, &valve]).expect("link source");
    valve
        .link(h.elements.get("live:identity_in_1").expect("identity_in_1"))
        .expect("link input 1");

    tap(&h, "live", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let before = observe_peaks(&h.pipeline, 2, Duration::from_secs(2));
    assert!(
        !is_silent(before[0]) && !is_silent(before[1]),
        "both inputs should be flowing to start with, got {before:?}"
    );

    valve.set_property("drop", true);
    let _ = observe_peaks(&h.pipeline, 2, Duration::from_millis(500));
    let after = observe_peaks(&h.pipeline, 2, Duration::from_secs(2));
    assert!(
        !is_silent(after[0]),
        "input 0 must keep flowing after input 1 stops, got {after:?}"
    );
}

// ============================================================================
// Fades — why a crosspoint is a `volume` element and not a matrix coefficient
// ============================================================================

#[test]
fn every_crosspoint_is_a_volume_element_with_a_controllable_gain() {
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(1)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(2)),
        ("output_0_channels", PropertyValue::UInt(2)),
    ]);

    let h = assemble("live", &properties);
    let ids: Vec<&str> = h.elements.keys().map(String::as_str).collect();
    let crosspoints = liveaudiorouter::crosspoint_targets("live", "{}", ids);
    assert_eq!(crosspoints.len(), 4, "2x2 crossbar: {crosspoints:?}");

    for (element_id, _) in crosspoints {
        let element = &h.elements[element_id];
        // Only the standalone `volume` element samples `volume` per sample
        // (volume_transform_ip in gstvolume.c), which is what the per-sample
        // crosspoint fade depends on. An `audiomixer` sink pad also has a
        // `volume` property but applies it once per output block, and
        // `audiomixmatrix`' matrix is not controllable at all — either would
        // step, and a step is an audible click.
        assert_eq!(
            element.factory().map(|f| f.name().to_string()).as_deref(),
            Some("volume"),
            "crosspoint {element_id} must be a `volume` element"
        );
        let pspec = element
            .find_property("volume")
            .unwrap_or_else(|| panic!("{element_id} has no volume property"));
        assert!(
            pspec.flags().contains(glib::ParamFlags::CONSTRUCT)
                || pspec.flags().bits() & CONTROLLABLE_FLAG != 0,
            "crosspoint {element_id}'s volume must be controllable for the fade to work"
        );
    }
}

#[test]
fn the_crossbar_costs_no_thread_per_crosspoint() {
    // Each `queue` is a streaming thread. A crossbar this size is 70
    // crosspoints, so a queue per branch would be 70 threads for one router.
    // Every branch ends on an audiomixer sink pad, which queues and returns,
    // and the bus is built with force-live + latency + ignore-inactive-pads so
    // it always drains — the decoupling a queue would add is already there.
    // The only queues a router needs are one per output bus.
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(3)),
        ("num_outputs", PropertyValue::UInt(2)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("input_1_channels", PropertyValue::UInt(2)),
        ("input_2_channels", PropertyValue::UInt(4)),
        ("output_0_channels", PropertyValue::UInt(2)),
        ("output_1_channels", PropertyValue::UInt(8)),
    ]);

    require_elements();
    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = liveaudiorouter::LiveAudioRouterBuilder
        .build("live", &properties, &ctx)
        .expect("build");

    let queues: Vec<&String> = result
        .elements
        .iter()
        .filter(|(_, e)| {
            e.factory()
                .map(|f| f.name().as_str() == "queue")
                .unwrap_or(false)
        })
        .map(|(id, _)| id)
        .collect();

    assert_eq!(
        queues.len(),
        2,
        "expected one queue per output bus and no more, got {queues:?}"
    );

    let crosspoints = liveaudiorouter::crosspoint_targets(
        "live",
        "{}",
        result.elements.iter().map(|(id, _)| id.as_str()),
    );
    assert_eq!(
        crosspoints.len(),
        70,
        "7 input channels x 10 output channels"
    );
}

/// `GST_PARAM_CONTROLLABLE` — a GStreamer-specific `GParamFlags` bit, which
/// glib's `ParamFlags` does not name.
const CONTROLLABLE_FLAG: u32 = 1 << 9;

// ============================================================================
// The block this one sits beside must keep working
// ============================================================================

/// `builtin.audiorouter` shares the routing-matrix format, the editor and the
/// `make_audiomixer` helper with the new block. It is the block people already
/// have in saved flows, so it has to keep routing audio exactly as before.
#[test]
fn the_original_audiorouter_still_routes_audio() {
    require_elements();
    let properties = props(&[
        ("num_inputs", PropertyValue::UInt(2)),
        ("num_outputs", PropertyValue::UInt(1)),
        ("input_0_channels", PropertyValue::UInt(1)),
        ("input_1_channels", PropertyValue::UInt(1)),
        ("output_0_channels", PropertyValue::UInt(2)),
        (
            "routing_matrix",
            // The list form, which is all this block understands.
            PropertyValue::String(r#"{"i0c0":["o0c0"],"i1c0":["o0c1"]}"#.to_string()),
        ),
    ]);

    let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
    let result = audiorouter::AudioRouterBuilder
        .build("old", &properties, &ctx)
        .expect("audiorouter build");

    let pipeline = gst::Pipeline::new();
    let mut elements: HashMap<String, gst::Element> = HashMap::new();
    for (id, element) in &result.elements {
        pipeline.add(element).expect("add element");
        elements.insert(id.clone(), element.clone());
    }
    for (from, to) in &result.internal_links {
        let src = &elements[&from.element_id];
        let dst = &elements[&to.element_id];
        match (&from.pad_name, &to.pad_name) {
            (Some(sp), Some(dp)) => {
                resolve_pad(src, sp)
                    .link(&resolve_pad(dst, dp))
                    .unwrap_or_else(|e| {
                        panic!(
                            "link {}:{sp} -> {}:{dp}: {e:?}",
                            from.element_id, to.element_id
                        )
                    });
            }
            _ => src.link(dst).expect("element link"),
        }
    }
    let h = Harness { pipeline, elements };

    feed(&h, "old", 0, &[(440.0, 0.5)]);
    feed(&h, "old", 1, &[(1300.0, 0.25)]);
    tap(&h, "old", 0);
    h.pipeline
        .set_state(gst::State::Playing)
        .expect("set Playing");

    let peaks = observe_peaks(&h.pipeline, 2, Duration::from_secs(3));
    assert!(
        (peaks[0] - amplitude_db(0.5)).abs() < 1.5,
        "output channel 0 must carry input 0 ({} dB), got {peaks:?}",
        amplitude_db(0.5)
    );
    assert!(
        (peaks[1] - amplitude_db(0.25)).abs() < 1.5,
        "output channel 1 must carry input 1 ({} dB), got {peaks:?}",
        amplitude_db(0.25)
    );
}

/// The editor is shared, so the old block must still be shown checkboxes and a
/// Save button rather than live gain controls.
#[test]
fn the_original_audiorouter_is_not_offered_live_routing_or_gains() {
    let old = definition(audiorouter::get_blocks(), "builtin.audiorouter");
    assert!(
        !old.exposed_properties
            .iter()
            .find(|p| p.name == "routing_matrix")
            .expect("routing_matrix")
            .live,
        "builtin.audiorouter's routing is topology; declaring it live would make \
         the editor send writes the backend rejects"
    );
    assert!(
        !old.exposed_properties
            .iter()
            .any(|p| p.name == "crosspoint_fade_ms"),
        "the gain control is gated on this property, which the old block must not have"
    );
}
