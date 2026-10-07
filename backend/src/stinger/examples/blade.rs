//! Track matte, side by side: `strom-blade.mkv`.

use super::draw::*;
use super::{FPS, H, W};

/// Slant of the blade: its top sits this far right of its middle.
pub(super) const BLADE_SLANT: f64 = H as f64 * 0.2;
/// How far behind the blade the matte's switch is centred.
pub(super) const BLADE_MATTE_LAG: f64 = 40.0;
/// Width of the matte's soft switch.
pub(super) const BLADE_FEATHER: f64 = 90.0;
pub(super) const BLADE_FRAMES: usize = 40;

/// Where the blade crosses the middle row at `t`.
pub(super) fn blade_x(t: f64) -> f64 {
    let w = W as f64;
    // Just far enough out that the glow is off the frame at either end.
    let from = -BLADE_SLANT - 340.0;
    let to = w + BLADE_SLANT + 340.0;
    // Mostly linear, so it is on screen almost all the clip; done
    // a little before the end, so the last frame's motion blur is clear.
    let p = clamp01(t / 0.96);
    let p = 0.8 * p + 0.2 * ease_in_out_cubic(p);
    from + (to - from) * p
}

/// Where the blade crosses row `y` at time `t`.
pub(super) fn blade_at(t: f64, y: f64) -> f64 {
    blade_x(t) + (H as f64 / 2.0 - y) * BLADE_SLANT / (H as f64 / 2.0)
}

