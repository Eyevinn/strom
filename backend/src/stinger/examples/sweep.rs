//! Classic: `strom-sweep.mkv`.

use super::draw::*;
use super::{H, W};

// Ink and graphite with one ultraviolet family, a hot magenta and a single
// acid volt accent: saturated and dark, never pastel.
pub(super) const INK: u32 = 0x07070d;
pub(super) const GRAPHITE: u32 = 0x17141f;
pub(super) const INDIGO: u32 = 0x24106b;
pub(super) const ULTRAVIOLET: u32 = 0x6a2cff;
pub(super) const MAGENTA: u32 = 0xff1f6d;
pub(super) const VOLT: u32 = 0xd7ff1e;

/// Slant of the sweep's edges: the top of an edge sits this far right of its
/// bottom.
pub(super) const SWEEP_SLANT: f64 = H as f64 * 0.34;

/// One slanted panel of the sweep: its leading edge enters, its trailing
/// edge leaves, both measured along the bottom of the frame.
pub(super) struct Panel {
    lead_at: f64,
    lead_len: f64,
    trail_at: f64,
    trail_len: f64,
}

pub(super) const SWEEP_PANELS: [Panel; 3] = [
    // Pink accent: first in, last out.
    Panel {
        lead_at: 0.0,
        lead_len: 0.30,
        trail_at: 0.65,
        trail_len: 0.29,
    },
    // Cyan.
    Panel {
        lead_at: 0.05,
        lead_len: 0.30,
        trail_at: 0.60,
        trail_len: 0.29,
    },
    // The navy panel that covers the frame and carries the name.
    Panel {
        lead_at: 0.10,
        lead_len: 0.30,
        trail_at: 0.55,
        trail_len: 0.29,
    },
];

impl Panel {
    /// Leading and trailing edge positions at `t`.
    fn edges(&self, t: f64) -> (f64, f64) {
        let w = W as f64;
        let s = SWEEP_SLANT;
        let off_left = -s - 600.0;
        let off_right = w + 160.0;
        // Fast in, decelerating onto its mark.
        let lead = off_left
            + (off_right - off_left) * ease_out_cubic(span(t, self.lead_at, self.lead_len));
        // A small pull back, then accelerating away.
        let trail_from = -s - 40.0;
        let trail_to = off_right + s + 40.0;
        let trail = trail_from
            + (trail_to - trail_from) * ease_in_back(span(t, self.trail_at, self.trail_len), 0.6);
        (lead, trail.min(lead))
    }
}

/// The sweep's parallelogram from trailing to leading edge.
pub(super) fn slanted_band(cr: &cairo::Context, trail: f64, lead: f64) {
    let (s, h) = (SWEEP_SLANT, H as f64);
    cr.move_to(trail + s, 0.0);
    cr.line_to(lead + s, 0.0);
    cr.line_to(lead, h);
    cr.line_to(trail, h);
    cr.close_path();
}

/// Where a slanted edge whose bottom is at `x` crosses row `y`.
pub(super) fn slanted_x(x: f64, y: f64) -> f64 {
    x + SWEEP_SLANT * (H as f64 - y) / H as f64
}

/// Into a space where lines of constant x follow the sweep's slant.
pub(super) fn slanted_space(cr: &cairo::Context) {
    let (s, h) = (SWEEP_SLANT, H as f64);
    cr.transform(cairo::Matrix::new(1.0, 0.0, -s / h, 1.0, s, 0.0));
}

/// A glowing line along a slanted edge.
pub(super) fn slanted_edge(cr: &cairo::Context, x: f64, core: f64, glow: f64, c: Rgb, a: f64) {
    let h = H as f64;
    if x + SWEEP_SLANT < -glow || x > W as f64 + glow {
        return;
    }
    let _ = cr.save();
    slanted_space(cr);
    let g = cairo::LinearGradient::new(x - glow, 0.0, x + glow, 0.0);
    stop(&g, 0.0, c, 0.0);
    stop(&g, 0.5, c, a * 0.55);
    stop(&g, 1.0, c, 0.0);
    let _ = cr.set_source(&g);
    cr.rectangle(x - glow, 0.0, 2.0 * glow, h);
    let _ = cr.fill();
    cr.set_source_rgba(1.0, 1.0, 1.0, a);
    cr.rectangle(x - core / 2.0, 0.0, core, h);
    let _ = cr.fill();
    let _ = cr.restore();
}

