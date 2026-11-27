//! Keep headless Strom out of the macOS Dock.
//!
//! `gst_macos_main` starts headless Strom as an Accessory app (`UIElement`: no
//! Dock icon, no menu bar). GStreamer's GL library then undoes that: every GL
//! context it creates on macOS makes a hidden `NSWindow` on the main thread and
//! sets the Regular activation policy, whether or not the window is ever shown.
//! Strom probes the GL renderer at startup and flows create GL contexts of their
//! own, so a headless server became a `Foreground` app with a Dock icon within
//! seconds of launch. `LSUIElement` in a bundle's `Info.plist` does not help, as
//! GL sets the policy explicitly after launch (#1279).
//!
//! A run loop observer on the main thread puts the policy back after each such
//! call. GL creates its window from a block dispatched to the main queue, so the
//! observer runs as soon as that block returns. GUI mode does not install it.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_core_foundation::{
        kCFRunLoopCommonModes, CFRunLoop, CFRunLoopActivity, CFRunLoopObserver,
    };

    /// Runs on the main thread each time its run loop is about to sleep, which
    /// is after every block GL dispatched to it.
    unsafe extern "C-unwind" fn restore_accessory_policy(
        _observer: *mut CFRunLoopObserver,
        _activity: CFRunLoopActivity,
        _info: *mut c_void,
    ) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
            return;
        }
        if app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
            tracing::debug!("macOS: reset activation policy from Regular to Accessory");
        } else {
            tracing::warn!("macOS: could not reset activation policy to Accessory");
        }
    }

    pub fn keep_out_of_dock() {
        let Some(main_loop) = CFRunLoop::main() else {
            tracing::warn!("macOS: no main run loop, headless Strom may show in the Dock");
            return;
        };
        // SAFETY: the callout matches `CFRunLoopObserverCallBack` and ignores its
        // info pointer, so a null context is valid.
        let observer = unsafe {
            CFRunLoopObserver::new(
                None,
                CFRunLoopActivity::BeforeWaiting.0,
                true,
                0,
                Some(restore_accessory_policy),
                std::ptr::null_mut(),
            )
        };
        let Some(observer) = observer else {
            tracing::warn!(
                "macOS: could not create a run loop observer, headless Strom may show in the Dock"
            );
            return;
        };
        // SAFETY: reading an immutable CoreFoundation constant.
        let modes = unsafe { kCFRunLoopCommonModes };
        // The run loop retains the observer for the life of the process.
        main_loop.add_observer(Some(&observer), modes);
        tracing::info!("macOS: keeping headless Strom out of the Dock");
    }
}

/// Keep the process an Accessory app (no Dock icon) for its whole run, even
/// after GStreamer's GL library sets the Regular activation policy.
///
/// Headless mode only. Call it before `gstreamer::macos_main`, which runs the
/// main run loop the observer is installed on.
#[cfg(target_os = "macos")]
pub fn keep_headless_out_of_dock() {
    imp::keep_out_of_dock();
}

/// No-op away from macOS.
#[cfg(not(target_os = "macos"))]
pub fn keep_headless_out_of_dock() {}
