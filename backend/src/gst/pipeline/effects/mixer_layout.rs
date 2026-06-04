//! Pad geometry + crop appliers for the vision mixer compositors.
//!
//! Shared by the take engine, the mixer runtime ops, and the reactive
//! geometry caps probe. Explicit geometry: pads run `sizing-policy=none`
//! and every rect here is aspect-fitted with the caller-supplied per-source
//! aspects (see `aspect_fit_rect` in strom-types).

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::warn;

/// Find a pad by name on an element, checking both static and request pads.
/// `static_pad()` doesn't find request pads on aggregator elements like glvideomixer.
pub(crate) fn find_pad(element: &gst::Element, pad_name: &str) -> Option<gst::Pad> {
    element.static_pad(pad_name).or_else(|| {
        element
            .pads()
            .into_iter()
            .find(|p| p.name().as_str() == pad_name)
    })
}

use crate::gst::crop::set_pad_crop;
use crate::gst::underlay::{set_underlay_color, underlay_rect, UnderlayCtx};

/// Remove any lingering control bindings (from a previous morph/fade) on the
/// properties layout code writes — a stale binding would silently override
/// `set_property`.
pub(super) fn clear_layout_bindings(pad: &gst::Pad) {
    for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
        if let Some(binding) = pad.control_binding(prop) {
            pad.remove_control_binding(&binding);
        }
    }
}

/// Snap an underlay pad to its target: positioned, colored, visible,
/// directly beneath its content pad in z-order.
pub(super) fn show_underlay(
    compositor: &gst::Element,
    t: &crate::gst::transitions::UnderlayTarget,
    content_zorder: u32,
) {
    let Some(pad) = find_pad(compositor, &format!("sink_{}", t.pad_idx)) else {
        return;
    };
    clear_layout_bindings(&pad);
    set_underlay_color(&pad, t.argb);
    pad.set_property("xpos", t.x);
    pad.set_property("ypos", t.y);
    pad.set_property("width", t.w);
    pad.set_property("height", t.h);
    pad.set_property(
        "zorder",
        strom_types::vision_mixer::underlay_zorder(content_zorder),
    );
    pad.set_property("alpha", 1.0f64);
}

/// Hide an underlay pad (no border on its content pad right now).
pub(super) fn hide_underlay(compositor: &gst::Element, pad_idx: usize) {
    let Some(pad) = find_pad(compositor, &format!("sink_{}", pad_idx)) else {
        return;
    };
    clear_layout_bindings(&pad);
    pad.set_property("alpha", 0.0f64);
}

/// Apply per-source crop to a region after a zone morph: pads present in both
/// old and new layouts animate crop alongside the geometry morph; pads about
/// to be revealed snap to their crop; fading-out pads keep theirs until hidden.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_pip_crop_after_morph(
    compositor: &gst::Element,
    ctrl: &crate::gst::transitions::TransitionController,
    pipeline: &gst::Pipeline,
    transforms: &strom_types::vision_mixer::PipTransforms,
    num_inputs: usize,
    pad_base: usize,
    old: &[crate::gst::transitions::PadTarget],
    new: &[crate::gst::transitions::PadTarget],
    duration_ms: u64,
) {
    let old_set: std::collections::HashSet<usize> = old.iter().map(|t| t.pad_idx).collect();
    let new_set: std::collections::HashSet<usize> = new.iter().map(|t| t.pad_idx).collect();
    for i in 0..num_inputs {
        let pad_idx = pad_base + i;
        let crop = transforms.get(&i).copied().unwrap_or_default();
        if old_set.contains(&pad_idx) && new_set.contains(&pad_idx) {
            // Staying pad — ease the crop in sync with the geometry morph
            // ("punch-in" zoom). No-op on the CPU backend.
            if let Err(e) = ctrl.animate_pad_crop(pad_idx, &crop, duration_ms, pipeline) {
                warn!("Crop morph failed on pad {}: {}", pad_idx, e);
            }
        } else if old_set.contains(&pad_idx) {
            // Departing pad — keeps its crop while fading out; whichever path
            // reveals it next re-applies the correct crop.
        } else if let Some(pad) = find_pad(compositor, &format!("sink_{}", pad_idx)) {
            set_pad_crop(&pad, &crop);
        }
    }
}

