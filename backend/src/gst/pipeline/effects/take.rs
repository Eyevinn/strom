//! The PGM take engine — `trigger_transition` with the PiP-aware path
//! (cut/fade across compositor regions) and the classic single-input path.

use super::super::{PipelineError, PipelineManager};
use gstreamer::prelude::*;
use tracing::{debug, info};

use crate::gst::crop::set_pad_crop;

use super::mixer_layout::{
    apply_input_group_to_region, apply_pip_crop_after_morph, apply_pip_layout_to_region, find_pad,
    pads_for_source,
};

impl PipelineManager {
    /// Trigger a transition on a compositor/mixer block.
    ///
    /// Reads the authoritative PGM/PVW source from overlay state. When either
    /// bus is a PiP, takes the source-aware path that animates pads across
    /// dist + multiview compositors; otherwise runs a standard single-pad
    /// transition on the mv mixer.
    ///
    /// Returns `(was_ftb_cancelled, old_pgm, new_pgm, actual_kind)`. The two
    /// middle elements are `None` when the corresponding bus is a PiP source.
    /// `actual_kind` is the transition that actually ran — differs from
    /// `transition_type` when the engine downgraded the request (e.g. Slide
    /// across heterogeneous PiP/input sources downgrades to "fade").
    pub fn trigger_transition(
        &self,
        block_instance_id: &str,
        from_input: usize,
        to_input: usize,
        transition_type: &str,
        duration_ms: u64,
    ) -> Result<(bool, Option<usize>, Option<usize>, String), PipelineError> {
        use crate::gst::transitions::{TransitionController, TransitionType};

        debug!(
            "Triggering {} transition on {} from input {} to {} ({}ms)",
            transition_type, block_instance_id, from_input, to_input, duration_ms
        );

        // Find the mixer element for this block
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;

        // Read authoritative PGM/PVW source from overlay state. `None` means
        // the bus is a PiP — handled by the PiP-aware branch below.
        let overlay_state =
            crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id);
        let num_video_inputs = overlay_state
            .as_ref()
            .map(|s| s.num_inputs)
            .unwrap_or(usize::MAX);
        // When overlay state is missing entirely, fall back to the request-
        // provided indices. When state exists but reports None (PiP on bus),
        // pass None through so the PiP-aware branch below handles it.
        let old_pgm: Option<usize> = match overlay_state.as_ref() {
            Some(s) => s.pgm_input(),
            None => Some(from_input),
        };
        let new_pgm: Option<usize> = match overlay_state.as_ref() {
            Some(s) => s.pvw_input(),
            None => Some(to_input),
        };

