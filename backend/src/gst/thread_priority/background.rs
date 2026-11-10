//! Keep the process out of macOS's Darwin background state.
//!
//! A process that macOS runs in the background (launchd
//! `ProcessType=Background`, a child of a background service such as a CI
//! runner, `taskpolicy -b`) gets its timers coalesced. Every clock wait in
//! GStreamer then wakes in bursts, up to about 100 ms late. A live flow
//! reaches its syncing sinks late, they send QoS upstream, and the flow drops
//! frames. Measured on a test flow: about 15 drops a second instead of none.
//! The QoS class that [`super::set_current_thread_priority`] gives a streaming
//! thread does not lift it above the background clamp of its process, so this
//! has to be process wide.

#[cfg(target_os = "macos")]
use tracing::{error, warn};

/// Move the process out of the Darwin background state, once per process.
/// A no-op on other platforms, and on macOS when the process is not in the
/// background.
pub fn leave_background_process() {
    #[cfg(target_os = "macos")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| match leave_background() {
            Ok(false) => {}
            Ok(true) => warn!(
                "Strom was started as a macOS background process, which delays its clock \
                 waits and drops frames in live flows; moved it out of the background"
            ),
            Err(e) => error!(
                "Strom runs as a macOS background process and could not leave it ({}); \
                 live flows may drop frames",
                e
            ),
        });
    }
}

/// Clear the Darwin background state of the process. `Ok(true)` when it was
/// set.
#[cfg(target_os = "macos")]
fn leave_background() -> Result<bool, std::io::Error> {
    if !is_background()? {
        return Ok(false);
    }
    // SAFETY: setpriority on the calling process (who = 0) takes and returns
    // plain integers.
    if unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(true)
}

/// Whether the process is in the Darwin background state.
#[cfg(target_os = "macos")]
fn is_background() -> Result<bool, std::io::Error> {
    // getpriority returns -1 on error, which is also a valid priority
    // elsewhere; PRIO_DARWIN_PROCESS only returns 0 or 1, but check errno
    // the documented way anyway.
    // SAFETY: errno is thread local; getpriority on the calling process takes
    // and returns plain integers.
    unsafe { *libc::__error() = 0 };
    let state = unsafe { libc::getpriority(libc::PRIO_DARWIN_PROCESS, 0) };
    if state == -1 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(0) {
            return Err(err);
        }
    }
    Ok(state > 0)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// A process put in the background is taken out of it. Reverting
    /// `leave_background` to a no-op leaves the process in the background
    /// and fails here.
    #[test]
    fn a_background_process_is_moved_out_of_the_background() {
        // SAFETY: see leave_background.
        let rc = unsafe { libc::setpriority(libc::PRIO_DARWIN_PROCESS, 0, libc::PRIO_DARWIN_BG) };
        assert_eq!(rc, 0, "could not put the test process in the background");
        assert!(is_background().unwrap());

        assert!(
            leave_background().unwrap(),
            "the process was in the background"
        );
        assert!(!is_background().unwrap());
        assert!(!leave_background().unwrap(), "nothing left to do");
    }
}
