//! Regression test: dropping the per-buffer upstream QoS events at a sink
//! must not log GStreamer criticals.
//!
//! Every qos-enabled sink in a flow gets a probe that throws its upstream QoS
//! events away (see `drop_upstream_qos_events`). Returned as
//! `PadProbeReturn::Drop`, GStreamer before 1.26 logged `gst_mini_object_unref:
//! assertion 'mini_object != NULL' failed` once per buffer at every such sink:
//! gstreamer-rs frees the event itself, and the core then unrefs the NULL it
//! left behind. GStreamer 1.26 checks for NULL there, so this test can only
//! fail on an older GStreamer (the Linux CI runner has 1.24).

use gstreamer as gst;
use gstreamer::glib;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_sink_qos_events_logs_no_criticals() {
    gst::init().unwrap();

    // Count GStreamer criticals from here on.
    let criticals = Arc::new(AtomicU64::new(0));
    let handler = {
        let criticals = Arc::clone(&criticals);
        glib::log_set_handler(
            Some("GStreamer"),
            glib::LogLevels::LEVEL_CRITICAL,
            false,
            false,
            move |_domain, _level, message| {
                eprintln!("GStreamer critical: {message}");
                criticals.fetch_add(1, Ordering::Relaxed);
            },
        )
    };

    // A live source into a qos-enabled sink that renders in sync, so it sends
    // an upstream QoS event for every buffer.
    let mut flow = Flow::new("qos_probe_criticals");
    flow.elements.push(elem(
        "src",
        "videotestsrc",
        vec![("is-live", PV::Bool(true))],
    ));
    flow.elements.push(elem(
        "caps",
        "capsfilter",
        vec![(
            "caps",
            PV::String("video/x-raw,width=64,height=64,framerate=30/1".into()),
        )],
    ));
    flow.elements
        .push(elem("sink", "fakesink", vec![("sync", PV::Bool(true))]));
    flow.links.push(strom_types::Link {
        from: "src:src".into(),
        to: "caps:sink".into(),
    });
    flow.links.push(strom_types::Link {
        from: "caps:src".into(),
        to: "sink:sink".into(),
    });

    let registry_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(registry_file.path());
    let mut manager = PipelineManager::new(
        &flow,
        EventBroadcaster::with_capacity(10),
        &registry,
        vec![],
        "all".to_string(),
        None,
        std::env::temp_dir(),
        Arc::new(std::sync::Mutex::new(HashMap::new())),
    )
    .expect("build pipeline");
    manager.start().expect("start pipeline");
    std::thread::sleep(std::time::Duration::from_secs(1));
    manager.stop().expect("stop");
    drop(manager);

    glib::log_remove_handler(Some("GStreamer"), handler);
    assert_eq!(
        criticals.load(Ordering::Relaxed),
        0,
        "GStreamer logged criticals while sinks dropped their QoS events"
    );
}
