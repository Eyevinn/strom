//! Cairo overlay drawing and shared state for the vision mixer multiview.

use super::layout::OverlayLayout;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

/// Global registry of vision mixer overlay states, keyed by block instance ID.
/// Used by the API layer to access overlay state for preview/PGM updates.
fn overlay_states() -> &'static Mutex<HashMap<String, Arc<VisionMixerOverlayState>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, Arc<VisionMixerOverlayState>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register an overlay state for a block instance.
pub fn register_overlay_state(block_id: &str, state: Arc<VisionMixerOverlayState>) {
    if let Ok(mut map) = overlay_states().lock() {
        map.insert(block_id.to_string(), state);
    }
}

/// Get the overlay state for a block instance (if registered).
pub fn get_overlay_state(block_id: &str) -> Option<Arc<VisionMixerOverlayState>> {
    overlay_states().lock().ok()?.get(block_id).cloned()
}

/// Unregister the overlay state for a block instance (call on flow stop).
pub fn unregister_overlay_state(block_id: &str) {
    if let Ok(mut map) = overlay_states().lock() {
        map.remove(block_id);
    }
}

/// Shared state read by the cairooverlay draw callback.
///
/// Updated atomically from the API thread; read lock-free from the streaming thread.
pub struct VisionMixerOverlayState {
    /// Index of the current PGM input.
    pub pgm_input: AtomicUsize,
    /// Index of the current PVW input.
    pub pvw_input: AtomicUsize,
    /// Number of inputs.
    pub num_inputs: usize,
    /// Pre-computed layout (immutable after construction).
    pub layout: OverlayLayout,
    /// Input labels (set at build time, read-only after).
    pub labels: Vec<String>,
    /// Monotonic instant captured at construction for wall-clock derivation.
    instant_base: Instant,
    /// UTC seconds at `instant_base` + local timezone offset (seconds east of UTC).
    base_local_secs: u64,
}

impl VisionMixerOverlayState {
    pub fn new(
        num_inputs: usize,
        pgm_input: usize,
        pvw_input: usize,
        labels: Vec<String>,
        layout: OverlayLayout,
    ) -> Self {
        let now_sys = SystemTime::now();
        let now_instant = Instant::now();
        let utc_secs = now_sys
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Compute local timezone offset using libc localtime_r (one-time syscall at construction)
        let local_offset_secs = local_utc_offset_secs();
        let base_local_secs = (utc_secs as i64 + local_offset_secs) as u64;

        Self {
            pgm_input: AtomicUsize::new(pgm_input),
            pvw_input: AtomicUsize::new(pvw_input),
            num_inputs,
            layout,
            labels,
            instant_base: now_instant,
            base_local_secs,
        }
    }

    /// Get local wall-clock time as (hours, minutes, seconds).
    /// Uses Instant::now() (vDSO fast path) with a pre-computed offset.
    fn wall_clock_hms(&self) -> (u32, u32, u32) {
        let elapsed_secs = self.instant_base.elapsed().as_secs();
        let local_secs = self.base_local_secs + elapsed_secs;
        let secs_of_day = local_secs % 86400;
        let h = (secs_of_day / 3600) as u32;
        let m = ((secs_of_day % 3600) / 60) as u32;
        let s = (secs_of_day % 60) as u32;
        (h, m, s)
    }
}

