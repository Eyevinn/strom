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
//! then EOS. A live aggregator does not wait for an EOS pad.
//!
//! An underlay pad holds that frame (`repeat-after-eos` on) while its zone
//! border is configured or the pad is still visible (alpha above 0, e.g. a
//! border fading out after its zone was removed), so a take that reveals a
//! configured border shows it on the cut frame without anything being
//! uploaded again. Otherwise it holds no frame: with `repeat-after-eos` off,
//! videoaggregator drops an EOS pad's expired frame, and a pad without a
//! frame is never prepared — the GL mixer would otherwise map every
//! underlay's texture on every output frame. See [`watch_underlay_pad`] and
//! [`set_underlay_configured`].
//!
//! The border color is changed by writing the source's `foreground-color`
//! (`0xAARRGGBB`) and, when the pad holds a frame, restarting the source,
//! which pushes one frame in the new color (see [`set_underlay_color`]).

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::{Mutex, MutexGuard, OnceLock};
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

/// Name suffix and infix of the sources the vision mixer builds for its
/// underlay pads (`<block>:underlay_<region>_<input>_src`).
const UNDERLAY_SRC_INFIX: &str = ":underlay_";
const UNDERLAY_SRC_SUFFIX: &str = "_src";

/// Whether `el` is an underlay source the vision mixer built — by name, so
/// a user's own `videotestsrc` upstream of a mixer input is never touched.
fn is_underlay_src(el: &gst::Element) -> bool {
    let name = el.name();
    name.contains(UNDERLAY_SRC_INFIX)
        && name.ends_with(UNDERLAY_SRC_SUFFIX)
        && el.factory().is_some_and(|f| f.name() == "videotestsrc")
}

/// The underlay source feeding an underlay pad, found by walking upstream
/// past the short static chain (capsfilter / glupload / videoconvert).
fn upstream_underlay_src(pad: &gst::Pad) -> Option<gst::Element> {
    let mut el = pad.peer()?.parent_element()?;
    for _ in 0..6 {
        if is_underlay_src(&el) {
            return Some(el);
        }
        let sink = el.static_pad("sink")?;
        el = sink.peer()?.parent_element()?;
    }
    None
}

