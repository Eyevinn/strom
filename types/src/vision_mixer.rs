//! Vision mixer constants and defaults.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// A source that can be assigned to PGM or PVW.
///
/// JSON encoding is externally tagged: `{"input": 5}` or `{"pip": 1}`.
/// The [`FromStr`] / [`fmt::Display`] impls use a separate compact form
/// (`"input:N"` / `"pip:N"`, case-insensitive) for stored block properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A regular video input by index.
    Input(usize),
    /// A configured PiP composition by index.
    Pip(usize),
}

impl Source {
    /// If this source is a plain input, return its index; otherwise `None`.
    pub fn as_input(self) -> Option<usize> {
        match self {
            Source::Input(i) => Some(i),
            Source::Pip(_) => None,
        }
    }

    /// If this source is a PiP, return its index; otherwise `None`.
    pub fn as_pip(self) -> Option<usize> {
        match self {
            Source::Pip(p) => Some(p),
            Source::Input(_) => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Input(i) => write!(f, "input:{}", i),
            Source::Pip(p) => write!(f, "pip:{}", p),
        }
    }
}

/// Error returned when a [`Source`] string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSourceError;

impl fmt::Display for ParseSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected 'input:N' or 'pip:N'")
    }
}

impl std::error::Error for ParseSourceError {}

impl FromStr for Source {
    type Err = ParseSourceError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (kind, num) = s.split_once(':').ok_or(ParseSourceError)?;
        let idx: usize = num.trim().parse().map_err(|_| ParseSourceError)?;
        match kind.trim().to_ascii_lowercase().as_str() {
            "input" | "in" => Ok(Source::Input(idx)),
            "pip" => Ok(Source::Pip(idx)),
            _ => Err(ParseSourceError),
        }
    }
}

/// Default number of video inputs.
pub const DEFAULT_NUM_INPUTS: usize = 4;

/// Maximum number of video inputs. Soft cap — CPU/GPU/memory are the real
/// ceiling. The multiview thumbnail grid scales rows dynamically so any value
/// in `[MIN_NUM_INPUTS, MAX_NUM_INPUTS]` produces a usable layout (smaller
/// thumbnails for larger N).
pub const MAX_NUM_INPUTS: usize = 16;

/// Minimum number of video inputs.
pub const MIN_NUM_INPUTS: usize = 2;

/// Default PGM (distribution) output resolution.
pub const DEFAULT_PGM_RESOLUTION: &str = "1920x1080";

/// Default multiview output resolution.
pub const DEFAULT_MULTIVIEW_RESOLUTION: &str = "1280x720";

/// Default initial PGM input index.
pub const DEFAULT_PGM_INPUT: usize = 0;

/// Default initial PVW input index.
pub const DEFAULT_PVW_INPUT: usize = 1;

/// Border width in pixels for PVW indicator on multiview.
pub const PVW_BORDER_WIDTH: f64 = 4.0;

/// Border width in pixels for PGM indicator on multiview.
pub const PGM_BORDER_WIDTH: f64 = 4.0;

/// Border width in pixels for selected thumbnail indicators on multiview.
pub const THUMBNAIL_BORDER_WIDTH: f64 = 4.0;

/// Maximum number of DSK (Downstream Keyer) inputs.
pub const MAX_DSK_INPUTS: usize = 4;

/// Default number of DSK inputs (0 = no DSK).
pub const DEFAULT_DSK_INPUTS: usize = 0;

/// Maximum number of PiP (Picture-in-Picture) tiles rendered virtually in the multiview.
/// Each PiP consumes one tile in the multiview thumbnail grid alongside the inputs.
pub const MAX_NUM_PIPS: usize = 4;

/// Default number of PiP tiles (0 = no PiP).
pub const DEFAULT_NUM_PIPS: usize = 0;

/// Maximum number of overlay sources placed on top of the PiP background.
/// Capped at `MAX_NUM_INPUTS - 1` because the bg consumes one input.
/// Auto-tiling supports any 1..=MAX_PIP_OVERLAYS via [`compute_pip_overlay_rects`].
pub const MAX_PIP_OVERLAYS: usize = MAX_NUM_INPUTS - 1;

/// Default compositor latency in milliseconds.
pub const DEFAULT_LATENCY_MS: u64 = 20;

/// Default minimum upstream latency in milliseconds.
pub const DEFAULT_MIN_UPSTREAM_LATENCY_MS: u64 = 20;

/// Default PGM output framerate (fps as "numerator/denominator").
pub const DEFAULT_PGM_FRAMERATE: &str = "30/1";

/// Default multiview output framerate.
pub const DEFAULT_MULTIVIEW_FRAMERATE: &str = "30/1";

/// Whether to download GPU memory to system memory on output (GPU path only).
pub const DEFAULT_GL_DOWNLOAD: bool = false;

/// Whether to build the shader FX slots (looks, wipes, master FX) into the
/// GPU pipeline. GPU path only — the CPU compositor has no FX engine.
pub const DEFAULT_ENABLE_FX: bool = true;

/// Whether to swap the PVW and PGM positions in the multiview layout. When
/// false (default) PVW is on the left and PGM on the right; when true they
/// are mirrored.
pub const DEFAULT_SWAP_PVW_PGM: bool = false;

// --- Z-order constants for compositor pads ---

/// Z-order for thumbnail pads on the multiview compositor.
pub const MV_THUMBNAIL_ZORDER: u32 = 1;

/// Z-order for PGM/PVW big display pads on the multiview compositor.
pub const MV_BIG_DISPLAY_ZORDER: u32 = 10;

/// Z-order for the PGM source on the distribution compositor.
pub const DIST_PGM_ZORDER: u32 = 1;

/// Base z-order for DSK pads on the distribution compositor (+ dsk index).
pub const DIST_DSK_BASE_ZORDER: u32 = 100;

/// Z-order for PiP overlay pads on the distribution compositor when PGM is a PiP source.
/// Must be above [`DIST_PGM_ZORDER`] (which the bg uses) and below DSK.
pub const DIST_PIP_OVERLAY_ZORDER: u32 = 2;

/// Z-order for PiP overlay pads on the multiview compositor's PVW big region
/// when PVW is a PiP source. Must be above [`MV_BIG_DISPLAY_ZORDER`] (the bg).
pub const MV_PVW_PIP_OVERLAY_ZORDER: u32 = 11;

