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

/// Neutralize a transition shader without recompiling: with `u_duration = 0`
/// wipe masks evaluate to fully revealed and master envelopes to zero — both
/// behave as identity. On slots still holding the identity fragment the
/// uniforms have no matching locations and are silently ignored.
fn neutral_uniforms() -> gst::Structure {
    gst::Structure::builder("uniforms")
        .field("u_start", 0.0f32)
        .field("u_duration", 0.0f32)
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

    /// Run a shader-mask wipe: the incoming source's TAKE slot reveals it
    /// over the outgoing source (which holds and snaps off at the end).
    pub(crate) fn shader_wipe_take(
        &self,
        block_instance_id: &str,
        controller: &TransitionController,
        from_input: usize,
        to_input: usize,
        kind: WipeKind,
        duration_ms: u64,
    ) -> Result<(), PipelineError> {
        let fx = self
            .fx_element(&format!("{}:fx_take_{}", block_instance_id, to_input))?
            .clone();
        let mixer_id = format!("{}:mixer", block_instance_id);
        let mixer = self
            .elements
            .get(&mixer_id)
            .ok_or_else(|| PipelineError::ElementNotFound(mixer_id.clone()))?;
        let to_pad = mixer
            .static_pad(&format!("sink_{}", to_input))
            .ok_or_else(|| PipelineError::PadNotFound {
                element: mixer_id,
                pad: format!("sink_{}", to_input),
            })?;

        let now = controller
            .current_stream_time(&self.pipeline)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;
        let end = now + gst::ClockTime::from_mseconds(duration_ms);

        // Outgoing stays fully visible under the wipe, snaps off at the end.
        controller
            .alpha_step(from_input, now, 1.0, end, 0.0)
            .map_err(|e| PipelineError::TransitionError(e.to_string()))?;

        // Program the mask before the incoming pad becomes visible. Buffers
        // reaching the TAKE slot carry PTS at/after `now`, so the reveal
        // starts from p=0 on the mixer's output timeline.
        let start_s = now.nseconds() as f64 / 1e9;
        let dur_s = duration_ms as f64 / 1000.0;
        apply_shader(&fx, &kind.fragment(), &kind.run_uniforms(start_s, dur_s));

        // Incoming above outgoing, fully opaque — the mask does the reveal.
        // The next take's classic reset restores the plain PGM zorder.
        to_pad.set_property("zorder", strom_types::vision_mixer::DIST_PGM_ZORDER + 1);
        to_pad.set_property("alpha", 1.0f64);

        info!(
            "Shader wipe '{}' started: {} -> {} ({}ms) on {}",
            kind.name(),
            from_input,
            to_input,
            duration_ms,
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
