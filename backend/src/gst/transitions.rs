//! Scene transitions using GStreamer Controller API.
//!
//! This module provides animated transitions between compositor inputs using
//! GStreamer's interpolation control source to animate pad properties over time.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::{DirectControlBinding, InterpolationControlSource, InterpolationMode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

/// Transition type for scene switching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionType {
    /// Instant cut (no animation).
    Cut,
    /// Cross-fade via alpha blending.
    Fade,
    /// Slide the new input in from the left (old stays in place).
    SlideLeft,
    /// Slide the new input in from the right (old stays in place).
    SlideRight,
    /// Slide the new input in from the top (old stays in place).
    SlideUp,
    /// Slide the new input in from the bottom (old stays in place).
    SlideDown,
    /// Push from the left (both move together).
    PushLeft,
    /// Push from the right (both move together).
    PushRight,
    /// Push from the top (both move together).
    PushUp,
    /// Push from the bottom (both move together).
    PushDown,
    /// Dip to black then reveal new source.
    DipToBlack,
}

impl std::str::FromStr for TransitionType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "cut" => Ok(Self::Cut),
            "fade" | "dissolve" | "crossfade" => Ok(Self::Fade),
            "slide_left" | "slideleft" => Ok(Self::SlideLeft),
            "slide_right" | "slideright" => Ok(Self::SlideRight),
            "slide_up" | "slideup" => Ok(Self::SlideUp),
            "slide_down" | "slidedown" => Ok(Self::SlideDown),
            "push_left" | "pushleft" => Ok(Self::PushLeft),
            "push_right" | "pushright" => Ok(Self::PushRight),
            "push_up" | "pushup" => Ok(Self::PushUp),
            "push_down" | "pushdown" => Ok(Self::PushDown),
            "dip_to_black" | "diptoblack" | "dip" => Ok(Self::DipToBlack),
            _ => Err(format!("Unknown transition type: {}", s)),
        }
    }
}

/// Error type for transition operations.
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("Mixer element not found: {0}")]
    MixerNotFound(String),
    #[error("Pad not found: {0}")]
    PadNotFound(String),
    #[error("Invalid input index: {0}")]
    InvalidInput(usize),
    #[error("Pipeline not running")]
    PipelineNotRunning,
    #[error("Failed to query pipeline position")]
    PositionQueryFailed,
    #[error("Failed to create control source: {0}")]
    ControlSourceError(String),
    #[error("GStreamer error: {0}")]
    GstError(String),
}

/// Manages transitions for a compositor element.
pub struct TransitionController {
    /// The compositor/mixer element.
    mixer: gst::Element,
    /// Canvas width for position calculations.
    canvas_width: i32,
    /// Canvas height for position calculations.
    canvas_height: i32,
    /// Active control sources for ongoing transitions (pad_name -> control_sources).
    /// We keep references to prevent them from being dropped during animation.
    active_transitions: Arc<Mutex<HashMap<String, Vec<InterpolationControlSource>>>>,
}

