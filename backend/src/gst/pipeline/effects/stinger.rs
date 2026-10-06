//! Stinger takes on the vision mixer's distribution compositor.
//!
//! A take is programmed in full before its clip starts. The take knows the
//! running time its clip's first frame lands at (the Media Player pins it,
//! see `MediaPlayerState::play_at`), so every change is a control-binding
//! keyframe at a clip timestamp: the mixer applies it on exactly that output
//! frame, and nothing waits on wall clock or holds the mixer's output.
//!
//! **Classic**: the graphic pad comes up on the clip's first frame and down
//! after its last; the program cuts, or mixes, beneath it at the cut point.
//!
//! **Track matte / mask only** (GPU): the matte pad draws nothing but takes
//! the clip's matte off the alpha of what is drawn below it (the outgoing
//! program): `dst.a = 1 - m` (`blend-equation-alpha=reverse-subtract`, see
//! the pad setup in the GPU builder). The incoming source, lifted just above
//! the matte for the clip's length, blends with `src·(1 - dst.a) + dst·dst.a`,
//! which is `B·m + A·(1 - m)`: the matte's white shows the new source. The
//! graphic goes on top. A matte frame that does not arrive leaves `dst.a` at
//! 1, so a late clip shows the old source, never a premature new one.

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_controller::prelude::*;
use gstreamer_controller::InterpolationMode;
use strom_types::stinger::{StingerLayout, StingerVariant};
use strom_types::vision_mixer::{
    DIST_PGM_ZORDER, DIST_STINGER_FILL_ZORDER, DIST_STINGER_INCOMING_ZORDER,
};
use tracing::{debug, info, warn};

use super::super::{PipelineError, PipelineManager};
use crate::blocks::builtin::vision_mixer::overlay;
use crate::stinger::ClipPlan;

/// Everything a take tells the mixer. Times are running times in ns, on the
/// mixer's output frame grid.
#[derive(Debug, Clone)]
pub struct StingerTake {
    /// One output frame.
    pub frame_ns: u64,
    pub from_input: usize,
    pub to_input: usize,
    /// The clip's first frame on air.
    pub start: u64,
    /// The first output frame after the clip.
    pub end: u64,
    /// Classic: where the program starts to change.
    pub cut_at: Option<u64>,
    /// Classic: how long the change takes (0 = cut).
    pub mix_ns: u64,
    pub plan: ClipPlan,
    /// The clip's frame size, to pick the graphic out of it.
    pub clip_size: Option<(u32, u32)>,
}

/// The stinger pads of one mixer.
pub struct StingerPads {
    pub fill: gst::Pad,
    /// Present on the GPU mixer only.
    pub matte: Option<gst::Pad>,
}

/// Clip frames reaching the mixer during a take, counted by a probe on the
/// graphic pad. Atomics only: the probe runs per buffer.
pub struct StingerStats {
    start: u64,
    end: u64,
    /// The mixer composites a frame up to this long after its timestamp.
    latency_ns: i64,
    /// Running time at `epoch`, so the probe can tell arrival time from an
    /// `Instant` instead of reading the clock.
    epoch: Instant,
    epoch_rt: i64,
    pub arrived: AtomicU32,
    pub late: AtomicU32,
    /// Smallest lead a frame had on its deadline (ns); `i64::MAX` = none.
    pub worst_margin_ns: AtomicI64,
}

/// A take's measurement probe; dropping it removes the probe.
pub struct StingerWatch {
    probes: Vec<(gst::Pad, gst::PadProbeId)>,
    pub stats: Arc<StingerStats>,
}

impl Drop for StingerWatch {
    fn drop(&mut self) {
        for (pad, probe) in self.probes.drain(..) {
            pad.remove_probe(probe);
        }
    }
}

impl StingerWatch {
    /// Smallest lead a clip frame had on its deadline, in ms.
    pub fn worst_margin_ms(&self) -> Option<f64> {
        let v = self.stats.worst_margin_ns.load(Ordering::Acquire);
        (v != i64::MAX).then_some(v as f64 / 1e6)
    }
}

/// The value a `DirectControlBinding` maps to enum value `nick` on `pad`'s
/// `prop`. The binding truncates `v·(n-1)` to an index, so aim a quarter step
/// into the bucket to stay clear of rounding.
fn enum_key(pad: &gst::Pad, prop: &str, nick: &str) -> Option<f64> {
    let pspec = pad.find_property(prop)?;
    let pspec = pspec.downcast_ref::<gst::glib::ParamSpecEnum>()?;
    let class = pspec.enum_class();
    let values = class.values();
    let index = values.iter().position(|v| v.nick() == nick)?;
    let n = values.len().max(2) as f64;
    Some(((index as f64 + 0.25) / (n - 1.0)).min(1.0))
}

