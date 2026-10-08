//! Stinger transitions: what a clip is, and how a take plays it.
//!
//! - [`analysis`] decodes a clip once to learn its shape, whether it carries
//!   alpha, and where its graphic covers or its matte switches.
//! - [`examples`] renders the example clips Strom ships, one per variant.
//! - This module turns a clip's settings and analysis into a [`ClipPlan`]:
//!   which variant runs on this mixer, and where a classic one cuts.
//!
//! The mixer side lives in `gst::pipeline::effects::stinger` (programming the
//! pads for a take) and `state::stinger` (cue, take, completion, events).

pub mod analysis;
pub mod examples;

use strom_types::stinger::{
    StingerBeneath, StingerClipInfo, StingerClipSettings, StingerLayout, StingerVariant,
};

/// The frame shapes a stinger is made in: 16:9, 4:3 and vertical 9:16.
const FRAME_SHAPES: [f64; 3] = [16.0 / 9.0, 4.0 / 3.0, 9.0 / 16.0];

/// How a clip of `width`x`height` lays out, from the clip alone, given
/// whether its frames carry alpha and look grey.
///
/// A clip made of two standard frames side by side (32:9 for HD) or one
/// above the other (16:18) is a track-matte clip, the two OBS layouts.
/// Anything else is one picture: a graphic when it carries alpha or colour,
/// a mask when it is grey and opaque. The program's format plays no part:
/// a clip is what it is, whatever the production runs.
pub fn detect_layout(width: u32, height: u32, has_alpha: bool, looks_grey: bool) -> StingerLayout {
    let single = || {
        if has_alpha || !looks_grey {
            StingerLayout::Classic
        } else {
            StingerLayout::MaskOnly
        }
    };
    if width == 0 || height == 0 {
        return single();
    }
    let aspect = width as f64 / height as f64;
    let near = |target: f64| (aspect - target).abs() <= target * 0.04;
    if FRAME_SHAPES.iter().any(|s| near(s * 2.0)) {
        StingerLayout::SideBySide
    } else if FRAME_SHAPES.iter().any(|s| near(s / 2.0)) {
        StingerLayout::Stacked
    } else {
        single()
    }
}

/// How one take plays one clip on one mixer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipPlan {
    /// What runs.
    pub variant: StingerVariant,
    /// What the clip asked for, when that could not run here.
    pub downgraded_from: Option<StingerVariant>,
    /// The clip's layout, resolved (never `Auto`).
    pub layout: StingerLayout,
    /// Clip length.
    pub duration_ms: u64,
    /// Classic: where the program starts to change, into the clip.
    pub cut_point_ms: Option<u64>,
    /// Classic: how long the change takes (0 = cut).
    pub mix_ms: u64,
    pub premultiplied: bool,
    pub invert_matte: bool,
}