/// Classic: three slanted panels whoosh in from the left, the navy one
/// covering the whole frame with the Strom name on it, then peel away to the
/// right with the accents trailing.
pub(super) fn render_sweep(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let edges: Vec<(f64, f64)> = SWEEP_PANELS.iter().map(|p| p.edges(t)).collect();
    let dt = 0.004;
    let speed: Vec<(f64, f64)> = SWEEP_PANELS
        .iter()
        .zip(&edges)
        .map(|(p, e)| {
            let n = p.edges(t + dt);
            ((n.0 - e.0) / dt, (n.1 - e.1) / dt)
        })
        .collect();

    // Accent panels.
    let accents = [
        (hex(ULTRAVIOLET), hex(MAGENTA)),
        (hex(INDIGO), hex(ULTRAVIOLET)),
    ];
    for (i, (a, b)) in accents.iter().enumerate() {
        let (lead, trail) = edges[i];
        if lead <= trail {
            continue;
        }
        let g = cairo::LinearGradient::new(0.0, h, SWEEP_SLANT, 0.0);
        stop(&g, 0.0, *a, 1.0);
        stop(&g, 1.0, *b, 1.0);
        let _ = cr.set_source(&g);
        slanted_band(cr, trail, lead);
        let _ = cr.fill();
        // The panel in front shades this one where they meet.
        let (front_lead, front_trail) = edges[i + 1];
        if front_lead > front_trail {
            let _ = cr.save();
            slanted_band(cr, trail, lead);
            cr.clip();
            slanted_space(cr);
            for (x, dir) in [(front_lead, 1.0), (front_trail, -1.0)] {
                let g = cairo::LinearGradient::new(x, 0.0, x + 110.0 * dir, 0.0);
                g.add_color_stop_rgba(0.0, 0.0, 0.0, 0.05, 0.55);
                g.add_color_stop_rgba(1.0, 0.0, 0.0, 0.05, 0.0);
                let _ = cr.set_source(&g);
                cr.rectangle(x.min(x + 110.0 * dir), 0.0, 110.0, h);
                let _ = cr.fill();
            }
            let _ = cr.restore();
        }
    }

    // The covering panel.
    let (lead, trail) = edges[2];
    if lead > trail {
        let _ = cr.save();
        slanted_band(cr, trail, lead);
        cr.clip();
        let g = cairo::LinearGradient::new(0.0, h, w * 0.75, -h * 0.2);
        stop(&g, 0.0, hex(INK), 1.0);
        stop(&g, 0.55, hex(GRAPHITE), 1.0);
        stop(&g, 1.0, hex(INDIGO), 1.0);
        let _ = cr.set_source(&g);
        let _ = cr.paint();
        // A pool of light behind the name.
        let pool = cairo::RadialGradient::new(w * 0.5, h * 0.46, 0.0, w * 0.5, h * 0.46, w * 0.55);
        stop(&pool, 0.0, hex(ULTRAVIOLET), 0.4);
        stop(&pool, 1.0, hex(ULTRAVIOLET), 0.0);
        let _ = cr.set_source(&pool);
        let _ = cr.paint();
        // Fine pinstripes along the slant.
        let _ = cr.save();
        slanted_space(cr);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.045);
        let mut x = (trail / 24.0).floor() * 24.0;
        while x < lead {
            cr.rectangle(x, 0.0, 2.0, h);
            x += 24.0;
        }
        let _ = cr.fill();
        // Translucent bars drifting slower than the panel, for depth.
        for (k, (bw, a)) in [(180.0, 0.07), (60.0, 0.1), (320.0, 0.05)]
            .iter()
            .enumerate()
        {
            let bx = -300.0 + k as f64 * 640.0 + 700.0 * smooth(span(t, 0.1, 0.8));
            cr.set_source_rgba(1.0, 1.0, 1.0, *a * 0.7);
            cr.rectangle(bx, 0.0, *bw, h);
            let _ = cr.fill();
        }
        // A sheen crossing the panel while it holds.
        let sx = -SWEEP_SLANT + (w + 2.0 * SWEEP_SLANT) * smooth(span(t, 0.28, 0.5));
        let g = cairo::LinearGradient::new(sx - 260.0, 0.0, sx + 260.0, 0.0);
        g.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.0);
        g.add_color_stop_rgba(0.5, 1.0, 1.0, 1.0, 0.12);
        g.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
        let _ = cr.set_source(&g);
        cr.rectangle(sx - 260.0, 0.0, 520.0, h);
        let _ = cr.fill();
        let _ = cr.restore();
        draw_wordmark(cr, t);
        let _ = cr.restore();
    }

    // Edge lines with glow.
    let edge_colours = [hex(MAGENTA), hex(VOLT), (1.0, 1.0, 1.0)];
    for (i, &(lead, trail)) in edges.iter().enumerate() {
        if lead <= trail {
            continue;
        }
        let core = if i == 2 { 6.0 } else { 3.0 };
        slanted_edge(cr, lead, core, 70.0, edge_colours[i], 0.9);
        slanted_edge(cr, trail, core, 50.0, edge_colours[i], 0.7);
    }

    // Light streaks thrown off the moving edges, as long as they are fast.
    let mut rng = Rng::new(0x5743_4545_5000);
    for j in 0..56 {
        let panel = j % 3;
        let y = rng.range(0.0, h);
        let off = rng.range(-140.0, 220.0);
        let len_k = rng.range(0.06, 0.16);
        let thick = rng.range(2.5, 6.0);
        let bright = rng.range(0.35, 1.0);
        let use_trail = rng.unit() < 0.5;
        let (lead, trail) = edges[panel];
        if lead <= trail {
            continue;
        }
        let (x_edge, v) = if use_trail {
            (trail, speed[panel].1)
        } else {
            (lead, speed[panel].0)
        };
        let len = (v * len_k / 2.2).min(420.0);
        if len < 30.0 {
            continue;
        }
        let head = slanted_x(x_edge, y) + off;
        let tail = head - len;
        if head < 0.0 || tail > w {
            continue;
        }
        let c = edge_colours[panel];
        let a = bright * clamp01(len / 300.0);
        let g = cairo::LinearGradient::new(tail, y, head, y);
        stop(&g, 0.0, c, 0.0);
        stop(&g, 0.8, c, a * 0.7);
        stop(&g, 1.0, (1.0, 1.0, 1.0), a);
        let _ = cr.set_source(&g);
        cr.rectangle(tail, y - thick / 2.0, len, thick);
        let _ = cr.fill();
    }

    // A flare riding the covering panel's leading edge while it is fast.
    let fy = h * 0.42;
    let fx = slanted_x(edges[2].0, fy);
    let flare = clamp01(speed[2].0 / 9000.0) * inside(fx, -300.0, w + 300.0, 400.0);
    bloom(cr, fx, fy, 380.0, (0.45, 0.75, 1.0), 0.55 * flare);
    anamorphic(cr, fx, fy, 1300.0, (0.4, 0.7, 1.0), 0.8 * flare);
}

