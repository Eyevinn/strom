//! Helpers shared by the integration tests.
//!
//! Include it as `pub mod common;`. The `pub` is what keeps the helpers a given
//! test binary does not call from being reported as dead code.

pub mod manager;
pub mod state;

use gstreamer as gst;

/// The elements in `required` that this GStreamer install does not provide.
/// Initialises GStreamer first, so it can run before anything else.
pub fn missing_elements<'a>(required: &[&'a str]) -> Vec<&'a str> {
    gst::init().expect("GStreamer initialises");
    required
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect()
}

/// Whether a test may skip because of `missing`: true when nothing is missing.
///
/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure. Every skip on a
/// missing element goes through here, so none can quietly opt out of that.
pub fn require_or_skip(missing: &[&str]) -> bool {
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    eprintln!(
        "SKIP: required GStreamer elements missing: {}",
        missing.join(", ")
    );
    false
}

/// True when every element in `required` is available. Otherwise skips, or
/// fails under `STROM_REQUIRE_GST_PLUGINS` — see [`require_or_skip`].
pub fn plugins_available(required: &[&str]) -> bool {
    require_or_skip(&missing_elements(required))
}

/// Fail outright if any element in `required` is missing, whatever
/// `STROM_REQUIRE_GST_PLUGINS` says. For tests that must never skip.
pub fn require_elements(required: &[&str]) {
    let missing = missing_elements(required);
    assert!(
        missing.is_empty(),
        "these GStreamer elements are missing, so this test would guard nothing: {}; \
         the CI image needs the package that provides them",
        missing.join(", ")
    );
}

/// Register the `gst-plugins-rs` WebRTC elements (`whipsink`, `whepsrc`, ...).
/// The server links them in and registers them at startup, so a test binary has
/// to do the same. Also initialises GStreamer. Safe to call repeatedly.
pub fn init_webrtc_plugins() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        gst::init().expect("GStreamer initialises");
        gstwebrtchttp::plugin_register_static().expect("register webrtchttp plugins");
        gstrswebrtc::plugin_register_static().expect("register webrtc plugins");
    });
}