/// Get local timezone offset in seconds east of UTC.
/// Called once at construction time — uses libc localtime_r.
fn local_utc_offset_secs() -> i64 {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

const PVW_R: f64 = 0.0;
const PVW_G: f64 = 0.8;
const PVW_B: f64 = 0.0;

const PGM_R: f64 = 0.9;
const PGM_G: f64 = 0.0;
const PGM_B: f64 = 0.0;

const GRAY: f64 = 0.5;

/// Helper to get text extents, returning (width, height) with a fallback.
fn text_size(cr: &cairo::Context, text: &str) -> (f64, f64) {
    match cr.text_extents(text) {
        Ok(ext) => (ext.width(), ext.height()),
        Err(_) => (text.len() as f64 * 8.0, 12.0), // rough fallback
    }
}

/// Draw a center-aligned text label with a filled background rectangle.
/// `cx` is the horizontal center, `y` is the text baseline.
#[allow(clippy::too_many_arguments)]
fn draw_label_centered(
    cr: &cairo::Context,
    text: &str,
    cx: f64,
    y: f64,
    bg_r: f64,
    bg_g: f64,
    bg_b: f64,
    bg_a: f64,
    pad_x: f64,
    pad_y: f64,
) {
    let (tw, th) = text_size(cr, text);
    let x = cx - tw / 2.0;
    cr.set_source_rgba(bg_r, bg_g, bg_b, bg_a);
    cr.rectangle(
        x - pad_x,
        y - th - pad_y,
        tw + pad_x * 2.0,
        th + pad_y * 2.0,
    );
    let _ = cr.fill();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.move_to(x, y);
    let _ = cr.show_text(text);
}

/// Draw the multiview overlay.
///
/// Called from the cairooverlay "draw" signal on the streaming thread.
pub fn draw_overlay(state: &VisionMixerOverlayState, cr: &cairo::Context) {
    let pgm = state.pgm_input.load(Ordering::Relaxed);
    let pvw = state.pvw_input.load(Ordering::Relaxed);
    let layout = &state.layout;

    // --- PVW large border ---
    cr.set_source_rgb(PVW_R, PVW_G, PVW_B);
    cr.set_line_width(layout.pvw_border_width);
    let r = &layout.pvw_rect;
    cr.rectangle(r.x, r.y, r.w, r.h);
    let _ = cr.stroke();

    // --- PGM large border ---
    cr.set_source_rgb(PGM_R, PGM_G, PGM_B);
    cr.set_line_width(layout.pgm_border_width);
    let r = &layout.pgm_rect;
    cr.rectangle(r.x, r.y, r.w, r.h);
    let _ = cr.stroke();

    // --- Thumbnail borders ---
    for i in 0..layout.num_inputs.min(layout.thumbnail_rects.len()) {
        let r = &layout.thumbnail_rects[i];
        if i == pgm {
            cr.set_source_rgb(PGM_R, PGM_G, PGM_B);
            cr.set_line_width(layout.thumb_border_width);
        } else if i == pvw {
            cr.set_source_rgb(PVW_R, PVW_G, PVW_B);
            cr.set_line_width(layout.thumb_border_width);
        } else {
            cr.set_source_rgb(GRAY, GRAY, GRAY);
            cr.set_line_width(1.0);
        }
        cr.rectangle(r.x, r.y, r.w, r.h);
        let _ = cr.stroke();
    }

    // --- Input labels on thumbnails ---
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(layout.label_font_size);

    for i in 0..layout.num_inputs.min(layout.label_positions.len()) {
        let pos = &layout.label_positions[i];
        draw_label_centered(
            cr,
            &state.labels[i],
            pos.x,
            pos.y,
            0.0,
            0.0,
            0.0,
            0.6,
            2.0,
            2.0,
        );
    }

    // --- PVW / PGM header labels ---
    cr.set_font_size(layout.header_font_size);

    draw_label_centered(
        cr,
        "PVW",
        layout.pvw_label_pos.x,
        layout.pvw_label_pos.y,
        PVW_R,
        PVW_G,
        PVW_B,
        0.7,
        4.0,
        2.0,
    );
    draw_label_centered(
        cr,
        "PGM",
        layout.pgm_label_pos.x,
        layout.pgm_label_pos.y,
        PGM_R,
        PGM_G,
        PGM_B,
        0.7,
        4.0,
        2.0,
    );

    // --- Clock ---
    let (h, m, s) = state.wall_clock_hms();
    let mut buf = [0u8; 8]; // "HH:MM:SS"
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (s / 10) as u8;
    buf[7] = b'0' + (s % 10) as u8;
    // SAFETY: buf contains only ASCII digits and colons
    let clock_str = unsafe { std::str::from_utf8_unchecked(&buf) };

    cr.set_font_size(layout.header_font_size * 0.8);
    let clock_cx = layout.canvas_width / 2.0;
    let clock_y = layout.header_font_size * 1.2;

    draw_label_centered(
        cr, clock_str, clock_cx, clock_y, 0.0, 0.0, 0.0, 0.7, 8.0, 4.0,
    );
}
