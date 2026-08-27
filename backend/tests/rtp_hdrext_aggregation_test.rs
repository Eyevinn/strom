//! Regression test: every RTP depayloader in a started pipeline must have
//! header-extension aggregation disabled.
//!
//! Leaving it on lets an interrupted H264 fragmentation unit trip a `g_assert`
//! in `GstRTPBaseDepayload` (gstreamer#5057, `gstrtpbasedepayload.c:942`) that
//! aborts the whole process, killing every unrelated flow on the server. The
//! switch is `Since: 1.24` and has no GObject property, so `gst::rtp_hdrext`
//! resolves the C setter at runtime and installs a `deep-element-added` handler
//! from `PipelineManager::start()`.
//!
//! The unit tests in `gst::rtp_hdrext` cover `install()` directly. This one
//! covers the wiring: it fails if the `rtp_hdrext::install()` call is removed
//! from `start()`, which the unit tests cannot see.
//!
//! `rtph264depay` ships in `gstreamer1.0-plugins-good`, installed in every
//! Linux CI job, so this runs rather than skipping.

use gstreamer::prelude::*;
use std::collections::HashMap;
use strom::blocks::BlockRegistry;
use strom::events::EventBroadcaster;
use strom::gst::pipeline::PipelineManager;
use strom::gst::rtp_hdrext;
use strom_types::{Flow, Link};
use tempfile::NamedTempFile;

/// `fakesrc → rtph264depay → fakesink`.
///
/// A user can place `rtph264depay` directly in a flow — block construction
/// accepts any element type name — so this is a real topology, not a contrivance.
/// The pipeline is not expected to run: nothing feeds the depayloader valid RTP.
/// That does not matter, because `install()` runs at the top of `start()`,
/// before any state change.
fn build_depayloader_flow(name: &str) -> Flow {
    let mut flow = Flow::new(name);

    for (id, element_type, x) in [
        ("src", "fakesrc", 100.0),
        ("depay", "rtph264depay", 250.0),
        ("sink", "fakesink", 400.0),
    ] {
        flow.elements.push(strom_types::Element {
            id: id.to_string(),
            element_type: element_type.to_string(),
            properties: HashMap::new(),
            position: [x, 200.0].into(),
            pad_properties: HashMap::new(),
        });
    }

    flow.links.push(Link {
        from: "src:src".to_string(),
        to: "depay:sink".to_string(),
    });
    flow.links.push(Link {
        from: "depay:src".to_string(),
        to: "sink:sink".to_string(),
    });

    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_start_disables_hdrext_aggregation_on_depayloaders() {
    gstreamer::init().unwrap();

    if !rtp_hdrext::is_supported() {
        // GStreamer < 1.24: aggregation does not exist, so neither does the bug.
        return;
    }

    let temp_file = NamedTempFile::new().unwrap();
    let registry = BlockRegistry::new(temp_file.path());
    let events = EventBroadcaster::new(10);

    let flow = build_depayloader_flow("hdrext_aggregation_test");

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
    .expect("Failed to create PipelineManager");

    // start() may fail — nothing feeds the depayloader real RTP — but
    // install() runs before any state change, which is what we are asserting.
    let _ = manager.start();

    let mut checked = 0;
    for element in manager.pipeline().iterate_recurse().into_iter().flatten() {
        if let Ok(depay) = element.downcast::<gstreamer_rtp::RTPBaseDepayload>() {
            assert_eq!(
                rtp_hdrext::is_enabled(&depay),
                Some(false),
                "depayloader {} still has RTP header extension aggregation enabled \
                 after start() — an interrupted H264 fragmentation unit will abort \
                 the whole process (gstreamer#5057)",
                depay.name()
            );
            checked += 1;
        }
    }

    assert_eq!(
        checked, 1,
        "expected exactly one depayloader in the pipeline, found {checked} — \
         the test topology no longer exercises what it claims to"
    );

    let _ = manager.stop();
}