/// Compute per-pad targets `(pad_idx, x, y, w, h, zorder)` for a Source rendered
/// into a compositor region. `pad_base` is the index offset of the first input
/// pad in the region (e.g. 0 for dist_comp, num_inputs+1 for mv_comp PVW big).
///
/// Geometry is explicit (pads run `sizing-policy=none`): every rect is
/// aspect-fitted with the source's effective aspect (crop-adjusted), so an
/// odd-aspect source letterboxes correctly and a locked crop fills its box.
///
/// When `underlay` is given, zone sources in bordered zones also carry an
/// [`crate::gst::transitions::UnderlayTarget`] for their border underlay pad.
#[allow(clippy::too_many_arguments)]
pub(super) fn pads_for_source(
    state: &crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState,
    pip: Option<usize>,
    input: Option<usize>,
    region: (i32, i32, i32, i32),
    pad_base: usize,
    bg_zorder: u32,
    overlay_zorder: u32,
    fallback_aspect: f64,
    src_aspects: &strom_types::vision_mixer::SourceAspects,
    underlay: Option<UnderlayCtx>,
) -> Vec<crate::gst::transitions::PadTarget> {
    use crate::gst::transitions::{PadTarget, UnderlayTarget};
    use strom_types::vision_mixer::{aspect_fit_rect, effective_source_aspect};
    let (rx, ry, rw, rh) = region;
    if let Some(p) = pip {
        let bg = state.pip_bg_input(p);
        let zones = state.pip_zones(p);
        let transforms = state.pip_transforms(p);
        let layouts = strom_types::vision_mixer::resolve_zone_pads(
            rx,
            ry,
            rw,
            rh,
            &zones,
            fallback_aspect,
            &transforms,
            src_aspects,
        );
        // Defensive dedupe: `apply_vision_mixer_pip_config` already filters bg
        // out of zone sources, but the on-disk state could become inconsistent
        // (e.g. legacy config or future code that bypasses the API). Two
        // PadTargets with the same `pad_idx` would confuse the transition
        // planner — keep the bg entry, drop any overlapping zone source.
        let mut out = Vec::new();
        let mut seen_pad_idxs = std::collections::HashSet::new();
        if let Some(b) = bg {
            let aspect = effective_source_aspect(
                src_aspects.get(&b).copied().unwrap_or(fallback_aspect),
                transforms.get(&b),
            );
            let (x, y, w, h) = aspect_fit_rect(rx, ry, rw, rh, aspect);
            out.push(PadTarget {
                pad_idx: pad_base + b,
                x,
                y,
                w,
                h,
                zorder: bg_zorder,
                underlay: None,
            });
            seen_pad_idxs.insert(pad_base + b);
        }
        for l in &layouts {
            let pad_idx = pad_base + l.input;
            if !seen_pad_idxs.insert(pad_idx) {
                continue;
            }
            out.push(PadTarget {
                pad_idx,
                x: l.x,
                y: l.y,
                w: l.w,
                h: l.h,
                zorder: strom_types::vision_mixer::zone_content_zorder(
                    overlay_zorder,
                    l.zorder_offset,
                ),
                underlay: match (underlay, l.border) {
                    (Some(u), Some(b)) => {
                        let (ux, uy, uw, uh) =
                            underlay_rect((l.x, l.y, l.w, l.h), b.width, region, u.scale);
                        Some(UnderlayTarget {
                            pad_idx: u.base + l.input,
                            x: ux,
                            y: uy,
                            w: uw,
                            h: uh,
                            argb: b.argb,
                        })
                    }
                    _ => None,
                },
            });
        }
        out
    } else {
        // Single-input source (multi-source compositions are PiPs now).
        // Plain inputs never crop — fit with the raw source aspect.
        input
            .map(|idx| {
                let aspect = src_aspects.get(&idx).copied().unwrap_or(fallback_aspect);
                let (x, y, w, h) = aspect_fit_rect(rx, ry, rw, rh, aspect);
                PadTarget {
                    pad_idx: pad_base + idx,
                    x,
                    y,
                    w,
                    h,
                    zorder: bg_zorder,
                    underlay: None,
                }
            })
            .into_iter()
            .collect()
    }
}

/// Apply a single-input source layout (aspect-fitted into the region) to a
/// contiguous range of compositor sink pads. Pads not equal to the active
/// input are hidden. Plain inputs never have borders, so the region's
/// underlay pads (when it has any) are all hidden too.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_input_group_to_region(
    compositor: &gst::Element,
    pad_base: usize,
    num_inputs: usize,
    region: (i32, i32, i32, i32),
    active: Option<usize>,
    fg_zorder: u32,
    fallback_aspect: f64,
    src_aspects: &strom_types::vision_mixer::SourceAspects,
    underlay: Option<UnderlayCtx>,
) {
    let (rx, ry, rw, rh) = region;
    for i in 0..num_inputs {
        let pad_name = format!("sink_{}", pad_base + i);
        let Some(pad) = find_pad(compositor, &pad_name) else {
            continue;
        };
        // Plain-input mode never crops — wipe any crop left by a PiP render.
        set_pad_crop(&pad, &Default::default());
        // Clear any lingering control bindings from a previous morph/fade so
        // our set_property writes aren't silently overridden by stale values.
        clear_layout_bindings(&pad);
        if Some(i) == active {
            let aspect = src_aspects.get(&i).copied().unwrap_or(fallback_aspect);
            let (x, y, w, h) = strom_types::vision_mixer::aspect_fit_rect(rx, ry, rw, rh, aspect);
            pad.set_property("xpos", x);
            pad.set_property("ypos", y);
            pad.set_property("width", w);
            pad.set_property("height", h);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", fg_zorder);
        } else {
            pad.set_property("alpha", 0.0f64);
        }
        if let Some(u) = underlay {
            hide_underlay(compositor, u.base + i);
        }
    }
}

