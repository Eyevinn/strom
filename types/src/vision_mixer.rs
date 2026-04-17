//! Vision mixer constants and defaults.

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

/// Default compositor latency in milliseconds.
pub const DEFAULT_LATENCY_MS: u64 = 20;

/// Default minimum upstream latency in milliseconds.
pub const DEFAULT_MIN_UPSTREAM_LATENCY_MS: u64 = 20;

/// Default PGM output framerate (fps as "numerator/denominator").
pub const DEFAULT_PGM_FRAMERATE: &str = "30/1";

/// Default multiview output framerate.
pub const DEFAULT_MULTIVIEW_FRAMERATE: &str = "30/1";

/// Whether to download GPU memory to system memory on output (GPU path only).
pub const DEFAULT_GL_DOWNLOAD: bool = true;

// --- Z-order constants for compositor pads ---

/// Z-order for thumbnail pads on the multiview compositor.
pub const MV_THUMBNAIL_ZORDER: u32 = 1;

/// Z-order for PGM/PVW big display pads on the multiview compositor.
pub const MV_BIG_DISPLAY_ZORDER: u32 = 10;

/// Z-order for the background source on the distribution compositor.
pub const DIST_BACKGROUND_ZORDER: u32 = 0;

/// Z-order for PGM group sources on the distribution compositor.
pub const DIST_PGM_ZORDER: u32 = 1;

/// Base z-order for DSK pads on the distribution compositor (+ dsk index).
pub const DIST_DSK_BASE_ZORDER: u32 = 100;

/// Sentinel value for "no background source".
pub const NO_BACKGROUND: u64 = u64::MAX;

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
    fn test_group_rects_with_offset() {
        let rects = compute_group_rects(100, 50, 400, 300, 2);
        assert_eq!(rects, vec![(100, 50, 200, 300), (300, 50, 200, 300)]);
    }
}
