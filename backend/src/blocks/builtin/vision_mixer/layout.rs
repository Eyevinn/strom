//! Multiview layout calculations.
//!
//! Top half: PVW (left) + PGM (right) big displays. Bottom half: thumbnail grid
//! with one slot per input + one slot per PiP tile.
//!
//! Both the grid choice (`pick_grid`) and the final cell sizing are driven by
//! the *source aspect* (typically `pgm_w / pgm_h`, i.e. 16:9). `pick_grid`
//! enumerates (cols, rows) candidates that fit the slot count and picks the
//! one whose natural cell aspect lands closest to the source. Then each cell
//! is snapped to source aspect within its allocated column/row box — so the
//! `keep-aspect-ratio` compositor pads fill exactly without transparent
//! letterbox bands. PGM/PVW big rects get the same snap for consistency.

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

/// Pick `(cols, rows)` for `total_slots` thumbnails inside a grid area of
/// `(area_w, area_h)`. Optimises:
/// 1. Cell aspect close to `source_aspect` (so the source fills cells cleanly).
/// 2. Few empty cells (so the grid doesn't leave half-empty dangling rows).
///
/// Both factors are folded into one cost in log-aspect space; the penalty
/// per empty cell is tuned so e.g. 4 slots prefers `(4, 1)` over `(3, 2)`
/// even though `(3, 2)` has slightly better cell aspect.
fn pick_grid(total_slots: usize, area_w: f64, area_h: f64, source_aspect: f64) -> (usize, usize) {
    if total_slots == 0 {
        return (1, 1);
    }
    if area_w <= 0.0 || area_h <= 0.0 || source_aspect <= 0.0 {
        return (total_slots.max(1), 1);
    }

    // Cap search at a sensible upper bound — beyond 16 cols cells become
    // unreadable thumbnails anyway, and we always have a fallback (cols = N,
    // rows = 1) inside this range as long as total_slots ≤ 16.
    let max_cols = total_slots.clamp(1, 16);

    // Empty-cell penalty, applied per *fractional* empty cell so larger
    // total_slots tolerate the same absolute number of holes better. Tuned
    // so 4 slots: (4,1) wins; 10: (5,2) wins; 14: (5,3) over (7,2).
    const EMPTY_CELL_PENALTY: f64 = 1.0;

    let mut best: Option<(usize, usize, f64)> = None;
    for cols in 1..=max_cols {
        let rows = total_slots.div_ceil(cols);
        let cell_w = area_w / cols as f64;
        let cell_h = area_h / rows as f64;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            continue;
        }
        let cell_aspect = cell_w / cell_h;
        // Compare aspects in log space — keeps the metric symmetric around
        // the target (a 2× too-wide cell is as bad as a 2× too-tall cell).
        let aspect_err = (cell_aspect / source_aspect).ln().abs();
        let empty = (cols * rows).saturating_sub(total_slots);
        let empty_cost = EMPTY_CELL_PENALTY * empty as f64 / total_slots as f64;
        let cost = aspect_err + empty_cost;
        match best {
            None => best = Some((cols, rows, cost)),
            Some((_, br, bc)) => {
                if cost < bc - 1e-9 || (cost < bc + 1e-9 && rows < br) {
                    best = Some((cols, rows, cost));
                }
            }
        }
    }
    best.map(|(c, r, _)| (c, r))
        .unwrap_or((total_slots.max(1), 1))
}

/// Fit the largest `source_aspect`-shaped rect inside a `(box_w, box_h)` box.
/// Returns `(rect_w, rect_h)` with `rect_w / rect_h == source_aspect`.
fn fit_to_aspect(box_w: f64, box_h: f64, source_aspect: f64) -> (f64, f64) {
    if box_w <= 0.0 || box_h <= 0.0 || source_aspect <= 0.0 {
        return (box_w.max(1.0), box_h.max(1.0));
    }
    let cand_w_from_h = box_h * source_aspect;
    if cand_w_from_h <= box_w {
        (cand_w_from_h.floor().max(1.0), box_h.floor().max(1.0))
    } else {
        let h = (box_w / source_aspect).floor().max(1.0);
        (box_w.floor().max(1.0), h)
    }
}

