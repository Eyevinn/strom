//! Unit tests for the vision mixer block.

use super::layout;
use super::properties;
use std::collections::HashMap;
use strom_types::PropertyValue;

/// The overlay timer registry, its running count and its shutdown flag are all
/// process-global, and cargo runs tests in the same process in parallel. Every
/// test that starts a real timer thread takes this first: otherwise one test's
/// `shutdown_overlay_timers` stops another's thread, and each sees a baseline
/// count that includes the other's.
static OVERLAY_TIMER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`OVERLAY_TIMER_TEST_LOCK`], ignoring poisoning: a panic in one timer
/// test should fail that test, not turn every later one into a poison error.
fn overlay_timer_test_guard() -> std::sync::MutexGuard<'static, ()> {
    OVERLAY_TIMER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
fn test_parse_num_inputs_clamped_to_max() {
    let mut props = HashMap::new();
    props.insert(
        "num_inputs".to_string(),
        PropertyValue::String("9999".to_string()),
    );
    assert_eq!(
        properties::parse_num_inputs(&props),
        strom_types::vision_mixer::MAX_NUM_INPUTS
    );
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

const ASPECT_16_9: f64 = 16.0 / 9.0;

#[test]
fn test_layout_compute_basic() {
    let l = layout::compute_layout(1920, 1080, 4, 0, ASPECT_16_9, false);
    assert_eq!(l.num_inputs, 4);
    assert_eq!(l.thumbnail_rects.len(), 4);
    assert_eq!(l.label_positions.len(), 4);
    // PVW is left, PGM is right
    assert!(l.pvw_rect.x < l.pgm_rect.x);
    // Both on same row
    assert_eq!(l.pvw_rect.y as i32, l.pgm_rect.y as i32);
    // Big rects are snapped to source aspect.
    let aspect = l.pgm_rect.w / l.pgm_rect.h;
    assert!(
        (aspect - ASPECT_16_9).abs() < 0.02,
        "PGM big rect aspect {aspect} != 16:9"
    );
    // Each thumbnail rect is also snapped to source aspect.
    for r in &l.thumbnail_rects {
        let a = r.w / r.h;
        assert!(
            (a - ASPECT_16_9).abs() < 0.02,
            "thumb aspect {a} != 16:9 for rect {r:?}"
        );
    }
}

#[test]
fn test_layout_swap_pvw_pgm() {
    let normal = layout::compute_layout(1920, 1080, 4, 0, ASPECT_16_9, false);
    let swapped = layout::compute_layout(1920, 1080, 4, 0, ASPECT_16_9, true);
    // Swap mirrors the two big rects: PGM is now on the left, PVW on the right.
    assert!(swapped.pgm_rect.x < swapped.pvw_rect.x);
    // The geometry is exactly mirrored — swapped PVW sits where PGM was.
    assert_eq!(swapped.pvw_rect.x as i32, normal.pgm_rect.x as i32);
    assert_eq!(swapped.pgm_rect.x as i32, normal.pvw_rect.x as i32);
    // Labels follow their rects, so they swap sides too.
    assert!(swapped.pgm_label_pos.x < swapped.pvw_label_pos.x);
}

#[test]
fn test_layout_compute_10_inputs() {
    // 10 slots → (5, 2) grid (cell aspect ≈ 1.43, zero empty cells beats
    // any alternative on the combined aspect+empty cost).
    let l = layout::compute_layout(1920, 1080, 10, 0, ASPECT_16_9, false);
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
    // 4 inputs + 1 PiP tile = 5 slots → (3, 2) grid: 3 in row 1, 2 in row 2.
    // PiP is slot 4 → row 1 (second row), col 1. Last input is slot 3 →
    // row 1, col 0. Same row, PiP to the right of last input.
    let l = layout::compute_layout(1920, 1080, 4, 1, ASPECT_16_9, false);
    assert_eq!(l.num_inputs, 4);
    assert_eq!(l.num_pips, 1);
    assert_eq!(l.thumbnail_rects.len(), 4);
    assert_eq!(l.pip_tile_rects.len(), 1);
    assert_eq!(l.pip_label_positions.len(), 1);

    let last_thumb = l.thumbnail_rects.last().unwrap();
    let pip = &l.pip_tile_rects[0];
    assert_eq!(
        pip.y as i32, last_thumb.y as i32,
        "PiP tile shares row with last input"
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

    // PiP tile is also snapped to source aspect.
    let a = pip.w / pip.h;
    assert!(
        (a - ASPECT_16_9).abs() < 0.02,
        "PiP tile aspect {a} != 16:9"
    );
}

#[test]
fn test_pip_overlay_rects_tile_within_bg() {
    let l = layout::compute_layout(1920, 1080, 4, 1, ASPECT_16_9, false);
    let (bx, by, bw, bh) = layout::pip_bg_pad_position(&l, 0);

    // Two overlays → side-by-side aspect-preserving cells within the PiP tile.
    let rects =
        strom_types::vision_mixer::compute_pip_overlay_rects(bx, by, bw, bh, 2, ASPECT_16_9);
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

/// `overlay_states` and `overlay_renderers` share the same lifecycle: they are
/// populated together in `build_overlay` and must be cleared together by
/// `unregister_flow`, which `state.rs::teardown_flow` calls on every teardown
/// path. If you add another per-block registry in this module, clear it in
/// `unregister_flow` too and extend this test.
///
/// Two flows are registered so the sweep is checked for aim as well as reach:
/// clearing one flow must not touch another flow's overlay.
#[test]
fn overlay_registries_round_trip() {
    use super::overlay::{
        get_overlay_renderer, get_overlay_state, register_overlay_renderer, register_overlay_state,
        unregister_flow, OverlayRenderer, VisionMixerOverlayState,
    };
    use gstreamer as gst;
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Mutex};

    gst::init().unwrap();

    let flow_id = strom_types::FlowId::new_v4();
    let other_flow_id = strom_types::FlowId::new_v4();
    let block_id = "test-vm-overlay-cleanup-block-id";
    let other_block_id = "test-vm-overlay-cleanup-other-flow-block-id";

    let lo = layout::compute_layout(1280, 720, 4, 0, ASPECT_16_9, false);
    let state = Arc::new(VisionMixerOverlayState::new(
        4,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into(), "D".into()],
        lo,
        1920,
        1080,
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

    register_overlay_state(flow_id, block_id, Arc::clone(&state));
    register_overlay_renderer(flow_id, block_id, Arc::clone(&renderer));
    register_overlay_state(other_flow_id, other_block_id, Arc::clone(&state));
    register_overlay_renderer(other_flow_id, other_block_id, Arc::clone(&renderer));

    assert!(
        get_overlay_state(block_id).is_some(),
        "state should be registered"
    );
    assert!(
        get_overlay_renderer(block_id).is_some(),
        "renderer should be registered"
    );

    unregister_flow(&flow_id);

    assert!(
        get_overlay_state(block_id).is_none(),
        "state must be cleaned (otherwise API still sees stale block)"
    );
    assert!(
        get_overlay_renderer(block_id).is_none(),
        "renderer must be cleaned (otherwise overlay-timer-* thread leaks)"
    );
    assert!(
        get_overlay_state(other_block_id).is_some()
            && get_overlay_renderer(other_block_id).is_some(),
        "tearing down one flow must not clear another flow's overlay"
    );

    unregister_flow(&other_flow_id);
}

/// `shutdown_overlay_timers` must wait until the timer thread has left cairo,
/// not just signal it: a thread still painting while `exit()` frees pixman's
/// globals is the `overlay-timer-*` SIGSEGV on graceful shutdown.
#[test]
fn shutdown_overlay_timers_joins_running_timer() {
    use super::overlay::{
        overlay_timers_running, register_overlay_renderer, register_overlay_state,
        shutdown_overlay_timers, start_overlay_timer, unregister_flow, OverlayRenderer,
        VisionMixerOverlayState,
    };
    use gstreamer as gst;
    use gstreamer::prelude::{ElementExt, GstBinExt};
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let _serialized = overlay_timer_test_guard();
    gst::init().unwrap();

    let flow_id = strom_types::FlowId::new_v4();
    let block_id = "test-vm-overlay-timer-shutdown-block-id";

    let lo = layout::compute_layout(1280, 720, 4, 0, ASPECT_16_9, false);
    let state = Arc::new(VisionMixerOverlayState::new(
        4,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into(), "D".into()],
        lo,
        1920,
        1080,
        false,
        super::overlay::PipInitialState::default(),
    ));

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", 1280i32)
        .field("height", 720i32)
        .field("framerate", gst::Fraction::new(50, 1))
        .build();
    // Mirror production: leaky and non-blocking, so a render in flight cannot
    // stall the join.
    let appsrc = gst_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .is_live(false)
        .do_timestamp(true)
        .max_buffers(2)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();
    // Parented like production: the timer treats an appsrc with no parent as a
    // torn-down pipeline and exits, so a bare appsrc would never reach cairo.
    // The bin stays NULL, leaving the appsrc's own state to the line below.
    let bin = gst::Bin::new();
    bin.add(&appsrc).expect("appsrc should add to bin");
    // The timer only enters its render loop once the appsrc reports PLAYING.
    appsrc
        .set_state(gst::State::Playing)
        .expect("appsrc should reach PLAYING");

    let renderer = Arc::new(Mutex::new(OverlayRenderer::new(
        appsrc.clone(),
        caps,
        Arc::clone(&state),
        1280,
        720,
    )));

    register_overlay_state(flow_id, block_id, Arc::clone(&state));
    register_overlay_renderer(flow_id, block_id, Arc::clone(&renderer));

    let before = overlay_timers_running();
    start_overlay_timer(block_id.to_string(), Arc::clone(&renderer), (50, 1));

    // Wait for a real render, so shutdown interrupts a thread inside cairo
    // rather than one waiting for PLAYING.
    let deadline = Instant::now() + Duration::from_secs(10);
    while appsrc.current_level_buffers() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        appsrc.current_level_buffers() > 0,
        "overlay timer should have rendered and pushed a frame before shutdown"
    );

    shutdown_overlay_timers();

    // No sleep, no retry: the thread must be gone once the call returns.
    assert_eq!(
        overlay_timers_running(),
        before,
        "shutdown_overlay_timers must join the timer thread, not just signal it"
    );

    let _ = appsrc.set_state(gst::State::Null);
    drop(bin);
    unregister_flow(&flow_id);
    // The flag is process-global and terminal; clear it for later tests.
    super::overlay::reset_overlay_timers_shutdown_for_test();
}

/// The timer's backstop: a teardown path that never unregisters the renderer
/// must still cost only a thread exit, not a core burning for the life of the
/// process. The registry is deliberately left populated here — the only signal
/// the thread gets is its appsrc losing its parent, which is what a finalizing
/// pipeline does to its children.
#[test]
fn overlay_timer_exits_when_its_appsrc_is_orphaned() {
    use super::overlay::{
        overlay_timers_running, register_overlay_renderer, register_overlay_state,
        start_overlay_timer, unregister_flow, OverlayRenderer, VisionMixerOverlayState,
    };
    use gstreamer as gst;
    use gstreamer::prelude::{ElementExt, GstBinExt};
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let _serialized = overlay_timer_test_guard();
    gst::init().unwrap();

    let flow_id = strom_types::FlowId::new_v4();
    let block_id = "test-vm-overlay-timer-orphan-block-id";

    let lo = layout::compute_layout(1280, 720, 4, 0, ASPECT_16_9, false);
    let state = Arc::new(VisionMixerOverlayState::new(
        4,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into(), "D".into()],
        lo,
        1920,
        1080,
        false,
        super::overlay::PipInitialState::default(),
    ));

    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", 1280i32)
        .field("height", 720i32)
        .field("framerate", gst::Fraction::new(50, 1))
        .build();
    let appsrc = gst_app::AppSrc::builder()
        .caps(&caps)
        .format(gst::Format::Time)
        .is_live(false)
        .do_timestamp(true)
        .max_buffers(2)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();
    let bin = gst::Bin::new();
    bin.add(&appsrc).expect("appsrc should add to bin");
    appsrc
        .set_state(gst::State::Playing)
        .expect("appsrc should reach PLAYING");

    let renderer = Arc::new(Mutex::new(OverlayRenderer::new(
        appsrc.clone(),
        caps,
        Arc::clone(&state),
        1280,
        720,
    )));

    register_overlay_state(flow_id, block_id, Arc::clone(&state));
    register_overlay_renderer(flow_id, block_id, Arc::clone(&renderer));

    let before = overlay_timers_running();
    start_overlay_timer(block_id.to_string(), Arc::clone(&renderer), (50, 1));

    // Let it reach the push loop first, so the exit is the backstop firing and
    // not the thread still waiting for PLAYING.
    let deadline = Instant::now() + Duration::from_secs(10);
    while appsrc.current_level_buffers() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        appsrc.current_level_buffers() > 0,
        "overlay timer should be in its push loop before the appsrc is orphaned"
    );

    // What a finalizing pipeline does to its children — but with the renderer
    // still registered, as a missed unregistration would leave it.
    bin.remove(&appsrc).expect("appsrc should leave the bin");

    let deadline = Instant::now() + Duration::from_secs(5);
    while overlay_timers_running() > before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        overlay_timers_running(),
        before,
        "an orphaned appsrc must stop the timer even with the renderer still registered"
    );

    let _ = appsrc.set_state(gst::State::Null);
    unregister_flow(&flow_id);
}

