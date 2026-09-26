//! An HTML source's Remote Control switch has to work on a running flow.
//!
//! The whole point of it is to intervene in a page that is already on air — to
//! log in, clear a consent dialog — so requiring a restart to turn it on would
//! defeat the feature. It maps to `_block`, and block-level properties are
//! otherwise refused as having no element to write to, so this guards the one
//! case where storing the value *is* the write.

use std::collections::HashMap;
use strom::state::AppState;
use strom::storage::JsonFileStorage;
use strom_types::block::{BlockInstance, Position};
use strom_types::{Flow, PropertyValue};
use tempfile::NamedTempFile;

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

fn html_flow() -> Flow {
    let mut flow = Flow::new("html-remote-control-test");
    let mut properties = HashMap::new();
    properties.insert(
        "url".to_string(),
        PropertyValue::String("https://example.com".to_string()),
    );
    properties.insert("remote_control".to_string(), PropertyValue::Bool(false));

    flow.blocks.push(BlockInstance {
        id: "html1".to_string(),
        block_definition_id: strom::blocks::builtin::html_input::BLOCK_ID.to_string(),
        name: Some("HTML".to_string()),
        properties,
        position: Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_control_can_be_switched_on_without_a_restart() {
    gstreamer::init().unwrap();
    let state = new_state();
    let flow = html_flow();
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");

    let (_current, rejected) = state
        .update_block_properties(
            &flow_id,
            "html1",
            HashMap::from([(
                strom::blocks::builtin::html_input::REMOTE_CONTROL_PROPERTY.to_string(),
                PropertyValue::Bool(true),
            )]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");

    assert!(
        rejected.is_empty(),
        "Remote Control must not be refused on a running flow, got {:?}",
        rejected
    );
    // The returned map mirrors the running pipeline, so it says nothing here;
    // what matters is that the value was stored, because that is what the link
    // endpoint reads before it mints anything. The refusal this guards against
    // happens before the pipeline is touched at all, so a stopped flow
    // exercises the same path.
    let stored = state.get_flow(&flow_id).await.expect("flow present");
    let block = stored
        .blocks
        .iter()
        .find(|b| b.id == "html1")
        .expect("html block");
    assert!(
        matches!(
            block.properties.get("remote_control"),
            Some(PropertyValue::Bool(true))
        ),
        "the switch must be stored, got {:?}",
        block.properties.get("remote_control")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_viewport_still_needs_a_restart() {
    gstreamer::init().unwrap();
    let state = new_state();
    let flow = html_flow();
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");

    // Width is baked into the capsfilter when the block is built, so changing
    // it live would report a size the pipeline is not rendering at.
    let (_current, rejected) = state
        .update_block_properties(
            &flow_id,
            "html1",
            HashMap::from([("width".to_string(), PropertyValue::UInt(640))]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");

    assert!(
        rejected.contains_key("width"),
        "width is not live and must say so rather than appearing to take effect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_control_value_that_is_not_a_bool_is_refused() {
    // Storing the value is the whole write, so this is the only place that can
    // check it. Persisting a string would report the switch as applied and
    // then read back as "off" when a link is asked for - the operator flips it
    // on, is refused anyway, and nothing says why.
    gstreamer::init().unwrap();
    let state = new_state();
    let flow = html_flow();
    let flow_id = flow.id;
    state.upsert_flow(flow).await.expect("upsert_flow");

    let (_current, rejected) = state
        .update_block_properties(
            &flow_id,
            "html1",
            HashMap::from([(
                strom::blocks::builtin::html_input::REMOTE_CONTROL_PROPERTY.to_string(),
                PropertyValue::String("true".to_string()),
            )]),
            None,
            None,
        )
        .await
        .expect("update_block_properties");

    assert!(
        rejected.contains_key(strom::blocks::builtin::html_input::REMOTE_CONTROL_PROPERTY),
        "a non-boolean must be refused rather than silently stored"
    );

    let stored = state.get_flow(&flow_id).await.expect("flow present");
    let block = stored
        .blocks
        .iter()
        .find(|b| b.id == "html1")
        .expect("html block");
    assert!(
        matches!(
            block.properties.get("remote_control"),
            Some(PropertyValue::Bool(false))
        ),
        "the refused value must not have overwritten the switch, got {:?}",
        block.properties.get("remote_control")
    );
}
