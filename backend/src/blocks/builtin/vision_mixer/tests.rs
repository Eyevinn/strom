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
    assert_eq!(labels, vec!["Input 1", "Input 2", "Input 3", "Input 4"]);
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
    assert_eq!(labels[1], "Input 2"); // default
    assert_eq!(labels[2], "Graphics");
    assert_eq!(labels[3], "Input 4"); // default
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
