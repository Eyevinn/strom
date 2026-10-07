//! Drawing helpers shared by the example clips: a seeded generator, easing
//! curves, colours and light effects.

use std::f64::consts::PI;

/// SplitMix64: a tiny seeded generator, so every render is identical.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub(super) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in 0..1.
    pub(super) fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(super) fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

pub(super) fn clamp01(t: f64) -> f64 {
    t.clamp(0.0, 1.0)
}

/// Progress of `t` through the span starting at `start` lasting `len`.
pub(super) fn span(t: f64, start: f64, len: f64) -> f64 {
    clamp01((t - start) / len)
}

pub(super) fn smooth(t: f64) -> f64 {
    let t = clamp01(t);
    t * t * (3.0 - 2.0 * t)
}

pub(super) fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - clamp01(t)).powi(3)
}

pub(super) fn ease_in_out_cubic(t: f64) -> f64 {
    let t = clamp01(t);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Overshoots past 1 and settles back.
pub(super) fn ease_out_back(t: f64) -> f64 {
    let t = clamp01(t);
    let s = 1.70158;
    1.0 + (s + 1.0) * (t - 1.0).powi(3) + s * (t - 1.0).powi(2)
}

/// Pulls back below 0 by an amount set by `s` before it sets off.
pub(super) fn ease_in_back(t: f64, s: f64) -> f64 {
    let t = clamp01(t);
    (s + 1.0) * t * t * t - s * t * t
}

/// Fades in over `ramp` pixels from `lo` and out over `ramp` before `hi`, so
/// flares die out before they reach past the frame.
pub(super) fn inside(x: f64, lo: f64, hi: f64, ramp: f64) -> f64 {
    smooth((x - lo) / ramp) * smooth((hi - x) / ramp)
}

pub(super) type Rgb = (f64, f64, f64);

pub(super) fn hex(v: u32) -> Rgb {
    (
        ((v >> 16) & 0xff) as f64 / 255.0,
        ((v >> 8) & 0xff) as f64 / 255.0,
        (v & 0xff) as f64 / 255.0,
    )
}

pub(super) fn mix(a: Rgb, b: Rgb, k: f64) -> Rgb {
    (
        a.0 + (b.0 - a.0) * k,
        a.1 + (b.1 - a.1) * k,
        a.2 + (b.2 - a.2) * k,
    )
}

pub(super) fn stop(g: &cairo::Gradient, at: f64, c: Rgb, a: f64) {
    g.add_color_stop_rgba(at, c.0, c.1, c.2, a);
}

/// A soft radial bloom.
pub(super) fn bloom(cr: &cairo::Context, x: f64, y: f64, r: f64, c: Rgb, a: f64) {
    if a <= 0.005 || r <= 0.0 {
        return;
    }
    let g = cairo::RadialGradient::new(x, y, 0.0, x, y, r);
    stop(&g, 0.0, (1.0, 1.0, 1.0), a);
    stop(&g, 0.15, c, a * 0.6);
    stop(&g, 0.5, c, a * 0.15);
    stop(&g, 1.0, c, 0.0);
    let _ = cr.set_source(&g);
    cr.arc(x, y, r, 0.0, 2.0 * PI);
    let _ = cr.fill();
}

/// A horizontal anamorphic lens streak centred on `x`, `y`.
pub(super) fn anamorphic(cr: &cairo::Context, x: f64, y: f64, len: f64, c: Rgb, a: f64) {
    if a <= 0.005 {
        return;
    }
    let _ = cr.save();
    cr.translate(x, y);
    cr.scale(1.0, 0.025);
    let g = cairo::RadialGradient::new(0.0, 0.0, 0.0, 0.0, 0.0, len);
    stop(&g, 0.0, (1.0, 1.0, 1.0), a);
    stop(&g, 0.2, c, a * 0.5);
    stop(&g, 1.0, c, 0.0);
    let _ = cr.set_source(&g);
    cr.arc(0.0, 0.0, len, 0.0, 2.0 * PI);
    let _ = cr.fill();
    let _ = cr.restore();
}
