//! Reactive explicit geometry for the vision mixer.
//!
//! All input video pads run `sizing-policy=none` and the layout code
//! aspect-fits every rect itself (see `aspect_fit_rect` in strom-types for
//! why — the compositor's `keep-aspect-ratio` cannot be combined with pad
//! crop, and flipping the policy enum mid-transition snaps visibly).
//!
//! Explicit geometry is static at apply time, so something must react when
//! an input's caps arrive (preroll) or change (mid-stream resolution switch):
//! this module installs a CAPS event probe on every input video sink pad of
//! both compositors and re-applies aspect-correct geometry from the current
//! overlay state. Event probes fire on caps negotiation only — they are NOT
//! per-buffer probes.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::InterpolationControlSource;
use std::sync::atomic::Ordering;
use std::time::Duration;
use strom_types::vision_mixer::{self, aspect_fit_rect, SourceAspects};
use tracing::debug;

use super::overlay;
use crate::gst::pipeline::effects::{
    apply_input_group_to_region, apply_pip_layout_to_region, find_pad,
};
use crate::gst::underlay::UnderlayCtx;
use strom_types::FlowId;

/// Watch the caps of every input video sink pad of the dist and multiview
/// compositors. Each caps arrival/change triggers a full geometry refresh
/// from the current overlay state.
///
/// Must run after linking (request pads exist). Elements are captured as
/// `WeakRef`s in the handlers — pads own their handlers, and a strong
/// element reference would create a cycle that leaks the pipeline.
pub fn install_caps_probes(
    flow_id: FlowId,
    block_id: &str,
    mixer: &gst::Element,
    mv_comp: &gst::Element,
    num_inputs: usize,
    num_pips: usize,
) {
    // All pads that carry raw input video: dist sink_0..N-1, mv thumbnails
    // sink_0..N-1, mv PVW candidates sink_{N+1..2N}, mv PiP candidates.
    let mut pads: Vec<gst::Pad> = Vec::new();
    for i in 0..num_inputs {
        pads.extend(find_pad(mixer, &format!("sink_{}", i)));
        pads.extend(find_pad(mv_comp, &format!("sink_{}", i)));
        pads.extend(find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + i)));
        for p in 0..num_pips {
            let idx = 2 * num_inputs + 1 + p * num_inputs + i;
            pads.extend(find_pad(mv_comp, &format!("sink_{}", idx)));
        }
    }

    let installed = pads.len();
    for pad in pads {
        let block_id = block_id.to_string();
        let mixer_weak = mixer.downgrade();
        let mv_weak = mv_comp.downgrade();
        // `notify::caps` fires once the pad has stored the new caps; a caps
        // EVENT probe fires before, so a refresh queued from one can read the
        // old size and leave the rect in the old shape.
        pad.connect_notify(Some("caps"), move |pad, _| {
            // Caps are cleared on deactivation; nothing to fit.
            if pad.current_caps().is_none() {
                return;
            }
            // Defer the refresh to the glib main loop instead of running it
            // inline: sibling branches negotiate concurrently on their own
            // streaming threads, and two notifies of the same input firing
            // simultaneously can each miss the other's caps, leaving that
            // input's thumbnail stretched forever. Serialized on the main
            // context, the last refresh always sees every negotiated pad.
            let block_id = block_id.clone();
            let mixer_weak = mixer_weak.clone();
            let mv_weak = mv_weak.clone();
            gst::glib::idle_add_once(move || {
                // Pipeline teardown in progress → nothing to refresh.
                let (Some(mixer), Some(mv_comp)) = (mixer_weak.upgrade(), mv_weak.upgrade()) else {
                    return;
                };
                refresh_geometry(flow_id, &block_id, &mixer, &mv_comp);
            });
        });
    }
    debug!(
        "Vision mixer {}: watching caps on {} pads for reactive geometry",
        block_id, installed
    );
}

/// Read the UNCROPPED source `(width, height)` feeding a sink pad. On the
/// CPU backend the pad's own caps are post-videocrop; this walks past it.
fn pad_caps_dims(element: &gst::Element, pad_name: &str) -> Option<(i32, i32)> {
    let pad = find_pad(element, pad_name)?;
    crate::gst::crop::source_dims_for_pad(&pad)
}