/// Z-order used for the *shared* pad during a morph transition — lifted above
/// any other video pad so the source that morphs visually covers the non-shared
/// pads underneath. Must sit above the highest static zone slot
/// ([`MV_PIP_OVERLAY_ZORDER`] 21 + 2·14 + 1 = 50 at [`MAX_PIP_OVERLAYS`])
/// and keep lifted values (this + new_z) below [`DIST_DSK_BASE_ZORDER`]
/// (100) on the dist mixer (dist new_z ≤ 31) and below the multiview
/// overlay (200) on mv_comp (mv new_z ≤ 50).
pub const TRANSITION_FOREGROUND_ZORDER: u32 = 60;

/// Compositor z-order for a zone source's *content* pad.
///
/// Zone slots use a doubled z-order scheme so every content pad has a slot
/// directly beneath it for its border underlay pad: content sits at
/// `overlay_zorder + 2·slot + 1`, its underlay at [`underlay_zorder`] (one
/// below). Box k's underlay thereby renders *above* box k-1's content — an
/// overlapping higher zone covers the lower zone's border exactly like a
/// stacked framed card.
pub fn zone_content_zorder(overlay_zorder: u32, slot_offset: u32) -> u32 {
    overlay_zorder + 2 * slot_offset + 1
}

/// Z-order of the border underlay pad paired with a content pad: always the
/// slot directly beneath it. Holds in every state — static zone layouts (the
/// doubled scheme of [`zone_content_zorder`]) and transition lifts
/// ([`TRANSITION_FOREGROUND_ZORDER`] + new_z preserves odd spacing).
pub fn underlay_zorder(content_zorder: u32) -> u32 {
    content_zorder.saturating_sub(1)
}

/// Z-order for the PiP background pad on the multiview compositor.
/// Above thumbnails (1) and the big PVW/PGM display (10), below cairo overlay (200).
pub const MV_PIP_BG_ZORDER: u32 = 20;

/// Z-order for PiP overlay pads on the multiview compositor (must be above the bg).
/// All overlays share the same z-order: in tile mode they don't overlap each other.
pub const MV_PIP_OVERLAY_ZORDER: u32 = 21;

/// Z-order for the overlay pad on the multiview compositor.
pub const MV_OVERLAY_ZORDER: u32 = 200;

// --- Overlay rendering constants ---

/// Overlay appsrc output framerate (fps).
pub const OVERLAY_FRAMERATE: i32 = 30;

/// Timezone refresh interval in seconds (for DST transitions).
pub const TIMEZONE_REFRESH_SECS: u64 = 60;

// --- VU meter constants ---

/// Default for rendering VU meters on the multiview overlay.
pub const DEFAULT_SHOW_VU_METERS: bool = true;

/// Lowest dBFS value represented on the VU meter (below this = empty bar).
pub const VU_METER_MIN_DB: f64 = -60.0;

/// Highest dBFS value represented on the VU meter (0 dBFS = full bar).
pub const VU_METER_MAX_DB: f64 = 0.0;

/// dBFS threshold above which the VU bar turns yellow.
pub const VU_METER_YELLOW_DB: f64 = -18.0;

/// dBFS threshold above which the VU bar turns orange.
pub const VU_METER_ORANGE_DB: f64 = -9.0;

/// dBFS threshold above which the VU bar turns red.
pub const VU_METER_RED_DB: f64 = -6.0;

/// Level meter message interval in nanoseconds (100 ms).
pub const VU_METER_INTERVAL_NS: u64 = 100_000_000;

/// Quantize an RMS/peak value in dBFS to u8 (0 = silence, 255 = 0 dBFS).
/// Used for lock-free atomic storage of per-input meter values.
pub fn quantize_db_to_u8(db: f64) -> u8 {
    let clamped = db.clamp(VU_METER_MIN_DB, VU_METER_MAX_DB);
    let norm = (clamped - VU_METER_MIN_DB) / (VU_METER_MAX_DB - VU_METER_MIN_DB);
    (norm * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Inverse of `quantize_db_to_u8` — maps u8 back to normalized 0.0..1.0 bar height.
pub fn u8_to_meter_fraction(v: u8) -> f64 {
    v as f64 / 255.0
}

// --- Transition animation constants ---

/// Number of keyframes for easing curve interpolation.
pub const TRANSITION_KEYFRAMES: usize = 10;

// --- Source layout helpers ---

/// Normalized rectangle in container coordinates. Each component is in `0.0..=1.0`.
/// `(x, y)` is the top-left corner; `(w, h)` is the size.
///
/// Used to position a PiP overlay slot anywhere inside its parent region without
/// coupling to the output resolution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct NormRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NormRect {
    /// Returns true if the rect lies entirely inside `0..=1` and has positive size.
    pub fn is_valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.w.is_finite()
            && self.h.is_finite()
            && self.x >= 0.0
            && self.y >= 0.0
            && self.w > 0.0
            && self.h > 0.0
            && self.x + self.w <= 1.0 + 1e-6
            && self.y + self.h <= 1.0 + 1e-6
    }

    /// Clamp components into `0..=1` and ensure `x + w <= 1`, `y + h <= 1`.
    pub fn clamped(&self) -> Self {
        let x = self.x.clamp(0.0, 1.0);
        let y = self.y.clamp(0.0, 1.0);
        let w = self.w.clamp(0.0, 1.0 - x);
        let h = self.h.clamp(0.0, 1.0 - y);
        Self { x, y, w, h }
    }

    /// Project the normalized rect into a container region given in pixels.
    /// Result is `(x, y, w, h)` with components clamped to `>= 0` and `w, h >= 1`.
    pub fn to_pixels(
        &self,
        container_x: i32,
        container_y: i32,
        container_w: i32,
        container_h: i32,
    ) -> (i32, i32, i32, i32) {
        let cw = container_w.max(0) as f32;
        let ch = container_h.max(0) as f32;
        let x = container_x + (self.x * cw).round() as i32;
        let y = container_y + (self.y * ch).round() as i32;
        let w = ((self.w * cw).round() as i32).max(1);
        let h = ((self.h * ch).round() as i32).max(1);
        (x, y, w, h)
    }
}