/// Track matte, side by side: a hot blade of light crosses the frame with a
/// chromatic fringe, crackling arcs and sparks; the matte half turns the new
/// source on right behind it.
pub(super) fn render_blade(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let k = BLADE_SLANT / (h / 2.0);
    let bx = blade_x(t);
    let speed = (blade_x(t + 0.005) - blade_x(t - 0.005)) / 0.01;
    let energy = clamp01(speed / 4000.0);
    let at = |y: f64| blade_at(t, y);
    // Lines of constant x in this space follow the blade.
    let shear = cairo::Matrix::new(1.0, 0.0, -k, 1.0, k * h / 2.0, 0.0);

    // Fill, left half only: anything drawn past it would land in the matte.
    let _ = cr.save();
    cr.rectangle(0.0, 0.0, w, h);
    cr.clip();

    let _ = cr.save();
    cr.transform(shear);
    // Energy trail over the revealed picture, as long as the blade is fast.
    let trail = 420.0 * energy;
    if trail > 1.0 {
        let g = cairo::LinearGradient::new(bx - trail, 0.0, bx, 0.0);
        g.add_color_stop_rgba(0.0, 0.4, 0.3, 1.0, 0.0);
        g.add_color_stop_rgba(0.7, 0.4, 0.6, 1.0, 0.12);
        g.add_color_stop_rgba(1.0, 0.6, 0.9, 1.0, 0.4);
        let _ = cr.set_source(&g);
        cr.rectangle(bx - trail, 0.0, trail, h);
        let _ = cr.fill();
    }
    // Wide and tight glow.
    for (half, a) in [(330.0, 0.35), (150.0, 0.6), (60.0, 0.85)] {
        let g = cairo::LinearGradient::new(bx - half, 0.0, bx + half, 0.0);
        g.add_color_stop_rgba(0.0, 0.2, 0.55, 1.0, 0.0);
        g.add_color_stop_rgba(0.5, 0.45, 0.85, 1.0, a);
        g.add_color_stop_rgba(1.0, 0.2, 0.55, 1.0, 0.0);
        let _ = cr.set_source(&g);
        cr.rectangle(bx - half, 0.0, 2.0 * half, h);
        let _ = cr.fill();
    }
    // Chromatic fringe: magenta ahead, blue behind.
    for (off, c) in [(28.0, (1.0, 0.15, 0.55)), (-28.0, (0.1, 0.35, 1.0))] {
        let x = bx + off;
        let g = cairo::LinearGradient::new(x - 16.0, 0.0, x + 16.0, 0.0);
        g.add_color_stop_rgba(0.0, c.0, c.1, c.2, 0.0);
        g.add_color_stop_rgba(0.5, c.0, c.1, c.2, 0.95);
        g.add_color_stop_rgba(1.0, c.0, c.1, c.2, 0.0);
        let _ = cr.set_source(&g);
        cr.rectangle(x - 16.0, 0.0, 32.0, h);
        let _ = cr.fill();
    }
    // White-hot core.
    let g = cairo::LinearGradient::new(bx - 20.0, 0.0, bx + 20.0, 0.0);
    g.add_color_stop_rgba(0.0, 0.8, 0.95, 1.0, 0.0);
    g.add_color_stop_rgba(0.3, 1.0, 1.0, 1.0, 1.0);
    g.add_color_stop_rgba(0.7, 1.0, 1.0, 1.0, 1.0);
    g.add_color_stop_rgba(1.0, 0.8, 0.95, 1.0, 0.0);
    let _ = cr.set_source(&g);
    cr.rectangle(bx - 20.0, 0.0, 40.0, h);
    let _ = cr.fill();
    let _ = cr.restore();

    // Speed lines streaming off behind the blade.
    let mut rng = Rng::new(0x0057_EA4D);
    for _ in 0..48 {
        let y = rng.range(0.0, h);
        let len = rng.range(0.3, 1.0) * 900.0 * energy;
        let gap = rng.range(0.0, 120.0);
        let thick = rng.range(1.5, 4.0);
        let a = rng.range(0.3, 0.8) * energy;
        if len < 20.0 {
            continue;
        }
        let head = at(y) - gap;
        let g = cairo::LinearGradient::new(head - len, y, head, y);
        g.add_color_stop_rgba(0.0, 0.4, 0.8, 1.0, 0.0);
        g.add_color_stop_rgba(1.0, 0.85, 0.97, 1.0, a);
        let _ = cr.set_source(&g);
        cr.rectangle(head - len, y - thick / 2.0, len, thick);
        let _ = cr.fill();
    }

    // Crackling arcs hugging the blade, new every frame.
    let frame = (t * (BLADE_FRAMES - 1) as f64).floor() as u64;
    let mut rng = Rng::new(0xB1ADE ^ frame.wrapping_mul(0x1000_0001));
    cr.set_line_join(cairo::LineJoin::Round);
    for arc in 0..3 {
        let spread = 22.0 + 14.0 * arc as f64;
        let mut pts = Vec::with_capacity(32);
        let mut y = -20.0;
        while y < h + 40.0 {
            pts.push((at(y) + rng.range(-spread, spread), y));
            y += rng.range(30.0, 60.0);
        }
        for (lw, a) in [(9.0, 0.18 * energy), (2.5, 0.85 * energy)] {
            if a <= 0.01 {
                continue;
            }
            for (i, (x, y)) in pts.iter().enumerate() {
                if i == 0 {
                    cr.move_to(*x, *y);
                } else {
                    cr.line_to(*x, *y);
                }
            }
            cr.set_source_rgba(0.75, 0.92, 1.0, a);
            cr.set_line_width(lw);
            let _ = cr.stroke();
        }
    }

    // Sparks: born on the blade, they fall behind it, arc down and fade.
    let dur = BLADE_FRAMES as f64 / FPS as f64;
    let now = t * dur;
    let mut rng = Rng::new(0x005B_A4C5);
    cr.set_line_cap(cairo::LineCap::Round);
    for _ in 0..420 {
        let born = rng.range(0.03, 0.66) * dur;
        let life = rng.range(0.15, 0.4);
        let y0 = rng.range(-40.0, h + 40.0);
        let vx = rng.range(-900.0, 600.0);
        let vy = rng.range(-500.0, 300.0);
        let size = rng.range(2.5, 7.0);
        let hot = rng.unit() < 0.25;
        let age = now - born;
        if age < 0.0 || age > life {
            continue;
        }
        let born_t = born / dur;
        let carry = (blade_x(born_t + 0.005) - blade_x(born_t - 0.005)) / 0.01 / dur * 0.35;
        let x = blade_at(born_t, y0) + (vx + carry) * age;
        let y = y0 + vy * age + 900.0 * age * age;
        let fade = 1.0 - age / life;
        let (dx, dy) = ((vx + carry) * 0.035, (vy + 1800.0 * age) * 0.035);
        let c = if hot {
            (1.0, 0.45, 0.75)
        } else {
            (0.75, 0.95, 1.0)
        };
        cr.set_source_rgba(c.0, c.1, c.2, 0.95 * fade);
        cr.set_line_width(size * (0.4 + 0.6 * fade));
        cr.move_to(x - dx, y - dy);
        cr.line_to(x, y);
        let _ = cr.stroke();
        if size > 4.5 {
            bloom(cr, x, y, 30.0 * fade + 6.0, c, 0.6 * fade);
        }
    }

    // Flares where the blade crosses the upper and lower thirds.
    for fy in [h * 0.3, h * 0.72] {
        let fx = at(fy);
        let a = energy * inside(fx, -200.0, w + 200.0, 400.0);
        bloom(cr, fx, fy, 230.0, (0.4, 0.75, 1.0), 0.5 * a);
        anamorphic(cr, fx, fy, 900.0, (0.4, 0.7, 1.0), 0.6 * a);
    }
    let _ = cr.restore();
}

/// The blade's matte, right half: white behind the blade, black ahead, a
/// soft switch just behind the core so the new source shows right behind
/// the light.
pub(super) fn blade_matte(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let k = BLADE_SLANT / (h / 2.0);
    let bx = blade_x(t);
    let shear = cairo::Matrix::new(1.0, 0.0, -k, 1.0, k * h / 2.0, 0.0);
    let _ = cr.save();
    cr.rectangle(w, 0.0, w, h);
    cr.clip();
    cr.set_source_rgb(0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.translate(w, 0.0);
    cr.transform(shear);
    let edge = bx - BLADE_MATTE_LAG;
    let g = cairo::LinearGradient::new(
        edge - BLADE_FEATHER / 2.0,
        0.0,
        edge + BLADE_FEATHER / 2.0,
        0.0,
    );
    g.add_color_stop_rgb(0.0, 1.0, 1.0, 1.0);
    g.add_color_stop_rgb(1.0, 0.0, 0.0, 0.0);
    let _ = cr.set_source(&g);
    cr.rectangle(-4.0 * w, 0.0, 8.0 * w, h);
    let _ = cr.fill();
    let _ = cr.restore();
}
