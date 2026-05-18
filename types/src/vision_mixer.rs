//! Vision mixer constants and defaults.

use std::fmt;
use std::str::FromStr;

/// A source that can be assigned to PGM or PVW.
///
/// Encoded as `"input:N"` or `"pip:N"` in string properties (case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Maximum number of video inputs (2-5-5 multiview grid).
pub const MAX_NUM_INPUTS: usize = 10;

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

/// Number of thumbnails per row in the multiview grid.
pub const THUMBNAILS_PER_ROW: usize = 5;

/// Maximum number of DSK (Downstream Keyer) inputs.
pub const MAX_DSK_INPUTS: usize = 4;

/// Default number of DSK inputs (0 = no DSK).
pub const DEFAULT_DSK_INPUTS: usize = 0;

/// Maximum number of PiP (Picture-in-Picture) tiles rendered virtually in the multiview.
/// Each PiP consumes one slot in the multiview thumbnail grid alongside the inputs.
pub const MAX_NUM_PIPS: usize = 4;

/// Default number of PiP tiles (0 = no PiP).
pub const DEFAULT_NUM_PIPS: usize = 0;

/// Maximum number of overlay sources placed on top of the PiP background.
/// Practically `MAX_NUM_INPUTS - 1` since the bg consumes one input slot.
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

// --- Z-order constants for compositor pads ---

/// Z-order for thumbnail pads on the multiview compositor.
pub const MV_THUMBNAIL_ZORDER: u32 = 1;

/// Z-order for PGM/PVW big display pads on the multiview compositor.
pub const MV_BIG_DISPLAY_ZORDER: u32 = 10;

/// Z-order for PGM group sources on the distribution compositor.
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
/// pads underneath. Must stay below [`DIST_DSK_BASE_ZORDER`] (100) and below the
/// cairo overlay z-order (200) so DSK + labels still render on top.
pub const TRANSITION_FOREGROUND_ZORDER: u32 = 50;

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

// --- Source group constants and helpers ---

/// Maximum number of sources in a multi-source group (split-screen layout).
pub const MAX_GROUP_SIZE: usize = 4;

/// Pack an ordered list of up to 4 source indices into a u64 for atomic storage.
///
/// Layout: bits 0-3 = count (0-4), bits 4-7 = idx[0], bits 8-11 = idx[1],
/// bits 12-15 = idx[2], bits 16-19 = idx[3]. Each index supports values 0-15.
pub fn pack_source_group(indices: &[usize]) -> u64 {
    let count = indices.len().min(MAX_GROUP_SIZE);
    let mut val = count as u64;
    for (i, &idx) in indices.iter().take(MAX_GROUP_SIZE).enumerate() {
        val |= ((idx as u64) & 0xF) << (4 + i * 4);
    }
    val
}

/// Unpack a packed source group into a Vec of source indices.
pub fn unpack_source_group(val: u64) -> Vec<usize> {
    let count = (val & 0xF) as usize;
    (0..count.min(MAX_GROUP_SIZE))
        .map(|i| ((val >> (4 + i * 4)) & 0xF) as usize)
        .collect()
}

/// Pack a single source index as a group of 1.
pub fn pack_single_source(idx: usize) -> u64 {
    pack_source_group(&[idx])
}

/// Get the first source index from a packed group (or 0 if empty).
pub fn group_first(val: u64) -> usize {
    let count = (val & 0xF) as usize;
    if count == 0 {
        0
    } else {
        ((val >> 4) & 0xF) as usize
    }
}

