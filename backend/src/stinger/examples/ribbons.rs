//! Track matte, stacked: `strom-ribbons.mkv`.

use super::draw::*;
use super::{H, W};
use std::f64::consts::PI;

/// Ribbons are drawn in a frame turned by this much, so they sweep up a
/// little as they cross.
pub(super) const RIBBON_TILT: f64 = -0.17;
/// Half the length and height of the turned frame that covers the picture.
pub(super) const RIBBON_HALF_U: f64 = 1150.0;
pub(super) const RIBBON_HALF_V: f64 = 600.0;
pub(super) const RIBBON_COUNT: usize = 7;
/// Half the thickness of a ribbon.
pub(super) const RIBBON_HALF: f64 = 200.0;
pub(super) const RIBBON_WAVE: f64 = 40.0;
pub(super) const RIBBON_FEATHER: f64 = 90.0;
/// How far the round head reaches past `head`, as a share of the thickness.
pub(super) const RIBBON_HEAD: f64 = 0.6;
/// Length over which a ribbon narrows towards its tail.
pub(super) const RIBBON_TAPER: f64 = 700.0;
/// How far behind the head the matte's switch is centred.
pub(super) const RIBBON_MATTE_LAG: f64 = 150.0;
/// Half the thickness of the fully white core of a ribbon's swept band in
/// the matte: half the spacing and the wave both ways, so the cores of
/// neighbouring bands always meet. A soft edge lies outside it.
pub(super) const RIBBON_BAND_HALF: f64 = RIBBON_HALF_V / (RIBBON_COUNT - 1) as f64 + RIBBON_WAVE;
/// Width of the band's soft top and bottom edge.
pub(super) const RIBBON_BAND_SOFT: f64 = 70.0;

pub(super) const RIBBON_COLOURS: [(u32, u32); RIBBON_COUNT] = [
    (0x8a3ffc, 0xd06bff),
    (0x2f6bff, 0x22d3ee),
    (0xff2e88, 0xff7ab8),
    (0xff7a1a, 0xffc93c),
    (0x18d2b5, 0x9cff6b),
    (0xff4d4d, 0xff9a3c),
    (0x22d3ee, 0xb8f4ff),
];

pub(super) struct Ribbon {
    v: f64,
    phase: f64,
    start: f64,
    colours: (Rgb, Rgb),
}

pub(super) fn ribbons() -> Vec<Ribbon> {
    // From the middle outwards, alternately, so the sweep fans open.
    let order = [3usize, 2, 4, 1, 5, 0, 6];
    let spacing = 2.0 * RIBBON_HALF_V / (RIBBON_COUNT - 1) as f64;
    let mut rng = Rng::new(0x41_BB0E);
    (0..RIBBON_COUNT)
        .map(|i| {
            let rank = order.iter().position(|o| *o == i).unwrap_or(i);
            Ribbon {
                v: -RIBBON_HALF_V + spacing * i as f64,
                phase: rng.range(0.0, 2.0 * PI),
                start: 0.03 * rank as f64 + rng.range(0.0, 0.015),
                colours: (hex(RIBBON_COLOURS[i].0), hex(RIBBON_COLOURS[i].1)),
            }
        })
        .collect()
}

impl Ribbon {
    /// Head and tail positions along the ribbon at `t`.
    fn ends(&self, t: f64) -> (f64, f64) {
        let from = -RIBBON_HALF_U - 100.0;
        let to = RIBBON_HALF_U + RIBBON_HALF + 120.0;
        let travel = 0.45;
        let ease = |p: f64| 0.4 * p + 0.6 * ease_in_out_cubic(p);
        let head = from + (to - from) * ease(span(t, self.start, travel));
        let tail = from + (to - from) * ease(span(t, self.start + 0.34, travel));
        (head, tail - 2.0 * RIBBON_HALF)
    }

    /// Centre line at `u`.
    fn centre(&self, u: f64) -> f64 {
        self.v + RIBBON_WAVE * (u / 520.0 + self.phase).sin()
    }
}

/// Into the turned frame centred on the picture whose top is at `top`.
pub(super) fn ribbon_space(cr: &cairo::Context, top: f64) {
    cr.translate(W as f64 / 2.0, top + H as f64 / 2.0);
    cr.rotate(RIBBON_TILT);
}