/// The value a `DirectControlBinding` maps to `value` on a uint property.
fn uint_key(pad: &gst::Pad, prop: &str, value: u32) -> Option<f64> {
    let pspec = pad.find_property(prop)?;
    let pspec = pspec.downcast_ref::<gst::glib::ParamSpecUInt>()?;
    let (min, max) = (pspec.minimum() as f64, pspec.maximum() as f64);
    Some(((value as f64 + 0.25 - min) / (max - min).max(1.0)).clamp(0.0, 1.0))
}

/// Program `pad.prop` with step keyframes `(time, binding value)`.
fn keyframes(
    pad: &gst::Pad,
    prop: &str,
    mode: InterpolationMode,
    keys: &[(u64, f64)],
) -> Result<(), PipelineError> {
    let cs = crate::gst::control_bindings::fresh_control_source(pad.upcast_ref(), prop, mode)
        .ok_or_else(|| {
            PipelineError::TransitionError(format!("cannot animate {} on {}", prop, pad.name()))
        })?;
    for (t, v) in keys {
        if !cs.set(gst::ClockTime::from_nseconds(*t), *v) {
            return Err(PipelineError::TransitionError(format!(
                "cannot set a {} keyframe on {}",
                prop,
                pad.name()
            )));
        }
    }
    Ok(())
}

fn enum_keyframes(pad: &gst::Pad, prop: &str, keys: &[(u64, &str)]) -> Result<(), PipelineError> {
    let values: Option<Vec<(u64, f64)>> = keys
        .iter()
        .map(|(t, nick)| enum_key(pad, prop, nick).map(|v| (*t, v)))
        .collect();
    let values = values.ok_or_else(|| {
        PipelineError::TransitionError(format!("{} has no such value on {}", prop, pad.name()))
    })?;
    keyframes(pad, prop, InterpolationMode::None, &values)
}

fn zorder_keyframes(pad: &gst::Pad, keys: &[(u64, u32)]) -> Result<(), PipelineError> {
    let values: Option<Vec<(u64, f64)>> = keys
        .iter()
        .map(|(t, z)| uint_key(pad, "zorder", *z).map(|v| (*t, v)))
        .collect();
    let values =
        values.ok_or_else(|| PipelineError::TransitionError("zorder is not a uint".to_string()))?;
    keyframes(pad, "zorder", InterpolationMode::None, &values)
}

/// Where the graphic sits in a clip of `size`: (left, right, top, bottom)
/// pixels to crop away.
fn graphic_crop(layout: StingerLayout, size: Option<(u32, u32)>) -> (i32, i32, i32, i32) {
    let Some((w, h)) = size else {
        return (0, 0, 0, 0);
    };
    match layout {
        StingerLayout::SideBySide => (0, (w / 2) as i32, 0, 0),
        StingerLayout::Stacked => (0, 0, 0, (h / 2) as i32),
        _ => (0, 0, 0, 0),
    }
}

const BLEND_PROPS: [&str; 2] = ["blend-function-src-rgb", "blend-function-dst-rgb"];

impl PipelineManager {
    fn dist_mixer(&self, block_instance_id: &str) -> Result<&gst::Element, PipelineError> {
        let id = format!("{}:mixer", block_instance_id);
        self.elements
            .get(&id)
            .ok_or(PipelineError::ElementNotFound(id))
    }

    /// The stinger pads of a vision mixer, found by what feeds them, or
    /// `None` when the mixer was built without a stinger input.
    pub fn stinger_pads(&self, block_instance_id: &str) -> Option<StingerPads> {
        let mixer = self.dist_mixer(block_instance_id).ok()?;
        let fed_by = |name: &str| {
            let want = format!("{}:{}", block_instance_id, name);
            mixer.sink_pads().into_iter().find(|pad| {
                pad.peer()
                    .and_then(|peer| peer.parent_element())
                    .is_some_and(|e| e.name() == want)
            })
        };
        let fill = fed_by("queue_stinger_fill").or_else(|| fed_by("videocrop_stinger"))?;
        Some(StingerPads {
            fill,
            matte: fed_by("stinger_matte"),
        })
    }

