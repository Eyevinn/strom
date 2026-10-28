//! The GPU Vision Mixer's PGM compositor must not wait on the multiview to
//! allocate (#1164), and must keep the metas the multiview supports.
//!
//! `tee_pgm` feeds PGM to the outputs and, through `queue_pgm_mv`, to the
//! multiview's PGM tile. An ALLOCATION query on that branch waits until the
//! multiview compositor's streaming thread takes it, and that thread can be
//! held behind a prerolled multiview sink. The queue answers the query itself,
//! with what the multiview compositor would propose.
//!
//! * [`pgm_allocation_does_not_wait_on_a_stalled_multiview`] holds the
//!   multiview compositor's streaming thread, renegotiates PGM, and fails if
//!   PGM stops. This is the start-up deadlock seen on Windows, made
//!   deterministic: there the multiview sink prerolled first, and here a
//!   blocking probe holds the thread instead.
//! * [`the_pgm_tile_answers_allocation_like_the_multiview`] fails if the
//!   queue's answer drops a meta the multiview proposes. An empty answer would
//!   make `tee_pgm` drop the GL sync meta from the PGM compositor's
//!   allocation.
//!
//! Both run one at a time with the other GL tests (`serial(gl)`): GL setup in
//! parallel tests races on the Linux runner (#1095).

pub mod common;

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV};

const GL_ELEMENTS: &[&str] = &["glvideomixerelement", "glupload"];

fn elem(id: &str, ty: &str, props: Vec<(&str, PV)>) -> strom_types::Element {
    strom_types::Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(strom_types::Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

/// A GPU mixer with two live `videotestsrc` inputs, PGM into an appsink and
/// the multiview into a fakesink. Started.
///
/// The appsink answers ALLOCATION, as a real PGM consumer (an encoder, a GL
/// sink) does. A bare appsink refuses it, and `tee_pgm` then gives up at that
/// branch and never asks the multiview, which hides the wait.
fn start_mixer(block_id: &str) -> PipelineManager {
    strom::gpu::detect_gpu_capabilities();
    let mut flow = Flow::new(format!("vm_gl_alloc_{block_id}"));
    flow.blocks.push(strom_types::BlockInstance {
        id: block_id.to_string(),
        block_definition_id: "builtin.vision_mixer".to_string(),
        name: None,
        properties: [
            ("compositor_preference", PV::String("gpu".into())),
            ("num_inputs", PV::UInt(2)),
            ("pgm_resolution", PV::String("320x180".into())),
            ("multiview_resolution", PV::String("320x180".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
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
        link(
            &mut flow,
            &format!("src{i}:src"),
            &format!("{block_id}:video_in_{i}"),
        );
    }
    flow.elements
        .push(elem("pgmsink", "appsink", vec![("sync", PV::Bool(false))]));
    flow.elements
        .push(elem("mvsink", "fakesink", vec![("sync", PV::Bool(false))]));
    link(&mut flow, &format!("{block_id}:pgm_out"), "pgmsink:sink");
    link(
        &mut flow,
        &format!("{block_id}:multiview_out"),
        "mvsink:sink",
    );

    let mut manager = common::manager::build(&flow).expect("build GPU mixer flow");
    manager
        .pipeline()
        .by_name("pgmsink")
        .and_then(|sink| sink.static_pad("sink"))
        .expect("pgmsink sink pad")
        .add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_, info| {
            match info.query().map(|q| q.view()) {
                Some(gst::QueryView::Allocation(_)) => gst::PadProbeReturn::Handled,
                _ => gst::PadProbeReturn::Ok,
            }
        });
    manager.start().expect("start GPU mixer flow");
    manager
}

fn pgm_sink(manager: &PipelineManager) -> gstreamer_app::AppSink {
    manager
        .pipeline()
        .by_name("pgmsink")
        .expect("pgmsink in pipeline")
        .downcast::<gstreamer_app::AppSink>()
        .expect("appsink type")
}

fn pull(sink: &gstreamer_app::AppSink, within: Duration) -> Option<gst::Sample> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Some(s) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            return Some(s);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn pgm_allocation_does_not_wait_on_a_stalled_multiview() {
    gst::init().unwrap();
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let block_id = "vm_gl_alloc_stall";
    let mut manager = start_mixer(block_id);
    let pgm = pgm_sink(&manager);
    pull(&pgm, Duration::from_secs(20)).expect("PGM running before the stall");

    // Hold the multiview compositor's streaming thread on its next push.
    let mv_src = manager
        .pipeline()
        .by_name(&format!("{block_id}:mv_comp"))
        .expect("mv_comp in pipeline")
        .static_pad("src")
        .expect("mv_comp src pad");
    let held = Arc::new(AtomicBool::new(false));
    let probe = {
        let held = Arc::clone(&held);
        mv_src
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                move |_, _| {
                    held.store(true, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                },
            )
            .expect("blocking probe")
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !held.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline, "multiview never held");
        std::thread::sleep(Duration::from_millis(10));
    }

    // Renegotiate the PGM compositor: its next output runs an ALLOCATION
    // query through tee_pgm, including the branch into the held multiview.
    manager
        .pipeline()
        .by_name(&format!("{block_id}:mixer"))
        .expect("mixer in pipeline")
        .static_pad("src")
        .expect("mixer src pad")
        .mark_reconfigure();
    // Drain what was already produced, then PGM must keep coming.
    let mut frames = 0;
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if pull(&pgm, Duration::from_millis(500)).is_some() {
            frames += 1;
        }
    }
    mv_src.remove_probe(probe);
    manager.stop().expect("stop");
    drop(manager);
    assert!(
        frames > 10,
        "PGM stalled while the multiview was held: {frames} frames in 2 s"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(gl)]
async fn the_pgm_tile_answers_allocation_like_the_multiview() {
    gst::init().unwrap();
    if !common::gl_available(GL_ELEMENTS) {
        return;
    }
    let block_id = "vm_gl_alloc_answer";
    let mut manager = start_mixer(block_id);
    pull(&pgm_sink(&manager), Duration::from_secs(20)).expect("PGM running");

    let pipeline = manager.pipeline();
    let queue_sink = pipeline
        .by_name(&format!("{block_id}:queue_pgm_mv"))
        .expect("queue_pgm_mv in pipeline")
        .static_pad("sink")
        .expect("queue sink pad");
    let tile = queue_sink
        .parent_element()
        .and_then(|q| q.static_pad("src"))
        .and_then(|p| p.peer())
        .expect("the multiview's PGM tile pad");
    let caps = tile.current_caps().expect("PGM tile negotiated");

    let multiview = common::allocation_proposal(&tile, &caps);
    let answer = common::allocation_proposal(&queue_sink, &caps);
    manager.stop().expect("stop");
    drop(manager);

    assert!(
        multiview.metas.iter().any(|api| api == "GstGLSyncMetaAPI"),
        "the multiview compositor no longer proposes GL sync meta ({multiview:?}); \
         update answer_allocation_query to what it proposes now"
    );
    assert_eq!(
        answer, multiview,
        "queue_pgm_mv answers ALLOCATION differently from the multiview's PGM tile; \
         tee_pgm keeps only metas every branch reports, so a missing meta here is \
         lost to the PGM compositor"
    );
}
