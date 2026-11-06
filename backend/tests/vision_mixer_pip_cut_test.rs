//! A cut to or from a PiP on the CPU (software compositor) backend: the
//! new picture is on before the old one goes off.

pub mod common;

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{Flow, PropertyValue as PV};

const W: usize = 320;
const H: usize = 180;
const BLOCK: &str = "vm";

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Red on input 0, blue on input 1, one PiP, program into an appsink.
fn build_flow() -> Flow {
    let mut flow = Flow::new("vm_pip_cut");
    let props: HashMap<String, PV> = [
        ("compositor_preference", PV::String("cpu".into())),
        ("num_inputs", PV::UInt(2)),
        ("num_pips", PV::String("1".into())),
        ("pgm_resolution", PV::String(format!("{}x{}", W, H))),
        ("multiview_resolution", PV::String(format!("{}x{}", W, H))),
        ("initial_pgm_input", PV::UInt(0)),
        ("initial_pvw_input", PV::UInt(1)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    flow.blocks.push(strom_types::BlockInstance {
        id: BLOCK.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for (i, (pattern, pad)) in [("red", "video_in_0"), ("blue", "video_in_1")]
        .iter()
        .enumerate()
    {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![
                ("pattern", PV::String((*pattern).into())),
                ("is-live", PV::Bool(true)),
            ],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String(format!("video/x-raw,width={W},height={H},framerate=30/1")),
            )],
        ));
        flow.links.push(strom_types::Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(strom_types::Link {
            from: format!("caps{i}:src"),
            to: format!("{BLOCK}:{pad}"),
        });
    }
    flow.elements.push(elem(
        "pgmsink",
        "appsink",
        vec![
            ("sync", PV::Bool(false)),
            ("max-buffers", PV::UInt(1)),
            ("drop", PV::Bool(true)),
        ],
    ));
    for block in &mut flow.blocks {
        if let Some(builder) = strom::blocks::builtin::get_builder(&block.block_definition_id) {
            block.computed_external_pads = builder.get_external_pads(&block.properties);
        }
    }
    flow.links.push(strom_types::Link {
        from: format!("{BLOCK}:pgm_out"),
        to: "pgmsink:sink".to_string(),
    });
    flow
}

/// A cut to a PiP and from a PiP to an input whose picture sits on a
/// higher pad than the old one: the new picture is on before the old one
/// goes off, so no frame between two writes is black.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cut_to_and_from_a_pip_shows_no_black() {
    gstreamer::init().unwrap();
    strom::gpu::detect_gpu_capabilities();
    let flow = build_flow();
    let mut manager = common::manager::build(&flow).expect("vision mixer pipeline builds");
    manager.start().expect("start pipeline");
    let mixer = manager
        .pipeline()
        .by_name(&format!("{BLOCK}:mixer"))
        .expect("mixer");
    let red = mixer.static_pad("sink_0").expect("red pad");
    let blue = mixer.static_pad("sink_1").expect("blue pad");

    // A cut and the swap after it, as the state layer does; each alpha
    // write logs (red, blue), and some pad must be on in every one.
    let cut = |what: &str| {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ids = Vec::new();
        for pad in [&red, &blue] {
            let (log, a, b) = (seen.clone(), red.downgrade(), blue.downgrade());
            ids.push(pad.connect_notify(Some("alpha"), move |_, _| {
                if let (Some(a), Some(b)) = (a.upgrade(), b.upgrade()) {
                    log.lock()
                        .unwrap()
                        .push((a.property::<f64>("alpha"), b.property::<f64>("alpha")));
                }
            }));
        }
        let (_, old_pgm, new_pgm, _) = manager
            .trigger_transition(BLOCK, 0, 0, "cut", 0)
            .expect("cut");
        for (pad, id) in [&red, &blue].into_iter().zip(ids) {
            pad.disconnect(id);
        }
        manager
            .update_vision_mixer_after_take(BLOCK, new_pgm, old_pgm, 2)
            .expect("swap");
        let states = seen.lock().unwrap().clone();
        assert!(
            states.iter().all(|&(r, b)| r > 0.0 || b > 0.0),
            "{what}: both pads off at once: {states:?}"
        );
    };
    let pip = |bg: usize| {
        manager
            .apply_vision_mixer_pip_config(
                BLOCK,
                0,
                Some(bg),
                Vec::new(),
                strom_types::vision_mixer::PipTransforms::new(),
            )
            .expect("PiP config");
    };

    // Red on program, a PiP of blue alone on preview.
    pip(1);
    manager
        .select_vision_mixer_pip_for_preview(BLOCK, 0)
        .expect("PiP to preview");
    cut("red to a PiP of blue");
    assert_eq!(blue.property::<f64>("alpha"), 1.0);
    cut("back to red");

    // A PiP of red alone on program, blue on preview.
    pip(0);
    cut("red to a PiP of red");
    manager
        .select_vision_mixer_preview(BLOCK, 1, 2)
        .expect("blue to preview");
    cut("a PiP of red to blue");
    assert_eq!(blue.property::<f64>("alpha"), 1.0);
    manager.stop().expect("stop");
}
