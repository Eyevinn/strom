//! Per-pad source cropping across both compositor backends.
//!
//! GL backend: `glvideomixerelement` sink pads expose
//! `crop-left/right/top/bottom` (texture-UV crop in source pixels,
//! controllable → animatable). CPU backend: the `compositor` element has no
//! crop pads (verified against GStreamer 1.24 and current upstream), so a
//! `videocrop` element sits directly upstream of each croppable sink pad
//! (`tee → queue → videocrop → compositor`). videocrop's `left/right/top/
//! bottom` are controllable too, and it calls `gst_object_sync_values()` per
//! buffer (verified upstream in `gst_video_crop_before_transform`), so crop
//! animates with the same `InterpolationControlSource` machinery on both
//! backends. On CPU each animated step renegotiates caps toward the
//! compositor (it does not advertise `GstVideoCropMeta`) — a short burst
//! during the morph window, handled by videoaggregator's live caps updates;
//! the geometry caps probe skips refreshes whose *uncropped* source dims are
//! unchanged so the burst cannot kill in-flight animations.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, warn};

/// GL mixer crop pad properties (absent on the CPU `compositor` backend).
pub(crate) const CROP_PAD_PROPS: [&str; 4] = ["crop-left", "crop-right", "crop-top", "crop-bottom"];

/// videocrop element properties (the CPU-backend crop carrier).
pub(crate) const VIDEOCROP_PROPS: [&str; 4] = ["left", "right", "top", "bottom"];

/// The `videocrop` element feeding `pad`, if the branch has one (CPU backend).
pub(crate) fn upstream_videocrop(pad: &gst::Pad) -> Option<gst::Element> {
    let peer = pad.peer()?;
    let parent = peer.parent_element()?;
    (parent.factory()?.name() == "videocrop").then_some(parent)
}

/// Uncropped source dimensions feeding `pad`.
///
/// GL path: the pad's own negotiated caps (crop is internal, caps stay at
/// source size). CPU path: the upstream `videocrop`'s SINK pad caps — the
/// compositor pad's own caps are post-crop there, and using them would
/// apply the crop window twice in the aspect math.
pub(crate) fn source_dims_for_pad(pad: &gst::Pad) -> Option<(i32, i32)> {
    let caps = match upstream_videocrop(pad) {
        Some(vc) => vc.static_pad("sink")?.current_caps()?,
        None => pad.current_caps()?,
    };
    let s = caps.structure(0)?;
    let w = s.get::<i32>("width").ok()?;
    let h = s.get::<i32>("height").ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Snap-apply a normalized source crop to a compositor sink pad, on either
/// backend.
///
/// GL: writes the pad's `crop-*` properties (clearing stale crop control
/// bindings first — a leftover binding from a crop animation would silently
/// override the writes). CPU: writes the upstream `videocrop`'s properties.
/// Values are NOT rounded to even: videocrop accepts arbitrary offsets (its
/// copy path handles subsampled chroma itself), and crop animations
/// interpolate through odd values per buffer anyway. Without negotiated caps
/// a non-zero crop is skipped — transforms are runtime-only, so a crop
/// always arrives while the pipeline is live, and any input that is actually
/// flowing has negotiated caps.
pub(crate) fn set_pad_crop(pad: &gst::Pad, crop: &strom_types::vision_mixer::SourceCrop) {
    if pad.find_property("crop-left").is_some() {
        // --- GL backend: crop pad properties. ---
        // Neutralize stale crop-animation bindings (keyframe wipe — never
        // removed, see crate::gst::control_bindings).
        crate::gst::control_bindings::wipe_control_bindings(pad.upcast_ref(), &CROP_PAD_PROPS);
        if crop.is_zero() {
            for prop in CROP_PAD_PROPS {
                pad.set_property(prop, 0i32);
            }
            return;
        }
        let Some((src_w, src_h)) = source_dims_for_pad(pad) else {
            debug!(
                "Pad {} has no negotiated caps yet — source crop skipped",
                pad.name()
            );
            return;
        };
        let (l, r, t, b) = crop.to_pixels(src_w, src_h);
        pad.set_property("crop-left", l);
        pad.set_property("crop-right", r);
        pad.set_property("crop-top", t);
        pad.set_property("crop-bottom", b);
        return;
    }

    if let Some(vc) = upstream_videocrop(pad) {
        // --- CPU backend: upstream videocrop element. ---
        // Neutralize stale crop-animation bindings first — they would
        // silently override the writes below. videocrop syncs control
        // values on EVERY buffer, so the keyframe-wipe protocol (never
        // remove a binding) is what keeps this from racing the streaming
        // thread — see crate::gst::control_bindings.
        crate::gst::control_bindings::wipe_control_bindings(vc.upcast_ref(), &VIDEOCROP_PROPS);
        if crop.is_zero() {
            for prop in VIDEOCROP_PROPS {
                vc.set_property(prop, 0i32);
            }
            return;
        }
        let Some((src_w, src_h)) = source_dims_for_pad(pad) else {
            debug!(
                "Pad {} has no negotiated caps yet — source crop skipped",
                pad.name()
            );
            return;
        };
        let (l, r, t, b) = crop.to_pixels(src_w, src_h);
        vc.set_property("left", l);
        vc.set_property("right", r);
        vc.set_property("top", t);
        vc.set_property("bottom", b);
        return;
    }

    if !crop.is_zero() {
        warn!(
            "Compositor pad {} has neither crop properties nor an upstream videocrop — source crop ignored",
            pad.name()
        );
    }
}
