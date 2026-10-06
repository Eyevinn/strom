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
//! (16×16, non-live so it adds no latency) that pushes a single frame and
//! then EOS; the mixer pad has `repeat-after-eos` set, so it keeps showing
//! that frame without anything being uploaded again. A live aggregator does
//! not wait for an EOS pad. The border color is changed by writing the
//! source's `foreground-color` (`0xAARRGGBB`) and restarting the source, which
//! pushes one frame in the new color (see [`set_underlay_color`]).

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::warn;

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

/// Create the source feeding one underlay pad: a non-live solid-color
/// `videotestsrc` that pushes one frame and then EOS. The compositor pad it
/// feeds must have `repeat-after-eos` set (see the vision mixer pad layout).
pub(crate) fn make_underlay_src(name: &str) -> Result<gst::Element, gst::glib::BoolError> {
    let src = gst::ElementFactory::make("videotestsrc")
        .name(name)
        .property("is-live", false)
        .property("num-buffers", 1i32)
        .build()?;
    src.set_property_from_str("pattern", "solid-color");
    Ok(src)
}

/// Set an underlay pad's border color (`0xAARRGGBB`). Skips the property
/// write when the color is unchanged.
///
/// The source has already pushed its one frame, so a new color also
/// restarts it: READY and back to the pipeline's state makes it send a new
/// stream (which clears the pad's EOS), one frame in the new color, and EOS
/// again. The frame is stamped with the current running time so the mixer
/// takes it on the next output frame rather than dropping it as late. The
/// mixer pad keeps the old frame until the new one is taken, so the border
/// never blinks out.
pub(crate) fn set_underlay_color(pad: &gst::Pad, argb: u32) {
    if let Some(src) = upstream_underlay_src(pad) {
        if src.property::<u32>("foreground-color") != argb {
            src.set_property("foreground-color", argb);
            restart_underlay_src(&src);
        }
    }
}

/// Make a started underlay source push one frame in its current color.
fn restart_underlay_src(src: &gst::Element) {
    // Not started yet: it will push the new color when it starts.
    if src.current_state() < gst::State::Paused {
        return;
    }
    let offset = src.current_running_time().map_or(0, |t| t.nseconds());
    if src.set_state(gst::State::Ready).is_err() {
        warn!(
            "Underlay source {} did not stop for a color change",
            src.name()
        );
        return;
    }
    src.set_property("timestamp-offset", offset as i64);
    if src.sync_state_with_parent().is_err() {
        warn!(
            "Underlay source {} did not restart for a color change",
            src.name()
        );
    }
}
