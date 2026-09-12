//! Regression test: stopping a flow must not remove more bus signal watches
//! than were added.
//!
//! `remove_bus_watch()` derived its remove count from the number of registered
//! block message handlers, but only some blocks add a signal watch of their
//! own. A flow holding a Media Player block therefore removed twice against a
//! single add, and the surplus call logged
//!
//! ```text
//! GStreamer-CRITICAL **: Bus bus0 has no signal watches attached
//! ```
//!
//! Nothing leaks — the error direction is over-removal — but a GLib critical in
//! production logs teaches operators to ignore criticals, and the same call
//! aborts the process under `G_DEBUG=fatal-criticals`.
//!
//! That critical is the only externally observable difference the imbalance
//! makes, so the test captures GLib log output across a start/stop cycle and
//! asserts the message never arrives. It lives in its own test binary because
//! the log handler it installs is process-global.

use gstreamer::glib;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::{Flow, Link, PropertyValue};
use tempfile::NamedTempFile;

/// The GStreamer text for one remove_signal_watch() past the matching add.
const SURPLUS_REMOVE: &str = "has no signal watches attached";

fn captured() -> &'static Mutex<Vec<String>> {
    static CAPTURED: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CAPTURED.get_or_init(|| Mutex::new(Vec::new()))
}

/// A startable flow holding a block that registers a bus message handler
/// without adding a signal watch of its own.
///
/// The Media Player is such a block: its handler is a no-op on the flow bus
/// (all its real work happens on its own internal bus), so it adds nothing,
/// while still occupying an entry in the handler list the old remove count was
/// read from. An empty playlist is enough — the block registers while the
/// pipeline is built, which is all this test needs from it.
fn build_flow_with_a_media_player(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    flow.elements.push(strom_types::Element {
        id: "src".to_string(),
        element_type: "audiotestsrc".to_string(),
        properties: HashMap::new(),
        position: [100.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.elements.push(strom_types::Element {
        id: "sink".to_string(),
        element_type: "fakesink".to_string(),
        properties: HashMap::new(),
        position: [300.0, 200.0].into(),
        pad_properties: HashMap::new(),
    });

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow.blocks.push(strom_types::BlockInstance {
        id: "player".to_string(),
        block_definition_id: "builtin.media_player".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "playlist".to_string(),
                PropertyValue::String("[]".to_string()),
            );
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 400.0 },
        runtime_data: None,
        computed_external_pads: None,
    });

    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_flow_removes_no_more_bus_watches_than_it_added() {
    gstreamer::init().unwrap();

    glib::log_set_default_handler(|domain, level, message| {
        captured()
            .lock()
            .expect("log capture lock poisoned")
            .push(format!(
                "{}-{:?}: {}",
                domain.unwrap_or("<none>"),
                level,
                message
            ));
    });

    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let state = AppState::new(
        JsonFileStorage::new(storage_file.path()),
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    );

    let flow = build_flow_with_a_media_player("bus_signal_watch_balance");
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow failed");

    state.start_flow(&flow_id).await.expect("start_flow failed");
    state.stop_flow(&flow_id).await.expect("stop_flow failed");

    let surplus: Vec<String> = captured()
        .lock()
        .expect("log capture lock poisoned")
        .iter()
        .filter(|message| message.contains(SURPLUS_REMOVE))
        .cloned()
        .collect();

    assert!(
        surplus.is_empty(),
        "stopping the flow removed more bus signal watches than it added, and \
         GStreamer logged a critical for each surplus call: {:?}. The remove \
         count must come from the adds, not from the number of registered block \
         message handlers — blocks that register a handler without taking a \
         watch of their own make those two numbers different.",
        surplus
    );
}
