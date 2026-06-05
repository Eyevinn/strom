//! Persistent control bindings — the only safe way to animate properties
//! on objects whose streaming threads call `gst_object_sync_values()`.
//!
//! `gst_object_sync_values()` iterates the object's `control_bindings`
//! GList **without holding any lock** (the lock is deliberately disabled
//! upstream: "FIXME: this deadlocks" in gstobject.c, present in 1.24 and
//! current master). `gst_object_remove_control_binding()` mutates and frees
//! list nodes from whatever thread calls it. Removing a binding while a
//! streaming thread is mid-iteration is therefore a use-after-free — we hit
//! it as a GP fault in production: rapid PiP updates removing/re-adding
//! crop bindings on a `videocrop` (which syncs values on EVERY buffer)
//! crashed the pipeline.
//!
//! The protocol here closes the race:
//!   - A (object, property) pair gets ONE `DirectControlBinding` with ONE
//!     `InterpolationControlSource`, attached on first use and **never
//!     removed**. First-time attachment appends to the list, which an
//!     unlocked reader tolerates (it sees either the old tail or the new
//!     node); removal is what frees memory under the reader.
//!   - "Clearing" a binding = wiping the control source's keyframes
//!     (`unset_all`). Keyframe access is guarded by the control source's
//!     internal lock, so this is safe against concurrent `sync_values`.
//!     An empty control source makes the binding inert: `get_value`
//!     returns FALSE and the property is left alone, so plain
//!     `set_property` writes behave exactly as with no binding.
//!   - Reprogramming = `unset_all` + `set_mode` + new keyframes on the
//!     existing source.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{DirectControlBinding, InterpolationControlSource, InterpolationMode};
use tracing::warn;

/// Get the persistent `InterpolationControlSource` driving `obj.prop`,
/// attaching the binding on first use. The returned source has its previous
/// keyframes wiped and its mode set — ready to program.
///
/// Returns `None` when the property carries a foreign binding type (nothing
/// in this codebase creates one) — in that case the caller must NOT fall
/// back to add/remove, that's the race this module exists to prevent.
pub(crate) fn fresh_control_source(
    obj: &gst::Object,
    prop: &str,
    mode: InterpolationMode,
) -> Option<InterpolationControlSource> {
    let cs = match obj.control_binding(prop) {
        Some(binding) => {
            let cs = binding
                .property::<Option<gst::ControlSource>>("control-source")
                .and_then(|cs| cs.downcast::<InterpolationControlSource>().ok());
            match cs {
                Some(cs) => cs,
                None => {
                    warn!(
                        "Property {} on {} has a foreign control binding — cannot animate",
                        prop,
                        obj.name()
                    );
                    return None;
                }
            }
        }
        None => {
            let cs = InterpolationControlSource::new();
            let binding = DirectControlBinding::new(obj, prop, &cs);
            if let Err(e) = obj.add_control_binding(&binding) {
                warn!(
                    "Failed to attach control binding for {} on {}: {}",
                    prop,
                    obj.name(),
                    e
                );
                return None;
            }
            cs
        }
    };
    cs.unset_all();
    cs.set_mode(mode);
    Some(cs)
}

/// Make `obj.prop`'s binding inert by wiping its keyframes (no-op when the
/// property has no binding). The drop-in replacement for every former
/// `remove_control_binding` call: after this, `set_property` writes stick.
pub(crate) fn wipe_control_binding(obj: &gst::Object, prop: &str) {
    if let Some(binding) = obj.control_binding(prop) {
        if let Some(cs) = binding
            .property::<Option<gst::ControlSource>>("control-source")
            .and_then(|cs| cs.downcast::<InterpolationControlSource>().ok())
        {
            cs.unset_all();
        }
    }
}

/// Wipe several properties' bindings at once.
pub(crate) fn wipe_control_bindings(obj: &gst::Object, props: &[&str]) {
    for prop in props {
        wipe_control_binding(obj, prop);
    }
}
