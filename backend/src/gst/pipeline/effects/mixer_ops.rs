//! Vision mixer runtime operations: PVW/PGM bus selection, DSK toggles,
//! fade-to-black, multiview overlay alpha, live PiP (re)configuration and
//! negotiated input resolutions.

use super::super::{PipelineError, PipelineManager};
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

use crate::gst::crop::set_pad_crop;

use super::mixer_layout::{
    apply_pip_crop_after_morph, apply_pip_layout_to_region, find_pad, pads_for_source,
};

impl PipelineManager {
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
        //
        // Transforms for inputs NOT currently in the PiP are kept: they are
        // inert (layout only applies crop to sources that render) and come
        // back to life when the source returns — the swap-zone workflow
        // (capacity 1, pushing between two sources) expects each source's
        // punch-in framing to survive the round trip. Removing a crop is an
        // explicit act (delete the entry / Reset in the UI), not a side
        // effect of leaving the composition.
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
            .filter(|r| !matches!(r, Ok((_, c)) if c.is_zero()))
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
