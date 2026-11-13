//! An SRT URI with a query key without `=` (`srt://:5000?latency`) segfaults
//! the process inside GStreamer's `srtsrc`/`srtsink` as soon as the URI is set
//! (gst-plugins-bad 1.22.0 and 1.28.6, see `docs/UPSTREAM_NOTES.md`). Each
//! block that hands a user URI to an SRT element must refuse it at build time.
//!
//! The flows go through `PipelineManager::new`, as `start_flow` builds them
//! (it needs a multi-threaded tokio runtime).
//! With the check removed from a block, its test does not fail an assertion:
//! the URI reaches the element and the test binary crashes.

use crate::common;

use std::collections::HashMap;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Flow, PropertyValue};

const BAD_URI: &str = "srt://:5000?latency";

fn flow_with_block(definition: &str, property: &str, value: &str) -> Flow {
    let mut flow = Flow::new(format!("srt_uri_check_{definition}"));
    let mut properties = HashMap::new();
    properties.insert(property.to_string(), PropertyValue::String(value.into()));
    flow.blocks.push(BlockInstance {
        id: "srt".to_string(),
        block_definition_id: definition.to_string(),
        name: None,
        properties,
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

/// Build the flow and return the error, failing the test if it builds.
fn build_error(flow: &Flow) -> String {
    match common::manager::build(flow) {
        Ok(_) => panic!("flow {} built with a bare SRT query key", flow.name),
        Err(e) => e.to_string(),
    }
}

fn assert_refused(definition: &str) {
    common::require_elements(&["srtsrc", "srtsink"]);
    let error = build_error(&flow_with_block(definition, "srt_uri", BAD_URI));
    assert!(
        error.contains("srt_uri: query parameter 'latency' has no value"),
        "{definition}: unexpected error: {error}"
    );
    assert!(
        !error.contains("srt://"),
        "{definition}: error leaks the URI"
    );
}

/// Control: an empty value is not the bug; GStreamer takes it, and so does
/// the block. Guards against a check that refuses every query.
fn assert_empty_value_builds(definition: &str) {
    common::require_elements(&["srtsrc", "srtsink"]);
    let flow = flow_with_block(definition, "srt_uri", "srt://:5000?latency=&&");
    if let Err(e) = common::manager::build(&flow) {
        panic!("{definition}: an empty value was refused: {e}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegtssrt_output_refuses_a_key_without_value() {
    assert_refused("builtin.mpegtssrt_output");
    assert_empty_value_builds("builtin.mpegtssrt_output");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mpegtssrt_input_refuses_a_key_without_value() {
    assert_refused("builtin.mpegtssrt_input");
    assert_empty_value_builds("builtin.mpegtssrt_input");
}

#[cfg(feature = "efp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn efpsrt_output_refuses_a_key_without_value() {
    assert_refused("builtin.efpsrt_output");
    assert_empty_value_builds("builtin.efpsrt_output");
}

#[cfg(feature = "efp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn efpsrt_input_refuses_a_key_without_value() {
    assert_refused("builtin.efpsrt_input");
    assert_empty_value_builds("builtin.efpsrt_input");
}

/// The Media Player hands an `srt://` playlist entry to `uridecodebin`, which
/// creates an `srtsrc` for it. Only `srt://` entries are checked: a bare key
/// is fine in an http(s) URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_player_refuses_an_srt_entry_with_a_key_without_value() {
    common::require_elements(&["srtsrc", "uridecodebin"]);
    let playlist = format!("[\"{BAD_URI}\"]");
    let error = build_error(&flow_with_block(
        "builtin.media_player",
        "playlist",
        &playlist,
    ));
    assert!(
        error.contains("playlist: query parameter 'latency' has no value"),
        "unexpected error: {error}"
    );
}
