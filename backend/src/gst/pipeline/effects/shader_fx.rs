//! Shader FX runtime operations — per-source looks, master effects, wipe
//! takes and master envelopes.
//!
//! GPU backend only: the FX `glshader` slots (`fx_look_{i}`, `fx_take_{i}`,
//! `fx_pgm`) exist only in the GPU pipeline with `enable_fx`. Every entry
//! point degrades gracefully (error for explicit effect ops, Fade fallback
//! for takes) when the slots are absent.
//!
//! Animation is evaluated inside the shaders at composite time (the `time`
//! uniform is buffer PTS): Rust programs `u_start`/`u_duration` once per
//! take and never touches uniforms per frame. See `crate::gst::shaders`.

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::effects::{EffectTarget, VideoEffect};
use tracing::{debug, info};

use super::super::{PipelineError, PipelineManager};
use crate::blocks::builtin::vision_mixer::overlay;
use crate::gst::shaders::{self, MasterBase, MasterFxKind, WipeKind};
use crate::gst::transitions::{TransitionController, TransitionType};

/// Program a glshader slot: uniforms first so the new fragment never renders
/// with stale parameters, then the fragment, then trigger the live recompile.
fn apply_shader(elem: &gst::Element, fragment: &str, uniforms: &gst::Structure) {
    elem.set_property("uniforms", uniforms);
    elem.set_property("fragment", fragment);
    elem.set_property("update-shader", true);
}

/// How long the incoming pad stays transparent at the start of a classic
/// (upright-mask) wipe — covers the in-flight buffers rendered before the
/// shader swap, whose mask is fully open. Two-three output frames.
const WIPE_ALPHA_LEAD_MS: u64 = 60;

/// Neutralize a transition shader without recompiling: with `u_duration = 0`
/// wipe masks evaluate to fully revealed and master envelopes to zero — both
/// behave as identity. `u_invert` must be reset too: uniform values persist
/// on the GL program, and a leftover inverted wipe at p=1 would otherwise
/// flip "fully revealed" into "fully transparent" and black out the branch.
/// On slots holding the identity fragment the uniforms have no matching
/// locations and are silently ignored.
fn neutral_uniforms() -> gst::Structure {
    gst::Structure::builder("uniforms")
        .field("u_start", 0.0f32)
        .field("u_duration", 0.0f32)
        .field("u_invert", 0.0f32)
        .build()
}

impl PipelineManager {
    /// Whether the shader FX engine was built into this block's pipeline.
    pub fn vision_mixer_fx_available(&self, block_instance_id: &str) -> bool {
        self.elements
            .contains_key(&format!("{}:fx_pgm", block_instance_id))
    }