/// Work out how a clip plays.
///
/// `info` is the clip's analysis when it has run; without it the layout must
/// be set explicitly or is taken as classic, and the cut point falls back to
/// the middle of `fallback_duration_ms`.
pub fn plan_clip(
    settings: &StingerClipSettings,
    info: Option<&StingerClipInfo>,
    matte_supported: bool,
    fallback_duration_ms: Option<u64>,
) -> Result<ClipPlan, String> {
    let duration_ms = info
        .map(|i| i.duration_ms)
        .filter(|d| *d > 0)
        .or(fallback_duration_ms.filter(|d| *d > 0))
        .ok_or_else(|| "the clip has no readable length".to_string())?;

    let layout = match settings.layout {
        StingerLayout::Auto => info
            .map(|i| i.detected_layout)
            .unwrap_or(StingerLayout::Classic),
        explicit => explicit,
    };
    let wanted = layout.variant().unwrap_or(StingerVariant::Classic);
    let (variant, downgraded_from) = if wanted.uses_matte() && !matte_supported {
        (StingerVariant::Classic, Some(wanted))
    } else {
        (wanted, None)
    };

    let last_ms = duration_ms.saturating_sub(1);
    let (cut_point_ms, mix_ms) = if variant != StingerVariant::Classic {
        (None, 0)
    } else if downgraded_from == Some(StingerVariant::MaskOnly) {
        // No graphic to hide a cut behind: mix across the span where the
        // matte moves, or across the middle third without an analysis. A
        // cut point the operator set is a classic cut point: it cuts or
        // mixes as `beneath` says, and the mix ends with the clip.
        match settings.cut_point_ms {
            Some(cut) => {
                let cut = cut.min(last_ms);
                let mix = match settings.beneath {
                    StingerBeneath::Cut => 0,
                    StingerBeneath::Mix => settings.mix_ms.min(duration_ms - cut),
                };
                (Some(cut), mix)
            }
            None => {
                let span = info.and_then(|i| Some((i.matte_start_ms?, i.matte_end_ms?)));
                match span {
                    Some((start, end)) if end > start => {
                        (Some(start.min(last_ms)), (end - start).min(duration_ms))
                    }
                    _ => (Some(duration_ms / 3), duration_ms / 3),
                }
            }
        }
    } else {
        let cut = settings
            .cut_point_ms
            .or_else(|| {
                let i = info?;
                if downgraded_from.is_some() {
                    // A track-matte clip's matte says where the switch is.
                    i.matte_midpoint_ms.or(i.cover_peak_ms)
                } else {
                    i.cover_peak_ms.filter(|_| i.cover_peak >= 0.5)
                }
            })
            .unwrap_or(duration_ms / 2)
            .min(last_ms);
        let mix = match settings.beneath {
            StingerBeneath::Cut => 0,
            StingerBeneath::Mix => settings.mix_ms.min(duration_ms - cut),
        };
        (Some(cut), mix)
    };

    Ok(ClipPlan {
        variant,
        downgraded_from,
        layout,
        duration_ms,
        cut_point_ms,
        mix_ms,
        premultiplied: settings.premultiplied,
        invert_matte: settings.invert_matte,
    })
}

/// A mixer's output frames: frame `n` is stamped `n · den/num` seconds,
/// rounded to the nanosecond the way the aggregator rounds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGrid {
    pub num: u64,
    pub den: u64,
}

impl FrameGrid {
    pub fn new(num: i32, den: i32) -> Option<Self> {
        (num > 0 && den > 0).then_some(Self {
            num: num as u64,
            den: den as u64,
        })
    }

    /// Timestamp of output frame `n`.
    pub fn pts(&self, n: u64) -> u64 {
        ((n as u128 * 1_000_000_000 * self.den as u128 + self.num as u128 / 2) / self.num as u128)
            as u64
    }

    /// The first output frame stamped at or after `t`, give or take a
    /// microsecond of rounding.
    pub fn at_or_after(&self, t: u64) -> u64 {
        let t = t.saturating_sub(1_000);
        let n = (t as u128 * self.num as u128).div_ceil(1_000_000_000 * self.den as u128) as u64;
        self.pts(n)
    }

    /// Nominal frame length.
    pub fn frame_ns(&self) -> u64 {
        self.pts(1)
    }
}

/// When a take's changes land, as running times on the mixer's output grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeTimes {
    /// The clip's first frame on air.
    pub start: u64,
    /// The first output frame after the clip.
    pub end: u64,
    /// Classic: where the program starts to change.
    pub cut_at: Option<u64>,
    /// Classic: how long the change takes (0 = cut).
    pub mix_ns: u64,
}