/// Compute sub-rectangles for PiP overlays within a container, preserving the
/// source aspect ratio.
///
/// Layout strategy:
///   - Cells are arranged in a `cols × rows` grid where
///     `cols = ceil(sqrt(N))`, `rows = ceil(N / cols)`. For 1..=4 the shape
///     matches the existing groups layout (1, 2-side, 2-top+1-bot, 2×2).
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
            (max_cell_w, cell_h_from_w.max(1))
        } else {
            let cell_w_from_h = (max_cell_h as f64 * source_aspect).floor() as i32;
            (cell_w_from_h.max(1), max_cell_h)
        }
    } else {
        (max_cell_w, max_cell_h)
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

/// Compute sub-rectangles for N sources within a container rectangle.
///
/// Returns (x, y, w, h) tuples for each source position:
/// - 1 source: fullscreen
/// - 2 sources: side-by-side
/// - 3 sources: 2 top + 1 bottom-left
/// - 4 sources: 2x2 grid
pub fn compute_group_rects(
    container_x: i32,
    container_y: i32,
    container_w: i32,
    container_h: i32,
    count: usize,
) -> Vec<(i32, i32, i32, i32)> {
    match count {
        0 => vec![],
        1 => vec![(container_x, container_y, container_w, container_h)],
        2 => {
            let half_w = container_w / 2;
            vec![
                (container_x, container_y, half_w, container_h),
                (
                    container_x + half_w,
                    container_y,
                    container_w - half_w,
                    container_h,
                ),
            ]
        }
        3 => {
            let half_w = container_w / 2;
            let half_h = container_h / 2;
            vec![
                (container_x, container_y, half_w, half_h),
                (
                    container_x + half_w,
                    container_y,
                    container_w - half_w,
                    half_h,
                ),
                (
                    container_x,
                    container_y + half_h,
                    half_w,
                    container_h - half_h,
                ),
            ]
        }
        _ => {
            // 4 (or more, clamped to 4)
            let half_w = container_w / 2;
            let half_h = container_h / 2;
            vec![
                (container_x, container_y, half_w, half_h),
                (
                    container_x + half_w,
                    container_y,
                    container_w - half_w,
                    half_h,
                ),
                (
                    container_x,
                    container_y + half_h,
                    half_w,
                    container_h - half_h,
                ),
                (
                    container_x + half_w,
                    container_y + half_h,
                    container_w - half_w,
                    container_h - half_h,
                ),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_unpack_single() {
        let packed = pack_single_source(3);
        assert_eq!(unpack_source_group(packed), vec![3]);
        assert_eq!(group_first(packed), 3);
    }

    #[test]
    fn test_pack_unpack_group() {
        let packed = pack_source_group(&[1, 3, 5]);
        let unpacked = unpack_source_group(packed);
        assert_eq!(unpacked, vec![1, 3, 5]);
        assert_eq!(group_first(packed), 1);
    }

    #[test]
    fn test_pack_unpack_max_group() {
        let packed = pack_source_group(&[0, 2, 4, 9]);
        let unpacked = unpack_source_group(packed);
        assert_eq!(unpacked, vec![0, 2, 4, 9]);
    }

    #[test]
    fn test_pack_clamps_to_max() {
        let packed = pack_source_group(&[0, 1, 2, 3, 4, 5]);
        let unpacked = unpack_source_group(packed);
        assert_eq!(unpacked, vec![0, 1, 2, 3]); // clamped to 4
    }

    #[test]
    fn test_empty_group() {
        let packed = pack_source_group(&[]);
        assert_eq!(unpack_source_group(packed), Vec::<usize>::new());
        assert_eq!(group_first(packed), 0);
    }

    #[test]
    fn test_group_rects_single() {
        let rects = compute_group_rects(0, 0, 1920, 1080, 1);
        assert_eq!(rects, vec![(0, 0, 1920, 1080)]);
    }

    #[test]
    fn test_group_rects_two() {
        let rects = compute_group_rects(0, 0, 1920, 1080, 2);
        assert_eq!(rects, vec![(0, 0, 960, 1080), (960, 0, 960, 1080)]);
    }

    #[test]
    fn test_group_rects_four() {
        let rects = compute_group_rects(0, 0, 1920, 1080, 4);
        assert_eq!(
            rects,
            vec![
                (0, 0, 960, 540),
                (960, 0, 960, 540),
                (0, 540, 960, 540),
                (960, 540, 960, 540),
            ]
        );
    }

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
        // source_aspect <= 0 → uniform cells filling the container (legacy).
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
    fn test_group_rects_with_offset() {
        let rects = compute_group_rects(100, 50, 400, 300, 2);
        assert_eq!(rects, vec![(100, 50, 200, 300), (300, 50, 200, 300)]);
    }
}
