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
use strom_types::vision_mixer::{self, aspect_fit_rect, SourceAspects};
use tracing::debug;

use super::overlay;
use crate::gst::pipeline::effects::{
    apply_input_group_to_region, apply_pip_layout_to_region, find_pad,
};

/// Install CAPS event probes on every input video sink pad of the dist and
/// multiview compositors. Each caps arrival/change triggers a full geometry
/// refresh from the current overlay state.
///
/// Must run after linking (request pads exist). Elements are captured as
/// `WeakRef`s in the probe closures — pads own their probes, and a strong
/// element reference would create a cycle that leaks the pipeline.
pub fn install_caps_probes(
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
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                if ev.type_() == gst::EventType::Caps {
                    // Defer the refresh to the glib main loop instead of
                    // running it inline: sticky caps are stored on the pad
                    // only AFTER the probes return, and sibling branches
                    // negotiate concurrently on their own streaming threads —
                    // an inline refresh races both, and two probes of the
                    // same input firing simultaneously can each miss the
                    // other's caps, leaving that input's thumbnail stretched
                    // forever. Serialized on the main context, the last
                    // refresh always sees every negotiated pad.
                    let block_id = block_id.clone();
                    let mixer_weak = mixer_weak.clone();
                    let mv_weak = mv_weak.clone();
                    gst::glib::idle_add_once(move || {
                        // Pipeline teardown in progress → nothing to refresh.
                        let (Some(mixer), Some(mv_comp)) =
                            (mixer_weak.upgrade(), mv_weak.upgrade())
                        else {
                            return;
                        };
                        refresh_geometry(&block_id, &mixer, &mv_comp);
                    });
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
    debug!(
        "Vision mixer {}: installed {} caps probes for reactive geometry",
        block_id, installed
    );
}

/// Read `(width, height)` from a pad's negotiated caps.
fn pad_caps_dims(element: &gst::Element, pad_name: &str) -> Option<(i32, i32)> {
    let pad = find_pad(element, pad_name)?;
    let caps = pad.current_caps()?;
    let s = caps.structure(0)?;
    let w = s.get::<i32>("width").ok()?;
    let h = s.get::<i32>("height").ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

/// Re-apply aspect-correct geometry (and crop pixel values) for every input
/// video pad from the current overlay state. Idempotent — safe to run on
/// every caps event.
fn refresh_geometry(block_id: &str, mixer: &gst::Element, mv_comp: &gst::Element) {
    let Some(state) = overlay::get_overlay_state(block_id) else {
        return;
    };
    let n = state.num_inputs;

    // Per-input source aspects from the dist mixer pads' negotiated caps
    // (refreshes run deferred on the main loop, so the triggering caps event
    // has been stored as a sticky event by the time we read it). The mv
    // thumbnail pad serves as fallback — same tee, same dimensions.
    let mut aspects = SourceAspects::new();
    for i in 0..n {
        let dims = pad_caps_dims(mixer, &format!("sink_{}", i))
            .or_else(|| pad_caps_dims(mv_comp, &format!("sink_{}", i)));
        if let Some((w, h)) = dims {
            aspects.insert(i, w as f64 / h as f64);
        }
    }

    // Dist canvas size from the mixer's negotiated output caps.
    let (cw, ch) = pad_caps_dims_src(mixer).unwrap_or_else(|| {
        strom_types::parse_resolution_string(vision_mixer::DEFAULT_PGM_RESOLUTION)
            .map(|(w, h)| (w as i32, h as i32))
            .expect("DEFAULT_PGM_RESOLUTION must be valid")
    });
    let fallback = if ch > 0 {
        cw as f64 / ch as f64
    } else {
        16.0 / 9.0
    };

    // --- Dist compositor (PGM) — skip while FTB drives the alphas. ---
    if !state.ftb_active.load(std::sync::atomic::Ordering::Relaxed) {
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
        );
    }

    // --- Multiview PiP tiles (always rendered). ---
    for p in 0..state.num_pips {
        let Some(tile) = state.layout.pip_tile_rects.get(p) else {
            continue;
        };
        let region = (tile.x as i32, tile.y as i32, tile.w as i32, tile.h as i32);
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
        );
    }

    debug!(
        "Vision mixer {}: geometry refreshed ({} known input aspects)",
        block_id,
        aspects.len()
    );
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