/// Resolve final pixel rects for all overlay slots within `(cx, cy, cw, ch)`.
///
/// Slots with `Some(rect)` use that rect (clamped + projected onto the container).
/// Slots with `None` fall back to the auto-tile position from
/// [`compute_pip_overlay_rects`] — including the case where every slot is `None`,
/// which yields the default auto-tile layout.
pub fn resolve_pip_overlay_rects(
    container_x: i32,
    container_y: i32,
    container_w: i32,
    container_h: i32,
    slots: &[Option<NormRect>],
    source_aspect: f64,
) -> Vec<(i32, i32, i32, i32)> {
    if slots.is_empty() {
        return Vec::new();
    }
    let auto = compute_pip_overlay_rects(
        container_x,
        container_y,
        container_w,
        container_h,
        slots.len(),
        source_aspect,
    );
    slots
        .iter()
        .enumerate()
        .map(|(i, slot)| match slot {
            Some(r) => r
                .clamped()
                .to_pixels(container_x, container_y, container_w, container_h),
            None => auto
                .get(i)
                .copied()
                .unwrap_or((container_x, container_y, 1, 1)),
        })
        .collect()
}

/// Smallest fraction of a source axis that must stay visible after cropping.
/// Keeps `SourceCrop::clamped` from producing zero-width/height frames.
pub const MIN_CROP_VISIBLE: f32 = 0.01;

/// Normalized per-source crop: the fraction of the source hidden from each
/// edge. All components are in `0.0..=1.0`; all zero = no crop.
///
/// The crop selects which part of the source fills its destination rect —
/// combined with a zone rect this gives "zoom"/"punch-in" framing (the
/// cropped region scales to fill the zone box; everything outside is hidden).
/// Normalized fractions keep the type resolution-independent; the backend
/// converts to pixels against the negotiated source caps.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SourceCrop {
    /// Fraction of the source width hidden from the left edge.
    #[serde(default)]
    pub left: f32,
    /// Fraction of the source height hidden from the top edge.
    #[serde(default)]
    pub top: f32,
    /// Fraction of the source width hidden from the right edge.
    #[serde(default)]
    pub right: f32,
    /// Fraction of the source height hidden from the bottom edge.
    #[serde(default)]
    pub bottom: f32,
}

impl SourceCrop {
    /// Returns `true` when the crop hides nothing (within float tolerance).
    pub fn is_zero(&self) -> bool {
        self.left.max(0.0) < 1e-6
            && self.top.max(0.0) < 1e-6
            && self.right.max(0.0) < 1e-6
            && self.bottom.max(0.0) < 1e-6
    }

    /// Clamp each component into `0..=1` and ensure at least
    /// [`MIN_CROP_VISIBLE`] of each axis stays visible. Non-finite components
    /// are treated as 0. Mirrors [`NormRect::clamped`]: clamping is a layout
    /// concern, not semantic state.
    pub fn clamped(&self) -> Self {
        let sanitize = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let left = sanitize(self.left);
        let top = sanitize(self.top);
        let right = sanitize(self.right).min((1.0 - left - MIN_CROP_VISIBLE).max(0.0));
        let bottom = sanitize(self.bottom).min((1.0 - top - MIN_CROP_VISIBLE).max(0.0));
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Convert to pixel crop values `(left, right, top, bottom)` for a source
    /// of `src_w` × `src_h` pixels — the value order matches the
    /// `crop-left`/`crop-right`/`crop-top`/`crop-bottom` compositor pad
    /// properties. At least one pixel per axis stays visible.
    pub fn to_pixels(&self, src_w: i32, src_h: i32) -> (i32, i32, i32, i32) {
        let c = self.clamped();
        let w = src_w.max(1) as f32;
        let h = src_h.max(1) as f32;
        let left = (c.left * w).round() as i32;
        let top = (c.top * h).round() as i32;
        let right = ((c.right * w).round() as i32).min(src_w - left - 1).max(0);
        let bottom = ((c.bottom * h).round() as i32).min(src_h - top - 1).max(0);
        (
            left.min(src_w - 1).max(0),
            right,
            top.min(src_h - 1).max(0),
            bottom,
        )
    }
}

/// Negotiated resolution of a video input, read from the compositor sink pad
/// caps. Inputs can have arbitrary resolutions and aspect ratios — nothing
/// normalizes them before the mixer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct InputResolution {
    pub width: u32,
    pub height: u32,
}

/// Per-input source aspect ratios (width / height of the negotiated caps),
/// keyed by input index. Inputs with unknown caps are simply absent.
pub type SourceAspects = std::collections::BTreeMap<usize, f64>;

/// Largest rect with `content_aspect` centered inside the given box — the
/// explicit-geometry replacement for the compositor's `keep-aspect-ratio`
/// sizing policy (which cannot be used together with pad crop: it fits by
/// the *uncropped* input DAR, and flipping the policy enum mid-transition
/// snaps visibly). All pads run `sizing-policy=none`; layout code computes
/// the letterbox/fill rect itself with this helper.
///
/// `content_aspect <= 0` (unknown caps — nothing is flowing yet) returns the
/// box unchanged; the caps probe re-applies geometry once caps arrive.
pub fn aspect_fit_rect(
    box_x: i32,
    box_y: i32,
    box_w: i32,
    box_h: i32,
    content_aspect: f64,
) -> (i32, i32, i32, i32) {
    if content_aspect <= 0.0 || !content_aspect.is_finite() || box_w <= 0 || box_h <= 0 {
        return (box_x, box_y, box_w.max(1), box_h.max(1));
    }
    let box_aspect = box_w as f64 / box_h as f64;
    if content_aspect > box_aspect {
        // Wider than the box → full width, reduced height (letterbox).
        let h = ((box_w as f64 / content_aspect).round() as i32).clamp(1, box_h);
        (box_x, box_y + (box_h - h) / 2, box_w, h)
    } else {
        // Taller than the box → full height, reduced width (pillarbox).
        let w = ((box_h as f64 * content_aspect).round() as i32).clamp(1, box_w);
        (box_x + (box_w - w) / 2, box_y, w, box_h)
    }
}