        // Auto-cancel FTB if active
        let was_ftb = overlay_state
            .as_ref()
            .map(|s| {
                s.ftb_active
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(false);
        if was_ftb {
            info!(
                "Auto-cancelling FTB before transition on {}",
                block_instance_id
            );
            // FTB bound the PGM graphics overlay pad's alpha (fading zone
            // borders toward 0 along with the picture). Cancelling only flips
            // the flag — strip that binding and restore full alpha here, or
            // borders stay frozen at whatever the fade last wrote. Covers
            // both the classic and the PiP-aware take paths.
            if let Some(state) = overlay_state.as_ref() {
                let overlay_pad_name = format!("sink_{}", state.num_inputs + state.num_dsk_inputs);
                if let Some(pad) = find_pad(mixer, &overlay_pad_name) {
                    if let Some(binding) = pad.control_binding("alpha") {
                        pad.remove_control_binding(&binding);
                    }
                    pad.set_property("alpha", 1.0f64);
                }
            }
        }

        let (canvas_width, canvas_height) = self.dist_canvas_size(block_instance_id);

        // --- PiP-aware Take ---
        // If either bus is currently a PiP (or will become one via swap), use a
        // Source-aware path. Cut snaps geometry+alpha; Fade cross-fades alphas
        // across both compositor regions (dist for PGM, mv PVW for PVW).
        // Slide/Push/Dip-to-Black with PiP downgrade to Fade — explicit position
        // animation across heterogeneous Source kinds isn't supported yet.
        if let Some(state) = overlay_state.as_ref() {
            let old_pgm_pip = state.pgm_pip();
            let old_pvw_pip = state.pvw_pip();
            if old_pgm_pip.is_some() || old_pvw_pip.is_some() {
                let mv_comp_id = format!("{}:mv_comp", block_instance_id);
                let mv_comp = self
                    .elements
                    .get(&mv_comp_id)
                    .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

                let parsed = transition_type.parse::<TransitionType>().ok();
                let is_cut = duration_ms == 0 || matches!(parsed, Some(TransitionType::Cut));
                // Explicit position animation across heterogeneous Source kinds
                // (input ↔ PiP) isn't supported yet — Slide/Push/Dip-to-Black
                // silently degrade to Fade in this branch. Surface that in the
                // log so operators don't think the requested transition ran.
                if !is_cut
                    && !matches!(
                        parsed,
                        Some(TransitionType::Fade) | Some(TransitionType::Cut)
                    )
                {
                    info!(
                        "PiP-aware Take on {}: transition '{}' downgraded to Fade ({}ms) — non-Fade transitions across PiPs not supported yet",
                        block_instance_id, transition_type, duration_ms
                    );
                }

                // Swap Source state PVW ↔ PGM.
                let new_pgm_pip = old_pvw_pip;
                let new_pvw_pip = old_pgm_pip;
                let new_pgm_swap = state.pvw_input();
                let new_pvw_swap = state.pgm_input();

                let dist_region = (0, 0, canvas_width, canvas_height);
                let r = &state.layout.pvw_rect;
                let pvw_region = (r.x as i32, r.y as i32, r.w as i32, r.h as i32);
                // Fallback aspect = PGM canvas aspect (typically 16:9), used
                // for tile-grid cells and for inputs whose caps are unknown.
                let src_aspect = if canvas_height > 0 {
                    canvas_width as f64 / canvas_height as f64
                } else {
                    16.0 / 9.0
                };
                let src_aspects =
                    self.vision_mixer_source_aspects(block_instance_id, state.num_inputs);

                let old_dist_targets = pads_for_source(
                    state,
                    old_pgm_pip,
                    old_pgm,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                );
                let new_dist_targets = pads_for_source(
                    state,
                    new_pgm_pip,
                    new_pgm_swap,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                );
                let old_pvw_targets = pads_for_source(
                    state,
                    old_pvw_pip,
                    state.pvw_input(),
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                );
                let new_pvw_targets = pads_for_source(
                    state,
                    new_pvw_pip,
                    new_pvw_swap,
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
                    &src_aspects,
                );

                if is_cut {
                    // Snap apply: hide everything in the region, then set new active pads.
                    if let Some(p) = new_pgm_pip {
                        apply_pip_layout_to_region(
                            mixer,
                            0,
                            state.num_inputs,
                            dist_region,
                            state.pip_bg_input(p),
                            &state.pip_zones(p),
                            &state.pip_transforms(p),
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                            strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                        );
                    } else {
                        apply_input_group_to_region(
                            mixer,
                            0,
                            state.num_inputs,
                            dist_region,
                            new_pgm_swap,
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                            src_aspect,
                            &src_aspects,
                        );
                    }
                    if let Some(p) = new_pvw_pip {
                        apply_pip_layout_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            state.pip_bg_input(p),
                            &state.pip_zones(p),
                            &state.pip_transforms(p),
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                            strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                        );
                    } else {
                        apply_input_group_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            new_pvw_swap,
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                            src_aspect,
                            &src_aspects,
                        );
                    }
                } else {
                    // Morph transition: sources shared between old and new
                    // animate position+size; fresh sources alpha-fade in; departing
                    // sources alpha-fade out. Runs in parallel on both dist (PGM)
                    // and mv PVW big regions.
                    let dist_controller =
                        TransitionController::new(mixer.clone(), canvas_width, canvas_height);
                    dist_controller
                        .animate_pad_transition(
                            &old_dist_targets,
                            &new_dist_targets,
                            duration_ms,
                            &self.pipeline,
                        )
                        .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

                    let mv_controller = TransitionController::new(
                        mv_comp.clone(),
                        state.layout.canvas_width as i32,
                        state.layout.canvas_height as i32,
                    );
                    mv_controller
                        .animate_pad_transition(
                            &old_pvw_targets,
                            &new_pvw_targets,
                            duration_ms,
                            &self.pipeline,
                        )
                        .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

                    // Apply per-source crop for the incoming compositions:
                    // pads shared between old and new (e.g. a source that is
                    // fullscreen on PGM and cropped in the incoming PiP)
                    // animate their crop in sync with the geometry morph;
                    // fresh pads snap; fading-out pads keep their crop until
                    // hidden. An input-mode side uses an empty transform map,
                    // so shared pads animate their punch-out back to no crop.
                    let new_pgm_transforms = new_pgm_pip
                        .map(|p| state.pip_transforms(p))
                        .unwrap_or_default();
                    apply_pip_crop_after_morph(
                        mixer,
                        &dist_controller,
                        &self.pipeline,
                        &new_pgm_transforms,
                        state.num_inputs,
                        0,
                        &old_dist_targets,
                        &new_dist_targets,
                        duration_ms,
                    );
                    let new_pvw_transforms = new_pvw_pip
                        .map(|p| state.pip_transforms(p))
                        .unwrap_or_default();
                    apply_pip_crop_after_morph(
                        mv_comp,
                        &mv_controller,
                        &self.pipeline,
                        &new_pvw_transforms,
                        state.num_inputs,
                        state.num_inputs + 1,
                        &old_pvw_targets,
                        &new_pvw_targets,
                        duration_ms,
                    );
                }

                // Persist new state.
                state.set_pgm_pip(new_pgm_pip);
                state.set_pvw_pip(new_pvw_pip);
                state.set_pgm_input(new_pgm_swap);
                state.set_pvw_input(new_pvw_swap);
                crate::blocks::builtin::vision_mixer::overlay::trigger_overlay_update(
                    block_instance_id,
                );

                info!(
                    "PiP-aware Take on {} ({}{}ms): PGM pip={:?} input={:?}; PVW pip={:?} input={:?}",
                    block_instance_id,
                    if is_cut { "Cut, " } else { "Fade, " },
                    duration_ms,
                    new_pgm_pip,
                    new_pgm_swap,
                    new_pvw_pip,
                    new_pvw_swap,
                );
                // After the downgrade guard above, the PiP-aware branch only
                // ever runs a Cut or a Fade.
                let actual_kind = if is_cut { "cut" } else { "fade" }.to_string();
                return Ok((was_ftb, old_pgm, new_pgm_swap, actual_kind));
            }
        }