/// Compute the multiview layout for a given canvas size, slot counts, and
/// source aspect. `source_aspect` is normally `pgm_w / pgm_h` (e.g. 16/9);
/// every rect (PGM/PVW big, input thumbnails, PiP tiles) is sized to match
/// it exactly, so `keep-aspect-ratio` compositor pads fill without
/// letterbox bands.
///
/// PiP tiles share the thumbnail grid: slot indices `num_inputs..num_inputs +
/// num_pips`. Grid dimensions come from [`pick_grid`].
pub fn compute_layout(
    canvas_width: u32,
    canvas_height: u32,
    num_inputs: usize,
    num_pips: usize,
    source_aspect: f64,
) -> OverlayLayout {
    let cw = canvas_width as f64;
    let ch = canvas_height as f64;
    let scale = ch / 720.0;
    let gap = (cw * GAP_FRACTION).round();
    let source_aspect = if source_aspect > 0.0 {
        source_aspect
    } else {
        16.0 / 9.0
    };

    // --- Top row: PVW (left half) and PGM (right half) -----------------
    // Allocate two equal half-canvas boxes, then snap each to source_aspect
    // inside its box and center horizontally so the pair stays symmetric.
    let top_h_box = (ch * TOP_ROW_HEIGHT_FRACTION).round();
    let half_w_box = ((cw - gap * 3.0) / 2.0).round();
    let (big_w, big_h) = fit_to_aspect(half_w_box, top_h_box, source_aspect);
    let big_pad_left = ((half_w_box - big_w) / 2.0).max(0.0);
    let big_pad_top = ((top_h_box - big_h) / 2.0).max(0.0);
    let pvw_x = gap + big_pad_left;
    let pgm_x = gap * 2.0 + half_w_box + big_pad_left;
    let big_y = gap + big_pad_top;
    let pvw_rect = Rect::new(pvw_x, big_y, big_w, big_h);
    let pgm_rect = Rect::new(pgm_x, big_y, big_w, big_h);

    // --- Thumbnail grid below the top row ------------------------------
    // The grid area starts under the top row and extends to the canvas
    // bottom. Grid choice maximises cell aspect match against source.
    let thumb_y_start = gap + top_h_box + gap;
    let grid_area_w = (cw - gap * 2.0).max(0.0);
    let grid_area_h = (ch - thumb_y_start - gap).max(0.0);
    let total_slots = num_inputs + num_pips;
    let (cols, rows) = pick_grid(total_slots, grid_area_w, grid_area_h, source_aspect);

    // Column/row box that holds one cell incl. its label area. Clamped by
    // a default fraction so a tiny slot count doesn't blow up cells.
    let column_box_w = ((grid_area_w - gap * (cols as f64 - 1.0)) / cols as f64).max(1.0);
    let available_h = (grid_area_h - gap * (rows as f64 - 1.0)).max(0.0);
    let default_thumb_h = (ch * DEFAULT_THUMB_ROW_HEIGHT_FRACTION).round();
    let row_box_h = (available_h / rows as f64).min(default_thumb_h).max(1.0);

    // Inside the column×row box, reserve a label band at the bottom, then
    // pick the largest source_aspect rect that fits in what remains.
    let label_font_size = (row_box_h * 0.10).max(1.0);
    let label_area_h = label_font_size * 1.6;
    let video_box_h = (row_box_h - label_area_h).max(1.0);
    let (video_w, video_h) = fit_to_aspect(column_box_w, video_box_h, source_aspect);
    let slot_w = video_w; // tight slot around the aspect-correct video
    let slot_h = video_h + label_area_h;

    // Center the row group horizontally within the grid area when the
    // aspect snap left slack.
    let row_total_w = slot_w * cols as f64 + gap * (cols as f64 - 1.0);
    let row_x_start = gap + ((grid_area_w - row_total_w) / 2.0).max(0.0);

    let mut thumbnail_rects = Vec::with_capacity(num_inputs);
    let mut thumbnail_slot_rects = Vec::with_capacity(num_inputs);
    let mut label_positions = Vec::with_capacity(num_inputs);
    let mut pip_tile_rects = Vec::with_capacity(num_pips);
    let mut pip_tile_slot_rects = Vec::with_capacity(num_pips);
    let mut pip_label_positions = Vec::with_capacity(num_pips);

    let slot_xy = |i: usize| -> (f64, f64) {
        let row = i / cols;
        let col = i % cols;
        let x = row_x_start + col as f64 * (slot_w + gap);
        let y = thumb_y_start + row as f64 * (slot_h + gap);
        (x, y)
    };

    for i in 0..num_inputs {
        let (x, y) = slot_xy(i);
        thumbnail_rects.push(Rect::new(x, y, video_w, video_h));
        thumbnail_slot_rects.push(Rect::new(x, y, slot_w, slot_h));
        label_positions.push(Point {
            x: x + slot_w / 2.0,
            y: y + video_h + label_area_h / 2.0 + label_font_size * 0.35,
        });
    }

    for i in 0..num_pips {
        let (x, y) = slot_xy(num_inputs + i);
        pip_tile_rects.push(Rect::new(x, y, video_w, video_h));
        pip_tile_slot_rects.push(Rect::new(x, y, slot_w, slot_h));
        pip_label_positions.push(Point {
            x: x + slot_w / 2.0,
            y: y + video_h + label_area_h / 2.0 + label_font_size * 0.35,
        });
    }

    let header_font_size = big_h * 0.06;

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
    use super::{fit_to_aspect, pick_grid};

    /// Typical 1920×1080 grid area: full width, ~half the canvas height for
    /// the thumbnails. Matches what `compute_layout` derives at 1920×1080.
    fn grid_area_1080p() -> (f64, f64) {
        (1900.0, 520.0)
    }

    fn cell_aspect(cols: usize, rows: usize, area_w: f64, area_h: f64) -> f64 {
        (area_w / cols as f64) / (area_h / rows as f64)
    }

    #[test]
    fn pick_grid_targets_source_aspect_at_low_counts() {
        let (w, h) = grid_area_1080p();
        // 4 slots at 16:9: tight 4×1 row (cell aspect ≈ 0.91) wins over the
        // wider 5×1 (aspect 0.73) — closer to 1.78 in log-space.
        let (c, r) = pick_grid(4, w, h, 16.0 / 9.0);
        assert_eq!((c, r), (4, 1));
        // Cell aspect is now within a factor of 2 of source on the *narrow*
        // side — still letterboxed top/bottom but better than 5×1.
        assert!(cell_aspect(c, r, w, h) < 16.0 / 9.0);
    }

    #[test]
    fn pick_grid_prefers_two_rows_for_16_9() {
        let (w, h) = grid_area_1080p();
        // 8 slots: (4,2) cell aspect ≈ 1.78 and zero empty cells → winner.
        assert_eq!(pick_grid(8, w, h, 16.0 / 9.0), (4, 2));
        // 14 slots: (5,3) wins — 15 cells (1 empty) at aspect ≈ 2.15 beats
        // (7,2) which has 0 empties but aspect ≈ 1.04, and (6,3) which has
        // closer aspect but 4 empty cells.
        assert_eq!(pick_grid(14, w, h, 16.0 / 9.0), (5, 3));
    }

    #[test]
    fn pick_grid_handles_zero_and_degenerate_inputs() {
        assert_eq!(pick_grid(0, 1920.0, 520.0, 16.0 / 9.0), (1, 1));
        // Non-positive area falls back to a single row of slots.
        assert_eq!(pick_grid(5, 0.0, 520.0, 16.0 / 9.0), (5, 1));
        assert_eq!(pick_grid(5, 1920.0, 0.0, 16.0 / 9.0), (5, 1));
        assert_eq!(pick_grid(5, 1920.0, 520.0, 0.0), (5, 1));
    }

    #[test]
    fn pick_grid_returns_packed_grid_for_small_slot_counts() {
        let (w, h) = grid_area_1080p();
        // 4 slots → (4,1), not (3,2): the empty-cell penalty outweighs the
        // slightly better cell aspect of the 3×2 alternative.
        assert_eq!(pick_grid(4, w, h, 16.0 / 9.0), (4, 1));
        // 6 slots → (3,2) is packed and cell aspect ≈ 2.38; (6,1) is wider
        // strip; (3,2) wins.
        assert_eq!(pick_grid(6, w, h, 16.0 / 9.0), (3, 2));
    }

    #[test]
    fn fit_to_aspect_picks_largest_inner_rect() {
        // 16:9 source in a 4:3-ish box: width-limited, height shrinks.
        let (w, h) = fit_to_aspect(320.0, 320.0, 16.0 / 9.0);
        assert!((w - 320.0).abs() < 1.0);
        assert!((h - 180.0).abs() < 1.0);

        // 16:9 source in a wide box: height-limited, width shrinks.
        let (w, h) = fit_to_aspect(1000.0, 100.0, 16.0 / 9.0);
        assert!((w - (100.0_f64 * 16.0 / 9.0).floor()).abs() < 1.0);
        assert!((h - 100.0).abs() < 1.0);
    }
}
