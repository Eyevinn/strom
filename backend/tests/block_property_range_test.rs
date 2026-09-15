//! Regression tests for client-supplied properties applied by a block builder.
//!
//! #724 closed the panic class on the generic property path, but block builders
//! apply their own properties with the raw setters it removed. `x264enc`'s
//! `bitrate` is a `guint` with range 1..=2048000, and `set_property_from_str`
//! aborts the calling thread whenever `g_param_value_validate` has to clamp —
//! so a bitrate of 0, straight out of a request body, unwinds across the axum
//! handler and skips the half-built flow's teardown.
//!
//! These tests drive the real `VideoEncBuilder`, so they fail if the checked
//! setter is taken back out. A panic inside `build()` fails the test the same
//! way an unexpected `Ok` does.

use std::collections::HashMap;
use strom::blocks::builtin::videoenc::VideoEncBuilder;
use strom::blocks::{BlockBuildContext, BlockBuilder};
use strom_types::PropertyValue;

use gstreamer as gst;

/// Elements the H.264 path of the encoder block builds. Missing on a bare image.
const REQUIRED: &[&str] = &["x264enc", "h264parse", "videoconvert", "capsfilter"];

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    gst::init().expect("GStreamer initialises");
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

/// Build a Video Encoder block with `bitrate` as the client sent it.
///
/// `encoder_preference: software` pins the H.264 path to `x264enc`, so the
/// range asserted below is that element's own and the test does not change
/// meaning on a machine that has a hardware encoder.
fn build_with_bitrate(bitrate: PropertyValue) -> Result<(), String> {
    let mut props = HashMap::new();
    props.insert("codec".to_string(), PropertyValue::String("h264".to_string()));
    props.insert(
        "encoder_preference".to_string(),
        PropertyValue::String("software".to_string()),
    );
    props.insert("bitrate".to_string(), bitrate);

    let ctx = BlockBuildContext::new(vec![], "all".to_string());
    VideoEncBuilder
        .build("videoenc-range-test", &props, &ctx)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[test]
fn zero_bitrate_is_rejected_rather_than_panicking() {
    if !plugins_available() {
        return;
    }

    let err = build_with_bitrate(PropertyValue::UInt(0))
        .expect_err("bitrate 0 is below x264enc's minimum of 1 and must not build");

    assert!(
        err.contains("bitrate"),
        "the error should name the property that was rejected, got: {}",
        err
    );
}

#[test]
fn bitrate_above_the_encoder_range_is_rejected_rather_than_panicking() {
    if !plugins_available() {
        return;
    }

    let err = build_with_bitrate(PropertyValue::UInt(3_000_000))
        .expect_err("bitrate 3000000 is above x264enc's maximum of 2048000 and must not build");

    assert!(
        err.contains("bitrate"),
        "the error should name the property that was rejected, got: {}",
        err
    );
}

#[test]
fn an_in_range_bitrate_still_builds() {
    if !plugins_available() {
        return;
    }

    build_with_bitrate(PropertyValue::UInt(6000))
        .expect("a bitrate inside x264enc's range must still build");
}