/// Apply a PiP composition (bg fills the region, overlays auto-tile on top) to a
/// contiguous range of compositor sink pads named `sink_{pad_base..pad_base+num_inputs}`.
///
/// Each pad whose input index matches `bg` becomes the background; each pad whose
/// input is found in `overlays` is positioned at its corresponding tile; all other
/// pads in the range are hidden (alpha=0).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_pip_layout_to_region(
    compositor: &gst::Element,
    pad_base: usize,
    num_inputs: usize,
    region: (i32, i32, i32, i32),
    bg: Option<usize>,
    zones: &[strom_types::vision_mixer::Zone],
    transforms: &strom_types::vision_mixer::PipTransforms,
    bg_zorder: u32,
    overlay_zorder: u32,
    fallback_aspect: f64,
    src_aspects: &strom_types::vision_mixer::SourceAspects,
    underlay: Option<UnderlayCtx>,
) {
    use strom_types::vision_mixer::{aspect_fit_rect, effective_source_aspect};
    let (rx, ry, rw, rh) = region;
    let layouts = strom_types::vision_mixer::resolve_zone_pads(
        rx,
        ry,
        rw,
        rh,
        zones,
        fallback_aspect,
        transforms,
        src_aspects,
    );
    // Fast lookup: input → its zone pad layout (rect + slot + border).
    let layout_map: std::collections::HashMap<usize, &strom_types::vision_mixer::ZonePadLayout> =
        layouts.iter().map(|l| (l.input, l)).collect();

    for i in 0..num_inputs {
        let pad_name = format!("sink_{}", pad_base + i);
        let Some(pad) = find_pad(compositor, &pad_name) else {
            continue;
        };
        // Clear any lingering control bindings from a previous fade — otherwise
        // they keep driving the property and our set_property calls below would
        // be invisible until the next take rebuilds bindings.
        clear_layout_bindings(&pad);
        // Border underlay for this input: shown only for a zone source whose
        // zone has a visible border; hidden otherwise (incl. bg + hidden pads).
        let mut underlay_shown = false;
        if Some(i) == bg {
            let aspect = effective_source_aspect(
                src_aspects.get(&i).copied().unwrap_or(fallback_aspect),
                transforms.get(&i),
            );
            let (x, y, w, h) = aspect_fit_rect(rx, ry, rw, rh, aspect);
            pad.set_property("xpos", x);
            pad.set_property("ypos", y);
            pad.set_property("width", w);
            pad.set_property("height", h);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", bg_zorder);
        } else if let Some(l) = layout_map.get(&i) {
            let content_zorder =
                strom_types::vision_mixer::zone_content_zorder(overlay_zorder, l.zorder_offset);
            pad.set_property("xpos", l.x);
            pad.set_property("ypos", l.y);
            pad.set_property("width", l.w);
            pad.set_property("height", l.h);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", content_zorder);
            if let (Some(u), Some(b)) = (underlay, l.border) {
                let (ux, uy, uw, uh) =
                    underlay_rect((l.x, l.y, l.w, l.h), b.width, region, u.scale);
                show_underlay(
                    compositor,
                    &crate::gst::transitions::UnderlayTarget {
                        pad_idx: u.base + i,
                        x: ux,
                        y: uy,
                        w: uw,
                        h: uh,
                        argb: b.argb,
                    },
                    content_zorder,
                );
                underlay_shown = true;
            }
        } else {
            pad.set_property("alpha", 0.0f64);
        }
        if let Some(u) = underlay {
            if !underlay_shown {
                hide_underlay(compositor, u.base + i);
            }
        }
        // Crop applies to the source wherever it renders in this PiP —
        // including hidden pads, whose retained transform stays staged so the
        // source returns pre-framed. Non-PiP reveal paths wipe crop
        // themselves (apply_input_group_to_region, the classic-take reset).
        let crop = transforms.get(&i).copied().unwrap_or_default();
        set_pad_crop(&pad, &crop);
    }
}
