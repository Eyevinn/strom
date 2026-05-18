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
use strom_types::vision_mixer;
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

/// A pad's target geometry + zorder in a composition. Used by [`plan_transition`]
/// and [`TransitionController::animate_pad_transition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadTarget {
    pub pad_idx: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub zorder: u32,
}

/// How a morphing pad's zorder is handled during the animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZHandling {
    /// Snap to the new zorder at the start of the animation. Used when the new
    /// composition has pads at a higher zorder than this pad — the morphing
    /// pad should slide *under* them as it moves into place.
    SnapToNew(u32),
    /// Lift to [`vision_mixer::TRANSITION_FOREGROUND_ZORDER`] for the duration
    /// so the moving pad stays on top of everything it crosses, then step to
    /// `new_z` at `end_time`.
    LiftAndStep { new_z: u32 },
}

/// What [`plan_transition`] decided each pad should do during the transition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PadAction {
    /// Pad appears in both old and new at *different* geometry — animate
    /// position+size linearly from old → new. Alpha stays 1.
    Morph {
        from_x: i32,
        from_y: i32,
        from_w: i32,
        from_h: i32,
        to_x: i32,
        to_y: i32,
        to_w: i32,
        to_h: i32,
        z_handling: ZHandling,
    },
    /// Pad appears in both old and new at the *same* geometry — no animation,
    /// just affirm the new state.
    AffirmStatic {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        zorder: u32,
    },
    /// Non-shared incoming pad: place at new geometry with alpha=1 immediately.
    /// Used when a morphing pad will reveal/cover this position over time.
    HoldFullAlpha {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        zorder: u32,
    },
    /// Non-shared incoming pad: pre-position then animate alpha 0 → 1.
    /// Used when no morphing pad exists or when there's a same-position
    /// outgoing partner (cross-fade in place).
    FadeIn {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        zorder: u32,
    },
    /// Non-shared outgoing pad: animate alpha 1 → 0 over the duration.
    FadeOut,
    /// Non-shared outgoing pad: stay at alpha=1 (covered by a morphing pad
    /// growing/moving), then snap to alpha=0 at end_time.
    StepOffAtEnd,
}

/// Returns true if `outer` fully contains `inner`.
fn rect_contains(outer: (i32, i32, i32, i32), inner: (i32, i32, i32, i32)) -> bool {
    outer.0 <= inner.0
        && outer.1 <= inner.1
        && outer.0 + outer.2 >= inner.0 + inner.2
        && outer.1 + outer.3 >= inner.1 + inner.3
}