/// Create the source feeding one underlay pad: a non-live solid-color
/// `videotestsrc` that pushes one frame and then EOS. `name` must follow
/// `<block>:underlay_<...>_src`; the pad it feeds is registered with
/// [`watch_underlay_pad`] once linked.
pub(crate) fn make_underlay_src(name: &str) -> Result<gst::Element, gst::glib::BoolError> {
    debug_assert!(name.contains(UNDERLAY_SRC_INFIX) && name.ends_with(UNDERLAY_SRC_SUFFIX));
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
/// restarts it when the pad holds a frame: READY and back to the pipeline's
/// state makes it send a new stream (which clears the pad's EOS), one frame
/// in the new color, and EOS again. The frame is stamped with the current
/// running time so the mixer takes it rather than dropping it as late. The
/// mixer pad keeps the old frame until the new one is taken, so the border
/// never blinks out. A pad holding no frame gets one in the then current
/// color when it starts holding.
///
/// Never blocks: the restart runs on GStreamer's async-call pool (see
/// [`schedule_underlay_restart`]), because stopping a source waits for its
/// streaming thread, which can sit inside the mixer's sink pad.
pub(crate) fn set_underlay_color(pad: &gst::Pad, argb: u32) {
    if let Some(src) = upstream_underlay_src(pad) {
        if src.property::<u32>("foreground-color") != argb {
            src.set_property("foreground-color", argb);
            if pad.has_property(REPEAT_AFTER_EOS) && pad.property::<bool>(REPEAT_AFTER_EOS) {
                schedule_underlay_restart(&src);
            }
        }
    }
}

/// The pad property that makes an underlay pad keep its frame.
const REPEAT_AFTER_EOS: &str = "repeat-after-eos";

/// Registered underlay pads and whether each one's zone border is
/// configured. Weak: the registry must not keep a torn-down pipeline's pads
/// alive. Its lock also serializes every hold decision, which is made from
/// API threads and from the mixer's streaming thread (alpha animations).
type HoldRegistry = Vec<(gst::glib::WeakRef<gst::Pad>, bool)>;

fn holds() -> MutexGuard<'static, HoldRegistry> {
    static HOLDS: OnceLock<Mutex<HoldRegistry>> = OnceLock::new();
    match HOLDS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        Ok(h) => h,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Register a vision mixer underlay pad: it starts with its border not
/// configured, and from now on holds a frame exactly while its border is
/// configured ([`set_underlay_configured`]) or it is visible (alpha above
/// 0). Call once the pad is linked, from the block's element setup.
///
/// Listens to `notify::alpha`, so layout snaps and control-binding
/// animations alike are seen. The handler gets the pad as its argument and
/// captures nothing.
pub(crate) fn watch_underlay_pad(pad: &gst::Pad) {
    if !pad.has_property(REPEAT_AFTER_EOS) || upstream_underlay_src(pad).is_none() {
        warn!("{} is not an underlay pad; not watching it", pad.name());
        return;
    }
    {
        let mut holds = holds();
        holds.retain(|(w, _)| w.upgrade().is_some_and(|p| &p != pad));
        holds.push((pad.downgrade(), false));
    }
    pad.connect_notify(Some("alpha"), |pad, _| sync_underlay_frame(pad));
    sync_underlay_frame(pad);
}

/// Mark whether an underlay pad's zone border is configured in the layout
/// (whether or not it is on screen right now). A configured border's pad
/// holds its frame, so a take that reveals it shows it at once.
pub(crate) fn set_underlay_configured(pad: &gst::Pad, configured: bool) {
    let mut holds = holds();
    if let Some(entry) = holds
        .iter_mut()
        .find(|(w, _)| w.upgrade().is_some_and(|p| &p == pad))
    {
        entry.1 = configured;
    }
    apply_hold(&holds, pad);
}

/// Re-decide whether an underlay pad holds a frame (`notify::alpha`).
fn sync_underlay_frame(pad: &gst::Pad) {
    let holds = holds();
    apply_hold(&holds, pad);
}

/// Make `pad` hold a frame if and only if its border is configured or it is
/// visible. Starting to hold restarts the source for a fresh frame
/// (asynchronously); stopping lets the mixer drop the frame on its next
/// output frame. Called with the registry locked, so concurrent decisions
/// for a pad apply in order and the last one sees the final alpha.
fn apply_hold(holds: &HoldRegistry, pad: &gst::Pad) {
    let Some(configured) = holds
        .iter()
        .find(|(w, _)| w.upgrade().is_some_and(|p| &p == pad))
        .map(|(_, c)| *c)
    else {
        return;
    };
    let hold = configured || pad.property::<f64>("alpha") > 0.0;
    if hold == pad.property::<bool>(REPEAT_AFTER_EOS) {
        return;
    }
    pad.set_property(REPEAT_AFTER_EOS, hold);
    if hold {
        if let Some(src) = upstream_underlay_src(pad) {
            schedule_underlay_restart(&src);
        }
    }
}

/// Underlay sources with a restart scheduled or running, each with a
/// "restart again" flag. Weak: a pending restart must not keep a torn-down
/// pipeline's source alive.
type RestartRegistry = Vec<(gst::glib::WeakRef<gst::Element>, bool)>;

fn restarts() -> MutexGuard<'static, RestartRegistry> {
    static PENDING: OnceLock<Mutex<RestartRegistry>> = OnceLock::new();
    match PENDING.get_or_init(|| Mutex::new(Vec::new())).lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Restart `src` on GStreamer's async-call pool so it pushes one frame in
/// its current color. One restart runs per source at a time: a request
/// while one is scheduled or running only flags it to run again, and each
/// run picks up the latest color — so rapid changes coalesce and none is
/// lost.
fn schedule_underlay_restart(src: &gst::Element) {
    {
        let mut pending = restarts();
        pending.retain(|(w, _)| w.upgrade().is_some());
        if let Some(entry) = pending
            .iter_mut()
            .find(|(w, _)| w.upgrade().is_some_and(|e| &e == src))
        {
            entry.1 = true;
            return;
        }
        pending.push((src.downgrade(), true));
    }
    // `call_async` holds a reference to the element only until the call
    // has run; the closure captures nothing.
    src.call_async(|src| {
        run_underlay_restarts(src);
    });
}

/// Restart `src` until no further restart was requested meanwhile, then
/// take it off the registry. Runs on the async-call pool, where waiting for
/// the streaming thread is harmless.
fn run_underlay_restarts(src: &gst::Element) {
    loop {
        {
            let mut pending = restarts();
            let Some(entry) = pending
                .iter_mut()
                .find(|(w, _)| w.upgrade().is_some_and(|e| &e == src))
            else {
                return;
            };
            if !entry.1 {
                pending.retain(|(w, _)| w.upgrade().is_some_and(|e| &e != src));
                return;
            }
            entry.1 = false;
        }
        if !restart_underlay_src(src) {
            restarts().retain(|(w, _)| w.upgrade().is_some_and(|e| &e != src));
            return;
        }
    }
}

/// Make a started underlay source push one frame in its current color.
/// Returns false when there is nothing to restart: the pipeline has not
/// started, or the flow is being torn down.
fn restart_underlay_src(src: &gst::Element) -> bool {
    // The pipeline's state, not the source's: the source itself is READY
    // for a moment during a restart.
    let Some(parent) = src.parent().and_then(|p| p.downcast::<gst::Element>().ok()) else {
        return false;
    };
    let (_, current, pending) = parent.state(gst::ClockTime::ZERO);
    if current < gst::State::Paused || pending == gst::State::Ready || pending == gst::State::Null {
        return false;
    }
    if src.set_state(gst::State::Ready).is_err() {
        warn!(
            "Underlay source {} did not stop for a color change",
            src.name()
        );
        return true;
    }
    // Now, after any wait for the streaming thread, so the frame is not
    // late. Read off the parent: a stopped source may have lost its clock.
    let offset = parent.current_running_time().map_or(0, |t| t.nseconds());
    src.set_property("timestamp-offset", offset as i64);
    if src.sync_state_with_parent().is_err() {
        warn!(
            "Underlay source {} did not restart for a color change",
            src.name()
        );
    }
    true
}
