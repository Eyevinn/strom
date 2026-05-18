//! Multiview layout calculations for the 2-5-5 broadcast grid.
//!
//! Layout:
//! ```text
//! +---------------------------+---------------------------+
//! |         PREVIEW           |          PROGRAM          |
//! |         (green)           |           (red)           |
//! +-----+-----+-----+-----+-----+
//! |  0  |  1  |  2  |  3  |  4  |  Row 1: inputs 0-4
//! +-----+-----+-----+-----+-----+
//! |  5  |  6  |  7  |  8  |  9  |  Row 2: inputs 5-9
//! +-----+-----+-----+-----+-----+
//! ```

use strom_types::vision_mixer::THUMBNAILS_PER_ROW;

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

/// Default height per thumbnail row when ≤ 2 rows fit naturally below the top
/// PVW/PGM band. Larger input counts shrink rows to fit the available height.
const DEFAULT_THUMB_ROW_HEIGHT_FRACTION: f64 = 0.235;

/// Compute the multiview layout for a given canvas size, input count, and PiP-tile count.
///
/// PiP tiles share the thumbnail grid: they are placed at slot indices
/// `num_inputs..num_inputs + num_pips`. The grid uses [`THUMBNAILS_PER_ROW`]
/// columns and as many rows as needed; row height scales down when extra rows
/// don't fit at the default size.
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

    // Thumbnail rows start below the top row.
    let thumb_y_start = gap * 2.0 + top_h;
    let total_slots = num_inputs + num_pips;
    let needed_rows = total_slots.div_ceil(THUMBNAILS_PER_ROW).max(1);
    // Available vertical space for thumbnails (one gap above the first row,
    // one between rows, and one below the last row).
    let available_h = (ch - thumb_y_start - gap * (needed_rows as f64 + 1.0)).max(0.0);
    let default_thumb_h = (ch * DEFAULT_THUMB_ROW_HEIGHT_FRACTION).round();
    let max_thumb_h = if needed_rows == 0 {
        default_thumb_h
    } else {
        (available_h / needed_rows as f64).floor()
    };
    let thumb_h = max_thumb_h.min(default_thumb_h).max(1.0);
    let thumb_w =
        ((cw - gap * (THUMBNAILS_PER_ROW as f64 + 1.0)) / THUMBNAILS_PER_ROW as f64).round();

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
        let row = i / THUMBNAILS_PER_ROW;
        let col = i % THUMBNAILS_PER_ROW;
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
