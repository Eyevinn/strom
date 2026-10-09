//! Keep macOS from putting the server into the background QoS band.
//!
//! Headless Strom registers with LaunchServices as a `Foreground` app but never
//! opens a window, and macOS App-Naps that shape ~32 s after launch: every thread
//! drops to scheduling priority 4, which on Apple Silicon means efficiency cores
//! at a throttled clock, and pipelines run ~7x slower for the rest of the
//! process's life. `gst-launch-1.0` is exempt because it registers as
//! `UIElement`; what makes Strom `Foreground` is not known.
//!
//! `beginActivityWithOptions:reason:` is the documented opt-out, and works
//! regardless of activation policy.
//!
//! Both launch modes take it. A window keeps the app out of App Nap only while
//! it is visible: hidden, minimised or covered by other windows, the GUI process
//! is demoted the same way. Its live mixers then miss their deadlines in bursts,
//! so video freezes and the audio mix goes silent while every element still
//! reports `Playing`.

/// Take a user-initiated activity assertion that lasts as long as the process.
///
/// Safe to call more than once; each call takes its own assertion.
#[cfg(target_os = "macos")]
pub fn hold_activity_for_process_lifetime() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    // `UserInitiatedAllowingIdleSystemSleep` rather than `UserInitiated`: both opt
    // out of App Nap, but the plain variant also blocks system idle sleep, and a
    // media server has no business keeping the machine awake. `LatencyCritical`
    // also opts out of timer coalescing.
    let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
        | NSActivityOptions::LatencyCritical;
    let reason = NSString::from_str("Strom is running media pipelines");
    let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(options, &reason);

    // The assertion lasts as long as the token, and we want it for the whole
    // process, so leaking it is deliberate.
    std::mem::forget(activity);

    tracing::info!("macOS: holding a user-initiated activity assertion (App Nap opt-out)");
}

/// No-op away from macOS.
#[cfg(not(target_os = "macos"))]
pub fn hold_activity_for_process_lifetime() {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Catches a wrong selector or option constant, which aborts the process
    /// rather than failing an assertion. `tests/macos_app_nap_test.rs` is the
    /// actual guard.
    #[test]
    fn taking_the_activity_assertion_does_not_abort() {
        super::hold_activity_for_process_lifetime();
        super::hold_activity_for_process_lifetime();
    }
}
