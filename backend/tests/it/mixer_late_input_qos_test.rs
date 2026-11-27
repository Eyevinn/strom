//! A late input to the audio mixer block is dropped by its `audiomixer`. The
//! flow has to say so, naming the block and the input (#1265).
//!
//! `audiomixer` posts a QoS message for a dropped input only when the input
//! pad's `qos-messages` is set (default false), and the pad, not the element,
//! is the message's source. Before the fix the mixer dropped a late input for
//! a whole connection and nothing was logged or shown.
//!
//! The flow: two live tones into the real `builtin.mixer` block. The second
//! passes an `identity` that sleeps 40 ms per ~23 ms buffer, so it falls
//! further behind with every buffer and reaches the mixer past its deadline.

use crate::common;

use gstreamer as gst;
use std::collections::HashMap;
use std::time::Duration;
use strom::events::EventBroadcaster;
use strom_types::{BlockInstance, Element, Flow, Link, PropertyValue as PV, StromEvent};

fn element(id: &str, ty: &str, props: &[(&str, PV)]) -> Element {
    Element {
        id: id.to_string(),
        element_type: ty.to_string(),
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        position: [0.0, 0.0].into(),
        pad_properties: HashMap::new(),
    }
}

fn link(flow: &mut Flow, from: &str, to: &str) {
    flow.links.push(Link {
        from: from.to_string(),
        to: to.to_string(),
    });
}

fn late_input_flow() -> Flow {
    let mut flow = Flow::new("mixer-late-input-qos");
    flow.elements.push(element(
        "on_time",
        "audiotestsrc",
        &[("is-live", PV::Bool(true))],
    ));
    flow.elements.push(element(
        "late",
        "audiotestsrc",
        &[("is-live", PV::Bool(true))],
    ));
    flow.elements.push(element(
        "slow",
        "identity",
        &[("sleep-time", PV::UInt(40_000))],
    ));
    flow.elements
        .push(element("out", "fakesink", &[("sync", PV::Bool(true))]));
    let mut props = HashMap::new();
    props.insert("num_channels".to_string(), PV::UInt(2));
    flow.blocks.push(BlockInstance {
        id: "mixer".to_string(),
        block_definition_id: "builtin.mixer".to_string(),
        name: None,
        properties: props,
        position: strom_types::block::Position { x: 0.0, y: 0.0 },
        runtime_data: None,
        computed_external_pads: None,
    });
    link(&mut flow, "on_time:src", "mixer:input_1");
    link(&mut flow, "late:src", "slow:sink");
    link(&mut flow, "slow:src", "mixer:input_2");
    link(&mut flow, "mixer:main_out", "out:sink");
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_mixer_input_is_reported_with_its_block_and_pad() {
    gst::init().unwrap();
    common::init_static_plugins();
    common::require_elements(&["audiotestsrc", "identity", "audiomixer", "fakesink"]);

    // The bus watch is a glib signal watch, so it only dispatches while a
    // main loop runs. The real app has one; a test does not.
    let main_loop = gst::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };

    let events = EventBroadcaster::with_capacity(4096);
    let mut event_rx = events.subscribe();
    let mut manager = common::manager::build_with(&late_input_flow(), events, std::env::temp_dir())
        .expect("build pipeline");
    manager.start().expect("start pipeline");

    let mut reports = Vec::new();
    let mut errors = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            match event_rx.recv().await {
                Ok(StromEvent::QoSStats {
                    block_id,
                    element_name,
                    pad_name,
                    upstream_element,
                    total_dropped,
                    dropped_in_period,
                    is_falling_behind,
                    ..
                }) => {
                    let found = block_id.as_deref() == Some("mixer")
                        && element_name == "mixer:audiomixer"
                        && pad_name.is_some()
                        && is_falling_behind;
                    reports.push((
                        block_id,
                        element_name,
                        pad_name,
                        upstream_element,
                        total_dropped,
                        dropped_in_period,
                        is_falling_behind,
                    ));
                    if found {
                        break;
                    }
                }
                Ok(StromEvent::PipelineError { error, source, .. }) => {
                    errors.push(format!("{error} (from {source:?})"))
                }
                Ok(_) => continue,
                Err(e) => panic!("event stream broke: {e}"),
            }
        }
    })
    .await;

    manager.stop().expect("stop");
    drop(manager);
    main_loop.quit();
    main_loop_thread.join().unwrap();

    assert!(errors.is_empty(), "the flow failed: {errors:?}");
    let report = reports
        .iter()
        .find(|r| {
            r.0.as_deref() == Some("mixer") && r.1 == "mixer:audiomixer" && r.2.is_some() && r.6
        })
        .unwrap_or_else(|| {
            panic!(
                "no QoS drop reported for an input pad of the mixer block's audiomixer; \
                 QoS reports: {reports:?}"
            )
        });
    // The late input is channel 2 (index 1), fed through its to-main queue.
    assert_eq!(
        report.3.as_deref(),
        Some("mixer:to_main_queue_1"),
        "the report does not name the late input: {report:?}"
    );
    // The dropped count, not the processed count, carries the drop.
    assert!(
        report.4.is_some_and(|d| d > 0) && report.5 > 0,
        "the report carries no dropped count: {report:?}"
    );
}
