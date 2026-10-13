//! A mixer that cannot configure its latency posts the same warning on every
//! aggregation cycle, naming only itself. The flow's bus handler has to pass it
//! on once, with the inputs that cause it.
//!
//! The flow: one input 2 s late (an RTP jitterbuffer at 2000 ms, as an SRT
//! input at `latency: 2000` is), and one from a live appsrc at its defaults,
//! which reports that it can hold nothing. Behind the default queue that is
//! 1 s, less than the 2 s the mixer has to wait for the first input.

use gstreamer as gst;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom_types::{Flow, PropertyValue as PV, StromEvent};
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

fn chain(flow: &mut Flow, ids: &[&str]) {
    for pair in ids.windows(2) {
        flow.links.push(strom_types::Link {
            from: format!("{}:src", pair[0]),
            to: if pair[1] == "mix" {
                "mix:sink_%u".to_string()
            } else {
                format!("{}:sink", pair[1])
            },
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mixer_latency_failure_is_reported_once_with_its_inputs() {
    gst::init().unwrap();

    let mut flow = Flow::new("latency_diagnosis");
    flow.elements.push(elem(
        "slow",
        "audiotestsrc",
        vec![("is-live", PV::Bool(true))],
    ));
    flow.elements
        .push(elem("slow_conv", "audioconvert", vec![]));
    flow.elements.push(elem("slow_pay", "rtpL16pay", vec![]));
    flow.elements.push(elem(
        "slow_jb",
        "rtpjitterbuffer",
        vec![("latency", PV::UInt(2000))],
    ));
    flow.elements
        .push(elem("slow_depay", "rtpL16depay", vec![]));
    flow.elements
        .push(elem("slow_conv2", "audioconvert", vec![]));
    flow.elements.push(elem("slow_queue", "queue", vec![]));
    flow.elements.push(elem(
        "bridge",
        "appsrc",
        vec![
            ("is-live", PV::Bool(true)),
            ("format", PV::String("time".into())),
            (
                "caps",
                PV::String(
                    "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved".into(),
                ),
            ),
        ],
    ));
    flow.elements.push(elem("bridge_queue", "queue", vec![]));
    flow.elements.push(elem("mix", "audiomixer", vec![]));
    flow.elements
        .push(elem("sink", "fakesink", vec![("sync", PV::Bool(true))]));
    chain(
        &mut flow,
        &[
            "slow",
            "slow_conv",
            "slow_pay",
            "slow_jb",
            "slow_depay",
            "slow_conv2",
            "slow_queue",
            "mix",
        ],
    );
    chain(&mut flow, &["bridge", "bridge_queue", "mix"]);
    chain(&mut flow, &["mix", "sink"]);

    // The bus watch is a glib signal watch, so it only dispatches while a
    // main loop runs. The real app has one; a test does not.
    let main_loop = gst::glib::MainLoop::new(None, false);
    let main_loop_thread = {
        let ml = main_loop.clone();
        std::thread::spawn(move || ml.run())
    };

    let events = EventBroadcaster::with_capacity(4096);
    let mut event_rx = events.subscribe();
    let registry_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let mut manager = PipelineManager::new(
        &flow,
        events,
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("build pipeline");
    manager.start().expect("start pipeline");

    // The mixer warns on every cycle, hundreds of times per second. Within
    // a few seconds, well inside the gate's interval, one warning gets out.
    let mut latency_warnings = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match event_rx.recv().await {
                Ok(StromEvent::PipelineWarning { warning, .. })
                    if warning.contains("held back") || warning.contains("late") =>
                {
                    latency_warnings.push(warning)
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

    assert_eq!(
        latency_warnings.len(),
        1,
        "expected the latency warning once, got {latency_warnings:?}"
    );
    let warning = &latency_warnings[0];
    assert!(
        warning.contains("'slow' arrives 20")
            && warning.contains("but 'bridge' can only be held back 1000 ms"),
        "the warning does not name both inputs: {warning}"
    );
}
