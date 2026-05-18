use super::{PipelineError, PipelineManager};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, info};

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
    /// Uses the server's authoritative PGM/PVW groups from overlay state.
    /// For single-source groups, uses standard single-pad transitions.
    /// For multi-source groups, cross-fades between group layouts.
    ///
    /// Returns (was_ftb_cancelled, old_pgm_group, new_pgm_group).
    pub fn trigger_transition(
        &self,
        block_instance_id: &str,
        from_input: usize,
        to_input: usize,
        transition_type: &str,
        duration_ms: u64,
    ) -> Result<(bool, Vec<usize>, Vec<usize>), PipelineError> {
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

        // Read authoritative PGM/PVW groups from overlay state
        let overlay_state =
            crate::blocks::builtin::vision_mixer::overlay::get_overlay_state(block_instance_id);
        let num_video_inputs = overlay_state
            .as_ref()
            .map(|s| s.num_inputs)
            .unwrap_or(usize::MAX);
        let old_pgm_group = overlay_state
            .as_ref()
            .map(|s| s.pgm_group())
            .unwrap_or_else(|| vec![from_input]);
        let new_pgm_group = overlay_state
            .as_ref()
            .map(|s| s.pvw_group())
            .unwrap_or_else(|| vec![to_input]);

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

                // Swap Source state PVW ↔ PGM.
                let new_pgm_pip = old_pvw_pip;
                let new_pvw_pip = old_pgm_pip;
                let new_pgm_group_swap = state.pvw_group();
                let new_pvw_group_swap = state.pgm_group();

                let dist_region = (0, 0, canvas_width, canvas_height);
                let r = &state.layout.pvw_rect;
                let pvw_region = (r.x as i32, r.y as i32, r.w as i32, r.h as i32);
                // Source aspect = PGM canvas aspect (typically 16:9). Used to
                // compute aspect-preserving tile cells so overlays fill cleanly.
                let src_aspect = if canvas_height > 0 {
                    canvas_width as f64 / canvas_height as f64
                } else {
                    16.0 / 9.0
                };

                let old_dist_targets = pads_for_source(
                    state,
                    old_pgm_pip,
                    &old_pgm_group,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                );
                let new_dist_targets = pads_for_source(
                    state,
                    new_pgm_pip,
                    &new_pgm_group_swap,
                    dist_region,
                    0,
                    strom_types::vision_mixer::DIST_PGM_ZORDER,
                    strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                );
                let old_pvw_targets = pads_for_source(
                    state,
                    old_pvw_pip,
                    &state.pvw_group(),
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
                );
                let new_pvw_targets = pads_for_source(
                    state,
                    new_pvw_pip,
                    &new_pvw_group_swap,
                    pvw_region,
                    state.num_inputs + 1,
                    strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                    strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                    src_aspect,
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
                            &state.pip_overlay_inputs(p),
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                            strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                            src_aspect,
                        );
                    } else {
                        apply_input_group_to_region(
                            mixer,
                            0,
                            state.num_inputs,
                            dist_region,
                            &new_pgm_group_swap,
                            strom_types::vision_mixer::DIST_PGM_ZORDER,
                        );
                    }
                    if let Some(p) = new_pvw_pip {
                        apply_pip_layout_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            state.pip_bg_input(p),
                            &state.pip_overlay_inputs(p),
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
                            strom_types::vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                            src_aspect,
                        );
                    } else {
                        apply_input_group_to_region(
                            mv_comp,
                            state.num_inputs + 1,
                            state.num_inputs,
                            pvw_region,
                            &new_pvw_group_swap,
                            strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER,
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
                }

                // Persist new state.
                state.set_pgm_pip(new_pgm_pip);
                state.set_pvw_pip(new_pvw_pip);
                state.set_pgm_group(&new_pgm_group_swap);
                state.set_pvw_group(&new_pvw_group_swap);
                crate::blocks::builtin::vision_mixer::overlay::trigger_overlay_update(
                    block_instance_id,
                );

                info!(
                    "PiP-aware Take on {} ({}{}ms): PGM pip={:?} group={:?}; PVW pip={:?} group={:?}",
                    block_instance_id,
                    if is_cut { "Cut, " } else { "Fade, " },
                    duration_ms,
                    new_pgm_pip,
                    new_pgm_group_swap,
                    new_pvw_pip,
                    new_pvw_group_swap,
                );
                return Ok((was_ftb, old_pgm_group, new_pgm_group_swap));
            }
        }

        // Reset all video pads to a clean state before the transition:
        // clear control bindings, restore alpha/position/size for the current
        // PGM input. Single-input PGM only (multi-source compositions are PiPs).
        let active_pgm_input = old_pgm_group.first().copied();

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
                        if Some(idx) == active_pgm_input {
                            // The active PGM input fills the canvas.
                            pad.set_property("alpha", 1.0f64);
                            pad.set_property("xpos", 0i32);
                            pad.set_property("ypos", 0i32);
                            pad.set_property("width", canvas_width);
                            pad.set_property("height", canvas_height);
                            pad.set_property("zorder", strom_types::vision_mixer::DIST_PGM_ZORDER);
                        } else {
                            pad.set_property("alpha", 0.0f64);
                            pad.set_property("xpos", 0i32);
                            pad.set_property("ypos", 0i32);
                            pad.set_property("width", canvas_width);
                            pad.set_property("height", canvas_height);
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
        let from = old_pgm_group.first().copied().unwrap_or(from_input);
        let to = new_pgm_group.first().copied().unwrap_or(to_input);
        let controller = TransitionController::new(mixer.clone(), canvas_width, canvas_height);
        controller
            .transition(from, to, trans_type, duration_ms, &self.pipeline)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

        Ok((was_ftb, old_pgm_group, new_pgm_group))
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
    /// If `multi` is false, replaces the PVW group with a single source (standard behavior).
    /// If `multi` is true, toggles the input in/out of the current PVW group (shift+click).
    ///
    /// Returns (pvw_group, pgm_group).
    pub fn select_vision_mixer_preview(
        &self,
        block_instance_id: &str,
        input: usize,
        num_inputs: usize,
    ) -> Result<(Vec<usize>, Vec<usize>), PipelineError> {
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

        let old_pvw_group = state.pvw_group();
        let pgm_group = state.pgm_group();
        // Picking a regular input clears any PiP-on-PVW mode. Also hide *all*
        // PVW-big pads — when leaving PiP mode the previous overlay pads aren't
        // in old_pvw_group, so the per-old-group hide pass would miss them.
        let leaving_pip = state.pvw_pip().is_some();
        state.set_pvw_pip(None);
        if leaving_pip {
            // Clear any lingering control bindings from a previous PiP-aware
            // fade, then hide every PVW big pad. The new selection below
            // re-activates only the chosen ones.
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

        if input >= num_inputs {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "preview_input".to_string(),
                reason: format!("Input {} out of range (max {})", input, num_inputs - 1),
            });
        }

        // Multi-input groups are gone — PVW is always exactly one input (or a
        // PiP, but the PiP path is handled by select_vision_mixer_pip_for_preview).
        if pgm_group.len() == 1 && pgm_group[0] == input && state.pgm_pip().is_none() {
            return Err(PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "preview_input".to_string(),
                reason: format!("Input {} is already the sole program source", input),
            });
        }
        let new_pvw_group = vec![input];

        // Hide all old PVW big pads (the new selection re-activates only its own).
        for &old_idx in &old_pvw_group {
            if !pgm_group.contains(&old_idx) {
                if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + old_idx))
                {
                    pad.set_property("alpha", 0.0f64);
                }
            }
        }

        // Position the new PVW pad at the PVW big rect.
        if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + input)) {
            let r = &state.layout.pvw_rect;
            pad.set_property("xpos", r.x as i32);
            pad.set_property("ypos", r.y as i32);
            pad.set_property("width", r.w as i32);
            pad.set_property("height", r.h as i32);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER);
        }

        state.set_pvw_group(&new_pvw_group);
        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} preview changed: {:?} -> {:?}",
            block_instance_id, old_pvw_group, new_pvw_group
        );

        Ok((new_pvw_group, pgm_group))
    }

    /// Update the multiview compositor after a PGM transition on a vision mixer.
    ///
    /// Swaps PGM and PVW groups: old PVW group becomes new PGM, old PGM group becomes new PVW.
    pub fn update_vision_mixer_after_take(
        &self,
        block_instance_id: &str,
        new_pgm_group: &[usize],
        new_pvw_group: &[usize],
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
            // Still persist the group state so cairo overlay stays consistent.
            state.set_pgm_group(new_pgm_group);
            state.set_pvw_group(new_pvw_group);
            overlay::trigger_overlay_update(block_instance_id);
            return Ok(());
        }

        // PGM big display (sink_N) is fed from tee_pgm — it always shows the dist_comp
        // output automatically, so no pad manipulation needed for PGM.
        // Only update PVW: hide all old PVW pads, show new PVW group pads.

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

        // Position the new PVW pad at the PVW big rect (single input only —
        // multi-source compositions are PiPs).
        if let Some(active) = new_pvw_group.first().copied() {
            if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", num_inputs + 1 + active)) {
                let r = &state.layout.pvw_rect;
                pad.set_property("xpos", r.x as i32);
                pad.set_property("ypos", r.y as i32);
                pad.set_property("width", r.w as i32);
                pad.set_property("height", r.h as i32);
                pad.set_property("alpha", 1.0f64);
                pad.set_property("zorder", strom_types::vision_mixer::MV_BIG_DISPLAY_ZORDER);
            }
        }

        // Update state
        state.set_pgm_group(new_pgm_group);
        state.set_pvw_group(new_pvw_group);

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} take: PGM -> {:?}, PVW -> {:?}",
            block_instance_id, new_pgm_group, new_pvw_group
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
        let pgm_group = state.pgm_group();
        let now_active = !was_active;

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
                    } else if pgm_group.contains(&idx) {
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

    /// Update a PiP composition at runtime: new bg + new overlay list.
    ///
    /// Re-applies geometry/alpha to the multiview PiP-tile pads, plus to
    /// dist/PVW pads if the corresponding bus is currently showing this PiP.
    pub fn apply_vision_mixer_pip_config(
        &self,
        block_instance_id: &str,
        pip_idx: usize,
        bg: Option<usize>,
        overlays: Vec<usize>,
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
                reason: format!(
                    "PiP index {} out of range (configured: {})",
                    pip_idx, state.num_pips
                ),
            });
        }

        // Clamp inputs to valid range; dedupe overlays preserving order.
        let bg = bg.filter(|i| *i < state.num_inputs);
        let mut seen = std::collections::HashSet::new();
        let overlays: Vec<usize> = overlays
            .into_iter()
            .filter(|i| *i < state.num_inputs && Some(*i) != bg && seen.insert(*i))
            .collect();

        // Persist runtime state.
        state.set_pip_bg_input(pip_idx, bg);
        state.set_pip_overlay_inputs(pip_idx, overlays.clone());

        let mv_comp_id = format!("{}:mv_comp", block_instance_id);
        let mv_comp = self
            .elements
            .get(&mv_comp_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mv_comp_id.clone()))?;

        let (cw, ch) = self.dist_canvas_size(block_instance_id);
        let src_aspect = if ch > 0 {
            cw as f64 / ch as f64
        } else {
            16.0 / 9.0
        };

        // Always update the PiP thumbnail tile.
        if let Some(tile) = state.layout.pip_tile_rects.get(pip_idx) {
            let pip_tile_base = 2 * state.num_inputs + 1 + pip_idx * state.num_inputs;
            apply_pip_layout_to_region(
                mv_comp,
                pip_tile_base,
                state.num_inputs,
                (tile.x as i32, tile.y as i32, tile.w as i32, tile.h as i32),
                bg,
                &overlays,
                vision_mixer::MV_PIP_BG_ZORDER,
                vision_mixer::MV_PIP_OVERLAY_ZORDER,
                src_aspect,
            );
        }

        // If this PiP is currently on PGM, refresh the dist compositor too.
        if state.pgm_pip() == Some(pip_idx) {
            let mixer_id = format!("{}:mixer", block_instance_id);
            if let Some(mixer) = self.elements.get(&mixer_id) {
                apply_pip_layout_to_region(
                    mixer,
                    0,
                    state.num_inputs,
                    (0, 0, cw, ch),
                    bg,
                    &overlays,
                    vision_mixer::DIST_PGM_ZORDER,
                    vision_mixer::DIST_PIP_OVERLAY_ZORDER,
                    src_aspect,
                );
            }
        }

        // If this PiP is currently on PVW, refresh the PVW big region on mv_comp.
        if state.pvw_pip() == Some(pip_idx) {
            let r = &state.layout.pvw_rect;
            apply_pip_layout_to_region(
                mv_comp,
                state.num_inputs + 1,
                state.num_inputs,
                (r.x as i32, r.y as i32, r.w as i32, r.h as i32),
                bg,
                &overlays,
                vision_mixer::MV_BIG_DISPLAY_ZORDER,
                vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
                src_aspect,
            );
        }

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} PiP {} config updated: bg={:?}, overlays={:?}",
            block_instance_id, pip_idx, bg, overlays
        );
        Ok(())
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
        // apply_pip_layout_to_region below re-activates only the new bg+overlay
        // pads, but the bg-shown-on-PVW special case from `select_vision_mixer_preview`
        // and PiP overlay pads from a previous PiP both need explicit clearing.
        for i in 0..state.num_inputs {
            if let Some(pad) = find_pad(mv_comp, &format!("sink_{}", state.num_inputs + 1 + i)) {
                pad.set_property("alpha", 0.0f64);
            }
        }

        // Mark PVW as PiP. Keep the underlying pvw_group as [bg] so legacy
        // single-source PGM-take fallback still has a defined "first input".
        state.set_pvw_pip(Some(pip_idx));
        let bg = state.pip_bg_input(pip_idx);
        let overlays = state.pip_overlay_inputs(pip_idx);
        if let Some(b) = bg {
            state.set_pvw_group(&[b]);
        } else {
            state.set_pvw_group(&[]);
        }

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
            &overlays,
            vision_mixer::MV_BIG_DISPLAY_ZORDER,
            vision_mixer::MV_PVW_PIP_OVERLAY_ZORDER,
            src_aspect,
        );

        overlay::trigger_overlay_update(block_instance_id);

        info!(
            "Vision mixer {} PVW set to PiP {}: bg={:?}, overlays={:?}",
            block_instance_id, pip_idx, bg, overlays
        );
        Ok(())
    }
}

