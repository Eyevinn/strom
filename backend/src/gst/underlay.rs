//! Border underlay pads for the vision mixer compositors.
//!
//! A zone border is rendered as a solid-color compositor pad sitting in
//! z-order directly beneath its content pad, sized to the content rect
//! inflated outward by the (region-scaled) border width. Because the
//! underlay is a regular pad, it is animated by the same control-binding
//! machinery as the content pad — borders track morphs/takes/fades exactly,
//! with no external renderer chasing the compositor's clock — and z-order
//! interleaving is correct by construction: an overlapping higher zone
//! covers a lower zone's border like a stacked framed card.
//!
//! Each underlay pad is fed by a tiny `videotestsrc pattern=solid-color`
//! (16×16, non-live so it adds no latency); the border color is changed by
//! writing the source's `foreground-color` (`0xAARRGGBB`).

use gstreamer as gst;
use gstreamer::prelude::*;

/// Where a region's underlay pads live and how PGM-pixel border widths
/// scale into the region.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UnderlayCtx {
    /// Sink index of input 0's underlay pad (input i's underlay = base + i).
    pub base: usize,
    /// `region_width / pgm_width` — keeps borders proportionally identical
    /// on PGM, the PVW big display and the PiP tiles regardless of
    /// resolution.
    pub scale: f64,
}

/// Inflate a content rect outward by the region-scaled border width,
/// clamped to the region so a border at a tile edge never bleeds into the
/// neighboring multiview tile. At least 1 px so thin borders don't vanish
/// on small tiles.
pub(crate) fn underlay_rect(
    content: (i32, i32, i32, i32),
    border_width_pgm_px: f32,
    region: (i32, i32, i32, i32),
    scale: f64,
) -> (i32, i32, i32, i32) {
    let bw = ((border_width_pgm_px as f64 * scale).round() as i32).max(1);
    let (cx, cy, cw, ch) = content;
    let (rx, ry, rw, rh) = region;
    let x0 = (cx - bw).max(rx);
    let y0 = (cy - bw).max(ry);
    let x1 = (cx + cw + bw).min(rx + rw);
    let y1 = (cy + ch + bw).min(ry + rh);
    (x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
}

/// The `videotestsrc` feeding an underlay pad, found by walking upstream
/// past the short static chain (capsfilter / glupload / queue).
fn upstream_underlay_src(pad: &gst::Pad) -> Option<gst::Element> {
    let mut el = pad.peer()?.parent_element()?;
    for _ in 0..6 {
        if el
            .factory()
            .map(|f| f.name() == "videotestsrc")
            .unwrap_or(false)
        {
            return Some(el);
        }
        let sink = el.static_pad("sink")?;
        el = sink.peer()?.parent_element()?;
    }
    None
}

/// Set an underlay pad's border color (`0xAARRGGBB`). Skips the property
/// write when the color is unchanged.
pub(crate) fn set_underlay_color(pad: &gst::Pad, argb: u32) {
    if let Some(src) = upstream_underlay_src(pad) {
        if src.property::<u32>("foreground-color") != argb {
            src.set_property("foreground-color", argb);
        }
    }
}
