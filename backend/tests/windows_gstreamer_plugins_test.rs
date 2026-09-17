//! Guards the Windows CI GStreamer install against the MSI's default feature set.
//!
//! `msiexec /quiet` installs only the features the MSI marks as default, which leave out
//! `gst-plugins-ugly` and so `x264enc`. Both recorder integration tests list that element,
//! so on Windows they skipped green instead of running — two regression guards inert on a
//! whole platform, with nothing in the job output saying so (#703).
//!
//! This is a text assertion on the workflow, not proof that the element arrives: only a
//! `platforms=windows` dispatch can show that. What it does stop is the argument being
//! dropped again without anyone noticing, which is how the gap lasted this long. If the
//! Windows runtime install moves or is renamed, move this guard with it.

use std::path::Path;

fn ci_workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join(".github/workflows/ci.yml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading ci.yml: {e}"))
}

#[test]
fn the_windows_gstreamer_runtime_is_installed_with_every_msi_feature() {
    let workflow = ci_workflow();

    let install = workflow
        .lines()
        .find(|line| line.contains("msiexec") && line.contains("/i gstreamer.msi"))
        .expect("ci.yml has no msiexec line installing gstreamer.msi");

    assert!(
        install.contains("ADDLOCAL=ALL"),
        "the Windows GStreamer runtime is installed with the MSI's default feature set, \
         which carries no gst-plugins-ugly and so no x264enc: {}",
        install.trim()
    );
}
