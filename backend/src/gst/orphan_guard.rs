//! Drive to NULL any element a bin removes from itself while it is still running.
//!
//! An element disposed above NULL never ran its READY→NULL transition, so the
//! UDP sockets, streaming threads and file descriptors it opened on the way up
//! are still open when the last reference goes:
//!
//! ```text
//! GStreamer-CRITICAL: Trying to dispose element bin31, but it is in PAUSED
//! instead of the NULL state.
//! ```
//!
//! A bin drives its children to whatever state it is driven to and only lets go
//! of one at NULL, so the only way in is a child removed from a different
//! thread than the one running the parent's state change.
//!
//! `whipserversrc` does exactly that. Each WHIP session lives in a plain
//! `GstBin` holding the session's `webrtcbin`, and `webrtcsrc` ends a session by
//! taking that bin to NULL and removing it — on its own thread
//! (`State::finalize_session` in gst-plugins-rs `net/webrtc`, via
//! `RUNTIME.spawn_blocking`). Nothing waits for it: the guard `unprepare()`
//! blocks on is cleared at the top of that task, before it touches the bin.
//!
//! Strom's abrupt-drop teardown runs into it because both are woken by the same
//! `notify::ice-connection-state` emission, and `webrtcsrc`'s handler is
//! connected first. From a GST_DEBUG trace of the collision — A is
//! `webrtcsrc`'s finalizer, B is `teardown_session_pipeline`:
//!
//! ```text
//! A  <bin0> completed state change to NULL       session bin down, sockets closed
//! B  <bin0> current NULL, desired next PAUSED    7 us later: the pipeline's
//! B  <bin0> completed state change to PAUSED     PLAYING->PAUSED step raises it
//!                                                back up, reopening everything
//! A  gst_bin_remove(whipserversrc, bin0)         removed, in PAUSED
//! B  <whipserversrc> children READY->NULL: iterator done, no children
//! ```
//!
//! A plain child at NULL is left alone by a downward transition, but
//! `gst_bin_element_set_state` recurses into a `GstBin` unconditionally ("always
//! recurse into bins so that we can set the base time"), which is why it is the
//! session bin, and not the elements inside it, that gets raised.
//!
//! The bin leaves the pipeline before the descent can reach it again, so nothing
//! takes it back to NULL. Waiting for `set_state(NULL)` to settle does not help:
//! PAUSED is a step on the way down, so the raise has already happened by the
//! time it returns.
//!
//! Strom cannot make the removal wait, so it catches the element on the way out.
//! `element-removed` fires with no bin lock held and the child already
//! unparented — the one point where the state can still be fixed.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, warn};

/// Guard every element `bin` removes from itself.
///
/// Locking the state before setting it is what makes this safe against a
/// removal that lands *during* the parent's transition. `gst_bin_element_set_state`
/// reads the locked flag under the child's state lock, so whichever thread gets
/// there second sees the effect of the first: either the parent skips the child
/// outright, or this call queues behind the raise and takes it back down.
pub fn install(bin: &gst::Bin) {
    // Captures nothing. Both the bin and the removed element arrive as
    // arguments, so this handler cannot form the reference cycle that a
    // captured `gst::Element`/`gst::Bin` clone would.
    bin.connect_element_removed(|bin, element| {
        // Read for the log only. It can still say NULL while the parent's
        // cascade holds the state lock mid-raise, so it is never a reason to
        // skip the work below.
        let state = element.current_state();
        if state == gst::State::Null {
            debug!(
                "Orphan guard: '{}' was removed from '{}' at NULL",
                element.name(),
                bin.name()
            );
        } else {
            warn!(
                "Orphan guard: '{}' was removed from '{}' in {:?}, forcing it to NULL",
                element.name(),
                bin.name(),
                state
            );
        }

        element.set_locked_state(true);
        if let Err(e) = element.set_state(gst::State::Null) {
            warn!(
                "Orphan guard: failed to set '{}' to NULL after removal: {:?}",
                element.name(),
                e
            );
        }
    });
}