impl TransitionController {
    /// Create a new transition controller for a mixer element.
    pub fn new(mixer: gst::Element, canvas_width: i32, canvas_height: i32) -> Self {
        Self {
            mixer,
            canvas_width,
            canvas_height,
            active_transitions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a sink pad by input index.
    fn get_sink_pad(&self, input_index: usize) -> Result<gst::Pad, TransitionError> {
        // Try sink_0, sink_1, etc.
        let pad_name = format!("sink_{}", input_index);
        self.mixer
            .static_pad(&pad_name)
            .ok_or(TransitionError::PadNotFound(pad_name))
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

        info!(
            "Starting {:?} transition from input {} to {} over {}ms",
            transition_type, from_input, to_input, duration_ms
        );

        // Clean up any previous transitions (they're no longer needed)
        if let Ok(mut transitions) = self.active_transitions.lock() {
            transitions.clear();
        }

        // Get current pipeline time
        let current_time = pipeline
            .query_position::<gst::ClockTime>()
            .ok_or(TransitionError::PositionQueryFailed)?;

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
        }
    }

    /// Perform an instant cut transition.
    fn transition_cut(&self, from_input: usize, to_input: usize) -> Result<(), TransitionError> {
        let from_pad = self.get_sink_pad(from_input)?;
        let to_pad = self.get_sink_pad(to_input)?;

        // Instant alpha change
        from_pad.set_property("alpha", 0.0f64);
        to_pad.set_property("alpha", 1.0f64);

        info!("Cut transition complete: {} -> {}", from_input, to_input);
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
        let key = format!("fade_{}_{}", from_input, to_input);
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

        // Get where the from_pad currently is (this is where to_pad should end up)
        let target_x = from_pad.property::<i32>("xpos");
        let target_y = from_pad.property::<i32>("ypos");

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
            let cs = self
                .setup_int_animation(&to_pad, "xpos", start_time, end_time, to_start_x, target_x)?;
            control_sources.push(cs);
        }

        if dy != 0 {
            let cs = self
                .setup_int_animation(&to_pad, "ypos", start_time, end_time, to_start_y, target_y)?;
            control_sources.push(cs);
        }

        // After transition completes, hide from_pad
        let cs = self.setup_alpha_animation(&from_pad, end_time, end_time, 1.0, 0.0)?;
        control_sources.push(cs);

        let key = format!("slide_{}_{}", from_input, to_input);
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

        // To pad enters from opposite side
        let to_start_x = from_start_x - dx * self.canvas_width;
        let to_start_y = from_start_y - dy * self.canvas_height;
        let to_end_x = from_start_x; // Ends where from started
        let to_end_y = from_start_y;

        // Set initial position for to_pad
        to_pad.set_property("xpos", to_start_x);
        to_pad.set_property("ypos", to_start_y);
        to_pad.set_property("alpha", 1.0f64);

        // Animate from_pad position (exits)
        if dx != 0 {
            let cs = self.setup_int_animation(
                &from_pad,
                "xpos",
                start_time,
                end_time,
                from_start_x,
                from_end_x,
            )?;
            control_sources.push(cs);

            let cs = self
                .setup_int_animation(&to_pad, "xpos", start_time, end_time, to_start_x, to_end_x)?;
            control_sources.push(cs);
        }

        if dy != 0 {
            let cs = self.setup_int_animation(
                &from_pad,
                "ypos",
                start_time,
                end_time,
                from_start_y,
                from_end_y,
            )?;
            control_sources.push(cs);

            let cs = self
                .setup_int_animation(&to_pad, "ypos", start_time, end_time, to_start_y, to_end_y)?;
            control_sources.push(cs);
        }

        // After transition, hide from_pad
        let cs = self.setup_alpha_animation(&from_pad, end_time, end_time, 1.0, 0.0)?;
        control_sources.push(cs);

        let key = format!("push_{}_{}", from_input, to_input);
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
        let cs_from = InterpolationControlSource::new();
        cs_from.set_mode(InterpolationMode::Linear);

        // Add eased keyframes for first half (fade out)
        let num_keyframes = 10;
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

        let binding = DirectControlBinding::new(&from_pad, "alpha", &cs_from);
        from_pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add control binding: {}", e))
        })?;
        control_sources.push(cs_from);

        // Second half: fade in to_pad (0.0 -> 1.0) with easing
        let cs_to = InterpolationControlSource::new();
        cs_to.set_mode(InterpolationMode::Linear);

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

        let binding = DirectControlBinding::new(&to_pad, "alpha", &cs_to);
        to_pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add control binding: {}", e))
        })?;
        control_sources.push(cs_to);

        let key = format!("dip_{}_{}", from_input, to_input);
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

    /// Compute ease-in-out value using cosine interpolation.
    /// t should be in range 0.0 to 1.0, returns value in same range.
    /// This creates more noticeable acceleration/deceleration than smoothstep.
    fn ease_in_out(t: f64) -> f64 {
        // Cosine ease-in-out: more pronounced than smoothstep
        (1.0 - (t * std::f64::consts::PI).cos()) / 2.0
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
        let cs = InterpolationControlSource::new();
        cs.set_mode(InterpolationMode::Linear);

        let duration = (end_time - start_time).nseconds() as f64;
        let value_range = end_value - start_value;

        // Add keyframes along ease-in-out curve for smooth animation
        let num_keyframes = 10;
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

        // Create binding and attach to pad
        let binding = DirectControlBinding::new(pad, "alpha", &cs);
        pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add control binding: {}", e))
        })?;

        debug!(
            "Alpha animation (eased): {} -> {} on pad {}",
            start_value,
            end_value,
            pad.name()
        );

        Ok(cs)
    }

    /// Set up integer property animation on a pad (for xpos, ypos) with ease-in-out.
    fn setup_int_animation(
        &self,
        pad: &gst::Pad,
        property: &str,
        start_time: gst::ClockTime,
        end_time: gst::ClockTime,
        start_value: i32,
        end_value: i32,
    ) -> Result<InterpolationControlSource, TransitionError> {
        let cs = InterpolationControlSource::new();
        cs.set_mode(InterpolationMode::Linear);

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
        let num_keyframes = 10;
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

        let binding = DirectControlBinding::new(pad, property, &cs);
        pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add control binding: {}", e))
        })?;

        debug!(
            "Int animation (eased) ({}): {} -> {} on pad {}",
            property,
            start_value,
            end_value,
            pad.name()
        );

        Ok(cs)
    }

    /// Remove all control bindings from a pad.
    fn clear_control_bindings(&self, pad: &gst::Pad) {
        for prop in ["alpha", "xpos", "ypos", "width", "height"] {
            if let Some(binding) = pad.control_binding(prop) {
                pad.remove_control_binding(&binding);
                debug!("Removed {} control binding from pad {}", prop, pad.name());
            }
        }
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

        // Get current pipeline time
        let current_time = pipeline
            .query_position::<gst::ClockTime>()
            .ok_or(TransitionError::PositionQueryFailed)?;
        let end_time = current_time + gst::ClockTime::from_mseconds(duration_ms);

        let mut control_sources = Vec::new();

        // Animate xpos if target provided
        if let Some(target) = target_xpos {
            let current = pad.property::<i32>("xpos");
            if current != target {
                let cs = self.setup_int_animation(
                    &pad,
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
                    &pad,
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
                    &pad,
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
                    &pad,
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
        let key = format!("animate_input_{}", input_index);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transition_type_from_str() {
        assert_eq!(
            "fade".parse::<TransitionType>().ok(),
            Some(TransitionType::Fade)
        );
        assert_eq!(
            "dissolve".parse::<TransitionType>().ok(),
            Some(TransitionType::Fade)
        );
        assert_eq!(
            "cut".parse::<TransitionType>().ok(),
            Some(TransitionType::Cut)
        );
        assert_eq!(
            "slide_left".parse::<TransitionType>().ok(),
            Some(TransitionType::SlideLeft)
        );
        assert_eq!(
            "push_left".parse::<TransitionType>().ok(),
            Some(TransitionType::PushLeft)
        );
        assert_eq!(
            "push_right".parse::<TransitionType>().ok(),
            Some(TransitionType::PushRight)
        );
        assert_eq!(
            "dip_to_black".parse::<TransitionType>().ok(),
            Some(TransitionType::DipToBlack)
        );
        assert_eq!(
            "dip".parse::<TransitionType>().ok(),
            Some(TransitionType::DipToBlack)
        );
        assert!("unknown".parse::<TransitionType>().is_err());
    }
}
