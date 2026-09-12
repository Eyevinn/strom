//! Regression test: the `/pip` write path must reject a zone whose source
//! list outgrows its `capacity`.
//!
//! `Zone::effective_sources` renders only the newest `capacity` entries. The
//! validator used to copy `sources` through verbatim, so an over-capacity PUT
//! returned 200, stored the whole list, and drew a subset — a GET then
//! reported a composition that had never been on air. Which sources are on
//! screen is semantic state, so this fails loudly instead.
//!
//! Runs on the CPU compositor backend: no GL context, no optional plugins.

use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::vision_mixer::Zone;
use strom_types::{Flow, PropertyValue};
use tempfile::NamedTempFile;

const BLOCK_ID: &str = "vm-pip-capacity";
const NUM_INPUTS: u64 = 4;

/// A vision mixer with one PiP, forced onto the CPU compositor. Inputs are
/// left unlinked, as in `vision_mixer_fx_test` — force-live compositors
/// output regardless.
fn build_vm_flow() -> Flow {
    let mut flow = Flow::new("vm_pip_capacity_test");
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK_ID.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: {
            let mut p = HashMap::new();
            p.insert(
                "compositor_preference".to_string(),
                PropertyValue::String("cpu".to_string()),
            );
            p.insert("num_inputs".to_string(), PropertyValue::UInt(NUM_INPUTS));
            p.insert("num_pips".to_string(), PropertyValue::UInt(1));
            p
        },
        position: strom_types::block::Position { x: 100.0, y: 100.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    flow
}

fn zone(capacity: Option<usize>, sources: Vec<usize>) -> Zone {
    Zone {
        rect: None,
        capacity,
        sources,
        border: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_capacity_zone_is_rejected() {
    gstreamer::init().unwrap();
    // The vision mixer's converters ask for the detected GPU mode, which
    // panics if nothing has probed for it — `main` does this at startup.
    strom::gpu::detect_gpu_capabilities();

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    let flow = build_vm_flow();
    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("CPU vision mixer pipeline should build");
    manager.start().expect("CPU vision mixer should start");

    let transforms = strom_types::vision_mixer::PipTransforms::new();

    // Positive control: a zone filled exactly to capacity is accepted, and
    // every source it names is stored. Without this, the rejection below
    // could pass on a validator that refuses all capacities.
    manager
        .apply_vision_mixer_pip_config(
            BLOCK_ID,
            0,
            Some(0),
            vec![zone(Some(2), vec![1, 2])],
            transforms.clone(),
        )
        .expect("a zone filled to capacity must be accepted");

    let state = strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(BLOCK_ID)
        .expect("overlay state for a running mixer");
    let accepted = state.pip_zones(0);
    assert_eq!(accepted.len(), 1, "expected one stored zone");
    assert_eq!(
        accepted[0].sources,
        vec![1, 2],
        "an at-capacity zone must store every source it named"
    );

    // One source past capacity: storing this would report [1, 2, 3] and
    // render [2, 3].
    let err = manager
        .apply_vision_mixer_pip_config(
            BLOCK_ID,
            0,
            Some(0),
            vec![zone(Some(2), vec![1, 2, 3])],
            transforms.clone(),
        )
        .expect_err(
            "a zone holding more sources than its capacity must be rejected — \
             storing it leaves the read-back state and the rendered picture \
             disagreeing permanently",
        );
    let msg = err.to_string();
    assert!(
        msg.contains("capacity"),
        "the rejection must say which rule was broken, got: {}",
        msg
    );

    // A rejected request must not have mutated anything: the whole PUT is
    // rejected, not the offending zone alone.
    let after = state.pip_zones(0);
    assert_eq!(
        after, accepted,
        "a rejected PiP config must leave the stored composition untouched"
    );

    // Capacity 0 with sources is the same violation, not an "unlimited" alias
    // — `None` is unlimited.
    manager
        .apply_vision_mixer_pip_config(
            BLOCK_ID,
            0,
            Some(0),
            vec![zone(Some(0), vec![1])],
            transforms.clone(),
        )
        .expect_err("capacity 0 with a source must be rejected, not read as unlimited");

    // No capacity means no cap: the same list is fine.
    manager
        .apply_vision_mixer_pip_config(
            BLOCK_ID,
            0,
            Some(0),
            vec![zone(None, vec![1, 2, 3])],
            transforms,
        )
        .expect("an uncapped zone must accept any in-range source list");

    manager.stop().expect("stop failed");
    drop(manager);
}