    fn fx_element(&self, id: &str) -> Result<&gst::Element, PipelineError> {
        self.elements.get(id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!(
                "{} (shader FX requires the GPU backend with Shader FX enabled)",
                id
            ))
        })
    }

    /// Apply a persistent video effect ("look") to an input's LOOK slot or
    /// the PGM MASTER slot. Returns the effect as applied (after clamping).
    pub fn set_vision_mixer_effect(
        &self,
        block_instance_id: &str,
        target: EffectTarget,
        effect: &VideoEffect,
    ) -> Result<VideoEffect, PipelineError> {
        let effect = effect
            .sanitized()
            .map_err(|reason| PipelineError::InvalidProperty {
                element: block_instance_id.to_string(),
                property: "effect".to_string(),
                reason,
            })?;
        let elem_id = match target {
            EffectTarget::Input(i) => format!("{}:fx_look_{}", block_instance_id, i),
            EffectTarget::Master => format!("{}:fx_pgm", block_instance_id),
        };
        let elem = self.fx_element(&elem_id)?;
        // Param-only change (same effect kind): swap uniforms without a
        // shader recompile — keeps UI sliders cheap.
        let same_kind = overlay::get_overlay_state(block_instance_id)
            .map(|state| {
                let stored = match target {
                    EffectTarget::Input(i) => state
                        .input_effects
                        .get(i)
                        .and_then(|m| m.lock().ok().map(|e| e.kind())),
                    EffectTarget::Master => state.master_effect.lock().ok().map(|e| e.kind()),
                };
                stored == Some(effect.kind())
            })
            .unwrap_or(false);
        let (fragment, uniforms) = shaders::effect_shader(&effect);
        if same_kind && effect != VideoEffect::None {
            elem.set_property("uniforms", &uniforms);
        } else {
            apply_shader(elem, &fragment, &uniforms);
        }

        if let Some(state) = overlay::get_overlay_state(block_instance_id) {
            match target {
                EffectTarget::Input(i) => {
                    if let Some(m) = state.input_effects.get(i) {
                        if let Ok(mut e) = m.lock() {
                            *e = effect.clone();
                        }
                    }
                }
                EffectTarget::Master => {
                    if let Ok(mut e) = state.master_effect.lock() {
                        *e = effect.clone();
                    }
                }
            }
        }
        info!(
            "Vision mixer {} effect on {}: {}",
            block_instance_id,
            target,
            effect.kind()
        );
        Ok(effect)
    }

    /// Reset transition shader state at the start of every take so an
    /// interrupted wipe can't leave a half-masked source behind, and a
    /// lingering master envelope can't replay. Uniform-only (no recompiles).
    /// The MASTER slot is left alone while it carries a persistent look.
    pub(crate) fn reset_take_fx(&self, block_instance_id: &str) {
        let Some(state) = overlay::get_overlay_state(block_instance_id) else {
            return;
        };
        if !self.vision_mixer_fx_available(block_instance_id) {
            return;
        }
        for i in 0..state.num_inputs {
            if let Some(e) = self
                .elements
                .get(&format!("{}:fx_take_{}", block_instance_id, i))
            {
                e.set_property("uniforms", neutral_uniforms());
            }
        }
        let master_is_look = state
            .master_effect
            .lock()
            .map(|e| *e != VideoEffect::None)
            .unwrap_or(false);
        if !master_is_look {
            if let Some(e) = self.elements.get(&format!("{}:fx_pgm", block_instance_id)) {
                e.set_property("uniforms", neutral_uniforms());
            }
        }
    }

    /// Run a shader-mask wipe. Two orientations, picked by geometry:
    ///
    /// **Inverted (preferred)** — when the outgoing rect covers the incoming
    /// rect: the OUTGOING source's TAKE slot masks it away on top of the
    /// incoming source sitting underneath. The mask runs upstream of the
    /// mixer, so at take time there are always in-flight buffers rendered
    /// before the shader swap (mask fully opaque); with the outgoing source
    /// carrying the mask those render as "PGM unchanged" — glitch-free by
    /// construction.
    ///
    /// **Classic** — when the outgoing rect does NOT cover the incoming one
    /// (e.g. a letterboxed 2.39:1 source out, 16:9 in): inverting would pop
    /// the destination instantly in the uncovered regions, so the mask goes
    /// on the INCOMING source above instead. Its pad alpha turns on a couple
    /// of frames late to let pre-swap in-flight buffers (mask fully open)
    /// drain — same flash, different cure. Outgoing remnants outside the
    /// incoming rect fade over the wipe (the slide transition's rule).
    pub(crate) fn shader_wipe_take(
        &self,
        block_instance_id: &str,
        controller: &TransitionController,
        from_input: usize,
        to_input: usize,
        kind: WipeKind,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        if from_input == to_input {
            return Ok(());
        }
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;
        let pad = |idx: usize| {
            mixer
                .static_pad(&format!("sink_{}", idx))
                .ok_or_else(|| PipelineError::PadNotFound {
                    element: mixer_id.clone(),
                    pad: format!("sink_{}", idx),
                })
        };
        let from_pad = pad(from_input)?;
        let to_pad = pad(to_input)?;
        let rect = |p: &gst::Pad| {
            (
                p.property::<i32>("xpos"),
                p.property::<i32>("ypos"),
                p.property::<i32>("width"),
                p.property::<i32>("height"),
            )
        };
        let from_rect = rect(&from_pad);
        let to_rect = rect(&to_pad);

        let now = controller
            .current_stream_time(&self.pipeline)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
        let end = now + gst::ClockTime::from_mseconds(duration_ms);
        let start_s = now.nseconds() as f64 / 1e9;
        let dur_s = duration_ms as f64 / 1000.0;
        let err = |e: crate::gst::transitions::TransitionError| {
            PipelineError::TransitionError(e.to_string())
        };

        let inverted = crate::gst::transitions::rect_contains(from_rect, to_rect);
        if inverted {
            let fx = self
                .fx_element(&format!("{}:fx_take_{}", block_instance_id, from_input))?
                .clone();
            // Outgoing holds while its inverted mask (run_uniforms sets
            // u_invert=1, evaluated against buffer PTS) wipes it away;
            // snaps off at the end as the safety net.
            controller
                .alpha_step(from_input, now, 1.0, end, 0.0)
                .map_err(err)?;
            apply_shader(&fx, &kind.fragment(), &kind.run_uniforms(start_s, dur_s));

            // Incoming below outgoing, fully opaque — revealed as the mask
            // eats away. The next take's classic reset restores the plain
            // PGM zorder on whichever pad is then active.
            to_pad.set_property(
                "zorder",
                strom_types::vision_mixer::DIST_PGM_ZORDER.saturating_sub(1),
            );
            to_pad.set_property("alpha", 1.0f64);
        } else {
            let fx = self
                .fx_element(&format!("{}:fx_take_{}", block_instance_id, to_input))?
                .clone();
            // Mask on the incoming source, composited above the outgoing.
            // Its mask is ~0 for the first frames, so delaying alpha-on by
            // two output frames hides exactly the pre-swap in-flight
            // buffers without visibly delaying the wipe.
            let alpha_on = now + gst::ClockTime::from_mseconds(WIPE_ALPHA_LEAD_MS.min(duration_ms));
            apply_shader(
                &fx,
                &kind.fragment(),
                &kind.run_uniforms_upright(start_s, dur_s),
            );
            to_pad.set_property("zorder", strom_types::vision_mixer::DIST_PGM_ZORDER + 1);
            controller
                .alpha_step(to_input, now, 0.0, alpha_on, 1.0)
                .map_err(err)?;
            // The incoming rect doesn't cover the outgoing one here, so
            // remnants outside it fade over the wipe instead of popping
            // off at the end (the slide transition's remnant rule).
            if crate::gst::transitions::rect_contains(to_rect, from_rect) {
                controller.alpha_step(from_input, now, 1.0, end, 0.0)
            } else {
                controller.animate_alpha(from_input, now, end, 1.0, 0.0)
            }
            .map_err(err)?;
        }

        info!(
            "Shader wipe '{}' started: {} -> {} ({}ms, {}) on {}",
            kind.name(),
            from_input,
            to_input,
            duration_ms,
            if inverted { "inverted" } else { "classic" },
            block_instance_id
        );
        Ok(())
    }

    /// Program the PGM MASTER slot with a take envelope (peaks at the cut
    /// point, identity before/after). Replaces any persistent master look.
    pub(crate) fn apply_master_envelope(
        &self,
        block_instance_id: &str,
        kind: MasterFxKind,
        start: gst::ClockTime,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        let fx = self.fx_element(&format!("{}:fx_pgm", block_instance_id))?;
        let start_s = start.nseconds() as f64 / 1e9;
        let dur_s = duration_ms as f64 / 1000.0;
        apply_shader(fx, &kind.fragment(), &kind.run_uniforms(start_s, dur_s));
        if let Some(state) = overlay::get_overlay_state(block_instance_id) {
            if let Ok(mut e) = state.master_effect.lock() {
                if *e != VideoEffect::None {
                    debug!(
                        "Master envelope '{}' replaced persistent master effect '{}' on {}",
                        kind.name(),
                        e.kind(),
                        block_instance_id
                    );
                }
                *e = VideoEffect::None;
            }
        }
        Ok(())
    }

    /// Run a master-FX take: a basic pad transition underneath (delayed cut,
    /// fade or push, per [`MasterFxKind::base`]) with the envelope shader on
    /// the PGM MASTER slot on top.
    pub(crate) fn master_fx_take(
        &self,
        block_instance_id: &str,
        controller: &TransitionController,
        from_input: usize,
        to_input: usize,
        kind: MasterFxKind,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        if from_input == to_input {
            return Ok(());
        }
        let now = controller
            .current_stream_time(&self.pipeline)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

        match kind.base() {
            MasterBase::DelayedCut => {
                // Hard cut at the envelope midpoint — the FX peak hides it.
                let mid = now + gst::ClockTime::from_mseconds(duration_ms / 2);
                controller
                    .alpha_step(from_input, now, 1.0, mid, 0.0)
                    .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
                controller
                    .alpha_step(to_input, now, 0.0, mid, 1.0)
                    .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
            }
            MasterBase::Fade => {
                controller
                    .transition(
                        from_input,
                        to_input,
                        TransitionType::Fade,
                        duration_ms,
                        &self.pipeline,
                    )
                    .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
            }
            MasterBase::Push(dx, _dy) => {
                let push = if dx < 0 {
                    TransitionType::PushLeft
                } else {
                    TransitionType::PushRight
                };
                controller
                    .transition(from_input, to_input, push, duration_ms, &self.pipeline)
                    .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
            }
        }

        self.apply_master_envelope(block_instance_id, kind, now, duration_ms)?;
        info!(
            "Master FX take '{}' started: {} -> {} ({}ms) on {}",
            kind.name(),
            from_input,
            to_input,
            duration_ms,
            block_instance_id
        );
        Ok(())
    }
}
