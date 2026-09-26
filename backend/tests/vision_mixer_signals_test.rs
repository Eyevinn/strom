//! What the vision mixer reports about its own picture, checked on a real
//! CPU mixer flow built through `PipelineManager`: `input_media_age_ms` from
//! the state, and `actual_transition_type` from a take.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::vision_mixer::{PipTransforms, SourceCrop};
use strom_types::{Flow, PropertyValue as PV};
use tempfile::NamedTempFile;

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// A CPU vision mixer with `num_inputs` inputs, of which the first `linked`
/// are fed by a live test source. Input 1, when linked, passes through a
/// valve named `valve1` so a test can stall it without EOS.
fn build_flow(block_id: &str, num_inputs: u64, linked: usize, extra: &[(&str, &str)]) -> Flow {
    let mut flow = Flow::new(format!("vm_signals_{}", block_id));
    let mut props = HashMap::new();
    props.insert(
        "compositor_preference".to_string(),
        PV::String("cpu".into()),
    );
    props.insert("num_inputs".to_string(), PV::UInt(num_inputs));
    props.insert("pgm_resolution".to_string(), PV::String("640x360".into()));
    props.insert(
        "multiview_resolution".to_string(),
        PV::String("640x360".into()),
    );
    for (k, v) in extra {
        props.insert(k.to_string(), PV::String(v.to_string()));
    }
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }
    flow.elements
        .push(elem("pgmsink", "fakesink", vec![("sync", PV::Bool(false))]));
    flow.elements
        .push(elem("mvsink", "fakesink", vec![("sync", PV::Bool(false))]));
    let mut links = vec![
        (format!("{}:pgm_out", block_id), "pgmsink:sink".to_string()),
        (
            format!("{}:multiview_out", block_id),
            "mvsink:sink".to_string(),
        ),
    ];
    for i in 0..linked {
        flow.elements.push(elem(
            &format!("src{}", i),
            "videotestsrc",
            vec![("is-live", PV::Bool(true))],
        ));
        flow.elements.push(elem(
            &format!("caps{}", i),
            "capsfilter",
            vec![(
                "caps",
                PV::String("video/x-raw,width=640,height=360,framerate=30/1".into()),
            )],
        ));
        if i == 1 {
            flow.elements.push(elem("valve1", "valve", vec![]));
            links.push(("src1:src".to_string(), "valve1:sink".to_string()));
            links.push(("valve1:src".to_string(), "caps1:sink".to_string()));
        } else {
            links.push((format!("src{}:src", i), format!("caps{}:sink", i)));
        }
        links.push((
            format!("caps{}:src", i),
            format!("{}:video_in_{}", block_id, i),
        ));
    }
    for (from, to) in links {
        flow.links.push(strom_types::Link { from, to });
    }
    flow
}

async fn start(flow: &Flow, block_id: &str) -> PipelineManager {
    gstreamer::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let mut manager = PipelineManager::new(
        flow,
        EventBroadcaster::new(10, false, false),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("build pipeline");
    manager.start().expect("start pipeline");
    // A live latency answer is not proof of data: wait for the mixer to run.
    let mixer = manager
        .pipeline()
        .by_name(&format!("{}:mixer", block_id))
        .expect("mixer");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while mixer.query_position::<gstreamer::ClockTime>().is_none() {
        assert!(std::time::Instant::now() < deadline, "mixer never ran");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    manager
}

fn ages(block_id: &str) -> Vec<Option<u64>> {
    let s = strom::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_id)
        .expect("overlay state registered");
    (0..s.num_inputs).map(|i| s.input_media_age_ms(i)).collect()
}

/// The mixer builder must install the activity probes: a live input reads
/// young, one that stalls without EOS reads old, and an unlinked one reads
/// null. Fails if the builder stops installing them, which the unit test on
/// the probe helper cannot see.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_age_separates_live_stalled_and_unlinked_inputs() {
    let block_id = "vmsig_age";
    let flow = build_flow(block_id, 3, 2, &[]);
    let mut manager = start(&flow, block_id).await;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    manager
        .pipeline()
        .by_name("valve1")
        .expect("valve1")
        .set_property("drop", true);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let a = ages(block_id);

    let _ = manager.stop();
    strom::blocks::builtin::vision_mixer::overlay::unregister_flow(&flow.id);

    let (live, stalled) = (
        a[0].expect("input 0 delivered"),
        a[1].expect("input 1 delivered"),
    );
    assert!(
        stalled >= live + 1500,
        "stalled input 1 must read at least 1.5 s older than live input 0: {:?}",
        a
    );
    assert_eq!(a[2], None, "unlinked input 2 never delivered: {:?}", a);
}

/// A punch-in (PiP 0: input 1 full frame, cropped 2x) taken to plain input 1
/// keeps the same box and animates only the crop. On air that is a zoom-out,
/// so the take must report "morph", not "fade".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn punch_in_taken_to_plain_input_reports_morph() {
    let block_id = "vmsig_punch";
    let flow = build_flow(
        block_id,
        2,
        2,
        &[
            ("num_pips", "1"),
            ("initial_pgm_source", "pip:0"),
            ("initial_pvw_source", "input:1"),
        ],
    );
    let mut manager = start(&flow, block_id).await;

    let mut crop = PipTransforms::new();
    crop.insert(
        1,
        SourceCrop {
            left: 0.25,
            top: 0.25,
            right: 0.25,
            bottom: 0.25,
        },
    );
    manager
        .apply_vision_mixer_pip_config(block_id, 0, Some(1), vec![], crop)
        .expect("pip config");

    let result = manager.trigger_transition(block_id, 0, 1, "fade", 500);

    let _ = manager.stop();
    strom::blocks::builtin::vision_mixer::overlay::unregister_flow(&flow.id);

    let (_, _, _, kind) = result.expect("take");
    assert_eq!(kind, "morph");
}
