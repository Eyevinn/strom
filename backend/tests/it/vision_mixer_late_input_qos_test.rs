//! Late input frames at the vision mixer must be reported as QoS (#1265).
//!
//! A live video aggregator drops an input buffer that ends before its next
//! output frame with only a debug line, so an input that reaches the mixer
//! later than its latency window disappears from program without a signal.
//! The block counts those drops and posts a QoS message per input, with the
//! mixer's sink pad as the source.
//!
//! The tests build the real block with two live inputs, one of them a second
//! late. The CPU (software compositor) variant runs wherever CI does; the GPU
//! (`glvideomixerelement`) variant skips where no GL context can be made,
//! unless `STROM_REQUIRE_GL` is set (see `common::gl_available`).

use crate::common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use strom_types::PropertyValue as PV;
use strom_types::{Flow, Link};

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

/// Two live 30 fps inputs into a vision mixer on `backend` ("cpu" or
/// "gpu"), both outputs into sinks.
fn build_flow(block: &str, backend: &str) -> Flow {
    let mut flow = Flow::new(format!("vm_late_input_qos_{backend}"));
    flow.blocks.push(strom_types::BlockInstance {
        id: block.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: HashMap::from([
            (
                "compositor_preference".to_string(),
                PV::String(backend.into()),
            ),
            ("num_inputs".to_string(), PV::UInt(2)),
            ("pgm_resolution".to_string(), PV::String("320x180".into())),
            (
                "multiview_resolution".to_string(),
                PV::String("320x180".into()),
            ),
        ]),
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    for i in 0..2 {
        flow.elements.push(elem(
            &format!("src{i}"),
            "videotestsrc",
            vec![("is-live", PV::Bool(true))],
        ));
        flow.elements.push(elem(
            &format!("caps{i}"),
            "capsfilter",
            vec![(
                "caps",
                PV::String("video/x-raw,width=320,height=180,framerate=30/1".into()),
            )],
        ));
        flow.links.push(Link {
            from: format!("src{i}:src"),
            to: format!("caps{i}:sink"),
        });
        flow.links.push(Link {
            from: format!("caps{i}:src"),
            to: format!("{block}:video_in_{i}"),
        });
    }
    for (out, sink) in [("pgm_out", "pgmsink"), ("multiview_out", "mvsink")] {
        flow.elements
            .push(elem(sink, "fakesink", vec![("sync", PV::Bool(false))]));
        flow.links.push(Link {
            from: format!("{block}:{out}"),
            to: format!("{sink}:sink"),
        });
    }
    flow
}

/// One QoS message whose source is a pad: (element, pad, processed, dropped).
type PadQos = (String, String, u64, u64);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_input_posts_qos_for_its_mixer_pad_only_cpu() {
    gst::init().unwrap();
    late_input_posts_qos_for_its_mixer_pad_only("vm_late_cpu", "cpu");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn late_input_posts_qos_for_its_mixer_pad_only_gpu() {
    gst::init().unwrap();
    if !common::gl_available(&["glvideomixerelement"]) {
        return;
    }
    late_input_posts_qos_for_its_mixer_pad_only("vm_late_gpu", "gpu");
}

fn late_input_posts_qos_for_its_mixer_pad_only(block: &str, backend: &str) {
    strom::gpu::detect_gpu_capabilities();
    let mut manager = common::manager::build(&build_flow(block, backend))
        .unwrap_or_else(|e| panic!("{backend} vision mixer pipeline should build: {e}"));
    let pipeline = manager.pipeline().clone();

    // Input 1 runs one second behind: every buffer is stamped 1 s earlier
    // than it was captured, far beyond the mixer's latency window, so the
    // mixer drops each of its frames as late. Input 0 is on time. (A negative
    // pad offset would do the same, but aborts the aggregator on 1.28: see
    // docs/UPSTREAM_NOTES.md.)
    const LATE: gst::ClockTime = gst::ClockTime::from_seconds(1);
    pipeline
        .by_name("caps1")
        .expect("caps1 in pipeline")
        .static_pad("src")
        .expect("caps1 src pad")
        .add_probe(gst::PadProbeType::BUFFER, |_pad, info| {
            let Some(buffer) = info.buffer_mut() else {
                return gst::PadProbeReturn::Ok;
            };
            match buffer.pts().and_then(|pts| pts.checked_sub(LATE)) {
                Some(pts) => {
                    buffer.make_mut().set_pts(pts);
                    gst::PadProbeReturn::Ok
                }
                // The first second has nowhere earlier to go.
                None => gst::PadProbeReturn::Drop,
            }
        });

    let seen: Arc<Mutex<Vec<PadQos>>> = Arc::new(Mutex::new(Vec::new()));
    // The flow's thread-priority setup owns the bus's sync handler, so the
    // test listens on the sync-message signal, which fires alongside it.
    let bus = pipeline.bus().expect("pipeline bus");
    bus.enable_sync_message_emission();
    let handler = {
        let seen = Arc::clone(&seen);
        bus.connect_sync_message(None, move |_bus, msg| {
            if let gst::MessageView::Qos(qos) = msg.view() {
                if let Some(pad) = msg.src().and_then(|s| s.downcast_ref::<gst::Pad>()) {
                    let (processed, dropped) = qos.stats();
                    let element = pad
                        .parent()
                        .map(|p| p.name().to_string())
                        .unwrap_or_default();
                    seen.lock().unwrap().push((
                        element,
                        pad.name().to_string(),
                        processed.value() as u64,
                        dropped.value() as u64,
                    ));
                }
            }
        })
    };

    manager
        .start()
        .unwrap_or_else(|e| panic!("start {backend} vision mixer: {e}"));

    let mixer = format!("{block}:mixer");
    let late_seen = |seen: &[PadQos]| {
        seen.iter()
            .any(|(e, p, _, dropped)| *e == mixer && p == "sink_1" && *dropped > 0)
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !late_seen(&seen.lock().unwrap()) {
        std::thread::sleep(Duration::from_millis(100));
    }
    // Keep watching, so a false report for the on-time input has time to show.
    std::thread::sleep(Duration::from_secs(3));

    manager.stop().expect("stop");
    bus.disconnect(handler);
    bus.disable_sync_message_emission();
    drop(manager);

    let seen = seen.lock().unwrap().clone();
    assert!(
        late_seen(&seen),
        "no QoS message with dropped > 0 for the late input's mixer pad {mixer}.sink_1; \
         pad QoS messages seen: {seen:?}"
    );
    let late: Vec<_> = seen
        .iter()
        .filter(|(e, p, _, _)| *e == mixer && p == "sink_1")
        .collect();
    assert!(
        late.iter()
            .all(|(_, _, processed, dropped)| processed >= dropped),
        "dropped exceeds processed: {late:?}"
    );
    let on_time: Vec<_> = seen
        .iter()
        .filter(|(e, p, _, _)| *e == mixer && p == "sink_0")
        .collect();
    assert!(
        on_time.is_empty(),
        "the on-time input's mixer pad reported late drops: {on_time:?}"
    );
}