    /// The mixer's output frame grid, from its negotiated caps.
    pub fn mixer_frame_grid(&self, block_instance_id: &str) -> Option<crate::stinger::FrameGrid> {
        let caps = self
            .dist_mixer(block_instance_id)
            .ok()?
            .static_pad("src")?
            .current_caps()?;
        let fps = caps.structure(0)?.get::<gst::Fraction>("framerate").ok()?;
        crate::stinger::FrameGrid::new(fps.numer(), fps.denom())
    }

    /// The flow's running time now.
    pub fn running_time_ns(&self) -> Option<u64> {
        self.pipeline.current_running_time().map(|t| t.nseconds())
    }

    /// Where the mixer's output has got to.
    pub fn mixer_position_ns(&self, block_instance_id: &str) -> Option<u64> {
        self.dist_mixer(block_instance_id)
            .ok()?
            .query_position::<gst::ClockTime>()
            .map(|t| t.nseconds())
    }

    /// Program a whole stinger take, and start counting the clip's frames.
    /// Also returns whether it cancelled a fade-to-black, as any take does.
    pub fn program_stinger(
        &self,
        block_instance_id: &str,
        take: &StingerTake,
    ) -> Result<(StingerWatch, bool), PipelineError> {
        let mixer = self.dist_mixer(block_instance_id)?;
        let pads =
            self.stinger_pads(block_instance_id)
                .ok_or_else(|| PipelineError::InvalidProperty {
                    element: block_instance_id.to_string(),
                    property: strom_types::stinger::ENABLE_STINGER_PROPERTY.to_string(),
                    reason: "this vision mixer has no stinger input".to_string(),
                })?;
        let variant = take.plan.variant;
        if variant.uses_matte() && pads.matte.is_none() {
            return Err(PipelineError::TransitionError(
                "a matte stinger needs the GPU mixer".to_string(),
            ));
        }
        let state = overlay::get_overlay_state(block_instance_id).ok_or_else(|| {
            PipelineError::ElementNotFound(format!("overlay state of {}", block_instance_id))
        })?;
        let pad = |idx: usize| {
            mixer
                .static_pad(&format!("sink_{}", idx))
                .ok_or_else(|| PipelineError::PadNotFound {
                    element: format!("{}:mixer", block_instance_id),
                    pad: format!("sink_{}", idx),
                })
        };
        let a = pad(take.from_input)?;
        let b = pad(take.to_input)?;
        let gpu = pads.matte.is_some();

        // Start from the resting state of a classic take: A alone on air,
        // the stinger pads down, no shader take left running. That also
        // ends a fade-to-black.
        let ftb_cancelled = state
            .ftb_active
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        self.reset_take_fx(block_instance_id);
        let (cw, ch) = self.dist_canvas_size(block_instance_id);
        self.reset_classic_take_pads(
            block_instance_id,
            mixer,
            Some(&state),
            Some(take.from_input),
            state.num_inputs,
            cw,
            ch,
        );
        if gpu {
            crate::gst::control_bindings::wipe_control_bindings(b.upcast_ref(), &BLEND_PROPS);
            b.set_property_from_str("blend-function-src-rgb", "src-alpha");
            b.set_property_from_str("blend-function-dst-rgb", "one-minus-src-alpha");
        }

        // The graphic: cut out of the clip, up for the clip's length.
        let (left, right, top, bottom) = graphic_crop(take.plan.layout, take.clip_size);
        if gpu {
            crate::gst::control_bindings::wipe_control_bindings(
                pads.fill.upcast_ref(),
                &crate::gst::crop::CROP_PAD_PROPS,
            );
            for (prop, v) in [
                ("crop-left", left),
                ("crop-right", right),
                ("crop-top", top),
                ("crop-bottom", bottom),
            ] {
                pads.fill.set_property(prop, v);
            }
            pads.fill.set_property_from_str(
                "blend-function-src-rgb",
                if take.plan.premultiplied {
                    "one"
                } else {
                    "src-alpha"
                },
            );
        } else {
            if let Some(crop) = self
                .elements
                .get(&format!("{}:videocrop_stinger", block_instance_id))
            {
                for (prop, v) in [
                    ("left", left),
                    ("right", right),
                    ("top", top),
                    ("bottom", bottom),
                ] {
                    crop.set_property(prop, v);
                }
            }
            if take.plan.premultiplied {
                warn!(
                    "Vision mixer {}: the software mixer composites premultiplied stinger graphics as straight; edges come out dark",
                    block_instance_id
                );
            }
        }
        pads.fill.set_property("zorder", DIST_STINGER_FILL_ZORDER);
        // A step key sits half a frame before the output frame it acts on.
        // The mixer stamps its frames from a rounded origin, so a frame's
        // timestamp can be a nanosecond either side of the grid's: a key
        // placed exactly on it would act on that frame or the next by chance.
        let step = |t: u64| t.saturating_sub(take.frame_ns / 2);
        let fill_keys: &[(u64, f64)] = if variant == StingerVariant::MaskOnly {
            &[(0, 0.0)]
        } else {
            &[(0, 0.0), (step(take.start), 1.0), (step(take.end), 0.0)]
        };
        keyframes(&pads.fill, "alpha", InterpolationMode::None, fill_keys)?;

        match (variant, &pads.matte) {
            (StingerVariant::Classic, _) => {
                let cut = take.cut_at.unwrap_or(take.start);
                if take.mix_ns == 0 {
                    b.set_property("zorder", DIST_PGM_ZORDER);
                    keyframes(
                        &a,
                        "alpha",
                        InterpolationMode::None,
                        &[(0, 1.0), (step(cut), 0.0)],
                    )?;
                    keyframes(
                        &b,
                        "alpha",
                        InterpolationMode::None,
                        &[(0, 0.0), (step(cut), 1.0)],
                    )?;
                } else {
                    // The incoming source fades in over the outgoing one.
                    let done = cut + take.mix_ns;
                    b.set_property("zorder", DIST_PGM_ZORDER + 1);
                    keyframes(
                        &b,
                        "alpha",
                        InterpolationMode::Linear,
                        &[(0, 0.0), (cut, 0.0), (done, 1.0)],
                    )?;
                    keyframes(
                        &a,
                        "alpha",
                        InterpolationMode::None,
                        &[(0, 1.0), (step(done), 0.0)],
                    )?;
                }
            }
            (_, Some(matte)) => {
                if let Some(shader) = self
                    .elements
                    .get(&format!("{}:stinger_matte", block_instance_id))
                {
                    shader.set_property(
                        "uniforms",
                        crate::gst::shaders::stinger_matte_uniforms(
                            take.plan.layout,
                            take.plan.invert_matte,
                        ),
                    );
                }
                let (start, end) = (step(take.start), step(take.end));
                keyframes(
                    matte,
                    "alpha",
                    InterpolationMode::None,
                    &[(0, 0.0), (start, 1.0), (end, 0.0)],
                )?;
                keyframes(
                    &b,
                    "alpha",
                    InterpolationMode::None,
                    &[(0, 0.0), (start, 1.0)],
                )?;
                zorder_keyframes(
                    &b,
                    &[
                        (0, DIST_PGM_ZORDER),
                        (start, DIST_STINGER_INCOMING_ZORDER),
                        (end, DIST_PGM_ZORDER),
                    ],
                )?;
                enum_keyframes(
                    &b,
                    "blend-function-src-rgb",
                    &[
                        (0, "src-alpha"),
                        (start, "one-minus-dst-alpha"),
                        (end, "src-alpha"),
                    ],
                )?;
                enum_keyframes(
                    &b,
                    "blend-function-dst-rgb",
                    &[
                        (0, "one-minus-src-alpha"),
                        (start, "dst-alpha"),
                        (end, "one-minus-src-alpha"),
                    ],
                )?;
                keyframes(
                    &a,
                    "alpha",
                    InterpolationMode::None,
                    &[(0, 1.0), (end, 0.0)],
                )?;
            }
            (_, None) => unreachable!("checked above"),
        }

        // Count the clip's frames as they reach the graphic pad.
        let latency_ns = mixer
            .find_property("latency")
            .map(|_| mixer.property::<u64>("latency") as i64)
            .unwrap_or(0);
        let stats = Arc::new(StingerStats {
            start: take.start,
            end: take.end,
            latency_ns,
            epoch: Instant::now(),
            epoch_rt: self.running_time_ns().unwrap_or(0) as i64,
            arrived: AtomicU32::new(0),
            late: AtomicU32::new(0),
            worst_margin_ns: AtomicI64::new(i64::MAX),
        });
        // Per-buffer probes for the length of one take: a timestamp check,
        // an Instant read and a few atomic updates. The graphic pad counts
        // the frames; both it and the matte pad (which has a shader pass of
        // its own ahead of it) count lateness, since a late matte frame
        // shows as much as a late graphic.
        let mut probes = Vec::new();
        let watched: Vec<(gst::Pad, bool)> = std::iter::once((pads.fill.clone(), true))
            .chain(pads.matte.clone().map(|m| (m, false)))
            .collect();
        for (pad, counts_frames) in watched {
            let counted = Arc::clone(&stats);
            let probe = pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                    return gst::PadProbeReturn::Ok;
                };
                let pts = pts.nseconds();
                if pts < counted.start || pts >= counted.end {
                    return gst::PadProbeReturn::Ok;
                }
                let arrival = counted.epoch_rt + counted.epoch.elapsed().as_nanos() as i64;
                let margin = pts as i64 + counted.latency_ns - arrival;
                if counts_frames {
                    counted.arrived.fetch_add(1, Ordering::Relaxed);
                }
                if margin < 0 {
                    counted.late.fetch_add(1, Ordering::Relaxed);
                }
                counted.worst_margin_ns.fetch_min(margin, Ordering::AcqRel);
                gst::PadProbeReturn::Ok
            });
            if let Some(probe) = probe {
                probes.push((pad, probe));
            }
        }

        info!(
            "Vision mixer {}: {:?} stinger {} -> {} on air {:.3}s..{:.3}s{}",
            block_instance_id,
            variant,
            take.from_input,
            take.to_input,
            take.start as f64 / 1e9,
            take.end as f64 / 1e9,
            take.cut_at
                .map(|c| format!(", cut at {:.3}s", c as f64 / 1e9))
                .unwrap_or_default()
        );
        Ok((StingerWatch { probes, stats }, ftb_cancelled))
    }

    /// After the take's last frame: replace its keyframes with the state they
    /// left behind, the new source plainly on air, so later takes and pad
    /// writes work on plain properties again. Changes nothing on screen.
    pub fn finish_stinger(&self, block_instance_id: &str, take: &StingerTake) {
        self.settle_stinger(block_instance_id, take, true);
    }

    /// Undo a programmed take whose clip did not start: the old source stays
    /// on air.
    pub fn abort_stinger(&self, block_instance_id: &str, take: &StingerTake) {
        self.settle_stinger(block_instance_id, take, false);
    }

    fn settle_stinger(&self, block_instance_id: &str, take: &StingerTake, completed: bool) {
        let Ok(mixer) = self.dist_mixer(block_instance_id) else {
            return;
        };
        let pad = |idx: usize| mixer.static_pad(&format!("sink_{}", idx));
        let (on, off) = if completed {
            (take.to_input, take.from_input)
        } else {
            (take.from_input, take.to_input)
        };
        let wipe = |p: &gst::Pad, props: &[&str]| {
            crate::gst::control_bindings::wipe_control_bindings(p.upcast_ref(), props)
        };
        // Order matters only for the incoming pad: plain blending first, so
        // it never sits low in the stack while still reading the matte.
        if let Some(b) = pad(on) {
            wipe(&b, &["alpha", "zorder"]);
            if b.has_property("blend-function-src-rgb") {
                wipe(&b, &BLEND_PROPS);
                b.set_property_from_str("blend-function-src-rgb", "src-alpha");
                b.set_property_from_str("blend-function-dst-rgb", "one-minus-src-alpha");
            }
            b.set_property("alpha", 1.0f64);
            b.set_property("zorder", DIST_PGM_ZORDER);
        }
        if let Some(a) = pad(off) {
            wipe(&a, &["alpha", "zorder"]);
            if a.has_property("blend-function-src-rgb") {
                wipe(&a, &BLEND_PROPS);
                a.set_property_from_str("blend-function-src-rgb", "src-alpha");
                a.set_property_from_str("blend-function-dst-rgb", "one-minus-src-alpha");
            }
            a.set_property("alpha", 0.0f64);
            a.set_property("zorder", DIST_PGM_ZORDER);
        }
        if let Some(pads) = self.stinger_pads(block_instance_id) {
            wipe(&pads.fill, &["alpha"]);
            pads.fill.set_property("alpha", 0.0f64);
            if let Some(matte) = pads.matte {
                wipe(&matte, &["alpha"]);
                matte.set_property("alpha", 0.0f64);
            }
        }
        debug!(
            "Vision mixer {}: stinger {} -> {} {}",
            block_instance_id,
            take.from_input,
            take.to_input,
            if completed { "settled" } else { "aborted" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graphic_is_cut_out_of_a_matte_clip() {
        assert_eq!(
            graphic_crop(StingerLayout::SideBySide, Some((3840, 1080))),
            (0, 1920, 0, 0)
        );
        assert_eq!(
            graphic_crop(StingerLayout::Stacked, Some((1920, 2160))),
            (0, 0, 0, 1080)
        );
        assert_eq!(
            graphic_crop(StingerLayout::Classic, Some((1920, 1080))),
            (0, 0, 0, 0)
        );
        assert_eq!(graphic_crop(StingerLayout::SideBySide, None), (0, 0, 0, 0));
    }
}
