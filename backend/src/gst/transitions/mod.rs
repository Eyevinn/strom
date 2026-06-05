//! Scene transitions using GStreamer Controller API.
//!
//! This module provides animated transitions between compositor inputs using
//! GStreamer's interpolation control source to animate pad properties over time.
//!
//! Sub-modules:
//!   - [`plan`] — pure `plan_transition` decision function (no GStreamer deps).
//!   - [`controller`] — `TransitionController` impl that drives compositor pads.

use crate::gst::shaders::{MasterFxKind, WipeKind};
use gstreamer as gst;
use gstreamer_controller::InterpolationControlSource;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

mod controller;
mod plan;

pub use plan::plan_transition;
pub(crate) use plan::rect_contains;

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
    /// Shader-mask wipe on the incoming source (GPU FX engine). Downgrades
    /// to Fade when the engine is unavailable (CPU backend or enable_fx=false).
    Wipe(crate::gst::shaders::WipeKind),
    /// Full-frame master FX (glitch, flash, whip, ...) riding on a basic pad
    /// transition (GPU FX engine). Downgrades like [`Self::Wipe`].
    MasterFx(crate::gst::shaders::MasterFxKind),
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
            "wipe_left" => Ok(Self::Wipe(WipeKind::Left)),
            "wipe_right" => Ok(Self::Wipe(WipeKind::Right)),
            "wipe_up" => Ok(Self::Wipe(WipeKind::Up)),
            "wipe_down" => Ok(Self::Wipe(WipeKind::Down)),
            "clock_wipe" | "clockwipe" | "clock" => Ok(Self::Wipe(WipeKind::Clock)),
            "iris_open" | "iris_in" => Ok(Self::Wipe(WipeKind::IrisOpen)),
            "iris_close" | "iris_out" => Ok(Self::Wipe(WipeKind::IrisClose)),
            "blinds" => Ok(Self::Wipe(WipeKind::Blinds)),
            "checker_wipe" | "checker" => Ok(Self::Wipe(WipeKind::Checker)),
            "noise_dissolve" | "noise" => Ok(Self::Wipe(WipeKind::Noise)),
            "luma_wipe" | "luma" => Ok(Self::Wipe(WipeKind::Luma)),
            "melt" | "doom" => Ok(Self::Wipe(WipeKind::Melt)),
            "barn_doors" | "barndoors" => Ok(Self::Wipe(WipeKind::BarnDoors)),
            "heart_iris" | "heart" => Ok(Self::Wipe(WipeKind::Heart)),
            "star_wipe" | "star" => Ok(Self::Wipe(WipeKind::Star)),
            "pinwheel" => Ok(Self::Wipe(WipeKind::Pinwheel)),
            "crosshatch" => Ok(Self::Wipe(WipeKind::Crosshatch)),
            "hex_dissolve" | "hex" => Ok(Self::Wipe(WipeKind::Hex)),
            "warp_wipe" | "warp" => Ok(Self::Wipe(WipeKind::Warp)),
            "glitch_cut" | "glitch" => Ok(Self::MasterFx(MasterFxKind::Glitch)),
            "flash_dissolve" | "flash" => Ok(Self::MasterFx(MasterFxKind::Flash)),
            "whip_pan_left" | "whip_left" => Ok(Self::MasterFx(MasterFxKind::WhipLeft)),
            "whip_pan_right" | "whip_right" => Ok(Self::MasterFx(MasterFxKind::WhipRight)),
            "punch_zoom" | "punch" => Ok(Self::MasterFx(MasterFxKind::Punch)),
            "pixelate_take" => Ok(Self::MasterFx(MasterFxKind::Pixelate)),
            "zoom_blur" | "zoomblur" => Ok(Self::MasterFx(MasterFxKind::ZoomBlur)),
            "spin" => Ok(Self::MasterFx(MasterFxKind::Spin)),
            "tv_roll" | "roll" => Ok(Self::MasterFx(MasterFxKind::Roll)),
            "negative_flash" | "negative" => Ok(Self::MasterFx(MasterFxKind::Negative)),
            "ripple" => Ok(Self::MasterFx(MasterFxKind::Ripple)),
            _ => Err(format!("Unknown transition type: {}", s)),
        }
    }
}

impl std::fmt::Display for TransitionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cut => "cut",
            Self::Fade => "fade",
            Self::SlideLeft => "slide_left",
            Self::SlideRight => "slide_right",
            Self::SlideUp => "slide_up",
            Self::SlideDown => "slide_down",
            Self::PushLeft => "push_left",
            Self::PushRight => "push_right",
            Self::PushUp => "push_up",
            Self::PushDown => "push_down",
            Self::DipToBlack => "dip_to_black",
            Self::Wipe(k) => k.name(),
            Self::MasterFx(k) => k.name(),
        })
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
    /// Active control sources for ongoing transitions (unique key -> control_sources).
    /// We keep references to prevent them from being dropped during animation.
    /// Keys are generated from `next_transition_id` so concurrent transitions never
    /// overwrite each other's bookkeeping — see `next_key`.
    active_transitions: Arc<Mutex<HashMap<String, Vec<InterpolationControlSource>>>>,
    /// Monotonic counter feeding unique suffixes into `active_transitions` keys.
    next_transition_id: Arc<AtomicU64>,
}

/// Target state for a content pad's border underlay — the solid-color pad
/// sitting directly beneath it in z-order that renders the zone border as a
/// frame around the box. Geometry is the content rect inflated outward by
/// the region-scaled border width (and clamped to the region), computed by
/// `pads_for_source` where the region is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderlayTarget {
    /// Compositor sink index of the underlay pad.
    pub pad_idx: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Border color as `0xAARRGGBB` (written to the underlay source's
    /// `foreground-color`).
    pub argb: u32,
}

/// A pad's target geometry + zorder in a composition. Used by [`plan_transition`]
/// and [`TransitionController::animate_pad_transition`].
///
/// `underlay` rides along outside the planner's view: [`plan_transition`]
/// decides actions from the content geometry only, and the controller drives
/// each content pad's underlay in lockstep with whatever action the content
/// pad got (underlay zorder = content zorder − 1 throughout — see
/// `strom_types::vision_mixer::underlay_zorder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadTarget {
    pub pad_idx: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub zorder: u32,
    /// Border underlay state for this pad, when its zone has a visible
    /// border (`None` = no border → underlay hidden).
    pub underlay: Option<UnderlayTarget>,
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