/// Aspect ratio of what a source actually shows after cropping: the crop
/// window scales the raw source aspect by the window's normalized w/h ratio.
/// Unknown source aspect (`<= 0`) stays unknown.
pub fn effective_source_aspect(src_aspect: f64, crop: Option<&SourceCrop>) -> f64 {
    if src_aspect <= 0.0 || !src_aspect.is_finite() {
        return 0.0;
    }
    match crop {
        Some(c) if !c.is_zero() => {
            let c = c.clamped();
            let win_w = (1.0 - c.left - c.right).max(MIN_CROP_VISIBLE) as f64;
            let win_h = (1.0 - c.top - c.bottom).max(MIN_CROP_VISIBLE) as f64;
            src_aspect * win_w / win_h
        }
        _ => src_aspect,
    }
}

/// Per-source crop transforms within a PiP, keyed by input index.
///
/// The crop applies to the input wherever it renders inside that PiP (bg or
/// any zone), and follows it across zone FIFO reshuffles. A missing key means
/// no crop. The same input can carry different crops in different PiPs.
///
/// Entries persist when their source leaves the PiP: they are inert while
/// the source is absent and re-apply when it returns, so swap-zone workflows
/// (capacity 1, pushing between sources) keep each source's punch-in framing.
pub type PipTransforms = std::collections::BTreeMap<usize, SourceCrop>;

/// Border drawn around each source box in a zone.
///
/// Rendered by the mixer itself on a PGM-side overlay — borders are a
/// function of the mixer's own live geometry (boxes move with morphs, takes
/// and punch-ins), so no external graphics source could stay in sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct ZoneBorder {
    /// Border color as `#RRGGBB` or `#RRGGBBAA` hex.
    pub color: String,
    /// Border width in PGM canvas pixels, drawn outward from the box edge.
    /// Every render target scales it by `region_width / pgm_width`, so the
    /// border looks proportionally identical on PGM, the PVW big display and
    /// the PiP thumbnails regardless of resolution.
    pub width: f32,
}

/// Maximum zone border width in PGM canvas pixels.
pub const MAX_ZONE_BORDER_WIDTH: f32 = 64.0;

impl ZoneBorder {
    /// Parse `color` into RGBA components in `0.0..=1.0`. Accepts `#RRGGBB`
    /// and `#RRGGBBAA` (case-insensitive). Returns `None` for anything else.
    pub fn rgba(&self) -> Option<(f64, f64, f64, f64)> {
        let s = self.color.trim().strip_prefix('#')?;
        if !(s.len() == 6 || s.len() == 8) || !s.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
        let (r, g, b) = (p(0)?, p(2)?, p(4)?);
        let a = if s.len() == 8 { p(6)? } else { 255 };
        Some((
            r as f64 / 255.0,
            g as f64 / 255.0,
            b as f64 / 255.0,
            a as f64 / 255.0,
        ))
    }

    /// Clamp the width into `0..=MAX_ZONE_BORDER_WIDTH` (non-finite → 0).
    pub fn clamped_width(&self) -> f32 {
        if self.width.is_finite() {
            self.width.clamp(0.0, MAX_ZONE_BORDER_WIDTH)
        } else {
            0.0
        }
    }

    /// Pack the color as `0xAARRGGBB` (big-endian ARGB — the format
    /// `videotestsrc`'s `foreground-color` property expects). `None` for
    /// unparseable colors, like [`Self::rgba`].
    pub fn argb(&self) -> Option<u32> {
        let (r, g, b, a) = self.rgba()?;
        let q = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u32;
        Some((q(a) << 24) | (q(r) << 16) | (q(g) << 8) | q(b))
    }

    /// Resolve into the `Copy` form carried by [`ZonePadLayout`]. `None`
    /// when the border would not draw anything (zero width, invalid color,
    /// or fully transparent).
    pub fn resolved(&self) -> Option<ResolvedBorder> {
        if !self.is_visible() {
            return None;
        }
        Some(ResolvedBorder {
            width: self.clamped_width(),
            argb: self.argb()?,
        })
    }

    /// A border that would actually draw something.
    pub fn is_visible(&self) -> bool {
        self.clamped_width() > 0.0 && self.rgba().map(|c| c.3 > 0.0).unwrap_or(false)
    }
}

/// A sub-region of a PiP that hosts one or more overlay sources.
///
/// Sources inside a zone auto-tile within its `rect` (using
/// [`compute_pip_overlay_rects`]) so the zone behaves like a "mini-PiP"
/// nested inside the parent PiP region. The `capacity` puts a cap on how
/// many sources can occupy the zone; pushing a new source into a full zone
/// is expected to evict the oldest (client-side FIFO).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct Zone {
    /// Where the zone sits within the parent PiP region.
    /// `None` = fill the entire PiP region.
    #[serde(default)]
    pub rect: Option<NormRect>,
    /// Max sources allowed in the zone. `None` = unlimited (up to
    /// [`MAX_PIP_OVERLAYS`]). A capacity of 1 is "swap mode": replacing
    /// the source animates a cross-fade.
    #[serde(default)]
    pub capacity: Option<usize>,
    /// Current sources (FIFO, oldest first). Sources auto-tile within `rect`.
    #[serde(default)]
    pub sources: Vec<usize>,
    /// Border drawn around each source box in this zone (None = no border).
    /// Rendered live on the PGM overlay — follows morphs/takes/punch-ins.
    #[serde(default)]
    pub border: Option<ZoneBorder>,
}

impl Zone {
    /// Returns `true` when the zone would not contribute any visible pads.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Effective source slice respecting `capacity` (truncate from the front,
    /// keeping the newest entries).
    pub fn effective_sources(&self) -> &[usize] {
        match self.capacity {
            Some(cap) if cap < self.sources.len() => {
                let start = self.sources.len() - cap;
                &self.sources[start..]
            }
            _ => &self.sources[..],
        }
    }
}

/// A zone border resolved into `Copy`-friendly form for per-pad layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedBorder {
    /// Border width in PGM canvas pixels (clamped, > 0).
    pub width: f32,
    /// Border color packed as `0xAARRGGBB`.
    pub argb: u32,
}

