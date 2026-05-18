//! Utility functions for dB/linear conversions and formatting.

use egui::{Color32, Painter, Rect, Stroke};

/// Format a linear fader value as dB string.
pub(super) fn format_db(linear: f32) -> String {
    if !linear.is_finite() || linear <= 0.001 {
        "-inf dB".to_string()
    } else {
        let db = 20.0 * linear.log10();
        if db <= -59.0 {
            "-inf dB".to_string()
        } else {
            format!("{:.1} dB", db)
        }
    }
}

/// Format a pan value as string.
pub(super) fn format_pan(pan: f32) -> String {
    if pan < -0.01 {
        format!("L{:.0}", (-pan * 100.0))
    } else if pan > 0.01 {
        format!("R{:.0}", (pan * 100.0))
    } else {
        "C".to_string()
    }
}

/// Map a dB value to a y-coordinate within a vertical range.
/// All faders, meters, and scales share this mapping for alignment.
/// Range: -60 dB at bottom (y_max - 5px) to +6 dB at top (y_min + 5px).
pub(super) fn db_to_y(db: f32, y_min: f32, y_max: f32) -> f32 {
    let db = if db.is_finite() { db } else { -60.0 };
    let normalized = ((db - (-60.0)) / 66.0).clamp(0.0, 1.0);
    let margin = 5.0;
    let usable = (y_max - y_min) - margin * 2.0;
    y_max - margin - normalized * usable
}

/// Meter zone boundaries in dB, matching the standalone VU meter block
/// and EBU/broadcast convention.
/// - Green:  -60 dB .. -18 dB (safe operational level)
/// - Yellow: -18 dB ..  -9 dB (loud but acceptable)
/// - Orange:  -9 dB ..  -6 dB (getting hot)
/// - Red:    -6 dB ..  +6 dB (clipping risk)
pub(super) const METER_ZONES_DB: [(f32, f32, Color32, Color32); 4] = [
    (
        -60.0,
        -18.0,
        Color32::from_rgb(0, 220, 0),
        Color32::from_rgb(0, 60, 0),
    ),
    (
        -18.0,
        -9.0,
        Color32::from_rgb(255, 220, 0),
        Color32::from_rgb(60, 60, 0),
    ),
    (
        -9.0,
        -6.0,
        Color32::from_rgb(255, 165, 0),
        Color32::from_rgb(60, 45, 0),
    ),
    (
        -6.0,
        6.0,
        Color32::from_rgb(255, 0, 0),
        Color32::from_rgb(60, 0, 0),
    ),
];

/// Draw the dim background sectors of a segmented level meter.
/// Sectors are positioned in dB using `db_to_y` so they align with the
/// shared dB scale (e.g. the fader).
pub(super) fn draw_meter_zones_background(painter: &Painter, rect: Rect) {
    for (zone_min_db, zone_max_db, _bright, dim) in METER_ZONES_DB {
        let y_top = db_to_y(zone_max_db, rect.min.y, rect.max.y);
        let y_bottom = db_to_y(zone_min_db, rect.min.y, rect.max.y);
        let zone_rect = Rect::from_min_max(
            egui::pos2(rect.min.x, y_top),
            egui::pos2(rect.max.x, y_bottom),
        );
        painter.rect(
            zone_rect,
            0.0,
            dim,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Light up the segmented level meter from the bottom up to `peak_db`.
/// Each zone keeps its own bright colour, so the visible color band reflects
/// the actual dB range — not just the highest reached level.
pub(super) fn draw_meter_zones_lit(painter: &Painter, rect: Rect, peak_db: f32) {
    let peak_db = if peak_db.is_finite() { peak_db } else { -60.0 };
    for (zone_min_db, zone_max_db, bright, _dim) in METER_ZONES_DB {
        if peak_db <= zone_min_db {
            break;
        }
        let lit_top_db = peak_db.min(zone_max_db);
        let y_top = db_to_y(lit_top_db, rect.min.y, rect.max.y);
        let y_bottom = db_to_y(zone_min_db, rect.min.y, rect.max.y);
        let lit_rect = Rect::from_min_max(
            egui::pos2(rect.min.x, y_top),
            egui::pos2(rect.max.x, y_bottom),
        );
        painter.rect(
            lit_rect,
            0.0,
            bright,
            Stroke::NONE,
            egui::epaint::StrokeKind::Inside,
        );
    }
}

/// Convert dB to linear scale (f64).
pub(super) fn db_to_linear_f64(db: f64) -> f64 {
    if !db.is_finite() {
        return 0.0;
    }
    10.0_f64.powf(db / 20.0)
}

/// Convert dB to linear scale (f32).
pub(super) fn db_to_linear_f32(db: f32) -> f32 {
    if !db.is_finite() || db <= -60.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

/// Convert linear to dB scale.
pub(super) fn linear_to_db(linear: f64) -> f64 {
    if !linear.is_finite() || linear <= 0.0001 {
        -60.0
    } else {
        20.0 * linear.log10()
    }
}

/// Convert a linear level (0.0-2.0) to a knob arc position (0.0-1.0).
///
/// dB-scaled: first half of arc = -60..0 dB, second half = 0..+6 dB.
/// This puts unity (0 dB, linear 1.0) at the center of the arc (12 o'clock).
pub(super) fn knob_linear_to_normalized(linear: f32) -> f32 {
    if !linear.is_finite() || linear <= 0.001 {
        return 0.0;
    }
    let db = 20.0 * linear.log10();
    if db <= -60.0 {
        0.0
    } else if db <= 0.0 {
        // -60..0 dB maps to 0.0..0.5
        0.5 * (db + 60.0) / 60.0
    } else {
        // 0..+6 dB maps to 0.5..1.0
        (0.5 + 0.5 * db / 6.0).min(1.0)
    }
}