/// Find a pad by name on an element, checking both static and request pads.
/// `static_pad()` doesn't find request pads on aggregator elements like glvideomixer.
fn find_pad(element: &gst::Element, pad_name: &str) -> Option<gst::Pad> {
    element.static_pad(pad_name).or_else(|| {
        element
            .pads()
            .into_iter()
            .find(|p| p.name().as_str() == pad_name)
    })
}

/// Compute per-pad targets `(pad_idx, x, y, w, h, zorder)` for a Source rendered
/// into a compositor region. `pad_base` is the index offset of the first input
/// pad in the region (e.g. 0 for dist_comp, num_inputs+1 for mv_comp PVW big).
#[allow(clippy::too_many_arguments)]
fn pads_for_source(
    state: &crate::blocks::builtin::vision_mixer::overlay::VisionMixerOverlayState,
    pip: Option<usize>,
    group: &[usize],
    region: (i32, i32, i32, i32),
    pad_base: usize,
    bg_zorder: u32,
    overlay_zorder: u32,
    source_aspect: f64,
) -> Vec<crate::gst::transitions::PadTarget> {
    use crate::gst::transitions::PadTarget;
    let (rx, ry, rw, rh) = region;
    if let Some(p) = pip {
        let bg = state.pip_bg_input(p);
        let overlays = state.pip_overlay_inputs(p);
        let overlay_rects = strom_types::vision_mixer::compute_pip_overlay_rects(
            rx,
            ry,
            rw,
            rh,
            overlays.len(),
            source_aspect,
        );
        let mut out = Vec::new();
        if let Some(b) = bg {
            out.push(PadTarget {
                pad_idx: pad_base + b,
                x: rx,
                y: ry,
                w: rw,
                h: rh,
                zorder: bg_zorder,
            });
        }
        for (slot, &idx) in overlays.iter().enumerate() {
            let (x, y, w, h) = overlay_rects.get(slot).copied().unwrap_or((rx, ry, rw, rh));
            out.push(PadTarget {
                pad_idx: pad_base + idx,
                x,
                y,
                w,
                h,
                zorder: overlay_zorder,
            });
        }
        out
    } else {
        // Single-input source — only first element of `group` is meaningful
        // (multi-input groups removed; multi-source compositions are PiPs now).
        group
            .first()
            .map(|&idx| PadTarget {
                pad_idx: pad_base + idx,
                x: rx,
                y: ry,
                w: rw,
                h: rh,
                zorder: bg_zorder,
            })
            .into_iter()
            .collect()
    }
}