/// Per-pad layout produced by [`resolve_zone_pads`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZonePadLayout {
    pub input: usize,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 0-based slot offset within the PiP. Sources later in a zone's FIFO
    /// render on top of earlier ones; pass through [`zone_content_zorder`]
    /// for the actual compositor z-order (slots are doubled to leave room
    /// for border underlay pads).
    pub zorder_offset: u32,
    /// The hosting zone's border, when it would actually draw. Rendered as
    /// a solid-color underlay pad directly beneath this source's pad.
    pub border: Option<ResolvedBorder>,
}

/// Compute pixel-space pad layouts for every source across every zone.
///
/// Each zone's `rect` is projected onto `(container_x, container_y,
/// container_w, container_h)` (or defaults to the full container when
/// `rect` is `None`). A zone holding a single source uses its full projected
/// rect as the cell; multiple sources auto-tile into a slot grid via
/// [`compute_pip_overlay_rects`] (sized with `fallback_aspect`, the canvas
/// aspect). Each source is then aspect-fitted inside its cell using its
/// *effective* aspect — the negotiated source aspect from `src_aspects`
/// adjusted by any crop window in `transforms` — so a crop locked to the box
/// aspect fills it exactly, an unlocked crop letterboxes correctly, and an
/// uncropped odd-aspect source letterboxes inside its cell. Sources with
/// unknown caps fall back to `fallback_aspect`.
///
/// This is explicit geometry: pads run `sizing-policy=none`, so these rects
/// are exactly what renders (see [`aspect_fit_rect`] for why).
///
/// Duplicate sources across zones are filtered: only the first occurrence
/// keeps its pad layout. Sources that exceed a zone's `capacity` are
/// dropped (oldest first), matching [`Zone::effective_sources`].
#[allow(clippy::too_many_arguments)]
pub fn resolve_zone_pads(
    container_x: i32,
    container_y: i32,
    container_w: i32,
    container_h: i32,
    zones: &[Zone],
    fallback_aspect: f64,
    transforms: &PipTransforms,
    src_aspects: &SourceAspects,
) -> Vec<ZonePadLayout> {
    let mut out: Vec<ZonePadLayout> = Vec::new();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for zone in zones {
        let sources = zone.effective_sources();
        if sources.is_empty() {
            continue;
        }
        let (zx, zy, zw, zh) = match zone.rect {
            Some(r) => r
                .clamped()
                .to_pixels(container_x, container_y, container_w, container_h),
            None => (container_x, container_y, container_w, container_h),
        };
        let cells = if sources.len() == 1 {
            vec![(zx, zy, zw, zh)]
        } else {
            compute_pip_overlay_rects(zx, zy, zw, zh, sources.len(), fallback_aspect)
        };
        for (i, &input) in sources.iter().enumerate() {
            if !seen.insert(input) {
                continue;
            }
            let (cx, cy, cw, ch) = cells.get(i).copied().unwrap_or((zx, zy, 1, 1));
            let aspect = effective_source_aspect(
                src_aspects.get(&input).copied().unwrap_or(fallback_aspect),
                transforms.get(&input),
            );
            let (x, y, w, h) = aspect_fit_rect(cx, cy, cw, ch, aspect);
            out.push(ZonePadLayout {
                input,
                x,
                y,
                w,
                h,
                zorder_offset: out.len() as u32,
                border: zone.border.as_ref().and_then(|b| b.resolved()),
            });
        }
    }
    out
}

