//! Multiview layout calculations.
//!
//! Top half: PVW (left) + PGM (right) big displays. Bottom half: thumbnail grid
//! with one slot per input + one slot per PiP tile.
//!
//! The grid scales to keep thumbnail aspect ratio in a reasonable 4:3..16:9
//! range as the number of slots grows:
//!
//! ```text
//! 1-5    slots:  5×1
//! 6-10   slots:  5×2
//! 11-12  slots:  6×2
//! 13-14  slots:  7×2
//! 15     slots:  5×3
//! 16-18  slots:  6×3
//! 19-21  slots:  7×3
//! 22-25  slots:  5×4 / 6×4 / 7×4
//! ...
//! ```
//!
//! Per tier we cycle cols 5 → 6 → 7 before bumping rows, so the thumbnail
//! aspect cycles through familiar broadcast shapes.

/// A rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Return integer values for compositor pad properties.
    pub fn as_ints(&self) -> (i32, i32, i32, i32) {
        (self.x as i32, self.y as i32, self.w as i32, self.h as i32)
    }
}

/// A 2D position for text placement.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Pre-computed layout for the multiview overlay.
///
/// All positions are in pixel coordinates relative to the multiview canvas.
#[derive(Debug, Clone)]
pub struct OverlayLayout {
    /// Canvas width in pixels.
    pub canvas_width: f64,
    /// Canvas height in pixels.
    pub canvas_height: f64,
    /// Number of active inputs.
    pub num_inputs: usize,
    /// Number of PiP tiles rendered virtually in the grid.
    pub num_pips: usize,

    /// PVW large display area (top-left).
    pub pvw_rect: Rect,
    /// PGM large display area (top-right).
    pub pgm_rect: Rect,

    /// Thumbnail video rectangles for each input (video area only, used for compositor pads).
    pub thumbnail_rects: Vec<Rect>,
    /// Full thumbnail slot rectangles (video + label area, used for borders).
    pub thumbnail_slot_rects: Vec<Rect>,
    /// PiP tile video rectangles. Placed in the grid after the input thumbnails
    /// (slot index `num_inputs + pip_idx`).
    pub pip_tile_rects: Vec<Rect>,
    /// Full PiP tile slot rectangles (video + label area, used for borders).
    pub pip_tile_slot_rects: Vec<Rect>,

    /// Label text positions (below each thumbnail).
    pub label_positions: Vec<Point>,
    /// Label text positions for each PiP tile (below the tile video area).
    pub pip_label_positions: Vec<Point>,
    /// PVW label position.
    pub pvw_label_pos: Point,
    /// PGM label position.
    pub pgm_label_pos: Point,

    /// Font size for input labels.
    pub label_font_size: f64,
    /// Font size for PVW/PGM labels.
    pub header_font_size: f64,
    /// PVW border width.
    pub pvw_border_width: f64,
    /// PGM border width.
    pub pgm_border_width: f64,
    /// Thumbnail border width.
    pub thumb_border_width: f64,
    /// Scale factor relative to 720p reference resolution (canvas_height / 720).
    pub scale: f64,
}

/// Spacing between panels as a fraction of canvas dimension.
const GAP_FRACTION: f64 = 0.005;

/// Height fraction for the top PVW/PGM row.
const TOP_ROW_HEIGHT_FRACTION: f64 = 0.48;

/// Default height per thumbnail row when the chosen grid fits naturally below
/// the top PVW/PGM band. Higher slot counts shrink to fit the available height.
const DEFAULT_THUMB_ROW_HEIGHT_FRACTION: f64 = 0.235;

/// Pick (cols, rows) for `total_slots` thumbnails. Cycles cols 5 → 6 → 7 per
/// row tier so the thumbnail aspect stays in the broadcast-friendly 4:3..16:9
/// range as the grid grows.
fn pick_grid(total_slots: usize) -> (usize, usize) {
    if total_slots == 0 {
        return (1, 1);
    }
    // Small counts: a single row is fine.
    if total_slots <= 5 {
        return (5, 1);
    }
    // 2-row tier (the classic broadcast layout).
    if total_slots <= 10 {
        return (5, 2);
    }
    if total_slots <= 12 {
        return (6, 2);
    }
    if total_slots <= 14 {
        return (7, 2);
    }
    // 3+ row tiers: cycle cols 5 → 6 → 7 per tier.
    let mut rows = 3usize;
    loop {
        for cols in [5usize, 6, 7] {
            if cols * rows >= total_slots {
                return (cols, rows);
            }
        }
        rows += 1;
    }
}