/// Apply a single-input source layout (fills the region) to a contiguous range
/// of compositor sink pads. Pads not equal to the active input are hidden.
fn apply_input_group_to_region(
    compositor: &gst::Element,
    pad_base: usize,
    num_inputs: usize,
    region: (i32, i32, i32, i32),
    group: &[usize],
    fg_zorder: u32,
) {
    let (rx, ry, rw, rh) = region;
    let active = group.first().copied();
    for i in 0..num_inputs {
        let pad_name = format!("sink_{}", pad_base + i);
        let Some(pad) = find_pad(compositor, &pad_name) else {
            continue;
        };
        if Some(i) == active {
            pad.set_property("xpos", rx);
            pad.set_property("ypos", ry);
            pad.set_property("width", rw);
            pad.set_property("height", rh);
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
fn apply_pip_layout_to_region(
    compositor: &gst::Element,
    pad_base: usize,
    num_inputs: usize,
    region: (i32, i32, i32, i32),
    bg: Option<usize>,
    overlays: &[usize],
    bg_zorder: u32,
    overlay_zorder: u32,
    source_aspect: f64,
) {
    let (rx, ry, rw, rh) = region;
    let overlay_rects = strom_types::vision_mixer::compute_pip_overlay_rects(
        rx,
        ry,
        rw,
        rh,
        overlays.len(),
        source_aspect,
    );
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
            pad.set_property_from_str("sizing-policy", "keep-aspect-ratio");
            pad.set_property("xpos", rx);
            pad.set_property("ypos", ry);
            pad.set_property("width", rw);
            pad.set_property("height", rh);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", bg_zorder);
        } else if let Some(slot) = overlays.iter().position(|&v| v == i) {
            // overlay_rects are already aspect-corrected to match the source
            // (see compute_pip_overlay_rects), so keep-aspect-ratio fits the
            // source exactly inside the cell — no transparent letterbox bands
            // and no stretching.
            let (ox, oy, ow, oh) = overlay_rects.get(slot).copied().unwrap_or((0, 0, 1, 1));
            pad.set_property_from_str("sizing-policy", "keep-aspect-ratio");
            pad.set_property("xpos", ox);
            pad.set_property("ypos", oy);
            pad.set_property("width", ow);
            pad.set_property("height", oh);
            pad.set_property("alpha", 1.0f64);
            pad.set_property("zorder", overlay_zorder);
        } else {
            pad.set_property("alpha", 0.0f64);
        }
    }
}