/// Compute sub-rectangles for PiP overlays within a container, preserving the
/// source aspect ratio.
///
/// Layout strategy:
///   - Cells are arranged in a `cols × rows` grid where
///     `cols = ceil(sqrt(N))`, `rows = ceil(N / cols)`. For 1..=4 the cells
///     lay out as 1, 2-side, 2-top+1-bot, 2×2.
///   - Each cell's size is constrained to `source_aspect`, so the rendered
///     source fills the cell exactly — no transparent letterbox bands that
///     would otherwise let the bg peek through when the pads are stacked.
///   - The grid is centered within the container, leaving symmetric margins
///     wherever the cells don't fill the full container area.
///
/// If `source_aspect <= 0.0` the function falls back to uniform-cell tiling
/// (no aspect preservation), matching the pre-aspect behavior.
pub fn compute_pip_overlay_rects(
    container_x: i32,
    container_y: i32,
    container_w: i32,
    container_h: i32,
    count: usize,
    source_aspect: f64,
) -> Vec<(i32, i32, i32, i32)> {
    if count == 0 || container_w <= 0 || container_h <= 0 {
        return Vec::new();
    }

    let cols = (count as f64).sqrt().ceil() as usize;
    let rows = count.div_ceil(cols);

    let max_cell_w = container_w / cols as i32;
    let max_cell_h = container_h / rows as i32;

    let (cell_w, cell_h) = if source_aspect > 0.0 {
        let cell_h_from_w = (max_cell_w as f64 / source_aspect).floor() as i32;
        if cell_h_from_w <= max_cell_h {
            (max_cell_w.max(1), cell_h_from_w.max(1))
        } else {
            let cell_w_from_h = (max_cell_h as f64 * source_aspect).floor() as i32;
            (cell_w_from_h.max(1), max_cell_h.max(1))
        }
    } else {
        (max_cell_w.max(1), max_cell_h.max(1))
    };

    let total_w = cell_w * cols as i32;
    let total_h = cell_h * rows as i32;
    let off_x = (container_w - total_w) / 2;
    let off_y = (container_h - total_h) / 2;

    (0..count)
        .map(|i| {
            let col = i % cols;
            let row = i / cols;
            (
                container_x + off_x + col as i32 * cell_w,
                container_y + off_y + row as i32 * cell_h,
                cell_w,
                cell_h,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pip_overlay_rects_one_full_aspect() {
        // 1 source in 1920×1080 with 16:9 aspect → fills the whole container.
        let rects = compute_pip_overlay_rects(0, 0, 1920, 1080, 1, 16.0 / 9.0);
        assert_eq!(rects, vec![(0, 0, 1920, 1080)]);
    }

    #[test]
    fn test_pip_overlay_rects_two_side_by_side_vertically_centered() {
        // 2 cells side-by-side in 1920×1080 with 16:9 aspect. Each cell width =
        // 960, height = 540 to preserve 16:9. Vertical center: top margin 270.
        let rects = compute_pip_overlay_rects(0, 0, 1920, 1080, 2, 16.0 / 9.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (0, 270, 960, 540));
        assert_eq!(rects[1], (960, 270, 960, 540));
    }

    #[test]
    fn test_pip_overlay_rects_two_in_wide_pvw_rect() {
        // PVW big is ~960×518 (aspect 1.85). Each 16:9 cell = 480×270.
        // Top margin = (518 - 270) / 2 = 124.
        let rects = compute_pip_overlay_rects(10, 10, 960, 518, 2, 16.0 / 9.0);
        assert_eq!(rects[0], (10, 10 + 124, 480, 270));
        assert_eq!(rects[1], (10 + 480, 10 + 124, 480, 270));
    }

    #[test]
    fn test_pip_overlay_rects_four_is_2x2_centered() {
        // 2×2 grid in 1920×1080, 16:9 cells → 960×540 each, fills exactly.
        let rects = compute_pip_overlay_rects(0, 0, 1920, 1080, 4, 16.0 / 9.0);
        assert_eq!(rects.len(), 4);
        assert_eq!(rects[0], (0, 0, 960, 540));
        assert_eq!(rects[1], (960, 0, 960, 540));
        assert_eq!(rects[2], (0, 540, 960, 540));
        assert_eq!(rects[3], (960, 540, 960, 540));
    }

    #[test]
    fn test_pip_overlay_rects_five_uses_3x2_grid() {
        // 5 sources → 3 cols × 2 rows.
        let rects = compute_pip_overlay_rects(0, 0, 1920, 1080, 5, 16.0 / 9.0);
        assert_eq!(rects.len(), 5);
        // cols=3, rows=2 → max_cell_w=640, max_cell_h=540.
        // 16:9 cell from width 640: height = 360. 360 <= 540 → cell = 640×360.
        // total_w = 1920, total_h = 720. off_y = (1080 - 720) / 2 = 180.
        assert_eq!(rects[0], (0, 180, 640, 360));
        assert_eq!(rects[2], (1280, 180, 640, 360));
        assert_eq!(rects[3], (0, 180 + 360, 640, 360));
    }

    #[test]
    fn test_pip_overlay_rects_falls_back_to_uniform_when_aspect_invalid() {
        // source_aspect <= 0 → uniform cells filling the container (no aspect preservation).
        let rects = compute_pip_overlay_rects(0, 0, 600, 400, 2, 0.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (0, 0, 300, 400));
        assert_eq!(rects[1], (300, 0, 300, 400));
    }

    #[test]
    fn test_source_roundtrip_input() {
        let s: Source = "input:3".parse().unwrap();
        assert_eq!(s, Source::Input(3));
        assert_eq!(s.to_string(), "input:3");
    }

    #[test]
    fn test_source_roundtrip_pip() {
        let s: Source = "pip:0".parse().unwrap();
        assert_eq!(s, Source::Pip(0));
        assert_eq!(s.to_string(), "pip:0");
    }

    #[test]
    fn test_source_parse_ignores_case_and_whitespace() {
        assert_eq!("  INPUT : 5  ".parse::<Source>().unwrap(), Source::Input(5));
        assert_eq!("In:2".parse::<Source>().unwrap(), Source::Input(2));
    }

    #[test]
    fn test_source_parse_rejects_garbage() {
        assert!("".parse::<Source>().is_err());
        assert!("foo:1".parse::<Source>().is_err());
        assert!("input".parse::<Source>().is_err());
        assert!("input:".parse::<Source>().is_err());
        assert!("pip:abc".parse::<Source>().is_err());
    }

    #[test]
    fn test_normrect_is_valid() {
        assert!(NormRect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0
        }
        .is_valid());
        assert!(NormRect {
            x: 0.5,
            y: 0.5,
            w: 0.5,
            h: 0.5
        }
        .is_valid());
        assert!(!NormRect {
            x: -0.1,
            y: 0.0,
            w: 0.5,
            h: 0.5
        }
        .is_valid());
        assert!(!NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.5
        }
        .is_valid());
        assert!(!NormRect {
            x: 0.6,
            y: 0.0,
            w: 0.5,
            h: 0.5
        }
        .is_valid()); // x+w > 1
    }

    #[test]
    fn test_normrect_clamped() {
        let r = NormRect {
            x: -0.2,
            y: 1.5,
            w: 2.0,
            h: 0.3,
        }
        .clamped();
        assert_eq!(r.x, 0.0);
        assert_eq!(r.y, 1.0);
        assert_eq!(r.w, 1.0);
        assert_eq!(r.h, 0.0);
    }

    #[test]
    fn test_normrect_to_pixels_basic() {
        let r = NormRect {
            x: 0.5,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        };
        assert_eq!(r.to_pixels(0, 0, 1920, 1080), (960, 270, 960, 540));
    }

    #[test]
    fn test_resolve_pip_overlay_rects_all_explicit() {
        let slots = vec![
            Some(NormRect {
                x: 0.55,
                y: 0.10,
                w: 0.40,
                h: 0.30,
            }),
            Some(NormRect {
                x: 0.05,
                y: 0.60,
                w: 0.40,
                h: 0.30,
            }),
        ];
        let rects = resolve_pip_overlay_rects(0, 0, 1000, 1000, &slots, 16.0 / 9.0);
        assert_eq!(rects[0], (550, 100, 400, 300));
        assert_eq!(rects[1], (50, 600, 400, 300));
    }

    #[test]
    fn test_resolve_pip_overlay_rects_all_none_matches_auto() {
        // All-None should produce the same layout as compute_pip_overlay_rects.
        let auto = compute_pip_overlay_rects(0, 0, 1920, 1080, 2, 16.0 / 9.0);
        let slots = vec![None, None];
        let resolved = resolve_pip_overlay_rects(0, 0, 1920, 1080, &slots, 16.0 / 9.0);
        assert_eq!(resolved, auto);
    }

    #[test]
    fn test_zone_effective_sources_uncapped() {
        let z = Zone {
            rect: None,
            capacity: None,
            border: None,
            sources: vec![1, 2, 3],
        };
        assert_eq!(z.effective_sources(), &[1, 2, 3]);
    }

    #[test]
    fn test_zone_effective_sources_capped_keeps_newest() {
        let z = Zone {
            rect: None,
            capacity: Some(2),
            border: None,
            sources: vec![1, 2, 3, 4],
        };
        assert_eq!(z.effective_sources(), &[3, 4]);
    }

    #[test]
    fn test_resolve_zone_pads_single_zone_full_region() {
        // One zone with no rect, three sources → auto-tile across full container.
        let z = Zone {
            rect: None,
            capacity: None,
            border: None,
            sources: vec![0, 1, 2],
        };
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            &[z],
            16.0 / 9.0,
            &PipTransforms::new(),
            &SourceAspects::new(),
        );
        assert_eq!(layouts.len(), 3);
        // First source covers ~upper-left cell of the 2x2 auto-tile.
        assert_eq!(layouts[0].input, 0);
        assert!(layouts[0].w > 0 && layouts[0].h > 0);
        // Z-order increments by 1 per pad.
        assert_eq!(layouts[0].zorder_offset, 0);
        assert_eq!(layouts[1].zorder_offset, 1);
        assert_eq!(layouts[2].zorder_offset, 2);
    }

    #[test]
    fn test_resolve_zone_pads_two_zones() {
        // Zone A: right half, one source. Zone B: bottom strip, three sources.
        let a = Zone {
            rect: Some(NormRect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
            }),
            capacity: Some(1),
            border: None,
            sources: vec![5],
        };
        let b = Zone {
            rect: Some(NormRect {
                x: 0.0,
                y: 0.75,
                w: 0.5,
                h: 0.25,
            }),
            capacity: Some(3),
            border: None,
            sources: vec![1, 2, 3],
        };
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            &[a, b],
            16.0 / 9.0,
            &PipTransforms::new(),
            &SourceAspects::new(),
        );
        assert_eq!(layouts.len(), 4);
        assert_eq!(layouts[0].input, 5);
        // Zone A: x starts at half the container width (960).
        assert!(layouts[0].x >= 960);
        // Zone B sources live in the bottom strip.
        for l in &layouts[1..] {
            assert!(l.y >= (1080.0 * 0.75) as i32 - 1);
        }
    }

    #[test]
    fn test_resolve_zone_pads_dedupes_across_zones() {
        let a = Zone {
            rect: None,
            capacity: None,
            border: None,
            sources: vec![1, 2],
        };
        let b = Zone {
            rect: None,
            capacity: None,
            border: None,
            sources: vec![2, 3],
        };
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            &[a, b],
            16.0 / 9.0,
            &PipTransforms::new(),
            &SourceAspects::new(),
        );
        // Source 2 should appear once (from zone A); zone B drops it.
        let inputs: Vec<usize> = layouts.iter().map(|l| l.input).collect();
        assert_eq!(inputs, vec![1, 2, 3]);
    }

    #[test]
    fn test_resolve_zone_pads_drops_oldest_when_overcap() {
        // Capacity 2 but 4 sources — should keep only the last 2.
        let z = Zone {
            rect: None,
            capacity: Some(2),
            border: None,
            sources: vec![1, 2, 3, 4],
        };
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            &[z],
            16.0 / 9.0,
            &PipTransforms::new(),
            &SourceAspects::new(),
        );
        let inputs: Vec<usize> = layouts.iter().map(|l| l.input).collect();
        assert_eq!(inputs, vec![3, 4]);
    }

    #[test]
    fn test_resolve_zone_pads_single_cropped_source_fills_rect() {
        // A single 16:9 source whose crop window matches the box aspect
        // (the UI's aspect lock) fills the zone rect exactly — punch-in.
        let z = Zone {
            rect: Some(NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.25, // portrait box: 480×1080, aspect 4:9
                h: 1.0,
            }),
            capacity: None,
            border: None,
            sources: vec![2],
        };
        let mut aspects = SourceAspects::new();
        aspects.insert(2, 16.0 / 9.0);
        // Window ratio for box aspect (4/9) on a 16:9 source: 0.25 of the
        // width, full height → effective aspect = 16/9 × 0.25 = 4/9 = box.
        let mut transforms = PipTransforms::new();
        transforms.insert(
            2,
            SourceCrop {
                left: 0.375,
                top: 0.0,
                right: 0.375,
                bottom: 0.0,
            },
        );
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            std::slice::from_ref(&z),
            16.0 / 9.0,
            &transforms,
            &aspects,
        );
        assert_eq!(layouts.len(), 1);
        assert_eq!(
            (layouts[0].x, layouts[0].y, layouts[0].w, layouts[0].h),
            (0, 0, 480, 1080)
        );

        // Without a transform the same source letterboxes inside the rect.
        let layouts = resolve_zone_pads(
            0,
            0,
            1920,
            1080,
            std::slice::from_ref(&z),
            16.0 / 9.0,
            &PipTransforms::new(),
            &aspects,
        );
        assert_eq!(layouts.len(), 1);
        assert!(
            layouts[0].h < 1080,
            "expected aspect-fitted (letterboxed) rect"
        );
        assert_eq!(layouts[0].w, 480, "16:9 in a portrait box keeps full width");
    }

    #[test]
    fn test_aspect_fit_rect_letterbox_and_pillarbox() {
        // 2.39:1 scope content in a 16:9 box → full width, reduced height.
        let (x, y, w, h) = aspect_fit_rect(0, 0, 1920, 1080, 2.39);
        assert_eq!((x, w), (0, 1920));
        assert_eq!(h, (1920.0 / 2.39_f64).round() as i32);
        assert_eq!(y, (1080 - h) / 2);

        // 16:9 content in a portrait box → full height of the fitted width.
        let (x, y, w, h) = aspect_fit_rect(100, 0, 480, 1080, 16.0 / 9.0);
        assert_eq!(w, 480);
        assert_eq!(h, 270);
        assert_eq!(x, 100);
        assert_eq!(y, (1080 - 270) / 2);

        // Matching aspect → exact box. Unknown aspect → box unchanged.
        assert_eq!(
            aspect_fit_rect(5, 7, 1600, 900, 16.0 / 9.0),
            (5, 7, 1600, 900)
        );
        assert_eq!(aspect_fit_rect(5, 7, 1600, 900, 0.0), (5, 7, 1600, 900));
    }

    #[test]
    fn test_effective_source_aspect() {
        // No crop → raw aspect; unknown stays unknown.
        assert_eq!(effective_source_aspect(2.39, None), 2.39);
        assert_eq!(effective_source_aspect(0.0, None), 0.0);
        // Horizontal-only crop narrows the effective aspect.
        let c = SourceCrop {
            left: 0.25,
            top: 0.0,
            right: 0.25,
            bottom: 0.0,
        };
        let a = effective_source_aspect(16.0 / 9.0, Some(&c));
        assert!((a - (16.0 / 9.0) * 0.5).abs() < 1e-6);
        // Zero crop entry behaves like no crop.
        assert_eq!(
            effective_source_aspect(1.5, Some(&SourceCrop::default())),
            1.5
        );
    }

    #[test]
    fn test_zone_border_rgba_parsing() {
        let b = |c: &str| ZoneBorder {
            color: c.to_string(),
            width: 4.0,
        };
        assert_eq!(b("#CC0000").rgba(), Some((0.8, 0.0, 0.0, 1.0)));
        let (r, g, bl, a) = b("#00ff0080").rgba().unwrap();
        assert_eq!((r, g, bl), (0.0, 1.0, 0.0));
        assert!((a - 128.0 / 255.0).abs() < 1e-9);
        // Case-insensitive + surrounding whitespace tolerated.
        assert!(b(" #aAbBcC ").rgba().is_some());
        // Rejected: missing #, wrong length, non-hex.
        assert_eq!(b("CC0000").rgba(), None);
        assert_eq!(b("#CC00").rgba(), None);
        assert_eq!(b("#GG0000").rgba(), None);
    }

    #[test]
    fn test_zone_border_width_and_visibility() {
        let mk = |color: &str, width: f32| ZoneBorder {
            color: color.to_string(),
            width,
        };
        assert_eq!(mk("#fff000", 1000.0).clamped_width(), MAX_ZONE_BORDER_WIDTH);
        assert_eq!(mk("#fff000", f32::NAN).clamped_width(), 0.0);
        assert!(mk("#CC0000", 4.0).is_visible());
        assert!(!mk("#CC0000", 0.0).is_visible()); // zero width
        assert!(!mk("#CC000000", 4.0).is_visible()); // alpha 0
        assert!(!mk("not-a-color", 4.0).is_visible()); // unparseable
    }

    #[test]
    fn test_zone_border_serde_default() {
        // Older clients omit `border` — deserializes to None.
        let z: Zone = serde_json::from_str(r#"{"sources":[1]}"#).unwrap();
        assert!(z.border.is_none());
        let z: Zone =
            serde_json::from_str(r##"{"sources":[1],"border":{"color":"#CC0000","width":4.0}}"##)
                .unwrap();
        assert!(z.border.unwrap().is_visible());
    }

    #[test]
    fn test_source_crop_default_is_zero() {
        assert!(SourceCrop::default().is_zero());
        assert!(!SourceCrop {
            left: 0.1,
            ..Default::default()
        }
        .is_zero());
    }

    #[test]
    fn test_source_crop_clamped_keeps_minimum_visible() {
        // left + right > 1 → right shrinks so MIN_CROP_VISIBLE remains.
        let c = SourceCrop {
            left: 0.7,
            top: 0.0,
            right: 0.7,
            bottom: 0.0,
        }
        .clamped();
        assert_eq!(c.left, 0.7);
        assert!((c.left + c.right) <= 1.0 - MIN_CROP_VISIBLE + 1e-6);
    }

    #[test]
    fn test_source_crop_clamped_sanitizes_garbage() {
        let c = SourceCrop {
            left: -0.5,
            top: f32::NAN,
            right: 2.0,
            bottom: f32::INFINITY,
        }
        .clamped();
        assert_eq!(c.left, 0.0);
        assert_eq!(c.top, 0.0);
        assert!(c.right <= 1.0 - MIN_CROP_VISIBLE + 1e-6);
        assert!(c.bottom <= 1.0 - MIN_CROP_VISIBLE + 1e-6);
    }

    #[test]
    fn test_source_crop_to_pixels_basic() {
        // Crop 25% from each side of 1920×1080 → 480/480 horizontal, 270/270 vertical.
        let c = SourceCrop {
            left: 0.25,
            top: 0.25,
            right: 0.25,
            bottom: 0.25,
        };
        assert_eq!(c.to_pixels(1920, 1080), (480, 480, 270, 270));
    }

    #[test]
    fn test_source_crop_to_pixels_zero() {
        assert_eq!(SourceCrop::default().to_pixels(1280, 720), (0, 0, 0, 0));
    }

    #[test]
    fn test_source_crop_to_pixels_never_consumes_full_axis() {
        // Extreme crop still leaves ≥1 px visible on each axis.
        let c = SourceCrop {
            left: 1.0,
            top: 1.0,
            right: 1.0,
            bottom: 1.0,
        };
        let (l, r, t, b) = c.to_pixels(100, 100);
        assert!(
            l + r < 100,
            "horizontal crop {} + {} consumed full width",
            l,
            r
        );
        assert!(
            t + b < 100,
            "vertical crop {} + {} consumed full height",
            t,
            b
        );
    }

    #[test]
    fn test_source_crop_serde_defaults() {
        // Missing fields deserialize to 0 (back-compat with older clients).
        let c: SourceCrop = serde_json::from_str(r#"{"left":0.1}"#).unwrap();
        assert_eq!(c.left, 0.1);
        assert_eq!(c.right, 0.0);
        assert!(serde_json::from_str::<SourceCrop>("{}").unwrap().is_zero());
    }

    #[test]
    fn test_resolve_pip_overlay_rects_mixed() {
        // Slot 0 explicit, slot 1 auto. Slot 1 falls back to the auto-tile cell
        // that would have been computed for 2-slot auto-tile.
        let auto = compute_pip_overlay_rects(0, 0, 1920, 1080, 2, 16.0 / 9.0);
        let slots = vec![
            Some(NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 0.5,
            }),
            None,
        ];
        let resolved = resolve_pip_overlay_rects(0, 0, 1920, 1080, &slots, 16.0 / 9.0);
        assert_eq!(resolved[0], (0, 0, 960, 540));
        assert_eq!(resolved[1], auto[1]);
    }
}