/// A ribbon's outline from `tail` to `head`, `half` thick, rounded at the
/// head and, with `taper`, narrowing towards the tail.
pub(super) fn ribbon_path(
    cr: &cairo::Context,
    r: &Ribbon,
    tail: f64,
    head: f64,
    half: f64,
    taper: bool,
) {
    let len = head - tail;
    let n = ((len / 24.0).ceil() as usize).max(2);
    let width = |u: f64| {
        if taper {
            half * (0.1 + 0.9 * smooth((u - tail) / RIBBON_TAPER))
        } else {
            half
        }
    };
    for i in 0..=n {
        let u = tail + len * i as f64 / n as f64;
        let y = r.centre(u) - width(u);
        if i == 0 {
            cr.move_to(u, y);
        } else {
            cr.line_to(u, y);
        }
    }
    // A flattened round head, like the front of a brush stroke.
    let _ = cr.save();
    cr.translate(head, r.centre(head));
    cr.scale(RIBBON_HEAD, 1.0);
    cr.arc(0.0, 0.0, half, -PI / 2.0, PI / 2.0);
    let _ = cr.restore();
    for i in (0..=n).rev() {
        let u = tail + len * i as f64 / n as f64;
        cr.line_to(u, r.centre(u) + width(u));
    }
    cr.close_path();
}

/// The tapered part of a ribbon from `tail` to `to`, without its head.
pub(super) fn ribbon_body(cr: &cairo::Context, r: &Ribbon, tail: f64, to: f64) {
    let len = to - tail;
    let n = ((len / 24.0).ceil() as usize).max(2);
    let width = |u: f64| RIBBON_HALF * (0.1 + 0.9 * smooth((u - tail) / RIBBON_TAPER));
    for i in 0..=n {
        let u = tail + len * i as f64 / n as f64;
        cr.line_to(u, r.centre(u) - width(u));
    }
    for i in (0..=n).rev() {
        let u = tail + len * i as f64 / n as f64;
        cr.line_to(u, r.centre(u) + width(u));
    }
    cr.close_path();
}

/// A line along a ribbon at `off` from its centre, from `u0` to `u1`.
pub(super) fn ribbon_line(cr: &cairo::Context, r: &Ribbon, u0: f64, u1: f64, off: f64) {
    let mut u = u0;
    cr.move_to(u, r.centre(u) + off);
    while u < u1 {
        u = (u + 30.0).min(u1);
        cr.line_to(u, r.centre(u) + off);
    }
}

