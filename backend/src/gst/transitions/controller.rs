//! `TransitionController` impl — drives compositor pads through plans produced
//! by [`super::plan_transition`].

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{InterpolationControlSource, InterpolationMode};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::vision_mixer;
use tracing::{debug, info};

use super::{
    plan_transition, PadAction, PadTarget, TransitionController, TransitionError, TransitionType,
    UnderlayTarget, ZHandling,
};

impl TransitionController {
    /// Create a new transition controller for a mixer element.
    pub fn new(mixer: gst::Element, canvas_width: i32, canvas_height: i32) -> Self {
        Self {
            mixer,
            canvas_width,
            canvas_height,
            active_transitions: Arc::new(Mutex::new(HashMap::new())),
            next_transition_id: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Build a unique key for `active_transitions`. The descriptive prefix is
    /// kept for log/debug readability; the monotonic suffix prevents concurrent
    /// transitions with the same prefix from overwriting each other's
    /// `control_sources` Vec.
    fn next_key(&self, prefix: &str) -> String {
        let id = self.next_transition_id.fetch_add(1, Ordering::Relaxed);
        format!("{}_{}", prefix, id)
    }

    /// Get a sink pad by input index.
    fn get_sink_pad(&self, input_index: usize) -> Result<gst::Pad, TransitionError> {
        // Try sink_0, sink_1, etc.
        let pad_name = format!("sink_{}", input_index);
        self.mixer
            .static_pad(&pad_name)
            .ok_or(TransitionError::PadNotFound(pad_name))
    }

    /// Get the current stream-time for scheduling control binding keyframes.
    ///
    /// GstVideoAggregator evaluates control bindings using **stream-time**
    /// (via `gst_segment_to_stream_time`), not running-time. We query the
    /// mixer element directly for its current position, which returns the
    /// stream-time of the output frame it is currently producing.
    fn query_stream_time(
        &self,
        pipeline: &gst::Pipeline,
    ) -> Result<gst::ClockTime, TransitionError> {
        let mixer_position = self.mixer.query_position::<gst::ClockTime>();

        let pipeline_position = pipeline.query_position::<gst::ClockTime>();

        info!(
            "Transition timing: mixer_position={:?}, pipeline_position={:?}",
            mixer_position.display(),
            pipeline_position.display(),
        );

        // Use mixer position — this is the correct stream-time for control bindings.
        // pipeline_position drifts behind over time as downstream sinks buffer,
        // causing keyframes to land in the past (hard cuts).
        // Any perceived transition delay is real transport time (encoder + webrtc).
        mixer_position.ok_or(TransitionError::PositionQueryFailed)
    }

    /// Trigger a transition from one input to another.
    ///
    /// # Arguments
    /// * `from_input` - The index of the currently active input.
    /// * `to_input` - The index of the input to transition to.
    /// * `transition_type` - The type of transition to perform.
    /// * `duration_ms` - Duration of the transition in milliseconds.
    /// * `pipeline` - The pipeline to query for current time.
    pub fn transition(
        &self,
        from_input: usize,
        to_input: usize,
        transition_type: TransitionType,
        duration_ms: u64,
        pipeline: &gst::Pipeline,
    ) -> Result<(), TransitionError> {
        if from_input == to_input {
            debug!("From and to inputs are the same, no transition needed");
            return Ok(());
        }

        debug!(
            "Starting {:?} transition from input {} to {} over {}ms",
            transition_type, from_input, to_input, duration_ms
        );

        // Clean up any previous transitions (they're no longer needed)
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.clear();
        }

        // Adjust for pipeline latency so keyframes align with compositor processing
        let current_time = self.query_stream_time(pipeline)?;
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        debug!(
            "Transition from {:?} to {:?}",
            current_time.display(),
            end_time.display()
        );

        match transition_type {
            TransitionType::Cut => self.transition_cut(from_input, to_input),
            TransitionType::Fade => {
                self.transition_fade(from_input, to_input, current_time, end_time)
            }
            TransitionType::SlideLeft => {
                self.transition_slide(from_input, to_input, current_time, end_time, -1, 0)
            }
            TransitionType::SlideRight => {
                self.transition_slide(from_input, to_input, current_time, end_time, 1, 0)
            }
            TransitionType::SlideUp => {
                self.transition_slide(from_input, to_input, current_time, end_time, 0, -1)
            }
            TransitionType::SlideDown => {
                self.transition_slide(from_input, to_input, current_time, end_time, 0, 1)
            }
            TransitionType::PushLeft => {
                self.transition_push(from_input, to_input, current_time, end_time, -1, 0)
            }
            TransitionType::PushRight => {
                self.transition_push(from_input, to_input, current_time, end_time, 1, 0)
            }
            TransitionType::PushUp => {
                self.transition_push(from_input, to_input, current_time, end_time, 0, -1)
            }
            TransitionType::PushDown => {
                self.transition_push(from_input, to_input, current_time, end_time, 0, 1)
            }
            TransitionType::DipToBlack => {
                self.transition_dip_to_black(from_input, to_input, current_time, end_time)
            }
            // Shader transitions are orchestrated by the take engine (they
            // need the FX glshader elements, which live outside the mixer).
            // Reaching here means the FX engine is unavailable — fall back
            // to a plain crossfade.
            TransitionType::Wipe(_) | TransitionType::MasterFx(_) => {
                self.transition_fade(from_input, to_input, current_time, end_time)
            }
        }
    }

    /// Perform an instant cut transition.
    fn transition_cut(&self, from_input: usize, to_input: usize) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        // Instant alpha change
        from_pad.set_property("alpha", 0.0f64);
        to_pad.set_property("alpha", 1.0f64);

        debug!("Cut transition complete: {} -> {}", from_input, to_input);
        Ok(())
    }

