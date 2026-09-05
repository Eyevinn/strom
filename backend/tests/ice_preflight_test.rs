//! A flow that needs WebRTC ICE must refuse to start when the libnice
//! elements are absent — not abort the server process.
//!
//! Without `nicesrc` / `nicesink`, upstream `webrtcsrc` panics inside a
//! function that cannot unwind and the process dies with SIGABRT, taking every
//! other running flow with it. Strom cannot catch that, so the only defence is
//! to never build the block. This test removes the ICE elements from the
//! GStreamer registry and asserts each WebRTC block builder refuses.
//!
//! It is meaningful in both environments: on a developer machine the elements
//! are present and get removed here; in CI they are absent to begin with and
//! the removal is a no-op. Either way the builders must refuse.

use gstreamer as gst;
use std::collections::HashMap;
use strom::blocks::{builtin, BlockBuildContext};

const WEBRTC_BLOCKS: [&str; 4] = [
    "builtin.whip_input",
    "builtin.whip_output",
    "builtin.whep_input",
    "builtin.whep_output",
];

/// Remove the ICE elements from this process's registry, so the rest of the
/// test runs as if the libnice plugin were never installed.
fn remove_ice_elements() {
    let registry = gst::Registry::get();
    for name in ["nicesrc", "nicesink"] {
        if let Some(factory) = gst::ElementFactory::find(name) {
            registry.remove_feature(&factory);
        }
    }
    assert!(
        gst::ElementFactory::find("nicesrc").is_none(),
        "nicesrc should be gone from the registry"
    );
    assert!(
        gst::ElementFactory::find("nicesink").is_none(),
        "nicesink should be gone from the registry"
    );
}

#[test]
fn webrtc_blocks_refuse_to_build_without_ice_elements() {
    gst::init().expect("gstreamer init");
    remove_ice_elements();

    let ctx = BlockBuildContext::new(vec![], "all".to_string());

    for definition_id in WEBRTC_BLOCKS {
        let builder = builtin::get_builder(definition_id)
            .unwrap_or_else(|| panic!("no builder for {}", definition_id));

        let properties: HashMap<String, strom_types::PropertyValue> = HashMap::new();
        let result = builder.build("test_block", &properties, &ctx);

        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!(
                "{} built a pipeline without ICE elements — a live session would abort the process",
                definition_id
            ),
        };

        // The message has to be actionable: what is missing, and what to
        // install. An operator reads this in a failed flow start, not a log.
        assert!(
            err.contains("nicesrc") && err.contains("nicesink"),
            "{}: message should name the missing elements, got: {}",
            definition_id,
            err
        );
        assert!(
            err.contains(strom::gst::ice_preflight::ice_package_hint()),
            "{}: message should name the package to install, got: {}",
            definition_id,
            err
        );
    }
}

#[test]
fn the_probe_reports_both_elements_missing() {
    gst::init().expect("gstreamer init");
    remove_ice_elements();

    let missing = strom::gst::ice_preflight::missing_ice_elements();
    assert_eq!(missing.len(), 2, "got {:?}", missing);
    assert!(strom::gst::ice_preflight::require_ice_elements("WHIP Input").is_err());
}
