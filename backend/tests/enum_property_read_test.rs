//! Regression test: enum properties must be readable from a running flow, on
//! elements and on pads, as the nick the write path accepts.
//!
//! The element reader matched the literal type name `"GEnum"`, which no enum
//! reports (each has its own, e.g. `GstVideoTestSrcPattern`), so every enum
//! fell through to "Unsupported property type". The pad reader matched enums
//! correctly but read the GValue as `i32`, which glib refuses for an enum.
//! Both listing calls swallow a failed read, so enum properties were silently
//! missing from `GET .../properties` and `GET .../pads/{pad}/properties` —
//! the compositor editor never saw an input's `sizing-policy`.
//!
//! `videotestsrc` and `compositor` are in gstreamer1.0-plugins-base, which
//! every CI test job installs.

pub mod common;

use std::collections::HashMap;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, Link, PropertyValue};

/// `videotestsrc pattern=ball → compositor → fakesink`.
///
/// `pattern` is set to a non-default member, so reading back the default
/// would not pass.
fn build_flow() -> Flow {
    let mut flow = Flow::new("enum_property_read_test");

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "videotestsrc".to_string(),
        properties: HashMap::from([(
            "pattern".to_string(),
            PropertyValue::String("ball".to_string()),
        )]),
        position: [100.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "mix".to_string(),
        element_type: "compositor".to_string(),
        properties: HashMap::new(),
        position: [250.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [400.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "mix:sink_0".to_string(),
    });
    flow.links.push(Link {
        from: "mix:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow
}

fn build_manager() -> PipelineManager {
    common::manager::build(&build_flow()).expect("Failed to create PipelineManager")
}

fn nick(s: &str) -> PropertyValue {
    PropertyValue::String(s.to_string())
}

/// `PropertyValue` has no `PartialEq`; compare the string form.
fn as_str(v: Option<&PropertyValue>) -> Option<&str> {
    match v {
        Some(PropertyValue::String(s)) => Some(s),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn element_enum_property_reads_as_nick_and_round_trips() {
    gstreamer::init().unwrap();
    let manager = build_manager();

    let value = manager
        .get_element_property("src", "pattern")
        .expect("reading an enum property failed");
    assert_eq!(as_str(Some(&value)), Some("ball"), "got {:?}", value);

    let all = manager.get_element_properties("src").unwrap();
    assert_eq!(
        as_str(all.get("pattern")),
        Some("ball"),
        "enum property missing from the element listing"
    );

    // What the reader returns is what the writer takes.
    manager
        .update_element_property("src", "pattern", &nick("snow"), None)
        .expect("writing a nick failed");
    assert_eq!(
        as_str(Some(
            &manager.get_element_property("src", "pattern").unwrap()
        )),
        Some("snow")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pad_enum_property_reads_as_nick_and_round_trips() {
    gstreamer::init().unwrap();
    let manager = build_manager();

    // Flow pad properties are applied in start(); write one directly instead.
    // `none` is the default, so a read that returned it would prove nothing.
    manager
        .update_pad_property("mix", "sink_0", "sizing-policy", &nick("keep-aspect-ratio"))
        .expect("writing a nick to a pad failed");

    let value = manager
        .get_pad_property("mix", "sink_0", "sizing-policy")
        .expect("reading an enum pad property failed");
    assert_eq!(
        as_str(Some(&value)),
        Some("keep-aspect-ratio"),
        "got {:?}",
        value
    );

    let all = manager.get_pad_properties("mix", "sink_0").unwrap();
    assert_eq!(
        as_str(all.get("sizing-policy")),
        Some("keep-aspect-ratio"),
        "enum property missing from the pad listing"
    );
}

/// Reading a pad that does not exist reports it missing and leaves the element
/// alone. Requesting one instead left an unlinked input on the compositor for
/// the life of the flow, one per pad name a client asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_a_missing_pad_does_not_create_it() {
    use gstreamer::prelude::*;

    gstreamer::init().unwrap();
    let manager = build_manager();

    assert!(manager.get_pad_properties("mix", "sink_5").is_err());
    assert!(manager
        .get_pad_property("mix", "sink_6", "sizing-policy")
        .is_err());

    let mix = manager
        .pipeline()
        .by_name("mix")
        .expect("the flow's compositor");
    let sinks: Vec<String> = mix
        .sink_pads()
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    assert_eq!(sinks, vec!["sink_0"], "a read requested pads on the mixer");
}
