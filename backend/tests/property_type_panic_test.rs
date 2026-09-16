//! Regression test: a property value of the wrong type in a flow definition
//! must fail the build, not panic the task that is serving the request.
//!
//! `POST /api/flows` lets a client name both a property and its value, and GLib
//! reports every kind of misuse by panicking. Handing one to
//! `element.set_property()` without checking it against the spec unwinds the
//! task at `POST /api/flows/{id}/start`: the client gets a dropped connection
//! instead of a response, and `start_flow`'s error path — which tears the
//! half-built flow down — never runs.
//!
//! Each case here panics if the checked conversion is reverted, which fails
//! the test.

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::{PipelineError, PipelineManager};
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

/// `videotestsrc` ships in gstreamer-plugins-base, which the CI job installs,
/// so these tests run there rather than skipping green.
const REQUIRED: &[&str] = &["videotestsrc", "fakesink"];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    gstreamer::init().unwrap();
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gstreamer::ElementFactory::find(e).is_none())
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

/// `videotestsrc → fakesink`, with `properties` applied to the source.
fn flow_with_source_properties(name: &str, properties: HashMap<String, PropertyValue>) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(strom_types::Element {
        id: "src0".to_string(),
        element_type: "videotestsrc".to_string(),
        properties,
        position: [100.0, 200.0].into(),
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
        from: "src0:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow
}

fn one_property(name: &str, value: PropertyValue) -> HashMap<String, PropertyValue> {
    let mut properties = HashMap::new();
    properties.insert(name.to_string(), value);
    properties
}

/// Build the pipeline the way `start_flow` does.
fn build(flow: &Flow) -> Result<PipelineManager, PipelineError> {
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());

    PipelineManager::new(
        flow,
        EventBroadcaster::new(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
}

fn expect_invalid_property(flow: &Flow, expected_property: &str) -> String {
    match build(flow) {
        Ok(_) => panic!("pipeline built despite an invalid value for '{expected_property}'"),
        Err(PipelineError::InvalidProperty {
            property, reason, ..
        }) => {
            assert_eq!(property, expected_property);
            reason
        }
        Err(other) => panic!("expected InvalidProperty, got {other:?}"),
    }
}

/// The reported repro: an integer for an enum property.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_range_enum_integer_is_rejected() {
    if !plugins_available() {
        return;
    }
    let flow = flow_with_source_properties(
        "enum_out_of_range",
        one_property("pattern", PropertyValue::Int(99_999)),
    );
    let reason = expect_invalid_property(&flow, "pattern");
    assert!(reason.contains("99999"), "unhelpful message: {reason}");
}

/// GLib enums do have integer values, so an in-range one is accepted rather
/// than turned into a second error. The nick string keeps working too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_range_enum_integer_is_accepted() {
    if !plugins_available() {
        return;
    }
    let flow = flow_with_source_properties(
        "enum_in_range",
        one_property("pattern", PropertyValue::Int(1)),
    );
    build(&flow).expect("pattern 1 (`snow`) is a valid GstVideoTestSrcPattern");

    let flow = flow_with_source_properties(
        "enum_nick",
        one_property("pattern", PropertyValue::String("snow".to_string())),
    );
    build(&flow).expect("the nick form regressed");
}

/// A string that is not a number, for a `gint` property.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unparseable_string_for_an_integer_property_is_rejected() {
    if !plugins_available() {
        return;
    }
    let flow = flow_with_source_properties(
        "string_for_int",
        one_property(
            "num-buffers",
            PropertyValue::String("not-a-number".to_string()),
        ),
    );
    expect_invalid_property(&flow, "num-buffers");
}

/// `num-buffers` is a `gint` whose spec starts at -1. GLib clamps a value
/// below that and then panics because it had to clamp — a type check alone
/// would not have caught this one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integer_below_the_declared_minimum_is_rejected() {
    if !plugins_available() {
        return;
    }
    let flow = flow_with_source_properties(
        "int_out_of_range",
        one_property("num-buffers", PropertyValue::Int(-5)),
    );
    let reason = expect_invalid_property(&flow, "num-buffers");
    assert!(reason.contains("-1"), "message omits the range: {reason}");
}

/// A property the element does not have must fail before it reaches GLib, which
/// panics on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_property_is_rejected() {
    if !plugins_available() {
        return;
    }
    let flow = flow_with_source_properties(
        "unknown_property",
        one_property("no-such-property", PropertyValue::Int(1)),
    );
    let reason = expect_invalid_property(&flow, "no-such-property");
    assert!(reason.contains("not found"), "unhelpful message: {reason}");
}

/// Control: the same harness on a flow with valid properties still builds and
/// runs. Guards against "fixing" the panic by rejecting everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_properties_still_build_and_start() {
    if !plugins_available() {
        return;
    }
    let mut properties = one_property("pattern", PropertyValue::String("snow".to_string()));
    properties.insert("num-buffers".to_string(), PropertyValue::Int(5));
    properties.insert("is-live".to_string(), PropertyValue::Bool(false));

    let flow = flow_with_source_properties("valid_properties", properties);
    let mut manager = build(&flow).expect("a valid flow no longer builds");

    let state = manager.start().expect("a valid flow no longer starts");
    assert_eq!(state, strom_types::PipelineState::Playing);
    manager.stop().expect("failed to stop pipeline");
}