/// An input that has never delivered a buffer reads as `None`; once it has,
/// the age is measured from that buffer and keeps growing while the input
/// stays silent. This is what separates a frozen participant from a
/// motionless one, which the picture alone cannot do.
#[test]
fn input_media_age_tracks_buffer_arrival() {
    use super::layout;
    use super::overlay::VisionMixerOverlayState;

    let lo = layout::compute_layout(1280, 720, 3, 0, ASPECT_16_9, false);
    let state = VisionMixerOverlayState::new(
        3,
        0,
        0,
        1,
        vec!["A".into(), "B".into(), "C".into()],
        lo,
        1920,
        1080,
        false,
        super::overlay::PipInitialState::default(),
    );

    // Nothing has arrived yet on any input.
    for i in 0..3 {
        assert_eq!(state.input_media_age_ms(i), None, "input {}", i);
    }

    state.note_input_buffer(1);
    let age = state
        .input_media_age_ms(1)
        .expect("input 1 has delivered a buffer");
    assert!(age < 1000, "fresh buffer should read as young, got {}", age);

    // Its silent neighbours are still untouched.
    assert_eq!(state.input_media_age_ms(0), None);
    assert_eq!(state.input_media_age_ms(2), None);

    // The age grows while the input stays silent.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let later = state
        .input_media_age_ms(1)
        .expect("input 1 has delivered a buffer");
    assert!(
        later >= age + 20,
        "age should grow while silent: {} -> {}",
        age,
        later
    );

    // A fresh buffer resets it.
    state.note_input_buffer(1);
    assert!(state.input_media_age_ms(1).unwrap() < later);

    // Out-of-range inputs have no slot rather than panicking.
    assert_eq!(state.input_media_age_ms(9), None);
    state.note_input_buffer(9);
}