        // Reset all video pads to a clean state before the transition:
        // clear control bindings, restore alpha/position/size for the current
        // PGM input. Single-input PGM only (multi-source compositions are PiPs).
        let active_pgm_input = old_pgm;
        let classic_aspects = self.vision_mixer_source_aspects(
            block_instance_id,
            if num_video_inputs == usize::MAX {
                0
            } else {
                num_video_inputs
            },
        );
        let canvas_aspect = if canvas_height > 0 {
            canvas_width as f64 / canvas_height as f64
        } else {
            16.0 / 9.0
        };

        for pad in mixer.sink_pads() {
            let name = pad.name();
            if name.starts_with("sink_") {
                if let Ok(idx) = name.trim_start_matches("sink_").parse::<usize>() {
                    for prop in ["alpha", "xpos", "ypos", "width", "height"] {
                        if let Some(binding) = pad.control_binding(prop) {
                            pad.remove_control_binding(&binding);
                        }
                    }
                    if idx < num_video_inputs {
                        // Classic takes are input↔input — wipe any crop left
                        // behind by an earlier PiP render on these pads.
                        set_pad_crop(&pad, &Default::default());
                        // Explicit geometry: aspect-fit the source into the
                        // canvas (pads run sizing-policy=none).
                        let aspect = classic_aspects.get(&idx).copied().unwrap_or(canvas_aspect);
                        let (x, y, w, h) = strom_types::vision_mixer::aspect_fit_rect(
                            0,
                            0,
                            canvas_width,
                            canvas_height,
                            aspect,
                        );
                        pad.set_property("xpos", x);
                        pad.set_property("ypos", y);
                        pad.set_property("width", w);
                        pad.set_property("height", h);
                        if Some(idx) == active_pgm_input {
                            pad.set_property("alpha", 1.0f64);
                            pad.set_property("zorder", strom_types::vision_mixer::DIST_PGM_ZORDER);
                        } else {
                            pad.set_property("alpha", 0.0f64);
                        }
                    } else if let Some(state) = overlay_state.as_ref() {
                        let dsk_idx = idx - num_video_inputs;
                        if dsk_idx < state.dsk_enabled.len() {
                            let enabled = state.dsk_enabled[dsk_idx]
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let alpha = if enabled { 1.0f64 } else { 0.0f64 };
                            pad.set_property("alpha", alpha);
                        } else {
                            // The PGM graphics overlay pad: its content is
                            // state-driven, but the loop above just stripped
                            // any alpha binding (e.g. a mid-flight FTB-off
                            // restore fade) — a take always lands with FTB
                            // off, so pin the pad back to full alpha.
                            pad.set_property("alpha", 1.0f64);
                        }
                    }
                }
            }
        }

        // Parse transition type
        let trans_type = transition_type.parse::<TransitionType>().map_err(|_| {
            PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "transition_type".to_string(),
                reason: format!("Unknown transition type: {}", transition_type),
            }
        })?;

        // Single-input transition only. Multi-source compositions are now
        // expressed as PiPs which are handled by the PiP-aware branch above.
        let from = old_pgm.unwrap_or(from_input);
        let to = new_pgm.unwrap_or(to_input);
        let controller = TransitionController::new(mixer.clone(), canvas_width, canvas_height);
        controller
            .transition(from, to, trans_type, duration_ms, &self.pipeline)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

        let actual_kind = if duration_ms == 0 {
            TransitionType::Cut.to_string()
        } else {
            trans_type.to_string()
        };
        Ok((was_ftb, old_pgm, new_pgm, actual_kind))
    }
}
