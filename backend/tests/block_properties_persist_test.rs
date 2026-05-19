//! End-to-end tests for the `persist: false` guard.
//!
//! Covers the regression that prompted introducing the `persist` flag: solo
//! state (`chN_pfl` / `chN_afl`) leaking back to disk via explicit flow saves,
//! re-engaging on the next pipeline restart.

use std::collections::HashMap;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Flow, PropertyValue};
use tempfile::NamedTempFile;

fn mixer_flow_with_solo() -> Flow {
    let mut flow = Flow::new("persist-guard-test");
    let mut properties = HashMap::new();
    // Transient — should be stripped.
    properties.insert("ch1_pfl".to_string(), PropertyValue::Bool(true));
    properties.insert("ch1_afl".to_string(), PropertyValue::Bool(true));
    properties.insert("ch2_pfl".to_string(), PropertyValue::Bool(true));
    // Persistent — should survive.
    properties.insert("ch1_fader".to_string(), PropertyValue::Float(0.8));
    properties.insert("main_fader".to_string(), PropertyValue::Float(0.5));
    properties.insert("ch1_mute".to_string(), PropertyValue::Bool(true));

    flow.blocks.push(BlockInstance {
        id: "mix1".to_string(),
        block_definition_id: "builtin.mixer".to_string(),
        name: Some("Mix".to_string()),
        properties,
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

fn new_state() -> AppState {
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();
    let storage = JsonFileStorage::new(storage_file.path());
    AppState::new(
        storage,
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    )
}

/// `upsert_flow` is the path the flow PATCH/PUT handlers use. It must strip
/// `persist: false` properties before they reach the in-memory map or disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_flow_strips_transient_properties() {
    gstreamer::init().unwrap();
    let state = new_state();
    let flow = mixer_flow_with_solo();
    let flow_id = flow.id;

    state.upsert_flow(flow).await.expect("upsert_flow");

    let stored = state.get_flow(&flow_id).await.expect("flow present");
    let block = stored
        .blocks
        .iter()
        .find(|b| b.id == "mix1")
        .expect("mixer block");

    // Solo state must be absent — both the live PATCH path and explicit save
    // path rely on these never surviving to the next run.
    assert!(
        !block.properties.contains_key("ch1_pfl"),
        "ch1_pfl should be stripped, got {:?}",
        block.properties.get("ch1_pfl")
    );
    assert!(
        !block.properties.contains_key("ch1_afl"),
        "ch1_afl should be stripped"
    );
    assert!(
        !block.properties.contains_key("ch2_pfl"),
        "ch2_pfl should be stripped"
    );

    // Genuinely persistent mix state must survive.
    assert!(
        matches!(block.properties.get("ch1_fader"), Some(PropertyValue::Float(f)) if (*f - 0.8).abs() < 1e-9),
        "ch1_fader must be preserved (got {:?})",
        block.properties.get("ch1_fader")
    );
    assert!(
        matches!(block.properties.get("main_fader"), Some(PropertyValue::Float(f)) if (*f - 0.5).abs() < 1e-9),
        "main_fader must be preserved (got {:?})",
        block.properties.get("main_fader")
    );
    assert!(
        matches!(
            block.properties.get("ch1_mute"),
            Some(PropertyValue::Bool(true))
        ),
        "ch1_mute must be preserved (live: true, persist: None → defaults to true), got {:?}",
        block.properties.get("ch1_mute")
    );
}

/// Legacy flow JSON on disk may carry PFL/AFL values from before the
/// `persist: false` guard existed. `load_from_storage` strips them on load so
/// users don't get phantom solo on first startup after upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_from_storage_strips_transient_properties() {
    gstreamer::init().unwrap();
    let storage_file = NamedTempFile::new().unwrap();
    let blocks_file = NamedTempFile::new().unwrap();

    // Seed the storage file with a "legacy" flow JSON that includes transient
    // solo state — simulating a flow saved before this guard existed. The
    // format matches JsonFileStorage's internal StorageFormat.
    let flow = mixer_flow_with_solo();
    let flow_id = flow.id;
    let storage_json = serde_json::json!({
        "version": 1,
        "flows": [flow],
    });
    std::fs::write(storage_file.path(), storage_json.to_string()).expect("write seed");

    // Now spin up a fresh AppState that loads from this storage file.
    let storage = JsonFileStorage::new(storage_file.path());
    let state = AppState::new(
        storage,
        blocks_file.path(),
        std::env::temp_dir(),
        vec![],
        "all".to_string(),
        vec![],
    );
    state.load_from_storage().await.expect("load_from_storage");

    let loaded = state.get_flow(&flow_id).await.expect("flow loaded");
    let block = loaded
        .blocks
        .iter()
        .find(|b| b.id == "mix1")
        .expect("mixer block");

    assert!(
        !block.properties.contains_key("ch1_pfl"),
        "legacy ch1_pfl should be stripped on load"
    );
    assert!(
        !block.properties.contains_key("ch1_afl"),
        "legacy ch1_afl should be stripped on load"
    );
    assert!(
        matches!(block.properties.get("ch1_fader"), Some(PropertyValue::Float(f)) if (*f - 0.8).abs() < 1e-9),
        "persistent values must survive load (got {:?})",
        block.properties.get("ch1_fader")
    );
}