/// Compute the multiview layout for a given canvas size, input count, and PiP-tile count.
///
/// PiP tiles share the thumbnail grid: they are placed at slot indices
/// `num_inputs..num_inputs + num_pips`. Grid dimensions come from [`pick_grid`]
/// — cols and rows both scale with the total slot count to keep thumbnails
/// reasonably square.
pub fn compute_layout(
    canvas_width: u32,
    canvas_height: u32,
    num_inputs: usize,
    num_pips: usize,
) -> OverlayLayout {
    let cw = canvas_width as f64;
    let ch = canvas_height as f64;
    let scale = ch / 720.0;
    let gap = (cw * GAP_FRACTION).round();

    // Top row: PVW (left half) and PGM (right half)
    let top_h = (ch * TOP_ROW_HEIGHT_FRACTION).round();
    let half_w = ((cw - gap * 3.0) / 2.0).round();

    let pvw_rect = Rect::new(gap, gap, half_w, top_h);
    let pgm_rect = Rect::new(gap * 2.0 + half_w, gap, half_w, top_h);

    // Thumbnail grid below the top row.
    let thumb_y_start = gap * 2.0 + top_h;
    let total_slots = num_inputs + num_pips;
    let (cols, rows) = pick_grid(total_slots);
    let available_h = (ch - thumb_y_start - gap * (rows as f64 + 1.0)).max(0.0);
    let default_thumb_h = (ch * DEFAULT_THUMB_ROW_HEIGHT_FRACTION).round();
    let max_thumb_h = (available_h / rows as f64).floor();
    let thumb_h = max_thumb_h.min(default_thumb_h).max(1.0);
    let thumb_w = ((cw - gap * (cols as f64 + 1.0)) / cols as f64).round();

    let mut thumbnail_rects = Vec::with_capacity(num_inputs);
    let mut thumbnail_slot_rects = Vec::with_capacity(num_inputs);
    let mut label_positions = Vec::with_capacity(num_inputs);
    let mut pip_tile_rects = Vec::with_capacity(num_pips);
    let mut pip_tile_slot_rects = Vec::with_capacity(num_pips);
    let mut pip_label_positions = Vec::with_capacity(num_pips);

    let label_font_size = thumb_h * 0.10;
    // Reserve space below the video for the label
    let label_area_h = label_font_size * 1.6;
    let video_h = (thumb_h - label_area_h).max(1.0);

    // Same slot geometry for inputs and PiPs — they share the grid.
    let slot_rect = |i: usize| -> (f64, f64) {
        let row = i / cols;
        let col = i % cols;
        let x = gap + col as f64 * (thumb_w + gap);
        let y = thumb_y_start + row as f64 * (thumb_h + gap);
        (x, y)
    };

    for i in 0..num_inputs {
        let (x, y) = slot_rect(i);
        thumbnail_rects.push(Rect::new(x, y, thumb_w, video_h));
        thumbnail_slot_rects.push(Rect::new(x, y, thumb_w, thumb_h));
        label_positions.push(Point {
            x: x + thumb_w / 2.0,
            y: y + video_h + label_area_h / 2.0 + label_font_size * 0.35,
        });
    }

    for i in 0..num_pips {
        let (x, y) = slot_rect(num_inputs + i);
        pip_tile_rects.push(Rect::new(x, y, thumb_w, video_h));
        pip_tile_slot_rects.push(Rect::new(x, y, thumb_w, thumb_h));
        pip_label_positions.push(Point {
            x: x + thumb_w / 2.0,
            y: y + video_h + label_area_h / 2.0 + label_font_size * 0.35,
        });
    }

    let header_font_size = top_h * 0.06;

    OverlayLayout {
        canvas_width: cw,
        canvas_height: ch,
        num_inputs,
        num_pips,
        pvw_rect,
        pgm_rect,
        thumbnail_rects,
        thumbnail_slot_rects,
        pip_tile_rects,
        pip_tile_slot_rects,
        label_positions,
        pip_label_positions,
        pvw_label_pos: Point {
            x: pvw_rect.x + pvw_rect.w / 2.0,
            y: pvw_rect.y + pvw_rect.h - header_font_size * 0.6,
        },
        pgm_label_pos: Point {
            x: pgm_rect.x + pgm_rect.w / 2.0,
            y: pgm_rect.y + pgm_rect.h - header_font_size * 0.6,
        },
        label_font_size,
        header_font_size,
        pvw_border_width: strom_types::vision_mixer::PVW_BORDER_WIDTH * scale,
        pgm_border_width: strom_types::vision_mixer::PGM_BORDER_WIDTH * scale,
        thumb_border_width: strom_types::vision_mixer::THUMBNAIL_BORDER_WIDTH * scale,
        scale,
    }
}