/// Re-apply aspect-correct geometry (and crop pixel values) for every input
/// video pad from the current overlay state. Idempotent — safe to run on
/// every caps event.
fn refresh_geometry(flow_id: FlowId, block_id: &str, mixer: &gst::Element, mv_comp: &gst::Element) {
    let Some(state) = overlay::get_overlay_state(&flow_id, block_id) else {
        return;
    };
    let n = state.num_inputs;

    // Per-input UNCROPPED source dims from the dist mixer pads' negotiated
    // caps (refreshes run deferred on the main loop, so the triggering caps
    // event has been stored as a sticky event by the time we read it). The
    // mv thumbnail pad serves as fallback — same tee, same dimensions.
    let dims: Vec<Option<(i32, i32)>> = (0..n)
        .map(|i| {
            pad_caps_dims(mixer, &format!("sink_{}", i))
                .or_else(|| pad_caps_dims(mv_comp, &format!("sink_{}", i)))
        })
        .collect();

    // Skip when no source actually changed dimensions. On the CPU backend
    // every animated videocrop step renegotiates the compositor sink caps —
    // refreshing on each of those events would clear control bindings and
    // kill the very animation producing them. Source dims are read upstream
    // of videocrop, so crop-induced caps events never count as changes.
    let dims_changed = state.update_input_dims(&dims);
    if !dims_changed && !state.layout_refit_pending.load(Ordering::Relaxed) {
        return;
    }

    // A take, FTB or DSK fade animates pad properties through control
    // bindings, and re-applying a layout wipes them: the animation would
    // end as a cut. Leave an animating compositor alone and re-fit it once
    // the animation is over. FTB off re-fits the dist mixer itself.
    let ftb = state.ftb_active.load(Ordering::Relaxed);
    let dist_animation = animation_remaining(mixer);
    let mv_animation = animation_remaining(mv_comp);
    let dist_busy = ftb || dist_animation.is_some();
    let mv_busy = mv_animation.is_some();
    state
        .layout_refit_pending
        .store(dist_busy || mv_busy, Ordering::Relaxed);
    let retry = [dist_animation, mv_animation].into_iter().flatten().max();
    if let Some(wait) = retry {
        refit_after(flow_id, block_id, mixer, mv_comp, wait + REFIT_MARGIN);
    }

    let mut aspects = SourceAspects::new();
    for (i, d) in dims.iter().enumerate() {
        if let Some((w, h)) = d {
            aspects.insert(i, *w as f64 / *h as f64);
        }
    }

    // Dist canvas size from the mixer's negotiated output caps, or the PGM
    // size it is built to produce: the first refresh can run before the
    // output has negotiated, and no later one runs unless an input changes.
    let (cw, ch) = pad_caps_dims_src(mixer).unwrap_or((state.pgm_w as i32, state.pgm_h as i32));
    let fallback = if ch > 0 {
        cw as f64 / ch as f64
    } else {
        16.0 / 9.0
    };

    let pgm_w = state.pgm_w.max(1) as f64;
    let dist_underlay = state.dist_underlay_base().map(|base| UnderlayCtx {
        base,
        scale: cw as f64 / pgm_w,
    });
    let pvw_underlay = state.mv_pvw_underlay_base().map(|base| UnderlayCtx {
        base,
        scale: state.layout.pvw_rect.w / pgm_w,
    });

    // --- Dist compositor (PGM). ---
    if !dist_busy {
        if let Some(p) = state.pgm_pip() {
            apply_pip_layout_to_region(
                mixer,
                0,
                n,
                (0, 0, cw, ch),
                state.pip_bg_input(p),
                &state.pip_zones(p),
                &state.pip_transforms(p),
                vision_mixer::DIST_PGM_ZORDER,
                vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                fallback,
                &aspects,
                dist_underlay,
            );
        } else {
            apply_input_group_to_region(
                mixer,
                0,
                n,
                (0, 0, cw, ch),
                state.pgm_input(),
                vision_mixer::DIST_PGM_ZORDER,
                fallback,
                &aspects,
                dist_underlay,
            );
        }
    }

    // --- Multiview thumbnails: always visible, aspect-fitted per source. ---
    for i in 0..n {
        let Some(pad) = find_pad(mv_comp, &format!("sink_{}", i)) else {
            continue;
        };
        let (tx, ty, tw, th) = super::layout::thumbnail_pad_position(&state.layout, i);
        let aspect = aspects.get(&i).copied().unwrap_or(fallback);
        let (x, y, w, h) = aspect_fit_rect(tx, ty, tw, th, aspect);
        pad.set_property("xpos", x);
        pad.set_property("ypos", y);
        pad.set_property("width", w);
        pad.set_property("height", h);
    }

    if mv_busy {
        return;
    }

    // --- Multiview PVW big region. ---
    let r = &state.layout.pvw_rect;
    let pvw_region = (r.x as i32, r.y as i32, r.w as i32, r.h as i32);
    if let Some(p) = state.pvw_pip() {
        apply_pip_layout_to_region(
            mv_comp,
            n + 1,
            n,
            pvw_region,
            state.pip_bg_input(p),
            &state.pip_zones(p),
            &state.pip_transforms(p),
            vision_mixer::MV_BIG_DISPLAY_ZORDER,
            vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
            fallback,
            &aspects,
            pvw_underlay,
        );
    } else {
        apply_input_group_to_region(
            mv_comp,
            n + 1,
            n,
            pvw_region,
            state.pvw_input(),
            vision_mixer::MV_BIG_DISPLAY_ZORDER,
            fallback,
            &aspects,
            pvw_underlay,
        );
    }

    // --- Multiview PiP tiles (always rendered). ---
    for p in 0..state.num_pips {
        let Some(tile) = state.layout.pip_tile_rects.get(p) else {
            continue;
        };
        let region = (tile.x as i32, tile.y as i32, tile.w as i32, tile.h as i32);
        let tile_underlay = state.mv_pip_underlay_base(p).map(|base| UnderlayCtx {
            base,
            scale: tile.w / pgm_w,
        });
        apply_pip_layout_to_region(
            mv_comp,
            2 * n + 1 + p * n,
            n,
            region,
            state.pip_bg_input(p),
            &state.pip_zones(p),
            &state.pip_transforms(p),
            vision_mixer::MV_PIP_BG_ZORDER,
            vision_mixer::MV_PIP_OVERLAY_ZORDER,
            fallback,
            &aspects,
            tile_underlay,
        );
    }

    debug!(
        "Vision mixer {}: geometry refreshed ({} known input aspects)",
        block_id,
        aspects.len()
    );
}