/// Track matte, stacked: seven brush ribbons fan out from the middle and
/// sweep across, with bristle texture, a highlight edge and drop shadows.
/// The matte below turns the new source on behind each ribbon's head.
pub(super) fn render_ribbons(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let ribbons = ribbons();

    // Fill, top half only.
    let _ = cr.save();
    cr.rectangle(0.0, 0.0, w, h);
    cr.clip();
    ribbon_space(cr, 0.0);
    // Back to front: the ones that start last lie on top.
    let mut order: Vec<&Ribbon> = ribbons.iter().collect();
    order.sort_by(|a, b| a.start.total_cmp(&b.start));
    for r in order {
        let (head, tail) = r.ends(t);
        if head - tail < 4.0
            || tail > RIBBON_HALF_U + RIBBON_HALF
            || head < -RIBBON_HALF_U - RIBBON_HALF
        {
            continue;
        }
        // The head fades in like paint thinning out at the front of a
        // stroke. Every layer carries the fade in its own source; a clip or
        // a group per ribbon would cost a whole surface each time.
        let front = head + RIBBON_HALF * RIBBON_HEAD;
        let fade_from = head - 120.0;
        let faded = |c: Rgb, a: f64| {
            let g = cairo::LinearGradient::new(fade_from, 0.0, front, 0.0);
            stop(&g, 0.0, c, a);
            stop(&g, 1.0, c, a * 0.1);
            g
        };
        // Drop shadow onto what is below.
        let _ = cr.save();
        cr.translate(14.0, 22.0);
        ribbon_path(cr, r, tail, head, RIBBON_HALF, true);
        let _ = cr.set_source(faded((0.0, 0.0, 0.05), 0.3));
        let _ = cr.fill();
        let _ = cr.restore();

        // Body: dark at the tail, bright at the head, fading at the front.
        let (a, b) = r.colours;
        let len = front - tail;
        let at = |u: f64| clamp01((u - tail) / len);
        let g = cairo::LinearGradient::new(tail, 0.0, front, 0.0);
        stop(&g, 0.0, mix(a, (0.0, 0.0, 0.0), 0.35), 1.0);
        let k = at(tail + 0.7 * (head - tail));
        if k < at(fade_from) {
            stop(&g, k, a, 1.0);
        }
        stop(&g, at(fade_from), mix(a, b, 0.8), 1.0);
        stop(&g, 1.0, b, 0.1);
        ribbon_path(cr, r, tail, head, RIBBON_HALF, true);
        let _ = cr.set_source(&g);
        let _ = cr.fill();
        // Shading across the ribbon, lit on top and darker underneath, up to
        // where the head starts to fade.
        if fade_from > tail + 4.0 {
            let span_v = RIBBON_HALF + RIBBON_WAVE;
            let g = cairo::LinearGradient::new(0.0, r.v - span_v, 0.0, r.v + span_v);
            g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.35);
            g.add_color_stop_rgba(0.35, 1.0, 1.0, 1.0, 0.0);
            g.add_color_stop_rgba(0.7, 0.0, 0.0, 0.0, 0.0);
            g.add_color_stop_rgba(1.0, 0.0, 0.0, 0.1, 0.4);
            let _ = cr.set_source(&g);
            ribbon_body(cr, r, tail, fade_from);
            let _ = cr.fill();
        }
        // Bristle streaks, where the ribbon is at full width.
        let mut rng = Rng::new(0xB815 ^ (r.v as i64 as u64));
        for _ in 0..12 {
            let off = rng.range(-0.9, 0.9) * RIBBON_HALF;
            let lw = rng.range(2.0, 9.0);
            let light = rng.unit() < 0.5;
            let from = rng.range(0.0, 0.5);
            let a = rng.range(0.08, 0.28);
            let u0 = (tail + RIBBON_TAPER + (head - tail) * from).max(tail + RIBBON_TAPER);
            if u0 >= head - 20.0 {
                continue;
            }
            ribbon_line(cr, r, u0, head - 20.0, off);
            let c = if light {
                (1.0, 1.0, 1.0)
            } else {
                (0.0, 0.0, 0.0)
            };
            let _ = cr.set_source(faded(c, a));
            cr.set_line_width(lw);
            let _ = cr.stroke();
        }
        // Highlight edge along the top.
        let u0 = tail + RIBBON_TAPER;
        if u0 < head {
            ribbon_line(cr, r, u0, head, -RIBBON_HALF + 6.0);
            let _ = cr.set_source(faded((1.0, 1.0, 1.0), 0.55));
            cr.set_line_width(3.0);
            let _ = cr.stroke();
        }
        // A glint on the head while it moves.
        let v = (r.ends(t + 0.005).0 - r.ends(t - 0.005).0) / 0.01;
        let glint = clamp01(v / 9000.0);
        bloom(
            cr,
            head + RIBBON_HALF * 0.4,
            r.centre(head) - RIBBON_HALF * 0.5,
            160.0,
            b,
            0.6 * glint,
        );
    }
    let _ = cr.restore();
}

/// The ribbons' matte, bottom half: each ribbon's swept band turns white
/// behind its head. The white cores of the bands meet, so once every head
/// has passed the matte is white everywhere; their soft edges keep a band
/// that is ahead of its neighbour from showing a hard line.
pub(super) fn ribbons_matte(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let ribbons = ribbons();
    let _ = cr.save();
    cr.rectangle(0.0, h, w, h);
    cr.clip();
    cr.set_source_rgb(0.0, 0.0, 0.0);
    let _ = cr.paint();
    ribbon_space(cr, h);
    for r in &ribbons {
        let (head, _) = r.ends(t);
        let edge = head - RIBBON_MATTE_LAG;
        // The soft edge in steps of translucent white, outermost first.
        // White over the matte only ever brightens it, so bands never darken
        // each other.
        let steps = 3;
        for k in (0..=steps).rev() {
            let level = if k == 0 { 1.0 } else { 0.3 };
            let half = RIBBON_BAND_HALF + RIBBON_BAND_SOFT * k as f64 / steps as f64;
            let g = cairo::LinearGradient::new(
                edge - RIBBON_FEATHER / 2.0,
                0.0,
                edge + RIBBON_FEATHER / 2.0,
                0.0,
            );
            g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, level);
            g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
            let _ = cr.set_source(&g);
            ribbon_path(
                cr,
                r,
                -RIBBON_HALF_U - 600.0,
                edge + RIBBON_FEATHER / 2.0,
                half,
                false,
            );
            let _ = cr.fill();
        }
    }
    let _ = cr.restore();
}