/// Compute compositor pad position for a thumbnail slot.
/// Returns (xpos, ypos, width, height) as integers.
pub fn thumbnail_pad_position(layout: &OverlayLayout, index: usize) -> (i32, i32, i32, i32) {
    if index < layout.thumbnail_rects.len() {
        layout.thumbnail_rects[index].as_ints()
    } else {
        // Off-screen for unused slots
        (0, 0, 1, 1)
    }
}

/// Compute compositor pad position for a PVW big display slot.
pub fn pvw_pad_position(layout: &OverlayLayout) -> (i32, i32, i32, i32) {
    layout.pvw_rect.as_ints()
}

/// Compute compositor pad position for a PGM big display slot.
pub fn pgm_pad_position(layout: &OverlayLayout) -> (i32, i32, i32, i32) {
    layout.pgm_rect.as_ints()
}

/// Compositor pad position for the PiP background pad — fills the whole PiP tile.
/// Returns a `(0, 0, 1, 1)` sentinel rect if the PiP index is out of range.
pub fn pip_bg_pad_position(layout: &OverlayLayout, pip_idx: usize) -> (i32, i32, i32, i32) {
    layout
        .pip_tile_rects
        .get(pip_idx)
        .map(Rect::as_ints)
        .unwrap_or((0, 0, 1, 1))
}

/// Compute auto-tiled overlay sub-rectangles inside a PiP tile, preserving
/// `source_aspect` so the rendered source fills each cell without transparent
/// letterbox bands. See [`strom_types::vision_mixer::compute_pip_overlay_rects`].
pub fn pip_overlay_pad_positions(
    layout: &OverlayLayout,
    pip_idx: usize,
    count: usize,
    source_aspect: f64,
) -> Vec<(i32, i32, i32, i32)> {
    let Some(tile) = layout.pip_tile_rects.get(pip_idx) else {
        return Vec::new();
    };
    strom_types::vision_mixer::compute_pip_overlay_rects(
        tile.x as i32,
        tile.y as i32,
        tile.w as i32,
        tile.h as i32,
        count,
        source_aspect,
    )
}

#[cfg(test)]
mod tests {
    use super::pick_grid;

    #[test]
    fn pick_grid_small_uses_one_row() {
        assert_eq!(pick_grid(1), (5, 1));
        assert_eq!(pick_grid(4), (5, 1));
        assert_eq!(pick_grid(5), (5, 1));
    }

    #[test]
    fn pick_grid_two_row_tier() {
        assert_eq!(pick_grid(6), (5, 2));
        assert_eq!(pick_grid(10), (5, 2));
        assert_eq!(pick_grid(11), (6, 2));
        assert_eq!(pick_grid(12), (6, 2));
        assert_eq!(pick_grid(13), (7, 2));
        assert_eq!(pick_grid(14), (7, 2));
    }

    #[test]
    fn pick_grid_three_row_tier_cycles_cols() {
        assert_eq!(pick_grid(15), (5, 3));
        assert_eq!(pick_grid(16), (6, 3));
        assert_eq!(pick_grid(18), (6, 3));
        assert_eq!(pick_grid(19), (7, 3));
        assert_eq!(pick_grid(21), (7, 3));
    }

    #[test]
    fn pick_grid_grows_to_more_rows() {
        // 20 still fits in the 3-row tier (7×3=21) — no need to bump rows.
        assert_eq!(pick_grid(20), (7, 3));
        // 22 exceeds 21 — jumps to the 4-row tier. 5×4=20 too small, so cycle
        // cols up to 6×4=24.
        assert_eq!(pick_grid(22), (6, 4));
        assert_eq!(pick_grid(28), (7, 4));
        // 29 doesn't fit in 5×5=25 — skips to 6×5=30 in the 5-row tier.
        assert_eq!(pick_grid(29), (6, 5));
        assert_eq!(pick_grid(35), (7, 5));
    }
}