/// The Strom name on the covering panel: letters fly in one after another,
/// overshoot and settle, then shoot off to the right after a small pull
/// back. Extruded, glowing, with a gradient face and an accent bar under it.
pub(super) fn draw_wordmark(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let text = "STROM";
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(h * 0.24);
    let tracking = h * 0.012;
    let advances: Vec<f64> = text
        .chars()
        .map(|c| {
            cr.text_extents(&c.to_string())
                .map(|e| e.x_advance())
                .unwrap_or(0.0)
        })
        .collect();
    let total: f64 = advances.iter().sum::<f64>() + tracking * (advances.len() - 1) as f64;
    let Ok(all) = cr.text_extents(text) else {
        return;
    };
    let baseline = h * 0.5 - all.height() / 2.0 - all.y_bearing();
    let mut x = w / 2.0 - total / 2.0;
    for (i, c) in text.chars().enumerate() {
        // Right to left in, right to left out, so no letter passes another.
        let from_right = (text.len() - 1 - i) as f64;
        let enter = ease_out_back(span(t, 0.18 + 0.03 * from_right, 0.22));
        let leave = ease_in_back(span(t, 0.56 + 0.012 * from_right, 0.3), 0.9);
        let dx = -(1.0 - enter) * 650.0 + leave * 900.0 + 30.0 * span(t, 0.4, 0.2);
        if enter > 0.0 {
            let _ = cr.save();
            // Leaning forward, whatever the font.
            cr.transform(cairo::Matrix::new(1.0, 0.0, -0.2, 1.0, 0.2 * baseline, 0.0));
            cr.new_path();
            cr.move_to(x + dx, baseline);
            cr.text_path(&c.to_string());
            let path = cr.copy_path();
            cr.new_path();
            if let Ok(p) = &path {
                // Extrusion, back to front.
                let depth = 16;
                for d in (1..=depth).rev() {
                    let k = d as f64 / depth as f64;
                    let col = mix(hex(INDIGO), hex(INK), k);
                    let _ = cr.save();
                    cr.translate(d as f64 * 1.1, d as f64 * 1.5);
                    cr.append_path(p);
                    cr.set_source_rgb(col.0, col.1, col.2);
                    let _ = cr.fill();
                    let _ = cr.restore();
                }
                // Glow.
                cr.set_line_join(cairo::LineJoin::Round);
                for (lw, a) in [(46.0, 0.05), (26.0, 0.09), (12.0, 0.16)] {
                    cr.append_path(p);
                    cr.set_source_rgba(0.42, 0.17, 1.0, a);
                    cr.set_line_width(lw);
                    let _ = cr.stroke();
                }
                // Face.
                cr.append_path(p);
                let top = baseline + all.y_bearing();
                let g = cairo::LinearGradient::new(0.0, top, 0.0, baseline);
                g.add_color_stop_rgb(0.0, 1.0, 1.0, 1.0);
                g.add_color_stop_rgb(0.6, 0.95, 0.94, 0.98);
                g.add_color_stop_rgb(1.0, 0.78, 0.76, 0.86);
                let _ = cr.set_source(&g);
                let _ = cr.fill_preserve();
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.7);
                cr.set_line_width(2.0);
                let _ = cr.stroke();
            }
            let _ = cr.restore();
        }
        x += advances[i] + tracking;
    }

    // Accent bar: grows out with an overshoot, then is pulled away right.
    let grow = ease_out_back(span(t, 0.30, 0.2));
    let gone = ease_out_cubic(span(t, 0.56, 0.14));
    let left = w / 2.0 - total / 2.0 + total * gone;
    let right = w / 2.0 - total / 2.0 + total * grow;
    if right > left + 1.0 {
        let y = baseline + h * 0.06;
        for (pad, a) in [(18.0, 0.12), (8.0, 0.25)] {
            cr.set_source_rgba(1.0, 0.12, 0.43, a);
            cr.rectangle(
                left - pad,
                y - pad,
                right - left + 2.0 * pad,
                12.0 + 2.0 * pad,
            );
            let _ = cr.fill();
        }
        let g = cairo::LinearGradient::new(left, 0.0, right, 0.0);
        stop(&g, 0.0, hex(MAGENTA), 1.0);
        stop(&g, 1.0, hex(VOLT), 1.0);
        let _ = cr.set_source(&g);
        cr.rectangle(left, y, right - left, 12.0);
        let _ = cr.fill();
    }
}