/// Place a take of `plan` on `grid`: the clip, `clip_ns` long, starts on the
/// first output frame at or after `earliest`.
pub fn take_times(grid: &FrameGrid, earliest: u64, clip_ns: u64, plan: &ClipPlan) -> TakeTimes {
    let start = grid.at_or_after(earliest);
    let end = grid.at_or_after(start + clip_ns);
    let cut_at = plan.cut_point_ms.map(|c| {
        grid.at_or_after(start + c * 1_000_000)
            .min(end.saturating_sub(grid.frame_ns()))
    });
    // The cut point moved up to the output grid; the mix still ends with the
    // clip, before the graphic comes down.
    let mix_ns = cut_at.map_or(0, |cut| {
        (plan.mix_ms * 1_000_000).min(end.saturating_sub(cut))
    });
    TakeTimes {
        start,
        end,
        cut_at,
        mix_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(layout: StingerLayout) -> StingerClipInfo {
        StingerClipInfo {
            width: 1920,
            height: 1080,
            framerate_num: 30,
            framerate_den: 1,
            frames: 45,
            duration_ms: 1500,
            has_alpha: true,
            detected_layout: layout,
            cover_peak_ms: Some(700),
            cover_peak: 1.0,
            matte_midpoint_ms: Some(800),
            matte_start_ms: Some(300),
            matte_end_ms: Some(1100),
            analysis_ms: 1,
        }
    }

    #[test]
    fn layout_follows_shape_and_alpha() {
        assert_eq!(
            detect_layout(3840, 1080, true, false),
            StingerLayout::SideBySide
        );
        assert_eq!(
            detect_layout(1920, 2160, false, false),
            StingerLayout::Stacked
        );
        assert_eq!(
            detect_layout(1920, 1080, true, false),
            StingerLayout::Classic
        );
        assert_eq!(
            detect_layout(1920, 1080, false, true),
            StingerLayout::MaskOnly
        );
        // A 4K side-by-side clip on an HD program is still side by side.
        assert_eq!(
            detect_layout(7680, 2160, true, false),
            StingerLayout::SideBySide
        );
        // Opaque colour footage covers the frame: a classic stinger.
        assert_eq!(
            detect_layout(1280, 720, false, false),
            StingerLayout::Classic
        );
        // 4:3 and vertical clips, whatever the program runs.
        assert_eq!(
            detect_layout(1440, 540, true, false),
            StingerLayout::SideBySide
        );
        assert_eq!(
            detect_layout(1080, 3840, true, false),
            StingerLayout::Stacked
        );
        assert_eq!(
            detect_layout(2160, 1920, true, false),
            StingerLayout::SideBySide,
            "two 9:16 frames side by side"
        );
        // A square graphic is one picture.
        assert_eq!(
            detect_layout(1080, 1080, true, false),
            StingerLayout::Classic
        );
    }

    #[test]
    fn a_classic_clip_cuts_where_its_graphic_covers_most() {
        let plan = plan_clip(
            &StingerClipSettings::default(),
            Some(&info(StingerLayout::Classic)),
            true,
            None,
        )
        .unwrap();
        assert_eq!(plan.variant, StingerVariant::Classic);
        assert_eq!(plan.cut_point_ms, Some(700));
        assert_eq!(plan.mix_ms, 0);
    }

    #[test]
    fn a_declared_cut_point_wins_and_stays_inside_the_clip() {
        let settings = StingerClipSettings {
            cut_point_ms: Some(5_000),
            beneath: StingerBeneath::Mix,
            mix_ms: 400,
            ..Default::default()
        };
        let plan = plan_clip(&settings, Some(&info(StingerLayout::Classic)), true, None).unwrap();
        assert_eq!(plan.cut_point_ms, Some(1499));
        assert_eq!(plan.mix_ms, 1, "the mix ends with the clip");
    }

    #[test]
    fn track_matte_runs_on_the_gpu_and_downgrades_on_the_cpu() {
        let i = info(StingerLayout::SideBySide);
        let gpu = plan_clip(&StingerClipSettings::default(), Some(&i), true, None).unwrap();
        assert_eq!(gpu.variant, StingerVariant::TrackMatte);
        assert_eq!(gpu.cut_point_ms, None);

        let cpu = plan_clip(&StingerClipSettings::default(), Some(&i), false, None).unwrap();
        assert_eq!(cpu.variant, StingerVariant::Classic);
        assert_eq!(cpu.downgraded_from, Some(StingerVariant::TrackMatte));
        assert_eq!(cpu.cut_point_ms, Some(800), "cuts where the matte switches");
        assert_eq!(cpu.layout, StingerLayout::SideBySide);
    }

    #[test]
    fn a_mask_downgrades_to_a_mix_across_the_matte() {
        let plan = plan_clip(
            &StingerClipSettings::default(),
            Some(&info(StingerLayout::MaskOnly)),
            false,
            None,
        )
        .unwrap();
        assert_eq!(plan.variant, StingerVariant::Classic);
        assert_eq!(plan.downgraded_from, Some(StingerVariant::MaskOnly));
        assert_eq!(plan.cut_point_ms, Some(300));
        assert_eq!(plan.mix_ms, 800);
    }

    #[test]
    fn a_downgraded_mask_with_a_cut_point_honours_cut() {
        let settings = StingerClipSettings {
            cut_point_ms: Some(1000),
            beneath: StingerBeneath::Cut,
            mix_ms: 1000,
            ..Default::default()
        };
        let plan = plan_clip(&settings, Some(&info(StingerLayout::MaskOnly)), false, None).unwrap();
        assert_eq!(plan.downgraded_from, Some(StingerVariant::MaskOnly));
        assert_eq!(plan.cut_point_ms, Some(1000));
        assert_eq!(plan.mix_ms, 0, "beneath = cut switches without a mix");
    }

    #[test]
    fn a_downgraded_mask_mix_ends_with_the_clip() {
        let settings = StingerClipSettings {
            cut_point_ms: Some(1000),
            beneath: StingerBeneath::Mix,
            mix_ms: 1000,
            ..Default::default()
        };
        // The analysed clip is 1500 ms long.
        let plan = plan_clip(&settings, Some(&info(StingerLayout::MaskOnly)), false, None).unwrap();
        assert_eq!(plan.downgraded_from, Some(StingerVariant::MaskOnly));
        assert_eq!(plan.cut_point_ms, Some(1000));
        assert_eq!(plan.mix_ms, 500, "the mix may not outlast the clip");
    }

    #[test]
    fn without_analysis_the_layout_must_be_set_or_is_classic() {
        let plan = plan_clip(&StingerClipSettings::default(), None, true, Some(1000)).unwrap();
        assert_eq!(plan.variant, StingerVariant::Classic);
        assert_eq!(plan.cut_point_ms, Some(500));
        assert!(plan_clip(&StingerClipSettings::default(), None, true, None).is_err());
    }

    #[test]
    fn take_times_land_on_the_output_grid() {
        let g = FrameGrid::new(30, 1).unwrap();
        assert_eq!(g.at_or_after(0), 0);
        assert_eq!(g.at_or_after(1), 0, "within rounding of frame 0");
        assert_eq!(g.at_or_after(2_000), 33_333_333);
        assert_eq!(g.pts(3), 100_000_000);
        // A clip of 30 frames at 30 fps from frame 7 ends exactly 30 frames on.
        let start = g.pts(7);
        assert_eq!(g.at_or_after(start + 1_000_000_000), g.pts(37));
        let ntsc = FrameGrid::new(30000, 1001).unwrap();
        assert_eq!(ntsc.pts(1), 33_366_667);
        assert_eq!(ntsc.at_or_after(ntsc.pts(1000)), ntsc.pts(1000));
        assert!(FrameGrid::new(0, 1).is_none());
    }

    #[test]
    fn a_mix_ends_with_the_clip_when_the_cut_point_moves_to_the_grid() {
        // On a 25 fps grid a 1520 ms clip ends on an output frame, but its
        // cut point at 990 ms moves up 10 ms to the next one. The mix,
        // planned to end with the clip from the unmoved cut point, would then
        // run 10 ms past the clip.
        let settings = StingerClipSettings {
            cut_point_ms: Some(990),
            beneath: StingerBeneath::Mix,
            mix_ms: 1000,
            ..Default::default()
        };
        let plan = plan_clip(&settings, None, true, Some(1520)).unwrap();
        assert_eq!(plan.mix_ms, 530);
        let grid = FrameGrid::new(25, 1).unwrap();
        let t = take_times(&grid, 13_000_000, 1_520_000_000, &plan);
        let cut = t.cut_at.unwrap();
        assert!(cut > t.start + 990_000_000, "the cut moved up: {t:?}");
        assert!(
            cut + t.mix_ns <= t.end,
            "the mix must end before the graphic comes down: {t:?}"
        );
        assert!(t.mix_ns > 0, "{t:?}");
    }
}
