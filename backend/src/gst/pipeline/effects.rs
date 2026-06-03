use super::{PipelineError, PipelineManager};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info, warn};

impl PipelineManager {
    /// Get the distribution compositor canvas size from its capsfilter.
    fn dist_canvas_size(&self, block_instance_id: &str) -> (i32, i32) {
        let default =
            strom_types::parse_resolution_string(strom_types::vision_mixer::DEFAULT_PGM_RESOLUTION)
                .map(|(w, h)| (w as i32, h as i32))
                .expect("DEFAULT_PGM_RESOLUTION must be valid");

        let capsfilter_id = format!("{}:capsfilter_dist", block_instance_id);
        self.elements
            .get(&capsfilter_id)
            .and_then(|cf| cf.property::<Option<gst::Caps>>("caps"))
            .and_then(|caps| {
                let s = caps.structure(0)?;
                Some((
                    s.get::<i32>("width").unwrap_or(default.0),
                    s.get::<i32>("height").unwrap_or(default.1),
                ))
            })
            .unwrap_or(default)
    }

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
                        let enabled = dsk_idx < state.dsk_enabled.len()
                            && state.dsk_enabled[dsk_idx]
                                .load(std::sync::atomic::Ordering::Relaxed);
                        let alpha = if enabled { 1.0f64 } else { 0.0f64 };
                        pad.set_property("alpha", alpha);
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

    /// Animate a single input's position/size on a compositor block.
    #[allow(clippy::too_many_arguments)]
    pub fn animate_input(
        &self,
        block_instance_id: &str,
        input_index: usize,
        target_xpos: Option<i32>,
        target_ypos: Option<i32>,
        target_width: Option<i32>,
        target_height: Option<i32>,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        use crate::gst::transitions::TransitionController;

        info!(
            "Animating input {} on {} to ({:?}, {:?}, {:?}, {:?}) over {}ms",
            input_index,
            block_instance_id,
            target_xpos,
            target_ypos,
            target_width,
            target_height,
            duration_ms
        );

        // Find the mixer element for this block
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;

        let (canvas_width, canvas_height) = self.dist_canvas_size(block_instance_id);

        // Create transition controller and animate
        let controller = TransitionController::new(mixer.clone(), canvas_width, canvas_height);
        controller
            .animate_input(
                input_index,
                target_xpos,
                target_ypos,
                target_width,
                target_height,
                duration_ms,
                &self.pipeline,
            )
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

        Ok(())
    }

    /// Reset accumulated loudness measurements on an EBU R128 meter block.
    pub fn reset_loudness(&self, block_instance_id: &str) -> Result<(), PipelineError> {
        let element_id = format!("{}:ebur128level", block_instance_id);
        let element = self
            .elements
            .get(&element_id)
            .ok_or_else(|| PipelineError::ElementNotFound(element_id.clone()))?;
        element.emit_by_name::<()>("reset", &[]);
        info!("Reset loudness measurements on {}", block_instance_id);
        Ok(())
    }

