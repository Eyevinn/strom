//! Unit tests for the vision mixer block.

use super::layout;
use super::properties;
use std::collections::HashMap;
use strom_types::PropertyValue;

#[test]
fn test_parse_num_inputs_default() {
    let props = HashMap::new();
    assert_eq!(properties::parse_num_inputs(&props), 4);
}

#[test]
fn test_parse_num_inputs_valid() {
    let mut props = HashMap::new();
    props.insert(
        "num_inputs".to_string(),
        PropertyValue::String("8".to_string()),
    );
    assert_eq!(properties::parse_num_inputs(&props), 8);
}

#[test]
fn test_parse_num_inputs_clamped() {
    let mut props = HashMap::new();
    props.insert(
        "num_inputs".to_string(),
        PropertyValue::String("20".to_string()),
    );
    assert_eq!(properties::parse_num_inputs(&props), 10); // MAX
}

#[test]
fn test_parse_num_inputs_clamped_min() {
    let mut props = HashMap::new();
    props.insert(
        "num_inputs".to_string(),
        PropertyValue::String("1".to_string()),
    );
    assert_eq!(properties::parse_num_inputs(&props), 2); // MIN
}

#[test]
fn test_parse_input_labels_defaults() {
    let props = HashMap::new();
    let labels = properties::parse_input_labels(&props, 4);
    assert_eq!(labels, vec!["In 1", "In 2", "In 3", "In 4"]);
}

#[test]
fn test_parse_input_labels_custom() {
    let mut props = HashMap::new();
    props.insert(
        "input_0_label".to_string(),
        PropertyValue::String("Camera 1".to_string()),
    );
    props.insert(
        "input_2_label".to_string(),
        PropertyValue::String("Graphics".to_string()),
    );
    let labels = properties::parse_input_labels(&props, 4);
    assert_eq!(labels[0], "Camera 1");
    assert_eq!(labels[1], "In 2"); // default
    assert_eq!(labels[2], "Graphics");
    assert_eq!(labels[3], "In 4"); // default
}

#[test]
fn test_layout_compute_basic() {
    let l = layout::compute_layout(1920, 1080, 4);
    assert_eq!(l.num_inputs, 4);
    assert_eq!(l.thumbnail_rects.len(), 4);
    assert_eq!(l.label_positions.len(), 4);
    // PVW is left, PGM is right
    assert!(l.pvw_rect.x < l.pgm_rect.x);
    // Both on same row
    assert_eq!(l.pvw_rect.y as i32, l.pgm_rect.y as i32);
}

#[test]
fn test_layout_compute_10_inputs() {
    let l = layout::compute_layout(1920, 1080, 10);
    assert_eq!(l.thumbnail_rects.len(), 10);
    // First 5 in row 1, next 5 in row 2
    let row1_y = l.thumbnail_rects[0].y;
    let row2_y = l.thumbnail_rects[5].y;
    assert!(row2_y > row1_y, "Row 2 should be below row 1");
    // All in row 1 same y
    for i in 0..5 {
        assert_eq!(l.thumbnail_rects[i].y as i32, row1_y as i32);
    }
    // All in row 2 same y
    for i in 5..10 {
        assert_eq!(l.thumbnail_rects[i].y as i32, row2_y as i32);
    }
}

#[test]
fn test_parse_initial_pgm_pvw() {
    let mut props = HashMap::new();
    props.insert("initial_pgm_input".to_string(), PropertyValue::UInt(3));
    props.insert("initial_pvw_input".to_string(), PropertyValue::UInt(1));
    assert_eq!(properties::parse_initial_pgm(&props, 4), 3);
    assert_eq!(properties::parse_initial_pvw(&props, 4), 1);
}

#[test]
fn test_parse_initial_pgm_clamped() {
    let mut props = HashMap::new();
    props.insert("initial_pgm_input".to_string(), PropertyValue::UInt(99));
    assert_eq!(properties::parse_initial_pgm(&props, 4), 3); // max index = 3
}

/// `overlay_states` and `overlay_renderers` share the same lifecycle:
/// they are populated together in `build_overlay` and must be cleared
/// together by the cleanup branch in `state.rs::stop_flow`. If you add
/// another per-block registry in this module, mirror its unregister call
/// in that branch and extend this test.
#[test]
fn overlay_registries_round_trip() {
    use super::overlay::{
        get_overlay_renderer, get_overlay_state, register_overlay_renderer, register_overlay_state,
        unregister_overlay_renderer, unregister_overlay_state, OverlayRenderer,
        VisionMixerOverlayState,
    };
    use gstreamer as gst;
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Mutex};

    gst::init().unwrap();

    let block_id = "test-vm-overlay-cleanup-block-id";

    let lo = layout::compute_layout(1280, 720, 4);
    let state = Arc::new(VisionMixerOverlayState::new(
        4,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into(), "D".into()],
        lo,
        false,
    ));

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", 1280i32)
        .field("height", 720i32)
        .field("framerate", gst::Fraction::new(50, 1))
        .build();
    let appsrc = gst_app::AppSrc::builder().caps(&caps).build();

    let renderer = Arc::new(Mutex::new(OverlayRenderer::new(
        appsrc,
        caps,
        Arc::clone(&state),
        1280,
        720,
    )));

    register_overlay_state(block_id, Arc::clone(&state));
    register_overlay_renderer(block_id, Arc::clone(&renderer));

    assert!(
        get_overlay_state(block_id).is_some(),
        "state should be registered"
    );
    assert!(
        get_overlay_renderer(block_id).is_some(),
        "renderer should be registered"
    );

    unregister_overlay_state(block_id);
    unregister_overlay_renderer(block_id);

    assert!(
        get_overlay_state(block_id).is_none(),
        "state must be cleaned (otherwise API still sees stale block)"
    );
    assert!(
        get_overlay_renderer(block_id).is_none(),
        "renderer must be cleaned (otherwise overlay-timer-* thread leaks)"
    );
}