/// Pad properties that takes, FTB and layout code animate.
const ANIMATED_PAD_PROPS: [&str; 6] = ["alpha", "xpos", "ypos", "width", "height", "zorder"];

/// Slack after an animation's last keyframe before re-fitting.
const REFIT_MARGIN: Duration = Duration::from_millis(50);

/// How long until the last keyframe programmed on any of `compositor`'s sink
/// pads, or `None` when none lies ahead. Keyframes are in the compositor's
/// stream time, which is what its position query reports.
fn animation_remaining(compositor: &gst::Element) -> Option<Duration> {
    let now = compositor.query_position::<gst::ClockTime>()?;
    compositor
        .sink_pads()
        .iter()
        .flat_map(|pad| {
            ANIMATED_PAD_PROPS
                .iter()
                .filter_map(move |prop| last_keyframe(pad, prop))
        })
        .max()
        .filter(|end| *end > now)
        .map(|end| Duration::from_nanos((end - now).nseconds()))
}

fn last_keyframe(pad: &gst::Pad, prop: &str) -> Option<gst::ClockTime> {
    let cs = pad
        .control_binding(prop)?
        .property::<Option<gst::ControlSource>>("control-source")?
        .downcast::<InterpolationControlSource>()
        .ok()?;
    cs.list_control_points().last().map(|tv| tv.timestamp())
}

/// Run a geometry refresh after `wait`, on the main loop.
fn refit_after(
    flow_id: FlowId,
    block_id: &str,
    mixer: &gst::Element,
    mv_comp: &gst::Element,
    wait: Duration,
) {
    let block_id = block_id.to_string();
    let mixer_weak = mixer.downgrade();
    let mv_weak = mv_comp.downgrade();
    gst::glib::timeout_add_once(wait, move || {
        // Pipeline teardown in progress → nothing to refresh.
        let (Some(mixer), Some(mv_comp)) = (mixer_weak.upgrade(), mv_weak.upgrade()) else {
            return;
        };
        refresh_geometry(flow_id, &block_id, &mixer, &mv_comp);
    });
}

/// Read `(width, height)` from an element's `src` pad caps.
fn pad_caps_dims_src(element: &gst::Element) -> Option<(i32, i32)> {
    let pad = element.static_pad("src")?;
    let caps = pad.current_caps()?;
    let s = caps.structure(0)?;
    let w = s.get::<i32>("width").ok()?;
    let h = s.get::<i32>("height").ok()?;
    (w > 0 && h > 0).then_some((w, h))
}