    /// Perform a fade/dissolve transition using alpha interpolation.
    fn transition_fade(
        &self,
        from_input: usize,
        to_input: usize,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
    ) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        // Clear any existing control bindings on these pads
        self.clear_control_bindings(&from_pad);
        self.clear_control_bindings(&to_pad);

        let mut control_sources = Vec::new();

        // Animate from_pad alpha: 1.0 -> 0.0
        let cs_from = self.setup_alpha_animation(&from_pad, start_time, end_time, 1.0, 0.0)?;
        control_sources.push(cs_from);

        // Animate to_pad alpha: 0.0 -> 1.0
        let cs_to = self.setup_alpha_animation(&to_pad, start_time, end_time, 0.0, 1.0)?;
        control_sources.push(cs_to);

        // Store control sources to keep them alive during animation
        let key = self.next_key(&format!("fade_{}_{}", from_input, to_input));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Fade transition started: {} -> {} ({}ms)",
            from_input,
            to_input,
            (end_time - start_time).mseconds()
        );

        Ok(())
    }

    /// Perform a slide transition - new source slides over the old one.
    /// The old source stays in place while the new one slides on top.
    fn transition_slide(
        &self,
        from_input: usize,
        to_input: usize,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        dx: i32, // -1 = left, 1 = right, 0 = no horizontal
        dy: i32, // -1 = up, 1 = down, 0 = no vertical
    ) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        self.clear_control_bindings(&from_pad);
        self.clear_control_bindings(&to_pad);

        let mut control_sources = Vec::new();

        // Where the incoming pad must END: its OWN aspect-fitted rect. The
        // classic-take reset positions every pad before the transition runs,
        // and with explicit geometry (sizing-policy=none) pads of different
        // aspect ratios have different rects — borrowing the outgoing pad's
        // position would land e.g. a 2.39:1 source at a 16:9 source's y.
        let target_x = to_pad.property::<i32>("xpos");
        let target_y = to_pad.property::<i32>("ypos");

        // To pad starts off-screen and slides over the from_pad
        // Direction: slide_left means new content comes from the right
        let to_start_x = target_x - dx * self.canvas_width;
        let to_start_y = target_y - dy * self.canvas_height;

        // Set initial position for to_pad (off-screen) and make it visible
        to_pad.set_property("xpos", to_start_x);
        to_pad.set_property("ypos", to_start_y);
        to_pad.set_property("alpha", 1.0f64);

        // Ensure to_pad renders on top by setting higher zorder
        let from_zorder = from_pad.property::<u32>("zorder");
        to_pad.set_property("zorder", from_zorder + 1);

        // Animate to_pad sliding in (from_pad stays still)
        if dx != 0 {
            let cs = self.setup_int_animation(
                to_pad.upcast_ref(),
                "xpos",
                start_time,
                end_time,
                to_start_x,
                target_x,
            )?;
            control_sources.push(cs);
        }

        if dy != 0 {
            let cs = self.setup_int_animation(
                to_pad.upcast_ref(),
                "ypos",
                start_time,
                end_time,
                to_start_y,
                target_y,
            )?;
            control_sources.push(cs);
        }

        // End-of-slide handling for the outgoing pad — the planner's rule
        // ("what's visible needs a transition; what's hidden doesn't"):
        // when the incoming rect fully covers the outgoing one, the old
        // picture is hidden at landing — snap it off (classic slide). With
        // mismatched aspect ratios the incoming rect can leave parts of the
        // old picture visible (letterbox/pillarbox remnants) — fade the old
        // picture out over the slide instead, so nothing pops at the end.
        let from_rect = (
            from_pad.property::<i32>("xpos"),
            from_pad.property::<i32>("ypos"),
            from_pad.property::<i32>("width"),
            from_pad.property::<i32>("height"),
        );
        let to_rect = (
            target_x,
            target_y,
            to_pad.property::<i32>("width"),
            to_pad.property::<i32>("height"),
        );
        let cs = if super::plan::rect_contains(to_rect, from_rect) {
            self.setup_alpha_animation(&from_pad, end_time, end_time, 1.0, 0.0)?
        } else {
            self.setup_alpha_animation(&from_pad, start_time, end_time, 1.0, 0.0)?
        };
        control_sources.push(cs);

        let key = self.next_key(&format!("slide_{}_{}", from_input, to_input));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Slide transition started: {} -> {} (dx={}, dy={}, {}ms)",
            from_input,
            to_input,
            dx,
            dy,
            (end_time - start_time).mseconds()
        );

        Ok(())
    }

    /// Perform a push transition where both sources move together.
    fn transition_push(
        &self,
        from_input: usize,
        to_input: usize,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        dx: i32, // -1 = left, 1 = right
        dy: i32, // -1 = up, 1 = down
    ) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        self.clear_control_bindings(&from_pad);
        self.clear_control_bindings(&to_pad);

        let mut control_sources = Vec::new();

        // Current position of from_pad
        let from_start_x = from_pad.property::<i32>("xpos");
        let from_start_y = from_pad.property::<i32>("ypos");

        // From pad exits in the direction of the push
        let from_end_x = from_start_x + dx * self.canvas_width;
        let from_end_y = from_start_y + dy * self.canvas_height;

        // To pad enters from the opposite side and ends at its OWN
        // aspect-fitted rect (set by the classic-take reset). With explicit
        // geometry, pads of different aspect ratios have different rects —
        // ending where the outgoing pad started would land e.g. a 2.39:1
        // source at a 16:9 source's y.
        let to_end_x = to_pad.property::<i32>("xpos");
        let to_end_y = to_pad.property::<i32>("ypos");
        let to_start_x = to_end_x - dx * self.canvas_width;
        let to_start_y = to_end_y - dy * self.canvas_height;

        // Set initial position for to_pad
        to_pad.set_property("xpos", to_start_x);
        to_pad.set_property("ypos", to_start_y);
        to_pad.set_property("alpha", 1.0f64);

        // Animate from_pad position (exits)
        if dx != 0 {
            let cs = self.setup_int_animation(
                from_pad.upcast_ref(),
                "xpos",
                start_time,
                end_time,
                from_start_x,
                from_end_x,
            )?;
            control_sources.push(cs);

            let cs = self.setup_int_animation(
                to_pad.upcast_ref(),
                "xpos",
                start_time,
                end_time,
                to_start_x,
                to_end_x,
            )?;
            control_sources.push(cs);
        }

        if dy != 0 {
            let cs = self.setup_int_animation(
                from_pad.upcast_ref(),
                "ypos",
                start_time,
                end_time,
                from_start_y,
                from_end_y,
            )?;
            control_sources.push(cs);

            let cs = self.setup_int_animation(
                to_pad.upcast_ref(),
                "ypos",
                start_time,
                end_time,
                to_start_y,
                to_end_y,
            )?;
            control_sources.push(cs);
        }

        // After transition, hide from_pad
        let cs = self.setup_alpha_animation(&from_pad, end_time, end_time, 1.0, 0.0)?;
        control_sources.push(cs);

        let key = self.next_key(&format!("push_{}_{}", from_input, to_input));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Push transition started: {} -> {} (dx={}, dy={}, {}ms)",
            from_input,
            to_input,
            dx,
            dy,
            (end_time - start_time).mseconds()
        );

        Ok(())
    }

    /// Perform a dip-to-black transition: fade out, then fade in.
    fn transition_dip_to_black(
        &self,
        from_input: usize,
        to_input: usize,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
    ) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        self.clear_control_bindings(&from_pad);
        self.clear_control_bindings(&to_pad);

        let mut control_sources = Vec::new();

        // Calculate midpoint
        let duration = end_time - start_time;
        let mid_time = start_time + duration / 2;
        let half_duration = duration / 2;

        // Ensure to_pad starts hidden
        to_pad.set_property("alpha", 0.0f64);

        // First half: fade out from_pad (1.0 -> 0.0) with easing
        let cs_from = Self::fresh_cs(from_pad.upcast_ref(), "alpha", InterpolationMode::Linear)?;

        // Add eased keyframes for first half (fade out)
        let num_keyframes = vision_mixer::TRANSITION_KEYFRAMES;
        for i in 0..=num_keyframes {
            let t = i as f64 / num_keyframes as f64;
            let eased_t = Self::ease_in_out(t);
            let value = 1.0 - eased_t; // 1.0 -> 0.0
            let time = start_time
                + gst::ClockTime::from_nseconds((half_duration.nseconds() as f64 * t) as u64);
            if !cs_from.set(time, value) {
                return Err(TransitionError::ControlSourceError(format!(
                    "Failed to set keyframe at t={}",
                    t
                )));
            }
        }
        // Keep at 0 for second half
        if !cs_from.set(end_time, 0.0) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set end keyframe".to_string(),
            ));
        }

        control_sources.push(cs_from);

        // Second half: fade in to_pad (0.0 -> 1.0) with easing
        let cs_to = Self::fresh_cs(to_pad.upcast_ref(), "alpha", InterpolationMode::Linear)?;

        // Stay at 0 until midpoint
        if !cs_to.set(start_time, 0.0) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set start keyframe".to_string(),
            ));
        }

        // Add eased keyframes for second half (fade in)
        for i in 0..=num_keyframes {
            let t = i as f64 / num_keyframes as f64;
            let eased_t = Self::ease_in_out(t);
            let value = eased_t; // 0.0 -> 1.0
            let time = mid_time
                + gst::ClockTime::from_nseconds((half_duration.nseconds() as f64 * t) as u64);
            if !cs_to.set(time, value) {
                return Err(TransitionError::ControlSourceError(format!(
                    "Failed to set keyframe at t={}",
                    t
                )));
            }
        }

        control_sources.push(cs_to);

        let key = self.next_key(&format!("dip_{}_{}", from_input, to_input));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Dip-to-black transition started: {} -> {} ({}ms)",
            from_input,
            to_input,
            (end_time - start_time).mseconds()
        );

        Ok(())
    }

    /// The mixer's current stream-time — the timebase for control-binding
    /// keyframes AND for the `time` uniform the FX shaders see (buffer PTS).
    /// Public for the shader take paths in `effects::shader_fx`.
    pub fn current_stream_time(
        &self,
        pipeline: &gst::Pipeline,
    ) -> Result<gst::ClockTime, TransitionError> {
        self.query_stream_time(pipeline)
    }

    /// Program a constant-then-step alpha on an input's pad: hold `v0` from
    /// `t0`, step to `v1` at `t1` (None-mode control source). Used by the
    /// shader take paths for delayed cuts and end-of-wipe step-offs.
    pub fn alpha_step(
        &self,
        input_index: usize,
        t0: gst::ClockTime,
        v0: f64,
        t1: gst::ClockTime,
        v1: f64,
    ) -> Result<(), TransitionError> {
        let pad = self.get_sink_pad(input_index)?;
        let cs = Self::fresh_cs(pad.upcast_ref(), "alpha", InterpolationMode::None)?;
        if !cs.set(t0, v0) || !cs.set(t1, v1) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set alpha step keyframes".to_string(),
            ));
        }
        let key = self.next_key(&format!("alpha_step_{}", input_index));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, vec![cs]);
        }
        Ok(())
    }

    /// Program an eased alpha fade on an input's pad between two times.
    /// Public for the shader take paths in `effects::shader_fx` (remnant
    /// fade-out when wipe rects don't cover each other).
    pub fn animate_alpha(
        &self,
        input_index: usize,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        start_value: f64,
        end_value: f64,
    ) -> Result<(), TransitionError> {
        let pad = self.get_sink_pad(input_index)?;
        let cs = self.setup_alpha_animation(&pad, start_time, end_time, start_value, end_value)?;
        let key = self.next_key(&format!("alpha_fade_{}", input_index));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, vec![cs]);
        }
        Ok(())
    }

    /// Compute ease-in-out value using cosine interpolation.
    /// t should be in range 0.0 to 1.0, returns value in same range.
    /// This creates more noticeable acceleration/deceleration than smoothstep.
    fn ease_in_out(t: f64) -> f64 {
        // Cosine ease-in-out: more pronounced than smoothstep
        (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
    }

    /// The persistent control source for `obj.prop`, wiped and ready to
    /// program (see `crate::gst::control_bindings` for why bindings are
    /// never removed).
    fn fresh_cs(
        obj: &gst::Object,
        prop: &str,
        mode: InterpolationMode,
    ) -> Result<InterpolationControlSource, TransitionError> {
        crate::gst::control_bindings::fresh_control_source(obj, prop, mode).ok_or_else(|| {
            TransitionError::ControlSourceError(format!("No animatable binding for {}", prop))
        })
    }

    /// Set up alpha property animation on a pad with ease-in-out curve.
    fn setup_alpha_animation(
        &self,
        pad: &gst::Pad,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        start_value: f64,
        end_value: f64,
    ) -> Result<InterpolationControlSource, TransitionError> {
        let cs = Self::fresh_cs(pad.upcast_ref(), "alpha", InterpolationMode::Linear)?;

        let duration = (end_time - start_time).nseconds() as f64;
        let value_range = end_value - start_value;

        // Add keyframes along ease-in-out curve for smooth animation
        let num_keyframes = vision_mixer::TRANSITION_KEYFRAMES;
        for i in 0..=num_keyframes {
            let t = i as f64 / num_keyframes as f64;
            let eased_t = Self::ease_in_out(t);
            let value = start_value + value_range * eased_t;
            let time = start_time + gst::ClockTime::from_nseconds((duration * t) as u64);

            if !cs.set(time, value) {
                return Err(TransitionError::ControlSourceError(format!(
                    "Failed to set keyframe at t={}",
                    t
                )));
            }
        }

        debug!(
            "Alpha animation (eased): {} -> {} on pad {}",
            start_value,
            end_value,
            pad.name()
        );

        Ok(cs)
    }

    /// Set up integer property animation with ease-in-out on any controllable
    /// GstObject — compositor sink pads (xpos/ypos/width/height, GL crop-*)
    /// or the CPU backend's upstream videocrop element (left/right/top/bottom).
    fn setup_int_animation(
        &self,
        pad: &gst::Object,
        property: &str,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        start_value: i32,
        end_value: i32,
    ) -> Result<InterpolationControlSource, TransitionError> {
        let cs = Self::fresh_cs(pad, property, InterpolationMode::Linear)?;

        // Get property range for normalization
        let pspec = pad.find_property(property).ok_or_else(|| {
            TransitionError::ControlSourceError(format!("Property {} not found on pad", property))
        })?;

        let (min, max) = if let Some(pspec) = pspec.downcast_ref::<gst::glib::ParamSpecInt>() {
            (pspec.minimum() as f64, pspec.maximum() as f64)
        } else {
            (i32::MIN as f64, i32::MAX as f64)
        };

        let prop_range = max - min;
        let duration = (end_time - start_time).nseconds() as f64;
        let value_range = (end_value - start_value) as f64;

        // Add keyframes along ease-in-out curve for smooth animation
        let num_keyframes = vision_mixer::TRANSITION_KEYFRAMES;
        for i in 0..=num_keyframes {
            let t = i as f64 / num_keyframes as f64;
            let eased_t = Self::ease_in_out(t);
            let value = start_value as f64 + value_range * eased_t;
            let norm_value = (value - min) / prop_range;
            let time = start_time + gst::ClockTime::from_nseconds((duration * t) as u64);

            if !cs.set(time, norm_value) {
                return Err(TransitionError::ControlSourceError(format!(
                    "Failed to set keyframe at t={}",
                    t
                )));
            }
        }

        debug!(
            "Int animation (eased) ({}): {} -> {} on pad {}",
            property,
            start_value,
            end_value,
            pad.name()
        );

        Ok(cs)
    }

    /// Neutralize all control bindings on a pad by wiping their keyframes
    /// (bindings stay attached — see `crate::gst::control_bindings` for why
    /// they must never be removed). Covers every property the transition
    /// setup helpers can bind: alpha (fades + step-off), xpos/ypos/width/
    /// height (position animation), zorder (morph z lift/step), and the GL
    /// mixer crop properties (crop morph — absent on the CPU backend, where
    /// `control_binding` simply returns `None`).
    fn clear_control_bindings(&self, pad: &gst::Pad) {
        crate::gst::control_bindings::wipe_control_bindings(
            pad.upcast_ref(),
            &[
                "alpha",
                "xpos",
                "ypos",
                "width",
                "height",
                "zorder",
                "crop-left",
                "crop-right",
                "crop-top",
                "crop-bottom",
            ],
        );
    }

    /// Animate a transition between two pad compositions ("PiP morph" style).
    ///
    /// Targets are `(pad_idx, x, y, w, h, zorder)`. The behaviour per pad:
    ///   - **Shared (in both old and new)**: position+size animates from old →
    ///     new linearly with ease-in-out, alpha stays 1, zorder snaps to new.
    ///     This drives the "PiP zoom" effect — a source that's fullscreen on
    ///     PGM and an overlay in the new PiP smoothly scales/translates between
    ///     the two layouts.
    ///   - **Incoming-only (fresh) pads**: pre-positioned at the new geometry
    ///     with alpha=1 immediately. They sit behind the shared pad in z-order
    ///     and are gradually revealed as the shared pad shrinks/moves.
    ///   - **Outgoing-only (departing) pads**: stay at their current geometry
    ///     with alpha=1 throughout the animation (the shared pad covers them
    ///     as it grows). At `end_time` they step to alpha=0 so the final state
    ///     is consistent with the new composition.
    pub fn animate_pad_transition(
        &self,
        outgoing: &[PadTarget],
        incoming: &[PadTarget],
        duration_ms: u64,
        pipeline: &gst::Pipeline,
    ) -> Result<(), TransitionError> {
        let current_time = self.query_stream_time(pipeline)?;
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        let plan = plan_transition(outgoing, incoming);
        let mut control_sources = Vec::new();

        // Border underlays ride along with their content pads: for each
        // planned content action, the pad's underlay (old and/or new state
        // from the targets) is driven through the same timeline.
        let out_map: HashMap<usize, &PadTarget> = outgoing.iter().map(|t| (t.pad_idx, t)).collect();
        let in_map: HashMap<usize, &PadTarget> = incoming.iter().map(|t| (t.pad_idx, t)).collect();

        for (idx, action) in &plan {
            let pad = self.get_sink_pad(*idx)?;
            self.clear_control_bindings(&pad);
            let old_u = out_map.get(idx).and_then(|t| t.underlay);
            let new_u = in_map.get(idx).and_then(|t| t.underlay);

            match *action {
                PadAction::Morph {
                    from_x,
                    from_y,
                    from_w,
                    from_h,
                    to_x,
                    to_y,
                    to_w,
                    to_h,
                    z_handling,
                } => {
                    pad.set_property("alpha", 1.0f64);
                    match z_handling {
                        ZHandling::SnapToNew(z) => pad.set_property("zorder", z),
                        ZHandling::LiftAndStep { new_z } => {
                            // Lift to TRANSITION_FOREGROUND_ZORDER + new_z so multiple
                            // simultaneously-morphing pads keep their relative target
                            // ordering throughout the lift. Otherwise both would sit at
                            // the same flat lifted z and ties would be broken by pad
                            // index — when they snap back to their real new_z values at
                            // end_time, the relative order can flip and a pad that was
                            // visually behind suddenly jumps in front.
                            let lifted = vision_mixer::TRANSITION_FOREGROUND_ZORDER + new_z;
                            pad.set_property("zorder", lifted);
                            control_sources.push(self.setup_zorder_step(
                                &pad,
                                current_time,
                                end_time,
                                lifted,
                                new_z,
                            )?);
                        }
                    }
                    pad.set_property("xpos", from_x);
                    pad.set_property("ypos", from_y);
                    pad.set_property("width", from_w);
                    pad.set_property("height", from_h);
                    control_sources.push(self.setup_int_animation(
                        pad.upcast_ref(),
                        "xpos",
                        current_time,
                        end_time,
                        from_x,
                        to_x,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        pad.upcast_ref(),
                        "ypos",
                        current_time,
                        end_time,
                        from_y,
                        to_y,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        pad.upcast_ref(),
                        "width",
                        current_time,
                        end_time,
                        from_w,
                        to_w,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        pad.upcast_ref(),
                        "height",
                        current_time,
                        end_time,
                        from_h,
                        to_h,
                    )?);
                    self.morph_underlay(
                        old_u,
                        new_u,
                        (from_x, from_y, from_w, from_h),
                        (to_x, to_y, to_w, to_h),
                        z_handling,
                        current_time,
                        end_time,
                        &mut control_sources,
                    )?;
                }
                PadAction::AffirmStatic { x, y, w, h, zorder }
                | PadAction::HoldFullAlpha { x, y, w, h, zorder } => {
                    pad.set_property("xpos", x);
                    pad.set_property("ypos", y);
                    pad.set_property("width", w);
                    pad.set_property("height", h);
                    pad.set_property("zorder", zorder);
                    pad.set_property("alpha", 1.0f64);
                    // Underlay snaps with its content pad.
                    self.snap_underlay(old_u, new_u, zorder)?;
                }
                PadAction::FadeIn { x, y, w, h, zorder } => {
                    pad.set_property("xpos", x);
                    pad.set_property("ypos", y);
                    pad.set_property("width", w);
                    pad.set_property("height", h);
                    pad.set_property("zorder", zorder);
                    pad.set_property("alpha", 0.0f64);
                    control_sources.push(self.setup_alpha_animation(
                        &pad,
                        current_time,
                        end_time,
                        0.0,
                        1.0,
                    )?);
                    if let Some(u) = new_u {
                        // Border fades in with its box.
                        let upad = self.place_underlay(&u, zorder)?;
                        upad.set_property("alpha", 0.0f64);
                        control_sources.push(self.setup_alpha_animation(
                            &upad,
                            current_time,
                            end_time,
                            0.0,
                            1.0,
                        )?);
                    } else if let Some(u) = old_u {
                        self.hide_underlay_pad(u.pad_idx)?;
                    }
                }
                PadAction::FadeOut => {
                    control_sources.push(self.setup_alpha_animation(
                        &pad,
                        current_time,
                        end_time,
                        1.0,
                        0.0,
                    )?);
                    if let Some(u) = old_u {
                        let upad = self.get_sink_pad(u.pad_idx)?;
                        self.clear_control_bindings(&upad);
                        control_sources.push(self.setup_alpha_animation(
                            &upad,
                            current_time,
                            end_time,
                            1.0,
                            0.0,
                        )?);
                    }
                }
                PadAction::StepOffAtEnd => {
                    control_sources.push(self.setup_alpha_step_off(
                        &pad,
                        current_time,
                        end_time,
                    )?);
                    if let Some(u) = old_u {
                        let upad = self.get_sink_pad(u.pad_idx)?;
                        self.clear_control_bindings(&upad);
                        control_sources.push(self.setup_alpha_step_off(
                            &upad,
                            current_time,
                            end_time,
                        )?);
                    }
                }
            }
        }

        let key = self.next_key(&format!("morph_o{}_i{}", outgoing.len(), incoming.len()));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Source morph transition started: out={:?}, in={:?} ({}ms, {} actions)",
            outgoing.iter().map(|t| t.pad_idx).collect::<Vec<_>>(),
            incoming.iter().map(|t| t.pad_idx).collect::<Vec<_>>(),
            duration_ms,
            plan.len()
        );

        Ok(())
    }

    /// Place an underlay pad at its target rect/color, directly beneath the
    /// given content zorder. Alpha is left to the caller. Clears stale
    /// control bindings first.
    fn place_underlay(
        &self,
        u: &UnderlayTarget,
        content_zorder: u32,
    ) -> Result<gst::Pad, TransitionError> {
        let pad = self.get_sink_pad(u.pad_idx)?;
        self.clear_control_bindings(&pad);
        crate::gst::underlay::set_underlay_color(&pad, u.argb);
        pad.set_property("xpos", u.x);
        pad.set_property("ypos", u.y);
        pad.set_property("width", u.w);
        pad.set_property("height", u.h);
        pad.set_property("zorder", vision_mixer::underlay_zorder(content_zorder));
        Ok(pad)
    }

    /// Hide an underlay pad (its content pad has no border in the new state).
    fn hide_underlay_pad(&self, pad_idx: usize) -> Result<(), TransitionError> {
        let pad = self.get_sink_pad(pad_idx)?;
        self.clear_control_bindings(&pad);
        pad.set_property("alpha", 0.0f64);
        Ok(())
    }

    /// Snap a content pad's underlay to its new state (used for the
    /// non-animating content actions).
    fn snap_underlay(
        &self,
        old_u: Option<UnderlayTarget>,
        new_u: Option<UnderlayTarget>,
        content_zorder: u32,
    ) -> Result<(), TransitionError> {
        if let Some(u) = new_u {
            let pad = self.place_underlay(&u, content_zorder)?;
            pad.set_property("alpha", 1.0f64);
        } else if let Some(u) = old_u {
            self.hide_underlay_pad(u.pad_idx)?;
        }
        Ok(())
    }

    /// Drive a morphing content pad's underlay through the same timeline:
    /// geometry animates from the old underlay rect to the new one, z-order
    /// mirrors the content's lift/snap one slot below, and alpha fades when
    /// the border appears/disappears mid-morph. When one side has no border,
    /// its rect is synthesized by inflating that side's content rect with
    /// the other side's inflation so the frame still tracks the box.
    #[allow(clippy::too_many_arguments)]
    fn morph_underlay(
        &self,
        old_u: Option<UnderlayTarget>,
        new_u: Option<UnderlayTarget>,
        from_rect: (i32, i32, i32, i32),
        to_rect: (i32, i32, i32, i32),
        z_handling: ZHandling,
        current_time: gst::ClockTime,
        end_time: gst::ClockTime,
        control_sources: &mut Vec<InterpolationControlSource>,
    ) -> Result<(), TransitionError> {
        let Some(spec) = new_u.or(old_u) else {
            return Ok(());
        };
        // Synthesized inflation for the side that lacks a border: use the
        // other side's (uniform outward inflation, derivable from its spec).
        let inflate = |content: (i32, i32, i32, i32), u: &UnderlayTarget, u_content_w: i32| {
            let bw = ((u.w - u_content_w) / 2).max(1);
            (
                content.0 - bw,
                content.1 - bw,
                content.2 + 2 * bw,
                content.3 + 2 * bw,
            )
        };
        let (fx, fy, fw, fh) = match old_u {
            Some(u) => (u.x, u.y, u.w, u.h),
            None => inflate(from_rect, &spec, to_rect.2),
        };
        let (tx, ty, tw, th) = match new_u {
            Some(u) => (u.x, u.y, u.w, u.h),
            None => inflate(to_rect, &spec, from_rect.2),
        };

        let pad = self.get_sink_pad(spec.pad_idx)?;
        self.clear_control_bindings(&pad);
        crate::gst::underlay::set_underlay_color(&pad, spec.argb);

        // Z mirrors the content pad, one slot below — including the lift.
        match z_handling {
            ZHandling::SnapToNew(z) => pad.set_property("zorder", vision_mixer::underlay_zorder(z)),
            ZHandling::LiftAndStep { new_z } => {
                let lifted = vision_mixer::TRANSITION_FOREGROUND_ZORDER + new_z;
                pad.set_property("zorder", vision_mixer::underlay_zorder(lifted));
                control_sources.push(self.setup_zorder_step(
                    &pad,
                    current_time,
                    end_time,
                    vision_mixer::underlay_zorder(lifted),
                    vision_mixer::underlay_zorder(new_z),
                )?);
            }
        }

        pad.set_property("xpos", fx);
        pad.set_property("ypos", fy);
        pad.set_property("width", fw);
        pad.set_property("height", fh);
        for (prop, from, to) in [
            ("xpos", fx, tx),
            ("ypos", fy, ty),
            ("width", fw, tw),
            ("height", fh, th),
        ] {
            control_sources.push(self.setup_int_animation(
                pad.upcast_ref(),
                prop,
                current_time,
                end_time,
                from,
                to,
            )?);
        }

        // Alpha: steady when bordered on both sides; fades in/out when the
        // border appears or disappears during the morph.
        match (old_u.is_some(), new_u.is_some()) {
            (true, true) => pad.set_property("alpha", 1.0f64),
            (false, true) => {
                pad.set_property("alpha", 0.0f64);
                control_sources.push(self.setup_alpha_animation(
                    &pad,
                    current_time,
                    end_time,
                    0.0,
                    1.0,
                )?);
            }
            (true, false) => {
                control_sources.push(self.setup_alpha_animation(
                    &pad,
                    current_time,
                    end_time,
                    1.0,
                    0.0,
                )?);
            }
            (false, false) => unreachable!("spec exists"),
        }
        Ok(())
    }

    /// Animate the GL mixer crop properties on a sink pad from their current
    /// values to the pixel values for `target` (normalized crop × the pad's
    /// negotiated source caps), with the same easing as the geometry morph.
    /// Both backends animate: GL crop pad properties, or the CPU backend's
    /// upstream videocrop element — its `left/right/top/bottom` are
    /// controllable and it syncs controller values per buffer (each step
    /// renegotiates caps toward the compositor; a short burst during the
    /// morph window that videoaggregator's live caps handling absorbs).
    pub fn animate_pad_crop(
        &self,
        pad_idx: usize,
        target: &vision_mixer::SourceCrop,
        duration_ms: u64,
        pipeline: &gst::Pipeline,
    ) -> Result<(), TransitionError> {
        use crate::gst::crop::{upstream_videocrop, CROP_PAD_PROPS, VIDEOCROP_PROPS};
        let pad = self.get_sink_pad(pad_idx)?;
        // Pick the crop carrier: GL pad properties, or the upstream videocrop.
        let (carrier, props): (gst::Object, [&str; 4]) = if pad.find_property("crop-left").is_some()
        {
            (pad.clone().upcast(), CROP_PAD_PROPS)
        } else if let Some(vc) = upstream_videocrop(&pad) {
            (vc.upcast(), VIDEOCROP_PROPS)
        } else {
            // Neither — nothing to crop on this branch (e.g. thumbnails).
            return Ok(());
        };
        let Some((src_w, src_h)) = crate::gst::crop::source_dims_for_pad(&pad) else {
            debug!(
                "Pad {} has no negotiated caps yet — crop animation skipped",
                pad.name()
            );
            return Ok(());
        };
        let (l, r, t, b) = target.to_pixels(src_w, src_h);
        let current_time = self.query_stream_time(pipeline)?;
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        let mut control_sources = Vec::new();
        for (prop, to) in props.into_iter().zip([l, r, t, b]) {
            // Neutralize a stale binding first so reads + writes hit the
            // real value (keyframe wipe — bindings are never removed, see
            // crate::gst::control_bindings).
            crate::gst::control_bindings::wipe_control_binding(carrier.upcast_ref(), prop);
            let from = carrier.property::<i32>(prop);
            if from == to {
                carrier.set_property(prop, to);
                continue;
            }
            control_sources.push(self.setup_int_animation(
                &carrier,
                prop,
                current_time,
                end_time,
                from,
                to,
            )?);
        }
        if !control_sources.is_empty() {
            let key = self.next_key(&format!("crop_p{}", pad_idx));
            if let Ok(mut transitions) = self.active_transitions.lock() {
                transitions.insert(key, control_sources);
            }
            debug!(
                "Crop animation started on pad {}: -> ({}, {}, {}, {}) over {}ms",
                pad_idx, l, r, t, b, duration_ms
            );
        }
        Ok(())
    }

    /// Step a `zorder` property from `start_z` (held during the animation) to
    /// `end_z` at `end_time` using a None-mode control source. The shared pad
    /// in a morph transition is lifted to a high zorder for the duration so it
    /// stays on top of any pad it crosses, then snaps back to its proper zorder
    /// when the morph completes.
    fn setup_zorder_step(
        &self,
        pad: &gst::Pad,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        start_z: u32,
        end_z: u32,
    ) -> Result<InterpolationControlSource, TransitionError> {
        let cs = Self::fresh_cs(pad.upcast_ref(), "zorder", InterpolationMode::None)?;

        // DirectControlBinding scales the source value by the property paramspec
        // range, so we have to express absolute zorder values as a 0..1 ratio.
        let pspec = pad.find_property("zorder").ok_or_else(|| {
            TransitionError::ControlSourceError("zorder property not found".to_string())
        })?;
        let (min, max) = if let Some(p) = pspec.downcast_ref::<gst::glib::ParamSpecUInt>() {
            (p.minimum() as f64, p.maximum() as f64)
        } else {
            (0.0, u32::MAX as f64)
        };
        let range = (max - min).max(1.0);

        if !cs.set(start_time, (start_z as f64 - min) / range) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set zorder start keyframe".to_string(),
            ));
        }
        if !cs.set(end_time, (end_z as f64 - min) / range) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set zorder end keyframe".to_string(),
            ));
        }
        Ok(cs)
    }

    /// Hold `alpha=1` from `start_time` until just before `end_time`, then step
    /// to `alpha=0`. Used for outgoing-only pads in a morph transition: they
    /// stay visible (covered by the growing shared pad) and snap off at the end
    /// so the final composition is clean.
    fn setup_alpha_step_off(
        &self,
        pad: &gst::Pad,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
    ) -> Result<InterpolationControlSource, TransitionError> {
        // None mode = constant-between-keyframes.
        let cs = Self::fresh_cs(pad.upcast_ref(), "alpha", InterpolationMode::None)?;
        if !cs.set(start_time, 1.0) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set start keyframe".to_string(),
            ));
        }
        if !cs.set(end_time, 0.0) {
            return Err(TransitionError::ControlSourceError(
                "Failed to set end keyframe".to_string(),
            ));
        }
        Ok(cs)
    }

    /// Clean up completed transitions.
    pub fn cleanup_old_transitions(&self) {
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.clear();
        }
    }

    /// Animate a single input's properties to target values.
    ///
    /// Smoothly animates position (xpos, ypos) and size (width, height) from
    /// current values to the specified targets.
    #[allow(clippy::too_many_arguments)]
    pub fn animate_input(
        &self,
        input_index: usize,
        target_xpos: Option<i32>,
        target_ypos: Option<i32>,
        target_width: Option<i32>,
        target_height: Option<i32>,
        duration_ms: u64,
        pipeline: &gst::Pipeline,
    ) -> Result<(), TransitionError> {
        let pad = self.get_sink_pad(input_index)?;

        // Clean up previous animations
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.clear();
        }
        self.clear_control_bindings(&pad);

        // Adjust for pipeline latency so keyframes align with compositor processing
        let current_time = self.query_stream_time(pipeline)?;
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        let mut control_sources = Vec::new();

        // Animate xpos if target provided
        if let Some(target) = target_xpos {
            let current = pad.property::<i32>("xpos");
            if current != target {
                let cs = self.setup_int_animation(
                    pad.upcast_ref(),
                    "xpos",
                    current_time,
                    end_time,
                    current,
                    target,
                )?;
                control_sources.push(cs);
            }
        }

        // Animate ypos if target provided
        if let Some(target) = target_ypos {
            let current = pad.property::<i32>("ypos");
            if current != target {
                let cs = self.setup_int_animation(
                    pad.upcast_ref(),
                    "ypos",
                    current_time,
                    end_time,
                    current,
                    target,
                )?;
                control_sources.push(cs);
            }
        }

        // Animate width if target provided
        if let Some(target) = target_width {
            let current = pad.property::<i32>("width");
            if current != target {
                let cs = self.setup_int_animation(
                    pad.upcast_ref(),
                    "width",
                    current_time,
                    end_time,
                    current,
                    target,
                )?;
                control_sources.push(cs);
            }
        }

        // Animate height if target provided
        if let Some(target) = target_height {
            let current = pad.property::<i32>("height");
            if current != target {
                let cs = self.setup_int_animation(
                    pad.upcast_ref(),
                    "height",
                    current_time,
                    end_time,
                    current,
                    target,
                )?;
                control_sources.push(cs);
            }
        }

        // Store control sources
        let key = self.next_key(&format!("animate_input_{}", input_index));
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.insert(key, control_sources);
        }

        info!(
            "Animating input {} to xpos={:?}, ypos={:?}, width={:?}, height={:?} over {}ms",
            input_index, target_xpos, target_ypos, target_width, target_height, duration_ms
        );

        Ok(())
    }
}
