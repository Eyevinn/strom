//! Guards that the `isobmff` elements Strom uses come from its static fork
//! of `gst-plugin-isobmff`, not from a system `isobmff` plugin. GStreamer
//! 1.28 installs one (Windows installer, Homebrew), and the fork only wins
//! because its static registration replaces the system plugin's registry
//! entries in `strom::gst::register_static_plugins()`, the call the server
//! makes at startup.
//!
//! Its own test binary: another test that creates `isofmp4mux` before the
//! registration would load the system plugin into the process first.

use gstreamer as gst;
use gstreamer::prelude::*;

#[test]
fn isofmp4mux_comes_from_the_static_fork() {
    gst::init().expect("GStreamer initialises");
    strom::gst::register_static_plugins();

    for name in ["isofmp4mux", "cmafmux", "isomp4mux"] {
        let factory = gst::ElementFactory::find(name)
            .unwrap_or_else(|| panic!("{name} is not in the registry"));
        let plugin = factory
            .plugin()
            .unwrap_or_else(|| panic!("{name} has no plugin"));
        // A static plugin has no file; a system plugin is a .so/.dll/.dylib.
        assert_eq!(
            plugin.filename(),
            None,
            "{name} comes from {:?}, not from Strom's static isobmff fork",
            plugin.filename()
        );
    }
}
