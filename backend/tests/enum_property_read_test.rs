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

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

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

fn build_manager(registry: &BlockRegistry) -> PipelineManager {
    PipelineManager::new(
        &build_flow(),
        EventBroadcaster::new(10),
        registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("Failed to create PipelineManager")
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
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let manager = build_manager(&registry);

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
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let manager = build_manager(&registry);

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
