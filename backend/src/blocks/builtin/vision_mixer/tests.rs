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
    let l = layout::compute_layout(1920, 1080, 4, 0);
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
    let l = layout::compute_layout(1920, 1080, 10, 0);
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
fn test_layout_compute_with_pip_tile() {
    // 4 inputs + 1 PiP tile = 5 thumbnail slots in row 1, all on the same y.
    let l = layout::compute_layout(1920, 1080, 4, 1);
    assert_eq!(l.num_inputs, 4);
    assert_eq!(l.num_pips, 1);
    assert_eq!(l.thumbnail_rects.len(), 4);
    assert_eq!(l.pip_tile_rects.len(), 1);
    assert_eq!(l.pip_label_positions.len(), 1);

    // The PiP tile takes the slot directly after the last input thumbnail.
    let last_thumb = l.thumbnail_rects.last().unwrap();
    let pip = &l.pip_tile_rects[0];
    assert_eq!(
        pip.y as i32, last_thumb.y as i32,
        "PiP tile shares row with inputs"
    );
    assert!(
        pip.x > last_thumb.x,
        "PiP tile sits to the right of last input"
    );

    // Bg pad position fills the whole tile.
    let (bx, by, bw, bh) = layout::pip_bg_pad_position(&l, 0);
    assert_eq!(bx, pip.x as i32);
    assert_eq!(by, pip.y as i32);
    assert_eq!(bw, pip.w as i32);
    assert_eq!(bh, pip.h as i32);
}

#[test]
fn test_pip_overlay_rects_tile_within_bg() {
    let l = layout::compute_layout(1920, 1080, 4, 1);
    let (bx, by, bw, bh) = layout::pip_bg_pad_position(&l, 0);

    // Two overlays → side-by-side aspect-preserving cells within the PiP tile.
    let rects = layout::pip_overlay_pad_positions(&l, 0, 2, 16.0 / 9.0);
    assert_eq!(rects.len(), 2);
    for (rx, ry, rw, rh) in &rects {
        assert!(*rx >= bx && *ry >= by);
        assert!(*rx + *rw <= bx + bw);
        assert!(*ry + *rh <= by + bh);
    }
    // Side-by-side: second rect to the right of the first, same y.
    assert!(rects[1].0 > rects[0].0);
    assert_eq!(rects[1].1, rects[0].1);
    // Each cell preserves 16:9.
    let (_, _, w, h) = rects[0];
    assert!((w as f64 / h as f64 - 16.0 / 9.0).abs() < 0.05);
}

#[test]
fn test_parse_num_pips_clamped() {
    let mut props = HashMap::new();
    props.insert(
        "num_pips".to_string(),
        PropertyValue::String("99".to_string()),
    );
    assert_eq!(
        properties::parse_num_pips(&props),
        strom_types::vision_mixer::MAX_NUM_PIPS
    );
}

#[test]
fn test_parse_pip_overlays_basic() {
    let mut props = HashMap::new();
    props.insert(
        "pip_0_overlays".to_string(),
        PropertyValue::String(" 1, 2 , 3 ".to_string()),
    );
    assert_eq!(
        properties::parse_pip_overlays(&props, 0, 4, None),
        vec![1, 2, 3]
    );
}

#[test]
fn test_parse_pip_overlays_clamped_deduped_truncated() {
    let mut props = HashMap::new();
    // 99 → clamped to 3 (num_inputs=4); duplicate 1 dropped; length capped at MAX_PIP_OVERLAYS=4.
    props.insert(
        "pip_0_overlays".to_string(),
        PropertyValue::String("1,99,2,1,3,0,9".to_string()),
    );
    let v = properties::parse_pip_overlays(&props, 0, 4, None);
    assert_eq!(v, vec![1, 3, 2, 0]);
}

#[test]
fn test_parse_pip_overlays_empty() {
    let props = HashMap::new();
    assert!(properties::parse_pip_overlays(&props, 0, 4, None).is_empty());
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
fn test_parse_initial_source_falls_back_to_legacy_uint() {
    use strom_types::vision_mixer::Source;
    let mut props = HashMap::new();
    props.insert("initial_pgm_input".to_string(), PropertyValue::UInt(2));
    assert_eq!(
        properties::parse_initial_pgm_source(&props, 4, 0),
        Source::Input(2),
        "missing initial_pgm_source falls back to UInt initial_pgm_input"
    );
}

#[test]
fn test_parse_initial_source_string_input() {
    use strom_types::vision_mixer::Source;
    let mut props = HashMap::new();
    props.insert(
        "initial_pgm_source".to_string(),
        PropertyValue::String("input:1".to_string()),
    );
    assert_eq!(
        properties::parse_initial_pgm_source(&props, 4, 1),
        Source::Input(1)
    );
}

#[test]
fn test_parse_initial_source_string_pip() {
    use strom_types::vision_mixer::Source;
    let mut props = HashMap::new();
    props.insert(
        "initial_pgm_source".to_string(),
        PropertyValue::String("pip:0".to_string()),
    );
    assert_eq!(
        properties::parse_initial_pgm_source(&props, 4, 1),
        Source::Pip(0)
    );
}

#[test]
fn test_parse_initial_source_pip_index_out_of_range_falls_back() {
    use strom_types::vision_mixer::Source;
    let mut props = HashMap::new();
    props.insert(
        "initial_pgm_source".to_string(),
        PropertyValue::String("pip:5".to_string()),
    );
    // Only 1 PiP exists → pip:5 is invalid → fall back to first input.
    assert_eq!(
        properties::parse_initial_pgm_source(&props, 4, 1),
        Source::Input(0)
    );
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

    let lo = layout::compute_layout(1280, 720, 4, 0);
    let state = Arc::new(VisionMixerOverlayState::new(
        4,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into(), "D".into()],
        lo,
        false,
        super::overlay::PipInitialState::default(),
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