/// Decide per-pad actions for a transition between two compositions. Pure
/// function with no GStreamer dependencies — drives the `animate_pad_transition`
/// test matrix.
///
/// The core rule: **what's visible needs a transition; what's hidden doesn't.**
///   - A non-shared incoming pad whose rect is fully *covered* by some morphing
///     pad at the start of the animation can sit at `alpha=1` immediately
///     (`HoldFullAlpha`) — it's hidden behind the morph until the morph reveals
///     it by shrinking/moving.
///   - A non-shared incoming pad that would be *visible* at t=0 (not covered by
///     any morph) needs a smooth `FadeIn` so it doesn't pop in.
///   - Symmetric for outgoing pads: covered at t=end → `StepOffAtEnd`; visible
///     at t=end → `FadeOut`.
///
/// Same-position swaps (e.g. two different PiP bgs both fullscreen) always
/// cross-fade so the swap is smooth.
pub fn plan_transition(outgoing: &[PadTarget], incoming: &[PadTarget]) -> Vec<(usize, PadAction)> {
    use std::collections::HashSet;

    let outgoing_map: HashMap<usize, PadTarget> =
        outgoing.iter().map(|t| (t.pad_idx, *t)).collect();
    let incoming_map: HashMap<usize, PadTarget> =
        incoming.iter().map(|t| (t.pad_idx, *t)).collect();

    // Collect start/end rects of every morphing (shared + position-changing) pad.
    let mut morph_start_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    let mut morph_end_rects: Vec<(i32, i32, i32, i32)> = Vec::new();
    for t in incoming {
        if let Some(o) = outgoing_map.get(&t.pad_idx) {
            if o.x != t.x || o.y != t.y || o.w != t.w || o.h != t.h {
                morph_start_rects.push((o.x, o.y, o.w, o.h));
                morph_end_rects.push((t.x, t.y, t.w, t.h));
            }
        }
    }

    let outgoing_nonshared_positions: HashSet<(i32, i32, i32, i32)> = outgoing
        .iter()
        .filter(|t| !incoming_map.contains_key(&t.pad_idx))
        .map(|t| (t.x, t.y, t.w, t.h))
        .collect();
    let incoming_nonshared_positions: HashSet<(i32, i32, i32, i32)> = incoming
        .iter()
        .filter(|t| !outgoing_map.contains_key(&t.pad_idx))
        .map(|t| (t.x, t.y, t.w, t.h))
        .collect();

    let mut plan = Vec::new();

    for t in incoming {
        if let Some(o) = outgoing_map.get(&t.pad_idx) {
            let morphing = o.x != t.x || o.y != t.y || o.w != t.w || o.h != t.h;
            if morphing {
                let new_has_overlays_above_me = incoming.iter().any(|i_t| {
                    i_t.pad_idx != t.pad_idx
                        && !outgoing_map.contains_key(&i_t.pad_idx)
                        && i_t.zorder > t.zorder
                });
                let z_handling = if new_has_overlays_above_me {
                    ZHandling::SnapToNew(t.zorder)
                } else {
                    ZHandling::LiftAndStep { new_z: t.zorder }
                };
                plan.push((
                    t.pad_idx,
                    PadAction::Morph {
                        from_x: o.x,
                        from_y: o.y,
                        from_w: o.w,
                        from_h: o.h,
                        to_x: t.x,
                        to_y: t.y,
                        to_w: t.w,
                        to_h: t.h,
                        z_handling,
                    },
                ));
            } else {
                plan.push((
                    t.pad_idx,
                    PadAction::AffirmStatic {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            }
        } else {
            let pad_rect = (t.x, t.y, t.w, t.h);
            let same_pos_outgoing = outgoing_nonshared_positions.contains(&pad_rect);
            let covered_at_start = morph_start_rects
                .iter()
                .any(|r| rect_contains(*r, pad_rect));
            // Cross-fade when there's a same-position swap (smooth same-rect blend)
            // OR when the pad is visible at t=0 (no morph covers it).
            // Hold at full alpha only when fully hidden behind a morphing pad.
            if !same_pos_outgoing && covered_at_start {
                plan.push((
                    t.pad_idx,
                    PadAction::HoldFullAlpha {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            } else {
                plan.push((
                    t.pad_idx,
                    PadAction::FadeIn {
                        x: t.x,
                        y: t.y,
                        w: t.w,
                        h: t.h,
                        zorder: t.zorder,
                    },
                ));
            }
        }
    }

    for t in outgoing {
        if incoming_map.contains_key(&t.pad_idx) {
            continue;
        }
        let pad_rect = (t.x, t.y, t.w, t.h);
        let same_pos_incoming = incoming_nonshared_positions.contains(&pad_rect);
        let covered_at_end = morph_end_rects.iter().any(|r| rect_contains(*r, pad_rect));
        if !same_pos_incoming && covered_at_end {
            plan.push((t.pad_idx, PadAction::StepOffAtEnd));
        } else {
            plan.push((t.pad_idx, PadAction::FadeOut));
        }
    }

    plan
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

    /// Remove all control bindings from a pad. Must cover every property the
    /// transition setup helpers can bind: alpha (fades + step-off), xpos/ypos/
    /// width/height (position animation), and zorder (morph z lift/step).
    fn clear_control_bindings(&self, pad: &gst::Pad) {
        for prop in ["alpha", "xpos", "ypos", "width", "height", "zorder"] {
            if let Some(binding) = pad.control_binding(prop) {
                pad.remove_control_binding(&binding);
                debug!("Removed {} control binding from pad {}", prop, pad.name());
            }
        }
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

        for (idx, action) in &plan {
            let pad = self.get_sink_pad(*idx)?;
            self.clear_control_bindings(&pad);

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
                            pad.set_property("zorder", vision_mixer::TRANSITION_FOREGROUND_ZORDER);
                            control_sources.push(self.setup_zorder_step(
                                &pad,
                                current_time,
                                end_time,
                                vision_mixer::TRANSITION_FOREGROUND_ZORDER,
                                new_z,
                            )?);
                        }
                    }
                    pad.set_property("xpos", from_x);
                    pad.set_property("ypos", from_y);
                    pad.set_property("width", from_w);
                    pad.set_property("height", from_h);
                    control_sources.push(self.setup_int_animation(
                        &pad,
                        "xpos",
                        current_time,
                        end_time,
                        from_x,
                        to_x,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        &pad,
                        "ypos",
                        current_time,
                        end_time,
                        from_y,
                        to_y,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        &pad,
                        "width",
                        current_time,
                        end_time,
                        from_w,
                        to_w,
                    )?);
                    control_sources.push(self.setup_int_animation(
                        &pad,
                        "height",
                        current_time,
                        end_time,
                        from_h,
                        to_h,
                    )?);
                }
                PadAction::AffirmStatic { x, y, w, h, zorder } => {
                    pad.set_property("alpha", 1.0f64);
                    pad.set_property("zorder", zorder);
                    pad.set_property("xpos", x);
                    pad.set_property("ypos", y);
                    pad.set_property("width", w);
                    pad.set_property("height", h);
                }
                PadAction::HoldFullAlpha { x, y, w, h, zorder } => {
                    pad.set_property("xpos", x);
                    pad.set_property("ypos", y);
                    pad.set_property("width", w);
                    pad.set_property("height", h);
                    pad.set_property("zorder", zorder);
                    pad.set_property("alpha", 1.0f64);
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
                }
                PadAction::FadeOut => {
                    control_sources.push(self.setup_alpha_animation(
                        &pad,
                        current_time,
                        end_time,
                        1.0,
                        0.0,
                    )?);
                }
                PadAction::StepOffAtEnd => {
                    control_sources.push(self.setup_alpha_step_off(
                        &pad,
                        current_time,
                        end_time,
                    )?);
                }
            }
        }

        let key = format!("morph_o{}_i{}", outgoing.len(), incoming.len());
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
        let cs = InterpolationControlSource::new();
        cs.set_mode(InterpolationMode::None);

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
        let binding = DirectControlBinding::new(pad, "zorder", &cs);
        pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add zorder control binding: {}", e))
        })?;
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
        let cs = InterpolationControlSource::new();
        cs.set_mode(InterpolationMode::None); // constant-between-keyframes
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
        let binding = DirectControlBinding::new(pad, "alpha", &cs);
        pad.add_control_binding(&binding).map_err(|e| {
            TransitionError::GstError(format!("Failed to add control binding: {}", e))
        })?;
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

    // ------------------------------------------------------------------------
    // plan_transition matrix
    // ------------------------------------------------------------------------
    //
    // Tests below treat input indices as pad indices (pad_base=0). The exact
    // zorder values are not critical — what matters is the relative ordering
    // (PGM=1, OVL=2) that drives the `has_overlays_above_me` decision.

    const PGM_Z: u32 = strom_types::vision_mixer::DIST_PGM_ZORDER;
    const OVL_Z: u32 = strom_types::vision_mixer::DIST_PIP_OVERLAY_ZORDER;

    fn pad(idx: usize, x: i32, y: i32, w: i32, h: i32, z: u32) -> PadTarget {
        PadTarget {
            pad_idx: idx,
            x,
            y,
            w,
            h,
            zorder: z,
        }
    }

    /// Look up a plan entry for a pad; panics if not present.
    fn action_of(plan: &[(usize, PadAction)], idx: usize) -> PadAction {
        plan.iter()
            .find(|(i, _)| *i == idx)
            .unwrap_or_else(|| panic!("no plan entry for pad {} in {:?}", idx, plan))
            .1
    }

    fn fullscreen(idx: usize, z: u32) -> PadTarget {
        pad(idx, 0, 0, 1920, 1080, z)
    }
    fn ovl_a(idx: usize) -> PadTarget {
        // overlay cell 0 (left half, vertically centered for 16:9)
        pad(idx, 0, 270, 960, 540, OVL_Z)
    }
    fn ovl_b(idx: usize) -> PadTarget {
        pad(idx, 960, 270, 960, 540, OVL_Z)
    }

    #[test]
    fn input_to_input_pure_crossfade() {
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(1, PGM_Z)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 1), PadAction::FadeIn { .. }));
    }

    #[test]
    fn input_to_pip_with_same_input_as_bg_keeps_pad_static() {
        // Input(0) fullscreen → Pip{bg=0, overlays=[1]}: 0 stays at same fullscreen position
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(0, PGM_Z), fullscreen(1, OVL_Z)];
        let plan = plan_transition(&old, &new);
        // Shared pad 0 with same geometry → AffirmStatic
        assert!(matches!(
            action_of(&plan, 0),
            PadAction::AffirmStatic { .. }
        ));
        // Incoming-only pad 1 → no morphing pad in plan → FadeIn
        assert!(matches!(action_of(&plan, 1), PadAction::FadeIn { .. }));
    }

    #[test]
    fn input_to_pip_source_becomes_overlay_morphs_lifted() {
        // Input(0) fullscreen → Pip{bg=1, overlays=[0]}:
        //   0 morphs from fullscreen to overlay (gets covered/revealed by going up)
        let old = vec![fullscreen(0, PGM_Z)];
        let new = vec![fullscreen(1, PGM_Z), ovl_a(0)];
        let plan = plan_transition(&old, &new);
        // 0 morphs; new state has no pad above pad 0 (pad 1 is at PGM_Z=1, pad 0 is at OVL_Z=2)
        // → LiftAndStep so 0 stays on top while moving
        match action_of(&plan, 0) {
            PadAction::Morph { z_handling, .. } => match z_handling {
                ZHandling::LiftAndStep { new_z } => assert_eq!(new_z, OVL_Z),
                _ => panic!("expected LiftAndStep, got {:?}", z_handling),
            },
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 1 is non-shared incoming, has_morphing_pad=true → HoldFullAlpha
        assert!(matches!(
            action_of(&plan, 1),
            PadAction::HoldFullAlpha { .. }
        ));
    }

    #[test]
    fn pip_to_input_overlay_zooms_to_fullscreen_lifted() {
        // Pip{bg=0, overlays=[1,2]} → Input(1) fullscreen
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1), ovl_b(2)];
        let new = vec![fullscreen(1, PGM_Z)];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs; new state has no other pads → LiftAndStep
        match action_of(&plan, 1) {
            PadAction::Morph { z_handling, .. } => {
                assert_eq!(z_handling, ZHandling::LiftAndStep { new_z: PGM_Z })
            }
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 0 (old bg, fullscreen) is outgoing-only; no same-position incoming → StepOffAtEnd
        assert!(matches!(action_of(&plan, 0), PadAction::StepOffAtEnd));
        assert!(matches!(action_of(&plan, 2), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_no_overlap_pure_crossfade() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[3]}: no shared sources
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(3)];
        let plan = plan_transition(&old, &new);
        // No morphing pad → bg-vs-bg same-position FadeIn/FadeOut
        // overlays at same position → also FadeIn/FadeOut
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 1), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
        assert!(matches!(action_of(&plan, 3), PadAction::FadeIn { .. }));
    }

    #[test]
    fn pip_to_pip_overlay_becomes_bg_slides_under() {
        // Pip{bg=0, overlays=[1,2]} → Pip{bg=1, overlays=[3,4]}:
        //   pad 1 morphs from overlay → bg with overlays above → SnapToNew.
        //   pad 4 is at cell-b which is also pad 2's old position → cross-fade.
        //   pad 3 is at cell-a which had pad 1 (shared, excluded) → HoldFullAlpha.
        //   pad 0 (old bg fullscreen) — no non-shared incoming at fullscreen → StepOffAtEnd.
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1), ovl_b(2)];
        let new = vec![fullscreen(1, PGM_Z), ovl_a(3), ovl_b(4)];
        let plan = plan_transition(&old, &new);
        match action_of(&plan, 1) {
            PadAction::Morph { z_handling, .. } => assert_eq!(
                z_handling,
                ZHandling::SnapToNew(PGM_Z),
                "becoming bg with overlays above → snap z",
            ),
            other => panic!("expected Morph, got {:?}", other),
        }
        // pad 3 has no same-position outgoing partner (cell-a was the morphing
        // pad's old position; shared pads don't count) → HoldFullAlpha.
        assert!(matches!(
            action_of(&plan, 3),
            PadAction::HoldFullAlpha { .. }
        ));
        // pad 4 at cell-b has same-position partner (pad 2 outgoing) → cross-fade.
        assert!(matches!(action_of(&plan, 4), PadAction::FadeIn { .. }));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeOut));
        // pad 0 fullscreen has no same-position incoming → step off.
        assert!(matches!(action_of(&plan, 0), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_bg_becomes_overlay_lifts() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[0]}: pad 0 bg→overlay
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(0)];
        let plan = plan_transition(&old, &new);
        // pad 0 morphs; new state has no pad above pad 0's new zorder (OVL_Z) — pad 2 is at PGM_Z below
        // → LiftAndStep
        match action_of(&plan, 0) {
            PadAction::Morph { z_handling, .. } => {
                assert_eq!(z_handling, ZHandling::LiftAndStep { new_z: OVL_Z })
            }
            other => panic!("expected Morph, got {:?}", other),
        }
        assert!(matches!(
            action_of(&plan, 2),
            PadAction::HoldFullAlpha { .. }
        ));
        assert!(matches!(action_of(&plan, 1), PadAction::StepOffAtEnd));
    }

    #[test]
    fn pip_to_pip_shared_static_bg_crossfade_overlays() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=0, overlays=[2]}: 0 stationary, 1 out, 2 in
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(0, PGM_Z), ovl_a(2)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(
            action_of(&plan, 0),
            PadAction::AffirmStatic { .. }
        ));
        // overlays at same position (cell-0) but different inputs → cross-fade
        assert!(matches!(action_of(&plan, 1), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn pip_to_pip_different_bg_same_overlay_crossfades_bg() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=2, overlays=[1]}: bg cross-fades, overlay static
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(2, PGM_Z), ovl_a(1)];
        let plan = plan_transition(&old, &new);
        // No morphing pad — pad 1 has same position. So no morph mode.
        assert!(matches!(
            action_of(&plan, 1),
            PadAction::AffirmStatic { .. }
        ));
        // Both bgs at fullscreen — same-position partners → cross-fade
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn incoming_outside_morph_start_fades_in_not_hold() {
        // Pip{overlays=[1]} → Pip{bg=2, overlays=[1 moved to cell-b], 3 at cell-c}
        // pad 1 morphs from cell-a → cell-b. morph_start = cell-a.
        // pad 2 (new bg) at fullscreen — NOT covered by cell-a → should FadeIn
        //   (the user's "background syns innan transition är klar" case).
        // pad 3 doesn't exist here; we use cell-c logic implicitly.
        let cell_a = pad(1, 0, 0, 480, 270, OVL_Z);
        let cell_b = pad(1, 480, 0, 480, 270, OVL_Z);
        let old = vec![cell_a];
        let new = vec![fullscreen(2, PGM_Z), cell_b];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs cell-a → cell-b
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // pad 2 fullscreen incoming — not covered by morph_start (cell-a) → FadeIn
        assert!(matches!(action_of(&plan, 2), PadAction::FadeIn { .. }));
    }

    #[test]
    fn outgoing_outside_morph_end_fades_out_not_step() {
        // Pip{bg=0, overlays=[1 at cell-a]} → Input(1) but positioned at cell-c
        //   (hypothetical — destination not fullscreen).
        // pad 0 (outgoing fullscreen) is NOT covered by morph_end (cell-c) → FadeOut.
        let old = vec![fullscreen(0, PGM_Z), pad(1, 0, 0, 480, 270, OVL_Z)];
        // pad 1 morphs to a small destination (not fullscreen)
        let new = vec![pad(1, 100, 100, 600, 400, PGM_Z)];
        let plan = plan_transition(&old, &new);
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // pad 0 fullscreen — not covered by morph_end (600×400) → FadeOut
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
    }

    #[test]
    fn morph_with_same_position_partners_crossfades_those_partners() {
        // Pip{bg=0, overlays=[1]} → Pip{bg=3, overlays=[1 moved]} where the bgs share
        // fullscreen position. pad 1 morphs (different overlay slot), so we ARE
        // in morph mode — but the bgs should still cross-fade because same-pos.
        let old = vec![fullscreen(0, PGM_Z), ovl_a(1)];
        let new = vec![fullscreen(3, PGM_Z), ovl_b(1)];
        let plan = plan_transition(&old, &new);
        // pad 1 morphs from cell-0 to cell-1
        assert!(matches!(action_of(&plan, 1), PadAction::Morph { .. }));
        // Bgs at fullscreen — same-position partners → cross-fade even in morph mode
        assert!(matches!(action_of(&plan, 0), PadAction::FadeOut));
        assert!(matches!(action_of(&plan, 3), PadAction::FadeIn { .. }));
    }
}
