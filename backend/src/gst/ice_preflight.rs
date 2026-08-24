//! Preflight check for the GStreamer ICE elements that WebRTC needs.
//!
//! `webrtcbin` cannot do ICE without the libnice elements (`nicesrc` /
//! `nicesink`). When they are absent, upstream `webrtcsrc` calls
//! `bin.sync_state_with_parent().unwrap()` in a context that cannot unwind, so
//! a WHIP or WHEP session does not fail — it aborts the whole process with
//! SIGABRT. Nothing in Strom can catch that, which leaves one option: never
//! reach it.
//!
//! So every block that brings up a `webrtcbin` asks [`require_ice_elements`]
//! first. A flow containing such a block then refuses to start with an error
//! naming the missing package, instead of taking the server down with it.
//!
//! The packaging bug that motivated this is fixed, but the check is not about
//! that one bug: any host missing the plugin fails the same way — a container
//! image, a hand-built environment, a `minimal` install, or the next time a
//! distribution renames the package.

use gstreamer as gst;
use tracing::{error, info};

use crate::blocks::BlockBuildError;

/// The element factories `webrtcbin` needs for ICE.
const ICE_ELEMENTS: [&str; 2] = ["nicesrc", "nicesink"];

/// Names of the ICE elements that are missing from this installation.
///
/// Empty means WebRTC can negotiate.
pub fn missing_ice_elements() -> Vec<&'static str> {
    ICE_ELEMENTS
        .iter()
        .copied()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect()
}

/// The package that ships the ICE elements on this platform.
///
/// Used in the operator-facing message, so it names what to install rather
/// than what is missing.
pub const fn ice_package_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "brew install libnice-gstreamer"
    }
    #[cfg(target_os = "windows")]
    {
        "the GStreamer MSI installer's full package set"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "gstreamer1.0-nice (Debian/Ubuntu), libnice-gstreamer1 (Fedora) or libnice (Arch)"
    }
}

/// The message shown when a WebRTC block cannot be built.
///
/// Kept separate from the check itself so its wording is testable on a host
/// where the elements are present.
pub fn ice_missing_message(block_label: &str, missing: &[&str]) -> String {
    format!(
        "{} needs WebRTC ICE support, but GStreamer has no {}. \
         webrtcbin cannot negotiate without them, and the failure would abort \
         the server process rather than this flow. Install them with: {}",
        block_label,
        missing.join(" or "),
        ice_package_hint()
    )
}

/// Refuse to build a WebRTC block when ICE is unavailable.
///
/// `block_label` is the operator-facing block name, e.g. "WHIP Input".
pub fn require_ice_elements(block_label: &str) -> Result<(), BlockBuildError> {
    let missing = missing_ice_elements();
    if missing.is_empty() {
        return Ok(());
    }
    Err(BlockBuildError::MissingPlugin(ice_missing_message(
        block_label,
        &missing,
    )))
}

/// Report ICE availability once at startup.
///
/// A flow that needs ICE fails cleanly on its own, but an operator setting the
/// server up should not have to start a flow to discover this.
pub fn log_ice_availability() {
    let missing = missing_ice_elements();
    if missing.is_empty() {
        info!("WebRTC ICE available (nicesrc, nicesink)");
    } else {
        error!(
            "WebRTC ICE unavailable: GStreamer has no {}. WHIP and WHEP blocks will refuse to start. Install them with: {}",
            missing.join(" or "),
            ice_package_hint()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The message has to tell an operator what to install, not just what
    /// broke — that is the whole point of the check.
    #[test]
    fn message_names_the_block_the_elements_and_the_remedy() {
        let msg = ice_missing_message("WHIP Input", &["nicesrc", "nicesink"]);
        assert!(msg.contains("WHIP Input"), "{}", msg);
        assert!(msg.contains("nicesrc"), "{}", msg);
        assert!(msg.contains("nicesink"), "{}", msg);
        assert!(msg.contains(ice_package_hint()), "{}", msg);
    }

    #[test]
    fn message_explains_why_it_is_refused_rather_than_attempted() {
        let msg = ice_missing_message("WHEP Output", &["nicesrc"]);
        assert!(msg.contains("abort"), "{}", msg);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hint_names_the_plugin_formula_not_the_library() {
        let hint = ice_package_hint();
        assert!(hint.contains("libnice-gstreamer"), "{}", hint);
        // `libnice` alone is the library without the GStreamer plugin — the
        // exact mistake that made every WHIP session abort.
        assert_ne!(hint, "brew install libnice");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_hint_covers_the_supported_distributions() {
        let hint = ice_package_hint();
        for pkg in ["gstreamer1.0-nice", "libnice-gstreamer1", "libnice"] {
            assert!(hint.contains(pkg), "{} missing from {}", pkg, hint);
        }
    }

    /// The guard and the probe must agree, or a block could be refused on a
    /// host that can actually do ICE (or worse, allowed on one that cannot).
    /// Passes whether or not the elements are installed, so it is meaningful
    /// in CI as well as on a developer machine.
    #[test]
    fn the_guard_agrees_with_the_probe() {
        let _ = gst::init();
        let available = missing_ice_elements().is_empty();
        assert_eq!(require_ice_elements("Block").is_ok(), available);
    }

    #[test]
    fn a_present_installation_reports_nothing_missing() {
        let _ = gst::init();
        if gst::ElementFactory::find("nicesrc").is_some()
            && gst::ElementFactory::find("nicesink").is_some()
        {
            assert!(missing_ice_elements().is_empty());
            assert!(require_ice_elements("WHIP Input").is_ok());
        } else {
            // No ICE here (this is the CI Linux image, among others): the
            // probe must say so and name both elements.
            assert_eq!(missing_ice_elements().len(), 2);
        }
    }
}
