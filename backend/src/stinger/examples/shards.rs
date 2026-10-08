//! Mask only: `strom-shards.mkv`.

use super::draw::*;
use super::{H, W};
use std::f64::consts::PI;

/// Mask only: hexagonal tiles flip open in a wave that spreads from left of
/// centre, each turning from black through grey to white as it flips face
/// on.
pub(super) fn render_shards(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    cr.set_source_rgb(0.0, 0.0, 0.0);
    let _ = cr.paint();
    // Each tile only ever brightens, and overlapping rims keep the brighter.
    cr.set_operator(cairo::Operator::Lighten);
    let r = 72.0;
    let dx = 3f64.sqrt() * r;
    let dy = 1.5 * r;
    let origin = (w * 0.3, h * 0.55);
    // The farthest corner of the frame.
    let reach = ((w - origin.0).powi(2) + origin.1.powi(2)).sqrt();
    let mut rng = Rng::new(0x5_4A2D);
    let rows = (h / dy).ceil() as i32 + 1;
    let cols = (w / dx).ceil() as i32 + 1;
    for row in -1..=rows {
        for col in -1..=cols {
            let cx = col as f64 * dx
                + if row.rem_euclid(2) == 1 {
                    dx / 2.0
                } else {
                    0.0
                };
            let cy = row as f64 * dy;
            let jitter = rng.range(0.0, 0.08);
            let axis = rng.range(-0.4, 0.4);
            let dist = ((cx - origin.0).powi(2) + (cy - origin.1).powi(2)).sqrt();
            // Tiles just outside the frame flip with the last ones inside.
            let delay = (0.64 * dist / reach).min(0.64) + jitter;
            let p = span(t, delay, 0.26);
            if p <= 0.0 {
                continue;
            }
            // Flip about an axis along the wave front: edge on, then face on.
            let dir = (cy - origin.1).atan2(cx - origin.0) + axis;
            let open = ease_out_cubic(p);
            let level = smooth(p * 1.15);
            let _ = cr.save();
            cr.translate(cx, cy);
            cr.rotate(dir);
            cr.scale(open.max(0.001), 1.0);
            cr.rotate(-dir);
            // A hair larger than the grid, so neighbours overlap.
            let rr = r + 2.0;
            for k in 0..6 {
                let a = PI / 6.0 + k as f64 * PI / 3.0;
                let (x, y) = (rr * a.cos(), rr * a.sin());
                if k == 0 {
                    cr.move_to(x, y);
                } else {
                    cr.line_to(x, y);
                }
            }
            cr.close_path();
            let _ = cr.restore();
            cr.set_source_rgb(level, level, level);
            let _ = cr.fill();
        }
    }
}