    /// Force an immediate file split on a recorder block.
    ///
    /// Emits the `split-now` signal on the splitmuxsink element, which triggers
    /// a file split at the next keyframe boundary.
    pub fn recorder_split_now(&self, block_instance_id: &str) -> Result<(), PipelineError> {
        use crate::blocks::builtin::recorder::SPLITMUXSINK_SUFFIX;
        let element_id = format!("{}:{}", block_instance_id, SPLITMUXSINK_SUFFIX);
        let element = self.elements.get(&element_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "{} (is this a recorder block in ts_passthrough mode?)",
                element_id
            ))
        })?;
        element.emit_by_name::<()>("split-now", &[]);
        info!(
            "Triggered split-now on recorder block {}",
            block_instance_id
        );
        Ok(())
    }

    /// Capture a thumbnail from a block's tee element at the given index.
    ///
    /// Lazily attaches a GStreamer-native processing branch to the block's tee
    /// element. The branch does format conversion and scaling using GStreamer
    /// elements, with lightweight JPEG encoding in the appsink callback.
    ///
    /// The meaning of `index` depends on the block type:
    /// - **Compositor**: input index (each input has its own tee named `{block_id}:thumb_tee_{index}`)
    /// - **Thumbnail block**: always 0 (single tee named `{block_id}:tee`)
    pub fn capture_block_thumbnail(
        &self,
        block_id: &str,
        index: usize,
    ) -> Result<Vec<u8>, PipelineError> {
        use crate::gst::thumbnail_tap::{ThumbnailTap, ThumbnailTapConfig};

        let mut taps = self.thumbnail_taps.lock().unwrap();
        let block_taps = taps.entry(block_id.to_string()).or_default();

        // Ensure we have a tap for this index (lazy creation)
        while block_taps.len() <= index {
            let idx = block_taps.len();
            // Try compositor naming first ({block_id}:thumb_tee_{idx}),
            // fall back to simple naming ({block_id}:tee) for index 0.
            let tee_name = format!("{}:thumb_tee_{}", block_id, idx);
            let tee = self
                .pipeline
                .by_name(&tee_name)
                .or_else(|| {
                    if idx == 0 {
                        self.pipeline.by_name(&format!("{}:tee", block_id))
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    PipelineError::ElementNotFound(format!(
                        "Thumbnail tee not found: {} (block {})",
                        tee_name, block_id
                    ))
                })?;

            let name_prefix = format!("{}:thumb_{}", block_id, idx);
            let tap = ThumbnailTap::new_with_tee(
                &self.pipeline,
                &name_prefix,
                tee,
                ThumbnailTapConfig::default(),
            );
            block_taps.push(tap);
        }

        block_taps[index]
            .get_thumbnail()
            .map_err(|e| PipelineError::ThumbnailCapture(e.to_string()))
    }

    /// Select a preview input on a vision mixer block.
    ///
    /// Replaces the PVW source with `input` (clearing any PiP-on-PVW mode).
    ///
    /// Returns (new_pvw, current_pgm). Both are `None` when the corresponding
    /// bus is a PiP source; `new_pvw` is always `Some(input)` here since this
    /// path always switches PVW to an input.
    pub fn select_vision_mixer_preview(
        &self,
        block_instance_id: &str,
        input: usize,
        num_inputs: usize,
    ) -> Result<(Option<usize>, Option<usize>), PipelineError> {
        use crate::blocks::builtin::vision_mixer::overlay;

        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "Vision mixer overlay state not found for {}",
                block_instance_id
            ))
        })?;

        // Validate first — don't mutate any state until we know we can complete.
        if input >= num_inputs {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "preview_input".to_string(),
                reason: format!("Input {} out of range (max {})", input, num_inputs - 1),
            });
        }

        let old_pvw = state.pvw_input();
        let pgm = state.pgm_input();
        // Picking a regular input clears any PiP-on-PVW mode. Also hide *all*
        // PVW-big pads — when leaving PiP mode the previous overlay pads aren't
        // covered by the single-pad hide below, so we sweep the full range.
        let leaving_pip = state.pvw_pip().is_some();
        state.set_pvw_pip(None);
        if leaving_pip {
            // Clear any lingering control bindings from a previous PiP-aware
            // fade, then hide every PVW big pad. The new selection below
            // re-activates only the chosen one.
            for i in 0..num_inputs {
                if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + i)) {
                    for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
                        if let Some(binding) = pad.control_binding(prop) {
                            pad.remove_control_binding(&binding);
                        }
                    }
                    pad.set_property("alpha", 0.0f64);
                }
            }
        }

        // PVW is always exactly one input (or a PiP — handled by
        // select_vision_mixer_pip_for_preview).
        if pgm == Some(input) && state.pgm_pip().is_none() {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "preview_input".to_string(),
                reason: format!("Input {} is already the sole program source", input),
            });
        }
        let new_pvw = Some(input);

        // Hide the old PVW pad (unless it's the same input the new PGM uses).
        if let Some(old_idx) = old_pvw {
            if pgm != Some(old_idx) {
                if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + old_idx))
                {
                    pad.set_property("alpha", 0.0f64);
                }
            }
        }

        // Position the new PVW pad aspect-fitted into the PVW big rect.
        if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + input)) {
            // Plain-input PVW never crops — wipe any crop from a PiP render.
            set_pad_crop(&pad, &Default::default());
            let r = &state.layout.pvw_rect;
            let aspect = self
                .vision_mixer_source_aspects(block_instance_id, num_inputs)
                .get(&input)
                .copied()
                .unwrap_or(0.0);
            let (x, y, w, h) = strom_types::vision_mixer::aspect_fit_rect(
                r.x as i32, r.y as i32, r.w as i32, r.h as i32, aspect,
            );
            pad.set_property("xpos", x);
            pad.set_property("ypos", y);
            pad.set_property("width", w);
            pad.set_property("height", h);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER);
        }

        state.set_pvw_input(new_pvw);
        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} preview changed: {:?} -> {:?}",
            block_instance_id, old_pvw, new_pvw
        );

        Ok((new_pvw, pgm))
    }

    /// Update the multiview compositor after a PGM transition on a vision mixer.
    ///
    /// Swaps PGM and PVW: old PVW becomes new PGM, old PGM becomes new PVW.
    /// `None` for either side means that bus is showing a PiP.
    pub fn update_vision_mixer_after_take(
        &self,
        block_instance_id: &str,
        new_pgm: Option<usize>,
        new_pvw: Option<usize>,
        num_inputs: usize,
    ) -> Result<(), PipelineError> {
        use crate::blocks::builtin::vision_mixer::overlay;

        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "Vision mixer overlay state not found for {}",
                block_instance_id
            ))
        })?;

        // Skip this entirely when PiP is involved on either bus — the PiP-aware
        // path in trigger_transition has already configured the right pads
        // (bg + overlays at proper geometry). Running the legacy single-input
        // logic here would wipe the PiP composition on PVW big.
        if state.pgm_pip().is_some() || state.pvw_pip().is_some() {
            // Still persist the input state so cairo overlay stays consistent.
            state.set_pgm_input(new_pgm);
            state.set_pvw_input(new_pvw);
            overlay::trigger_overlay_update(block_instance_id);
            return Ok(());
        }

        // PGM big display (sink_N) is fed from tee_pgm — it always shows the dist_comp
        // output automatically, so no pad manipulation needed for PGM.
        // Only update PVW: hide all old PVW pads, show new PVW pad.

        // Hide all PVW candidate pads first. Clear any control bindings too —
        // a previous fade may have left lingering bindings that would otherwise
        // override our property writes below.
        for i in 0..num_inputs {
            if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + i)) {
                for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
                    if let Some(binding) = pad.control_binding(prop) {
                        pad.remove_control_binding(&binding);
                    }
                }
                pad.set_property("alpha", 0.0f64);
            }
        }

        // Position the new PVW pad aspect-fitted into the PVW big rect
        // (single input only — multi-source compositions are PiPs).
        if let Some(active) = new_pvw {
            if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + active)) {
                // Plain-input PVW never crops — wipe any crop from a PiP render.
                set_pad_crop(&pad, &Default::default());
                let r = &state.layout.pvw_rect;
                let aspect = self
                    .vision_mixer_source_aspects(block_instance_id, num_inputs)
                    .get(&active)
                    .copied()
                    .unwrap_or(0.0);
                let (x, y, w, h) = strom_types::vision_mixer::aspect_fit_rect(
                    r.x as i32, r.y as i32, r.w as i32, r.h as i32, aspect,
                );
                pad.set_property("xpos", x);
                pad.set_property("ypos", y);
                pad.set_property("width", w);
                pad.set_property("height", h);
                pad.set_property("alpha", 1.0f64);
                pad.set_property("zorder", strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER);
            }
        }

        // Update state
        state.set_pgm_input(new_pgm);
        state.set_pvw_input(new_pvw);

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} take: PGM -> {:?}, PVW -> {:?}",
            block_instance_id, new_pgm, new_pvw
        );

        Ok(())
    }

    /// Toggle a DSK (Downstream Keyer) layer on or off.
    pub fn set_dsk_enabled(
        &self,
        block_instance_id: &str,
        dsk_index: usize,
        num_inputs: usize,
        enabled: bool,
    ) -> Result<(), PipelineError> {
        // DSK pads are on the dist compositor (mixer) at sink_{num_inputs + dsk_index}
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;

        let pad_name = format!("sink_{}", num_inputs + dsk_index);
        if let Some(pad) = find_pad(mixer, &pad_name) {
            let alpha = if enabled { 1.0f64 } else { 0.0f64 };
            pad.set_property("alpha", alpha);
            // Update overlay state for DSK tracking
            if let Some(state) =
                crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id)
            {
                if dsk_index < state.dsk_enabled.len() {
                    state.dsk_enabled[dsk_index]
                        .store(enabled, std::sync::atomic::Ordering::Relaxed);
                }
            }
            info!(
                "Vision mixer {} DSK {} {}",
                block_instance_id,
                dsk_index,
                if enabled { "enabled" } else { "disabled" }
            );
            Ok(())
        } else {
            Err(PipelineError::PadNotFound {
                element: mixer_id,
                pad: pad_name,
            })
        }
    }

    /// Set the multiview overlay alpha on a vision mixer block.
    pub fn set_overlay_alpha(
        &self,
        block_instance_id: &str,
        num_inputs: usize,
        alpha: f64,
    ) -> Result<(), PipelineError> {
        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

        // Overlay pad is the last pad on mv_comp. Pad layout:
        //   sink_0..N-1      : thumbnails
        //   sink_N           : PGM big (from tee_pgm)
        //   sink_N+1..2N     : PVW big candidates
        //   sink_2N+1..2N+P  : PiP-tile candidates (P = num_pips * num_inputs)
        //   sink_2N+1+P      : cairo overlay  ← this one
        let num_pips =
            crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id)
                .as_ref()
                .map(|s| s.num_pips)
                .unwrap_or(0);
        let overlay_idx = 2 * num_inputs + 1 + num_pips * num_inputs;
        let pad_name = format!("sink_{}", overlay_idx);
        if let Some(pad) = find_pad(mv_comp, &pad_name) {
            pad.set_property("alpha", alpha);
            if let Some(state) =
                crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id)
            {
                state.set_overlay_alpha(alpha);
            }
            info!(
                "Vision mixer {} overlay alpha set to {}",
                block_instance_id, alpha
            );
            Ok(())
        } else {
            Err(PipelineError::PadNotFound {
                element: mv_comp_id,
                pad: pad_name,
            })
        }
    }

    /// Toggle Fade to Black on a vision mixer block.
    ///
    /// Animates ALL mixer sink pads alpha to 0 (fade out) or restores them (fade in).
    /// Returns the new FTB state (true = active/black).
    pub fn fade_to_black(
        &self,
        block_instance_id: &str,
        duration_ms: u64,
    ) -> Result<bool, PipelineError> {
        use crate::blocks::builtin::vision_mixer::overlay;
        use gstreamer_controller::prelude::*;
        use gstreamer_controller::{
            DirectControlBinding, InterpolationControlSource, InterpolationMode,
        };

        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;

        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "Vision mixer overlay state not found for {}",
                block_instance_id
            ))
        })?;

        let was_active = state.ftb_active.load(std::sync::atomic::Ordering::Relaxed);
        let pgm = state.pgm_input();
        let now_active = !was_active;

        // When restoring (FTB-off), fade back exactly the pads the current PGM
        // source occupies on the dist mixer. For input-mode this is one pad;
        // for PiP-mode it's the bg pad plus one pad per overlay. Computing
        // this via `pads_for_source` keeps the deactivate path in sync with
        // the activation rules without duplicating the per-source logic here.
        let (cw, ch) = self.dist_canvas_size(block_instance_id);
        let src_aspect = if ch > 0 {
            cw as f64 / ch as f64
        } else {
            16.0 / 9.0
        };
        let active_pad_idxs: std::collections::HashSet<usize> = if now_active {
            std::collections::HashSet::new()
        } else {
            pads_for_source(
                &state,
                state.pgm_pip(),
                pgm,
                (0, 0, cw, ch),
                0,
                strom_types::vision_mixer::DIST_PGM_ZORDER,
                strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                src_aspect,
                &self.vision_mixer_source_aspects(block_instance_id, state.num_inputs),
            )
            .into_iter()
            .map(|t| t.pad_idx)
            .collect()
        };

        // Use mixer position for stream-time (same as transitions).
        // pipeline.query_position() drifts behind the compositor over time.
        let current_time = mixer
            .query_position::<gst::ClockTime>()
            .unwrap_or(gst::ClockTime::ZERO);
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        // Collect control sources so they stay alive for the duration of the animation
        let mut control_sources: Vec<InterpolationControlSource> = Vec::new();

        for pad in mixer.sink_pads() {
            let name = pad.name();
            if name.starts_with("sink_") {
                if let Ok(idx) = name.trim_start_matches("sink_").parse::<usize>() {
                    let (start_alpha, end_alpha) = if now_active {
                        // FTB on: fade current alpha to 0
                        let current = pad.property::<f64>("alpha");
                        (current, 0.0)
                    } else if active_pad_idxs.contains(&idx) {
                        (0.0, 1.0)
                    } else if idx >= state.num_inputs {
                        let dsk_idx = idx - state.num_inputs;
                        let enabled = dsk_idx < state.dsk_enabled.len()
                            && state.dsk_enabled[dsk_idx]
                                .load(std::sync::atomic::Ordering::Relaxed);
                        if enabled {
                            (0.0, 1.0)
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    };

                    if (start_alpha - end_alpha).abs() < f64::EPSILON {
                        continue;
                    }

                    // Clear any existing alpha control binding
                    if let Some(binding) = pad.control_binding("alpha") {
                        pad.remove_control_binding(&binding);
                    }

                    let cs = InterpolationControlSource::new();
                    cs.set_mode(InterpolationMode::Linear);

                    // Ease-in-out keyframes
                    let duration_ns = (end_time - current_time).nseconds() as f64;
                    let num_keyframes = strom_types::vision_mixer::TRANSITION_KEYFRAMES as u32;
                    for i in 0..=num_keyframes {
                        let t = i as f64 / num_keyframes as f64;
                        let eased = (1.0 - (t * std::f64::consts::PI).cos()) / 2.0;
                        let value = start_alpha + (end_alpha - start_alpha) * eased;
                        let time =
                            current_time + gst::ClockTime::from_nseconds((duration_ns * t) as u64);
                        cs.set(time, value);
                    }

                    let binding = DirectControlBinding::new(&pad, "alpha", &cs);
                    let _ = pad.add_control_binding(&binding);
                    control_sources.push(cs);
                }
            }
        }

        // Keep control sources alive until the animation completes, then clean up bindings
        if !control_sources.is_empty() {
            let cleanup_mixer = mixer.clone();
            let cleanup_duration = duration_ms + 100; // small margin
            gst::glib::timeout_add_once(
                std::time::Duration::from_millis(cleanup_duration),
                move || {
                    for pad in cleanup_mixer.sink_pads() {
                        if let Some(binding) = pad.control_binding("alpha") {
                            pad.remove_control_binding(&binding);
                        }
                    }
                    drop(control_sources);
                },
            );
        }

        state
            .ftb_active
            .store(now_active, std::sync::atomic::Ordering::Relaxed);

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} FTB {}",
            block_instance_id,
            if now_active {
                "activated"
            } else {
                "deactivated"
            }
        );

        Ok(now_active)
    }

    /// Update a PiP composition at runtime: new bg + new zone list + new
    /// per-source crop transforms.
    ///
    /// Morphs each affected compositor region (PiP-tile always; PGM/PVW if the
    /// bus is currently showing this PiP) from its previous pad layout to the
    /// new one — staying sources animate position+size (and crop, on the GL
    /// backend), fresh sources fade in, departing sources fade out.
    pub fn apply_vision_mixer_pip_config(
        &self,
        block_instance_id: &str,
        pip_idx: usize,
        bg: Option<usize>,
        zones: Vec<strom_types::vision_mixer::Zone>,
        transforms: strom_types::vision_mixer::PipTransforms,
    ) -> Result<(), PipelineError> {
        use crate::blocks::builtin::vision_mixer::overlay;
        use crate::gst::transitions::TransitionController;
        use strom_types::vision_mixer;

        const ZONE_MORPH_MS: u64 = 250;

        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "Vision mixer overlay state not found for {}",
                block_instance_id
            ))
        })?;

        if pip_idx >= state.num_pips {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "pip_idx".to_string(),
                reason: format!(
                    "PiP index {} out of range (configured: {})",
                    pip_idx, state.num_pips
                ),
            });
        }

        // Validate bg + zone sources: indices must be in range, must not
        // overlap each other, and must not equal bg. Empty zones (no sources)
        // are allowed — they still hold rect/capacity config. Rects are
        // clamped to [0,1] (silent — clamping is a layout concern, not
        // semantic state).
        if let Some(b) = bg {
            if b >= state.num_inputs {
                return Err(PipelineError::InvalidProperty {
                    element: block_instance_id.to_string(),
                    property: "bg".to_string(),
                    reason: format!(
                        "Background input {} out of range (num_inputs={})",
                        b, state.num_inputs
                    ),
                });
            }
        }
        let mut seen = std::collections::HashSet::new();
        let zones: Vec<strom_types::vision_mixer::Zone> = zones
            .into_iter()
            .map(|z| {
                for &input in &z.sources {
                    if input >= state.num_inputs {
                        return Err(PipelineError::InvalidProperty {
                            element: block_instance_id.to_string(),
                            property: "zones".to_string(),
                            reason: format!(
                                "Zone source {} out of range (num_inputs={})",
                                input, state.num_inputs
                            ),
                        });
                    }
                    if Some(input) == bg {
                        return Err(PipelineError::InvalidProperty {
                            element: block_instance_id.to_string(),
                            property: "zones".to_string(),
                            reason: format!("Zone source {} duplicates bg", input),
                        });
                    }
                    if !seen.insert(input) {
                        return Err(PipelineError::InvalidProperty {
                            element: block_instance_id.to_string(),
                            property: "zones".to_string(),
                            reason: format!("Zone source {} appears in more than one zone", input),
                        });
                    }
                }
                Ok(strom_types::vision_mixer::Zone {
                    rect: z.rect.map(|r| r.clamped()),
                    capacity: z.capacity,
                    sources: z.sources,
                })
            })
            .collect::<Result<_, _>>()?;

        // Validate transforms: input indices must be in range. Crop fractions
        // are clamped (like rects, clamping is a layout concern) and entries
        // that end up as no-crop are dropped to keep the state minimal.
        // Transforms for inputs no longer part of the PiP (bg or any zone)
        // are pruned — keeps the authoritative state (and the UI's crop
        // source list) in sync with the composition.
        let used_inputs: std::collections::HashSet<usize> = bg
            .into_iter()
            .chain(
                zones
                    .iter()
                    .flat_map(|z| z.effective_sources().iter().copied()),
            )
            .collect();
        let transforms: strom_types::vision_mixer::PipTransforms = transforms
            .into_iter()
            .map(|(input, crop)| {
                if input >= state.num_inputs {
                    return Err(PipelineError::InvalidProperty {
                        element: block_instance_id.to_string(),
                        property: "transforms".to_string(),
                        reason: format!(
                            "Transform input {} out of range (num_inputs={})",
                            input, state.num_inputs
                        ),
                    });
                }
                Ok((input, crop.clamped()))
            })
            .filter(|r| !matches!(r, Ok((i, c)) if c.is_zero() || !used_inputs.contains(i)))
            .collect::<Result<_, _>>()?;

        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;
        let mixer = self.elements.get(&format!("{}:mixer", block_instance_id));

        let (cw, ch) = self.dist_canvas_size(block_instance_id);
        let src_aspect = if ch > 0 {
            cw as f64 / ch as f64
        } else {
            16.0 / 9.0
        };
        let src_aspects = self.vision_mixer_source_aspects(block_instance_id, state.num_inputs);

        let on_pgm = state.pgm_pip() == Some(pip_idx);
        let on_pvw = state.pvw_pip() == Some(pip_idx);

        // ---- 1) Capture OLD pad targets (from current state, before mutation).
        let pip_tile_rect = state
            .layout
            .pip_tile_rects
            .get(pip_idx)
            .map(|t| (t.x as i32, t.y as i32, t.w as i32, t.h as i32));
        let pvw_rect = {
            let r = &state.layout.pvw_rect;
            (r.x as i32, r.y as i32, r.w as i32, r.h as i32)
        };
        let pip_tile_base = 2 * state.num_inputs + 1 + pip_idx * state.num_inputs;

        let old_tile = pip_tile_rect.map(|reg| {
            pads_for_source(
                &state,
                Some(pip_idx),
                None,
                reg,
                pip_tile_base,
                vision_mixer::MV_PIP_BG_ZORDER,
                vision_mixer::MV_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            )
        });
        let old_pgm = if on_pgm {
            Some(pads_for_source(
                &state,
                Some(pip_idx),
                None,
                (0, 0, cw, ch),
                0,
                vision_mixer::DIST_PGM_ZORDER,
                vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            ))
        } else {
            None
        };
        let old_pvw = if on_pvw {
            Some(pads_for_source(
                &state,
                Some(pip_idx),
                None,
                pvw_rect,
                state.num_inputs + 1,
                vision_mixer::MV_BIG_DISPLAY_ZORDER,
                vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            ))
        } else {
            None
        };

        // ---- 2) Persist new state.
        state.set_pip_bg_input(pip_idx, bg);
        state.set_pip_zones(pip_idx, zones.clone());
        state.set_pip_transforms(pip_idx, transforms.clone());
        if on_pgm {
            state.set_pgm_input(bg);
        }
        if on_pvw {
            state.set_pvw_input(bg);
        }

        // ---- 3) Compute NEW pad targets (after mutation).
        let new_tile = pip_tile_rect.map(|reg| {
            pads_for_source(
                &state,
                Some(pip_idx),
                None,
                reg,
                pip_tile_base,
                vision_mixer::MV_PIP_BG_ZORDER,
                vision_mixer::MV_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            )
        });
        let new_pgm = on_pgm.then(|| {
            pads_for_source(
                &state,
                Some(pip_idx),
                None,
                (0, 0, cw, ch),
                0,
                vision_mixer::DIST_PGM_ZORDER,
                vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            )
        });
        let new_pvw = on_pvw.then(|| {
            pads_for_source(
                &state,
                Some(pip_idx),
                None,
                pvw_rect,
                state.num_inputs + 1,
                vision_mixer::MV_BIG_DISPLAY_ZORDER,
                vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                src_aspect,
                &src_aspects,
            )
        });

        // ---- 4) Morph each affected region: staying pads animate position+size
        // (and crop, on the GL backend), entering pads fade in, departing pads
        // fade out.
        if let (Some(old_t), Some(new_t)) = (old_tile, new_tile) {
            let ctrl = TransitionController::new(
                mv_comp.clone(),
                state.layout.canvas_width as i32,
                state.layout.canvas_height as i32,
            );
            if let Err(e) =
                ctrl.animate_pad_transition(&old_t, &new_t, ZONE_MORPH_MS, &self.pipeline)
            {
                warn!("PiP tile morph failed: {}", e);
            }
            apply_pip_crop_after_morph(
                mv_comp,
                &ctrl,
                &self.pipeline,
                &transforms,
                state.num_inputs,
                pip_tile_base,
                &old_t,
                &new_t,
                ZONE_MORPH_MS,
            );
        }
        if let (Some(m), Some(old_p), Some(new_p)) = (mixer, old_pgm, new_pgm) {
            let ctrl = TransitionController::new(m.clone(), cw, ch);
            if let Err(e) =
                ctrl.animate_pad_transition(&old_p, &new_p, ZONE_MORPH_MS, &self.pipeline)
            {
                warn!("PGM morph failed: {}", e);
            }
            apply_pip_crop_after_morph(
                m,
                &ctrl,
                &self.pipeline,
                &transforms,
                state.num_inputs,
                0,
                &old_p,
                &new_p,
                ZONE_MORPH_MS,
            );
        }
        if let (Some(old_p), Some(new_p)) = (old_pvw, new_pvw) {
            let ctrl = TransitionController::new(
                mv_comp.clone(),
                state.layout.canvas_width as i32,
                state.layout.canvas_height as i32,
            );
            if let Err(e) =
                ctrl.animate_pad_transition(&old_p, &new_p, ZONE_MORPH_MS, &self.pipeline)
            {
                warn!("PVW morph failed: {}", e);
            }
            apply_pip_crop_after_morph(
                mv_comp,
                &ctrl,
                &self.pipeline,
                &transforms,
                state.num_inputs,
                state.num_inputs + 1,
                &old_p,
                &new_p,
                ZONE_MORPH_MS,
            );
        }

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} PiP {} config updated: bg={:?}, zones={:?}",
            block_instance_id, pip_idx, bg, zones
        );
        Ok(())
    }

    /// Read the negotiated input resolutions for a vision mixer block from
    /// its dist compositor sink pads. `None` for inputs without negotiated
    /// caps yet (not connected / not prerolled). Inputs can have arbitrary
    /// resolutions and aspect ratios — nothing scales them before the mixer.
    /// Always the UNCROPPED source dimensions: on the CPU backend the pad's
    /// own caps are post-videocrop, so this walks upstream past it.
    pub fn vision_mixer_input_resolutions(
        &self,
        block_instance_id: &str,
        num_inputs: usize,
    ) -> Vec<Option<strom_types::vision_mixer::InputResolution>> {
        let mixer_id = format!("{}:mixer", block_instance_id);
        let Some(mixer) = self.elements.get(&mixer_id) else {
            return vec![None; num_inputs];
        };
        (0..num_inputs)
            .map(|i| {
                let pad = find_pad(mixer, &format!("sink_{}", i))?;
                let (w, h) = crate::gst::crop::source_dims_for_pad(&pad)?;
                Some(strom_types::vision_mixer::InputResolution {
                    width: w as u32,
                    height: h as u32,
                })
            })
            .collect()
    }

    /// Build the per-input source-aspect map for explicit geometry from the
    /// negotiated input resolutions. Inputs without caps are absent (layout
    /// code falls back to the canvas aspect until the caps probe fires).
    pub fn vision_mixer_source_aspects(
        &self,
        block_instance_id: &str,
        num_inputs: usize,
    ) -> strom_types::vision_mixer::SourceAspects {
        self.vision_mixer_input_resolutions(block_instance_id, num_inputs)
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|r| (i, r.width as f64 / r.height as f64)))
            .collect()
    }

    /// Select a PiP composition as the PVW source.
    ///
    /// Hides any input-group PVW pads, then renders the PiP (bg + tiled overlays)
    /// inside the PVW big rectangle on the multiview compositor.
    pub fn select_vision_mixer_pip_for_preview(
        &self,
        block_instance_id: &str,
        pip_idx: usize,
    ) -> Result<(), PipelineError> {
        use crate::blocks::builtin::vision_mixer::overlay;
        use strom_types::vision_mixer;

        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "Vision mixer overlay state not found for {}",
                block_instance_id
            ))
        })?;

        if pip_idx >= state.num_pips {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "pip_idx".to_string(),
                reason: format!("PiP index {} out of range", pip_idx),
            });
        }

        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

        // Hide *all* previous PVW big pads — covers both input-group leftovers
        // and PiP-overlay leftovers when transitioning between source kinds.
        // Clear control bindings first so the alpha=0 actually takes effect
        // (a stale binding from a previous morph would otherwise override).
        for i in 0..state.num_inputs {
            if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", state.num_inputs + 1 + i)) {
                for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
                    if let Some(binding) = pad.control_binding(prop) {
                        pad.remove_control_binding(&binding);
                    }
                }
                pad.set_property("alpha", 0.0f64);
            }
        }

        // Mark PVW as PiP. Stash the PiP's bg as the underlying pvw_input so
        // the non-PiP-aware Take fallback still has a defined source.
        state.set_pvw_pip(Some(pip_idx));
        let bg = state.pip_bg_input(pip_idx);
        let zones = state.pip_zones(pip_idx);
        state.set_pvw_input(bg);

        // Render PiP layout into the PVW big rectangle.
        let r = &state.layout.pvw_rect;
        let (cw, ch) = self.dist_canvas_size(block_instance_id);
        let src_aspect = if ch > 0 {
            cw as f64 / ch as f64
        } else {
            16.0 / 9.0
        };
        apply_pip_layout_to_region(
            mv_comp,
            state.num_inputs + 1,
            state.num_inputs,
            (r.x as i32, r.y as i32, r.w as i32, r.h as i32),
            bg,
            &zones,
            &state.pip_transforms(pip_idx),
            vision_mixer::MV_BIG_DISPLAY_ZORDER,
            vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
            src_aspect,
            &self.vision_mixer_source_aspects(block_instance_id, state.num_inputs),
        );

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} PVW set to PiP {}: bg={:?}, zones={:?}",
            block_instance_id, pip_idx, bg, zones
        );
        Ok(())
    }
}

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

