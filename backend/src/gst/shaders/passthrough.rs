//! Passthrough for FX slots that hold no effect.
//!
//! A `glshader` slot holding the identity fragment still renders every frame:
//! a full-frame copy into a pool buffer, an FBO render and a GL sync. Neither
//! `glshader` nor its GL base classes ever enter `GstBaseTransform`
//! passthrough on their own, but both honour it (`GstGLFilter` forwards caps
//! unchanged, `GstGLBaseFilter` forwards the ALLOCATION query, and the base
//! transform pushes the input buffer). So an idle slot is put in passthrough,
//! and taken out of it before an effect is programmed.
//!
//! The switch must not race the streaming thread: `GstBaseTransform` reads
//! its passthrough flag once to pick the output buffer and again to decide
//! whether to render into it, so flipping it from another thread mid-buffer
//! can push an unrendered pool buffer, or render a frame into its own input.
//! The flip therefore runs in a one-shot probe on the sink pad, which fires
//! under the pad's stream lock before the next buffer reaches the element.
//! It also marks the src pad for reconfigure, so that same buffer first
//! renegotiates: leaving passthrough runs the allocation that sets up the GL
//! output pool, and the buffer is rendered with the newly programmed shader.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use tracing::debug;

/// Put a `glshader` FX slot in or out of passthrough, from its next buffer.
///
/// `passthrough` must be true only while the slot's program is an identity
/// pass (the identity fragment, or a transition shader parked neutral).
/// Requests apply in call order: probes on a pad run in the order they were
/// added, so of several requests pending on the same buffer the last one wins.
pub fn request_passthrough(elem: &gst::Element, passthrough: bool) {
    let Some(pad) = elem.static_pad("sink") else {
        return;
    };
    let Some(transform) = elem.downcast_ref::<gst_base::BaseTransform>() else {
        return;
    };
    // Pads own their probes: a weak ref, never a strong one (leak rule).
    let weak = transform.downgrade();
    // One-shot: fires on the next buffer and removes itself. Never per buffer.
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, _info| {
            let Some(transform) = weak.upgrade() else {
                return gst::PadProbeReturn::Remove;
            };
            if transform.is_passthrough() != passthrough {
                transform.set_passthrough(passthrough);
                transform.reconfigure_src();
                debug!("{}: FX slot passthrough={}", transform.name(), passthrough);
            }
            gst::PadProbeReturn::Remove
        },
    );
}