/// Apply per-source crop to a region after a zone morph: pads present in both
/// old and new layouts animate crop alongside the geometry morph; pads about
/// to be revealed snap to their crop; fading-out pads keep theirs until hidden.
#[allow(clippy::too_many_arguments)]
fn apply_pip_crop_after_morph(
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
#[allow(clippy::too_many_arguments)]
fn pads_for_source(
    state: &crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState,
    pip: Option<usize>,
    input: Option<usize>,
    region: (i32, i32, i32, i32),
    pad_base: usize,
    bg_zorder: u32,
    overlay_zorder: u32,
    fallback_aspect: f64,
    src_aspects: &strom_types::vision_mixer::SourceAspects,
) -> Vec<crate::gst::transitions::PadTarget> {
    use crate::gst::transitions::PadTarget;
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
                zorder: overlay_zorder + l.zorder_offset,
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
                }
            })
            .into_iter()
            .collect()
    }
}

/// Apply a single-input source layout (aspect-fitted into the region) to a
/// contiguous range of compositor sink pads. Pads not equal to the active
/// input are hidden.
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
        for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
            if let Some(binding) = pad.control_binding(prop) {
                pad.remove_control_binding(&binding);
            }
        }
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
    // Fast lookup: input → (x, y, w, h, zorder_offset).
    let layout_map: std::collections::HashMap<usize, (i32, i32, i32, i32, u32)> = layouts
        .iter()
        .map(|l| (l.input, (l.x, l.y, l.w, l.h, l.zorder_offset)))
        .collect();

    for i in 0..num_inputs {
        let pad_name = format!("sink_{}", pad_base + i);
        let Some(pad) = find_pad(compositor, &pad_name) else {
            continue;
        };
        // Clear any lingering control bindings from a previous fade — otherwise
        // they keep driving the property and our set_property calls below would
        // be invisible until the next take rebuilds bindings.
        for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
            if let Some(binding) = pad.control_binding(prop) {
                pad.remove_control_binding(&binding);
            }
        }
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
        } else if let Some(&(ox, oy, ow, oh, off)) = layout_map.get(&i) {
            pad.set_property("xpos", ox);
            pad.set_property("ypos", oy);
            pad.set_property("width", ow);
            pad.set_property("height", oh);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", overlay_zorder + off);
        } else {
            pad.set_property("alpha", 0.0f64);
        }
        // Crop applies to the source wherever it renders in this PiP; hidden
        // pads reset to no crop so later non-PiP reveals start clean.
        let crop = transforms.get(&i).copied().unwrap_or_default();
        set_pad_crop(&pad, &crop);
    }
}
