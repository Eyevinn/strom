//! Cairo overlay rendering and shared state for the vision mixer multiview.
//!
//! The overlay is rendered to a BGRA buffer and pushed via appsrc into the
//! multiview compositor as a separate input pad. The compositor composites it
//! in GPU/software as a texture at high zorder. Buffers are pushed at the
//! multiview framerate to keep the compositor fed; re-rendering only happens
//! when state changes (PGM/PVW switches, clock tick). Non-dirty frames
//! re-push the last pixel data in a zero-copy buffer (Arc refcount bump).

use super::layout::OverlayLayout;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};
use strom_types::vision_mixer::{self, TIMEZONE_REFRESH_SECS};
use tracing::{debug, warn};

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
    /// Packed PGM source group (up to 4 source indices). See `vision_mixer::pack_source_group`.
    pgm_group: AtomicU64,
    /// Packed PVW source group (up to 4 source indices). See `vision_mixer::pack_source_group`.
    pvw_group: AtomicU64,
    /// PiP currently shown on PGM (u64::MAX = PGM is an input group, not a PiP).
    /// Set at build time from `initial_pgm_source`; cleared/changed via runtime API.
    pgm_pip: AtomicU64,
    /// PiP currently shown on PVW (u64::MAX = PVW is an input group, not a PiP).
    pvw_pip: AtomicU64,
    /// Current PiP bg input (u64::MAX = no bg). One entry per configured PiP.
    pub pip_bg: Vec<AtomicU64>,
    /// Current PiP overlays (input indices). One Mutex<Vec> per configured PiP.
    pub pip_overlays: Vec<std::sync::Mutex<Vec<usize>>>,
    /// Number of configured PiP tiles (also `pip_bg.len()` / `pip_overlays.len()`).
    pub num_pips: usize,
    /// Number of inputs.
    pub num_inputs: usize,
    /// Whether Fade to Black is active.
    pub ftb_active: AtomicBool,
    /// Multiview overlay alpha (0.0–1.0), stored as f64 bits.
    overlay_alpha: AtomicU64,
    /// DSK enabled states (one per DSK input, max 4).
    pub dsk_enabled: Vec<AtomicBool>,
    /// Number of DSK inputs.
    pub num_dsk_inputs: usize,
    /// Pre-computed layout (immutable after construction).
    pub layout: OverlayLayout,
    /// Input labels (set at build time, read-only after).
    pub labels: Vec<String>,
    /// Monotonic instant captured at construction for wall-clock derivation.
    instant_base: Instant,
    /// UTC seconds at `instant_base` (no timezone offset applied).
    base_utc_secs: u64,
    /// Local timezone offset in seconds east of UTC. Refreshed periodically for DST changes.
    tz_offset_secs: AtomicI64,
    /// Timezone abbreviation packed as bytes (up to 7 ASCII chars + 1 length byte in MSB).
    tz_abbr_packed: AtomicU64,
    /// Elapsed seconds (from instant_base) when we next refresh timezone info.
    tz_next_refresh: AtomicU64,
    /// Whether VU meters are rendered on the multiview overlay.
    show_vu_meters: AtomicBool,
    /// Quantized peak (0..255) per audio input; max of L/R. Drives the bar fill.
    pub input_peak: Vec<AtomicU8>,
    /// Quantized decay (0..255) per audio input; max of L/R. Drives the tick.
    pub input_decay: Vec<AtomicU8>,
    /// Quantized peak (0..255) for the PGM audio input; max of L/R.
    pub pgm_peak: AtomicU8,
    /// Quantized decay (0..255) for the PGM audio input; max of L/R.
    pub pgm_decay: AtomicU8,
}

/// Initial PiP runtime state passed to [`VisionMixerOverlayState::new`].
///
/// `pip_bgs` and `pip_overlays` must have length `num_pips`. `pgm_pip` / `pvw_pip`
/// are `Some(idx)` if the corresponding bus starts in PiP mode.
#[derive(Default)]
pub struct PipInitialState {
    pub num_pips: usize,
    pub pip_bgs: Vec<Option<usize>>,
    pub pip_overlays: Vec<Vec<usize>>,
    pub pgm_pip: Option<usize>,
    pub pvw_pip: Option<usize>,
}

/// Sentinel used in [`VisionMixerOverlayState::pgm_pip`] / `pvw_pip` for
/// "this bus is an input group, not a PiP".
pub const NO_PIP: u64 = u64::MAX;

impl VisionMixerOverlayState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        num_inputs: usize,
        num_dsk_inputs: usize,
        pgm_input: usize,
        pvw_input: usize,
        labels: Vec<String>,
        layout: OverlayLayout,
        show_vu_meters: bool,
        pip: PipInitialState,
    ) -> Self {
        let now_sys = SystemTime::now();
        let now_instant = Instant::now();
        let utc_secs = now_sys
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (offset_secs, tz_abbr) = local_tz_info();

        let pip_bg = (0..pip.num_pips)
            .map(|i| {
                let v = pip
                    .pip_bgs
                    .get(i)
                    .copied()
                    .unwrap_or(None)
                    .map(|x| x as u64)
                    .unwrap_or(NO_PIP);
                AtomicU64::new(v)
            })
            .collect();
        let pip_overlays = (0..pip.num_pips)
            .map(|i| std::sync::Mutex::new(pip.pip_overlays.get(i).cloned().unwrap_or_default()))
            .collect();

        Self {
            pgm_group: AtomicU64::new(vision_mixer::pack_single_source(pgm_input)),
            pvw_group: AtomicU64::new(vision_mixer::pack_single_source(pvw_input)),
            pgm_pip: AtomicU64::new(pip.pgm_pip.map(|x| x as u64).unwrap_or(NO_PIP)),
            pvw_pip: AtomicU64::new(pip.pvw_pip.map(|x| x as u64).unwrap_or(NO_PIP)),
            pip_bg,
            pip_overlays,
            num_pips: pip.num_pips,
            num_inputs,
            ftb_active: AtomicBool::new(false),
            overlay_alpha: AtomicU64::new(1.0f64.to_bits()),
            dsk_enabled: (0..num_dsk_inputs)
                .map(|_| AtomicBool::new(false))
                .collect(),
            num_dsk_inputs,
            layout,
            labels,
            instant_base: now_instant,
            base_utc_secs: utc_secs,
            tz_offset_secs: AtomicI64::new(offset_secs),
            tz_abbr_packed: AtomicU64::new(pack_tz_abbr(&tz_abbr)),
            tz_next_refresh: AtomicU64::new(TIMEZONE_REFRESH_SECS),
            show_vu_meters: AtomicBool::new(show_vu_meters),
            input_peak: (0..num_inputs).map(|_| AtomicU8::new(0)).collect(),
            input_decay: (0..num_inputs).map(|_| AtomicU8::new(0)).collect(),
            pgm_peak: AtomicU8::new(0),
            pgm_decay: AtomicU8::new(0),
        }
    }

    /// Returns the PiP index currently displayed on PGM, or `None` if PGM
    /// is an input group.
    pub fn pgm_pip(&self) -> Option<usize> {
        let v = self.pgm_pip.load(Ordering::Relaxed);
        if v == NO_PIP {
            None
        } else {
            Some(v as usize)
        }
    }

    /// Returns the PiP index currently displayed on PVW, or `None` if PVW
    /// is an input group.
    pub fn pvw_pip(&self) -> Option<usize> {
        let v = self.pvw_pip.load(Ordering::Relaxed);
        if v == NO_PIP {
            None
        } else {
            Some(v as usize)
        }
    }

    /// Set PGM to be a PiP (or clear it back to input-group mode with `None`).
    pub fn set_pgm_pip(&self, pip_idx: Option<usize>) {
        self.pgm_pip.store(
            pip_idx.map(|x| x as u64).unwrap_or(NO_PIP),
            Ordering::Relaxed,
        );
    }

    /// Set PVW to be a PiP (or clear it back to input-group mode with `None`).
    pub fn set_pvw_pip(&self, pip_idx: Option<usize>) {
        self.pvw_pip.store(
            pip_idx.map(|x| x as u64).unwrap_or(NO_PIP),
            Ordering::Relaxed,
        );
    }

    /// Packed PGM-pip atomic value (for dirty checking).
    pub fn pgm_pip_packed(&self) -> u64 {
        self.pgm_pip.load(Ordering::Relaxed)
    }

    /// Packed PVW-pip atomic value (for dirty checking).
    pub fn pvw_pip_packed(&self) -> u64 {
        self.pvw_pip.load(Ordering::Relaxed)
    }

    /// Get the bg input for a configured PiP (None if PiP doesn't exist or has no bg).
    pub fn pip_bg_input(&self, pip_idx: usize) -> Option<usize> {
        let v = self.pip_bg.get(pip_idx)?.load(Ordering::Relaxed);
        if v == NO_PIP {
            None
        } else {
            Some(v as usize)
        }
    }

    /// Set the bg input for a configured PiP.
    pub fn set_pip_bg_input(&self, pip_idx: usize, input: Option<usize>) {
        if let Some(slot) = self.pip_bg.get(pip_idx) {
            slot.store(input.map(|x| x as u64).unwrap_or(NO_PIP), Ordering::Relaxed);
        }
    }

    /// Get the overlay input list for a configured PiP (empty if PiP doesn't exist).
    pub fn pip_overlay_inputs(&self, pip_idx: usize) -> Vec<usize> {
        self.pip_overlays
            .get(pip_idx)
            .and_then(|m| m.lock().ok().map(|v| v.clone()))
            .unwrap_or_default()
    }

    /// Replace the overlay input list for a configured PiP.
    pub fn set_pip_overlay_inputs(&self, pip_idx: usize, overlays: Vec<usize>) {
        if let Some(slot) = self.pip_overlays.get(pip_idx) {
            if let Ok(mut v) = slot.lock() {
                *v = overlays;
            }
        }
    }

    /// Whether VU meters should be rendered.
    pub fn show_vu_meters(&self) -> bool {
        self.show_vu_meters.load(Ordering::Relaxed)
    }

    /// Toggle VU meter rendering.
    pub fn set_show_vu_meters(&self, show: bool) {
        self.show_vu_meters.store(show, Ordering::Relaxed);
    }

    /// Update a single input's peak + decay from the level handler (max of L/R).
    pub fn set_input_levels(&self, index: usize, peak_db: f64, decay_db: f64) {
        if let (Some(peak_slot), Some(decay_slot)) =
            (self.input_peak.get(index), self.input_decay.get(index))
        {
            peak_slot.store(vision_mixer::quantize_db_to_u8(peak_db), Ordering::Relaxed);
            decay_slot.store(vision_mixer::quantize_db_to_u8(decay_db), Ordering::Relaxed);
        }
    }

    /// Update the PGM audio peak + decay.
    pub fn set_pgm_levels(&self, peak_db: f64, decay_db: f64) {
        self.pgm_peak
            .store(vision_mixer::quantize_db_to_u8(peak_db), Ordering::Relaxed);
        self.pgm_decay
            .store(vision_mixer::quantize_db_to_u8(decay_db), Ordering::Relaxed);
    }

    /// Get the PGM source group as a Vec of indices.
    pub fn pgm_group(&self) -> Vec<usize> {
        vision_mixer::unpack_source_group(self.pgm_group.load(Ordering::Relaxed))
    }

    /// Get the PVW source group as a Vec of indices.
    pub fn pvw_group(&self) -> Vec<usize> {
        vision_mixer::unpack_source_group(self.pvw_group.load(Ordering::Relaxed))
    }

    /// Get the packed PGM group value (for atomic comparison).
    pub fn pgm_group_packed(&self) -> u64 {
        self.pgm_group.load(Ordering::Relaxed)
    }

    /// Get the packed PVW group value (for atomic comparison).
    pub fn pvw_group_packed(&self) -> u64 {
        self.pvw_group.load(Ordering::Relaxed)
    }

    /// Get first PGM source index (backward compat).
    pub fn pgm_first(&self) -> usize {
        vision_mixer::group_first(self.pgm_group.load(Ordering::Relaxed))
    }

    /// Get first PVW source index (backward compat).
    pub fn pvw_first(&self) -> usize {
        vision_mixer::group_first(self.pvw_group.load(Ordering::Relaxed))
    }

    /// Set the PGM source group.
    pub fn set_pgm_group(&self, indices: &[usize]) {
        self.pgm_group
            .store(vision_mixer::pack_source_group(indices), Ordering::Relaxed);
    }

    /// Set the PVW source group.
    pub fn set_pvw_group(&self, indices: &[usize]) {
        self.pvw_group
            .store(vision_mixer::pack_source_group(indices), Ordering::Relaxed);
    }

    /// Get the multiview overlay alpha (0.0–1.0).
    pub fn overlay_alpha(&self) -> f64 {
        f64::from_bits(self.overlay_alpha.load(Ordering::Relaxed))
    }

    /// Set the multiview overlay alpha (0.0–1.0).
    pub fn set_overlay_alpha(&self, alpha: f64) {
        self.overlay_alpha.store(alpha.to_bits(), Ordering::Relaxed);
    }

    /// Get local wall-clock time as (hours, minutes, seconds) and timezone abbreviation.
    /// Uses Instant::now() (vDSO fast path) with a cached offset that refreshes every 60s.
    fn wall_clock_hms(&self) -> (u32, u32, u32) {
        let elapsed_secs = self.instant_base.elapsed().as_secs();

        // Refresh timezone info periodically (handles DST transitions)
        let next_refresh = self.tz_next_refresh.load(Ordering::Relaxed);
        if elapsed_secs >= next_refresh {
            let (offset, abbr) = local_tz_info();
            self.tz_offset_secs.store(offset, Ordering::Relaxed);
            self.tz_abbr_packed
                .store(pack_tz_abbr(&abbr), Ordering::Relaxed);
            self.tz_next_refresh
                .store(elapsed_secs + TIMEZONE_REFRESH_SECS, Ordering::Relaxed);
        }

        let offset = self.tz_offset_secs.load(Ordering::Relaxed);
        let utc_secs = self.base_utc_secs + elapsed_secs;
        let local_secs = (utc_secs as i64 + offset) as u64;
        let secs_of_day = local_secs % 86400;
        let h = (secs_of_day / 3600) as u32;
        let m = ((secs_of_day % 3600) / 60) as u32;
        let s = (secs_of_day % 60) as u32;
        (h, m, s)
    }

    /// Unpack the cached timezone abbreviation into a stack buffer.
    /// Returns the number of valid bytes written.
    fn tz_abbr_bytes(&self, out: &mut [u8; 7]) -> usize {
        let packed = self.tz_abbr_packed.load(Ordering::Relaxed);
        let len = ((packed >> 56) & 0x7F) as usize;
        let bytes = packed.to_le_bytes();
        let n = len.min(7);
        out[..n].copy_from_slice(&bytes[..n]);
        n
    }
}

/// Get local timezone offset in seconds east of UTC and the timezone abbreviation.
fn local_tz_info() -> (i64, String) {
    let now = chrono::Local::now();
    let offset_secs = now.offset().local_minus_utc() as i64;
    let abbr = now.format("%Z").to_string();
    (offset_secs, abbr)
}

/// Pack a timezone abbreviation (up to 7 ASCII bytes) into a u64.
/// Layout: bits 63..56 = length, bits 55..0 = bytes in little-endian order.
fn pack_tz_abbr(abbr: &str) -> u64 {
    let bytes = abbr.as_bytes();
    let len = bytes.len().min(7);
    let mut le = [0u8; 8];
    le[..len].copy_from_slice(&bytes[..len]);
    let val = u64::from_le_bytes(le);
    val | ((len as u64) << 56)
}

// Colors are R↔B swapped: cairo stores BGRA in memory, but we output as RGBA
// without byte-swapping. So we feed cairo (B,G,R) where we want (R,G,B) output.
const PVW_R: f64 = 0.0; // actually fed to cairo B channel → outputs as R=0
const PVW_G: f64 = 1.0;
const PVW_B: f64 = 0.0; // actually fed to cairo R channel → outputs as B=0

const PGM_R: f64 = 0.0; // want R=1.0 in output → feed to cairo B channel
const PGM_G: f64 = 0.0;
const PGM_B: f64 = 1.0; // want B=0 in output → feed to cairo R channel

// Yellow for background indicator: want output R=1.0, G=0.8, B=0

const GRAY: f64 = 0.5;

/// Draw a vertical VU meter in the bottom-left of `container`.
///
/// The bar fills from the bottom up; the fill height is proportional to the
/// quantized peak value. A thin white line marks the decay (slower-moving
/// peak indicator). Bar color goes green → yellow → red following the
/// VU_METER_*_DB thresholds.
fn draw_vu_meter(
    cr: &cairo::Context,
    container: &super::layout::Rect,
    peak: u8,
    decay: u8,
    scale: f64,
) {
    // Size: roughly a quarter of the container height, pinned to the bottom-left
    // with a comfortable margin on the left and bottom.
    let margin = 8.0 * scale;
    let bar_h = (container.h * 0.25).max(10.0);
    let bar_w = (container.w * 0.025).clamp(3.0 * scale, 10.0 * scale);
    let x = container.x + margin;
    let y = container.y + container.h - margin - bar_h;

    // Dark translucent background rectangle.
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
    cr.rectangle(x, y, bar_w, bar_h);
    let _ = cr.fill();

    if peak > 0 {
        let fill_frac = vision_mixer::u8_to_meter_fraction(peak);
        let fill_h = bar_h * fill_frac;
        // Color by peak level. The R/B channels are swapped here because the
        // overlay surface is BGRA in memory but emitted as RGBA (see comment
        // on the PVW/PGM color constants).
        let peak_db = db_from_u8(peak);
        let (cr_r, cr_g, cr_b) = if peak_db >= vision_mixer::VU_METER_RED_DB {
            (0.0, 0.0, 1.0) // red output
        } else if peak_db >= vision_mixer::VU_METER_YELLOW_DB {
            (0.0, 0.9, 1.0) // yellow output
        } else {
            (0.0, 0.9, 0.0) // green output
        };
        cr.set_source_rgba(cr_r, cr_g, cr_b, 0.9);
        cr.rectangle(x, y + bar_h - fill_h, bar_w, fill_h);
        let _ = cr.fill();
    }

    // Decay tick — a short horizontal line at the decay position, lagging
    // behind the peak to give a classic held-peak indicator.
    if decay > 0 {
        let decay_frac = vision_mixer::u8_to_meter_fraction(decay);
        let decay_y = y + bar_h - bar_h * decay_frac;
        let tick_h = (1.5 * scale).max(1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.rectangle(x, decay_y - tick_h / 2.0, bar_w, tick_h);
        let _ = cr.fill();
    }

    // Thin outline for legibility.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
    cr.set_line_width((0.75 * scale).max(0.5));
    cr.rectangle(x, y, bar_w, bar_h);
    let _ = cr.stroke();
}

/// Inverse of quantize_db_to_u8: approximate dBFS for color-band selection.
fn db_from_u8(v: u8) -> f64 {
    let frac = vision_mixer::u8_to_meter_fraction(v);
    vision_mixer::VU_METER_MIN_DB
        + frac * (vision_mixer::VU_METER_MAX_DB - vision_mixer::VU_METER_MIN_DB)
}

/// Compute a cheap hash of all per-input + PGM meter values so the dirty check
/// can detect meaningful level changes without re-rendering on every push.
fn hash_meters(state: &VisionMixerOverlayState) -> u64 {
    let mut h: u64 = 0x9E3779B97F4A7C15;
    for slot in &state.input_peak {
        h = h
            .rotate_left(7)
            .wrapping_add(slot.load(Ordering::Relaxed) as u64);
    }
    for slot in &state.input_decay {
        h = h
            .rotate_left(7)
            .wrapping_add(slot.load(Ordering::Relaxed) as u64);
    }
    h = h
        .rotate_left(7)
        .wrapping_add(state.pgm_peak.load(Ordering::Relaxed) as u64);
    h = h
        .rotate_left(7)
        .wrapping_add(state.pgm_decay.load(Ordering::Relaxed) as u64);
    h
}

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

// ============================================================================
// Overlay renderer (appsrc-based)
// ============================================================================

/// Renders the multiview overlay to BGRA buffers and pushes them via appsrc.
///
/// Pushes at the multiview framerate so the compositor always has a current
/// buffer on the overlay pad. Only re-renders when state actually changes;
/// otherwise re-pushes the last pixel data in a new zero-copy buffer.
pub struct OverlayRenderer {
    pub appsrc: gst_app::AppSrc,
    caps: gst::Caps,
    state: Arc<VisionMixerOverlayState>,
    width: i32,
    height: i32,
    surface: Option<cairo::ImageSurface>,
    /// Last rendered pixel data, shared via Arc so repush can wrap it in a
    /// new GstBuffer without copying the pixel bytes (only Arc refcount bump).
    last_overlay_data: Option<Arc<[u8]>>,
    last_pgm: u64,
    last_pvw: u64,
    last_ftb: bool,
    last_clock_secs: u64,
    /// Meter state hash from the last render; used to avoid re-rendering when
    /// quantized levels haven't changed. Recomputed each tick when meters are on.
    last_meters_hash: u64,
    /// Whether meters were on at the last render.
    last_show_vu: bool,
    /// Previous PGM-on-PiP index packed (NO_PIP if PGM was an input group).
    last_pgm_pip: u64,
    /// Previous PVW-on-PiP index packed (NO_PIP if PVW was an input group).
    last_pvw_pip: u64,
}

// SAFETY: OverlayRenderer is accessed via Mutex from the timer thread and API
// thread. Cairo surfaces are not Send/Sync but exclusive Mutex access is safe.
unsafe impl Send for OverlayRenderer {}
unsafe impl Sync for OverlayRenderer {}

impl OverlayRenderer {
    pub fn new(
        appsrc: gst_app::AppSrc,
        caps: gst::Caps,
        state: Arc<VisionMixerOverlayState>,
        width: i32,
        height: i32,
    ) -> Self {
        Self {
            appsrc,
            caps,
            state,
            width,
            height,
            surface: None,
            last_overlay_data: None,
            last_pgm: u64::MAX,
            last_pvw: u64::MAX,
            last_ftb: false,
            last_clock_secs: u64::MAX,
            last_meters_hash: u64::MAX,
            last_show_vu: false,
            last_pgm_pip: u64::MAX - 2,
            last_pvw_pip: u64::MAX - 2,
        }
    }

    /// Render overlay if state changed, then push to appsrc.
    ///
    /// Always pushes a frame (re-pushing the last sample if nothing changed)
    /// so the multiview compositor has a steady stream of overlay buffers and
    /// does not stall waiting for the overlay pad.
    pub fn render_if_dirty(&mut self) -> bool {
        let pgm_packed = self.state.pgm_group_packed();
        let pvw_packed = self.state.pvw_group_packed();
        let ftb = self.state.ftb_active.load(Ordering::Relaxed);
        let pgm_pip_packed = self.state.pgm_pip_packed();
        let pvw_pip_packed = self.state.pvw_pip_packed();
        let (h, m, s) = self.state.wall_clock_hms();
        let clock_secs = h as u64 * 3600 + m as u64 * 60 + s as u64;
        let show_vu = self.state.show_vu_meters();
        let meters_hash = if show_vu { hash_meters(&self.state) } else { 0 };

        let dirty = self.last_pgm != pgm_packed
            || self.last_pvw != pvw_packed
            || self.last_ftb != ftb
            || self.last_pgm_pip != pgm_pip_packed
            || self.last_pvw_pip != pvw_pip_packed
            || self.last_clock_secs != clock_secs
            || self.last_show_vu != show_vu
            || (show_vu && self.last_meters_hash != meters_hash);

        if dirty {
            let pgm_group = vision_mixer::unpack_source_group(pgm_packed);
            let pvw_group = vision_mixer::unpack_source_group(pvw_packed);

            let t0 = std::time::Instant::now();
            let pushed = self.push_frame(&pgm_group, &pvw_group, ftb, h, m, s);
            let elapsed = t0.elapsed();
            debug!(
                "Overlay render+push: {:.1}ms (pgm={:?}, pvw={:?}, ftb={}, pushed={})",
                elapsed.as_secs_f64() * 1000.0,
                pgm_group,
                pvw_group,
                ftb,
                pushed
            );

            if pushed {
                self.last_pgm = pgm_packed;
                self.last_pvw = pvw_packed;
                self.last_ftb = ftb;
                self.last_pgm_pip = pgm_pip_packed;
                self.last_pvw_pip = pvw_pip_packed;
                self.last_clock_secs = clock_secs;
                self.last_show_vu = show_vu;
                self.last_meters_hash = meters_hash;
            }
            pushed
        } else {
            // Not dirty — re-push the last sample to keep the compositor fed
            self.repush_last_sample()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_frame(
        &mut self,
        pgm_group: &[usize],
        pvw_group: &[usize],
        ftb: bool,
        h: u32,
        m: u32,
        s: u32,
    ) -> bool {
        let t0 = Instant::now();

        // Reuse or create cairo surface
        let mut surface = self
            .surface
            .take()
            .filter(|s| s.width() == self.width && s.height() == self.height)
            .unwrap_or_else(|| {
                cairo::ImageSurface::create(cairo::Format::ARgb32, self.width, self.height)
                    .expect("failed to create overlay surface")
            });

        let t_surface = t0.elapsed();

        // Clear to transparent and render overlay content
        {
            let cr = cairo::Context::new(&surface).expect("failed to create overlay cairo context");
            cr.set_operator(cairo::Operator::Clear);
            let _ = cr.paint();
            cr.set_operator(cairo::Operator::Over);
            render_overlay(&self.state, &cr, pgm_group, pvw_group, ftb, h, m, s);
        }

        let t_cairo = t0.elapsed();

        // Copy pixel data into a GstBuffer
        let row_bytes = self.width as usize * 4;
        let buf_size = row_bytes * self.height as usize;
        let cairo_stride = surface.stride() as usize;

        let pushed = (|| -> Option<()> {
            let data = surface.data().ok()?;
            // Copy cairo pixel data into a Vec, then share via Arc<[u8]> so
            // repush_last_sample can wrap it in a new buffer without copying.
            let mut pixel_data = vec![0u8; buf_size];
            // No R↔B swap needed — render_overlay uses swapped colors so that
            // cairo's BGRA memory layout produces correct RGBA output directly.
            if cairo_stride == row_bytes {
                pixel_data[..buf_size].copy_from_slice(&data[..buf_size]);
            } else {
                for y in 0..self.height as usize {
                    let src = y * cairo_stride;
                    let d = y * row_bytes;
                    pixel_data[d..d + row_bytes]
                        .copy_from_slice(&data[src..src + row_bytes]);
                }
            }

            let shared_data: Arc<[u8]> = pixel_data.into();
            self.last_overlay_data = Some(shared_data.clone());

            let t_copy = t0.elapsed();

            // do-timestamp=true on the appsrc sets PTS to the current
            // pipeline running time automatically. Do NOT set PTS=0 here —
            // that makes the compositor see the overlay as perpetually stale,
            // causing it to wait up to its full deadline on every frame.

            // Buffer wraps the Arc'd data (no copy). Buffer refcount is 1 so
            // BaseSrc can set PTS via do-timestamp without triggering a copy.
            let buffer = gst::Buffer::from_slice(shared_data);
            let sample = gst::Sample::builder()
                .buffer(&buffer)
                .caps(&self.caps)
                .build();
            self.appsrc.push_sample(&sample).ok()?;

            let t_push = t0.elapsed();

            debug!(
                "Overlay breakdown: surface={:.1}ms cairo={:.1}ms copy={:.1}ms push={:.1}ms total={:.1}ms ({}x{})",
                t_surface.as_secs_f64() * 1000.0,
                (t_cairo - t_surface).as_secs_f64() * 1000.0,
                (t_copy - t_cairo).as_secs_f64() * 1000.0,
                (t_push - t_copy).as_secs_f64() * 1000.0,
                t_push.as_secs_f64() * 1000.0,
                self.width, self.height
            );

            Some(())
        })()
        .is_some();

        self.surface = Some(surface);
        pushed
    }

    /// Re-push the last overlay frame without re-rendering.
    ///
    /// Creates a new GstBuffer wrapping the shared pixel data (Arc refcount
    /// bump only — no pixel copy). The new buffer has refcount=1, so BaseSrc's
    /// do-timestamp can set PTS without triggering make_writable copies.
    fn repush_last_sample(&self) -> bool {
        if let Some(ref data) = self.last_overlay_data {
            let buffer = gst::Buffer::from_slice(data.clone());
            let sample = gst::Sample::builder()
                .buffer(&buffer)
                .caps(&self.caps)
                .build();
            self.appsrc.push_sample(&sample).is_ok()
        } else {
            false
        }
    }
}

/// Global registry of overlay renderers, keyed by block instance ID.
fn overlay_renderers() -> &'static Mutex<HashMap<String, Arc<Mutex<OverlayRenderer>>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, Arc<Mutex<OverlayRenderer>>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_overlay_renderer(block_id: &str, renderer: Arc<Mutex<OverlayRenderer>>) {
    if let Ok(mut map) = overlay_renderers().lock() {
        map.insert(block_id.to_string(), renderer);
    }
}

pub fn get_overlay_renderer(block_id: &str) -> Option<Arc<Mutex<OverlayRenderer>>> {
    overlay_renderers().lock().ok()?.get(block_id).cloned()
}

pub fn unregister_overlay_renderer(block_id: &str) {
    if let Ok(mut map) = overlay_renderers().lock() {
        map.remove(block_id);
    }
}

/// Trigger an immediate overlay re-render (called from API on state changes).
pub fn trigger_overlay_update(block_id: &str) {
    if let Some(renderer) = get_overlay_renderer(block_id) {
        if let Ok(mut r) = renderer.lock() {
            let pushed = r.render_if_dirty();
            debug!(
                "Overlay trigger for {}: pushed={}",
                &block_id[..8.min(block_id.len())],
                pushed
            );
        } else {
            warn!(
                "Overlay trigger: mutex poisoned for {}",
                &block_id[..8.min(block_id.len())]
            );
        }
    } else {
        warn!("Overlay trigger: no renderer found for {}", block_id);
    }
}

/// Start the overlay push timer.
///
/// Pushes at the multiview framerate so the compositor always has a current
/// buffer on the overlay pad. Only re-renders when state actually changes
/// (PGM/PVW switch, clock tick); otherwise re-pushes the last sample.
/// The thread stops when the renderer is unregistered (flow stop).
pub fn start_overlay_timer(
    block_id: String,
    renderer: Arc<Mutex<OverlayRenderer>>,
    mv_framerate: (i32, i32),
) {
    let frame_interval = std::time::Duration::from_nanos(
        (mv_framerate.1 as u64 * 1_000_000_000) / mv_framerate.0.max(1) as u64,
    );
    std::thread::Builder::new()
        .name(format!(
            "overlay-timer-{}",
            &block_id[..8.min(block_id.len())]
        ))
        .spawn(move || {
            debug!("Overlay timer started for {}", block_id);
            // Wait for pipeline to reach PLAYING before pushing first frame.
            // The appsrc needs caps negotiation to complete first.
            let ready = loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if get_overlay_renderer(&block_id).is_none() {
                    break false;
                }
                if let Ok(r) = renderer.lock() {
                    if r.appsrc.current_state() == gst::State::Playing {
                        break true;
                    }
                }
            };
            if !ready {
                debug!("Overlay timer exiting early (renderer unregistered)");
                return;
            }
            debug!("Overlay appsrc PLAYING, pushing initial frame");
            if let Ok(mut r) = renderer.lock() {
                r.render_if_dirty();
            }
            // Deadline-based loop: advance by frame_interval per tick so that
            // render/push time doesn't accumulate as drift.
            let mut next_tick = std::time::Instant::now();
            loop {
                next_tick += frame_interval;
                let now = std::time::Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                } else if now - next_tick > frame_interval {
                    // Fell behind by more than one frame (mutex contention,
                    // system sleep, etc.) — skip missed ticks instead of
                    // spinning a catch-up burst.
                    next_tick = now;
                }
                if get_overlay_renderer(&block_id).is_none() {
                    debug!("Overlay timer stopping for {}", block_id);
                    break;
                }
                if let Ok(mut r) = renderer.lock() {
                    r.render_if_dirty();
                }
            }
        })
        .unwrap_or_else(|e| {
            warn!("Failed to start overlay timer: {}", e);
            // Return a dummy handle — the overlay just won't update the clock
            std::thread::spawn(|| {})
        });
}

/// Render overlay content to a cairo context (called only when state changes).
#[allow(clippy::too_many_arguments)]
fn render_overlay(
    state: &VisionMixerOverlayState,
    cr: &cairo::Context,
    pgm_group: &[usize],
    pvw_group: &[usize],
    ftb: bool,
    h: u32,
    m: u32,
    s: u32,
) {
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

    // --- Thumbnail borders (drawn around full slot including label area) ---
    for i in 0..layout.num_inputs.min(layout.thumbnail_slot_rects.len()) {
        let r = &layout.thumbnail_slot_rects[i];
        if pgm_group.contains(&i) {
            cr.set_source_rgb(PGM_R, PGM_G, PGM_B);
        } else if pvw_group.contains(&i) {
            cr.set_source_rgb(PVW_R, PVW_G, PVW_B);
        } else {
            cr.set_source_rgb(GRAY, GRAY, GRAY);
        }
        cr.set_line_width(layout.thumb_border_width);
        cr.rectangle(r.x, r.y, r.w, r.h);
        let _ = cr.stroke();
    }

    // --- PiP tile borders ---
    // Mirror the thumbnail color scheme: red if this PiP is on PGM, green if on
    // PVW, gray otherwise. PGM wins over PVW if both somehow apply.
    let pgm_pip_idx = state.pgm_pip();
    let pvw_pip_idx = state.pvw_pip();
    for (i, r) in layout
        .pip_tile_slot_rects
        .iter()
        .take(layout.num_pips)
        .enumerate()
    {
        if pgm_pip_idx == Some(i) {
            cr.set_source_rgb(PGM_R, PGM_G, PGM_B);
        } else if pvw_pip_idx == Some(i) {
            cr.set_source_rgb(PVW_R, PVW_G, PVW_B);
        } else {
            cr.set_source_rgb(GRAY, GRAY, GRAY);
        }
        cr.set_line_width(layout.thumb_border_width);
        cr.rectangle(r.x, r.y, r.w, r.h);
        let _ = cr.stroke();
    }

    // --- VU meters ---
    if state.show_vu_meters() {
        // Per-input meters: bottom-left of each thumbnail video rect.
        for i in 0..layout.num_inputs.min(layout.thumbnail_rects.len()) {
            let r = &layout.thumbnail_rects[i];
            let peak = state
                .input_peak
                .get(i)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0);
            let decay = state
                .input_decay
                .get(i)
                .map(|v| v.load(Ordering::Relaxed))
                .unwrap_or(0);
            draw_vu_meter(cr, r, peak, decay, layout.scale);
        }

        // PVW meters: one per source in the group, each drawn in the
        // bottom-left of its sub-tile. Sub-tiles follow the 1/2/3/4-source
        // layout from `compute_group_sub_rects`.
        if !pvw_group.is_empty() {
            let rects = super::layout::compute_group_sub_rects(&layout.pvw_rect, pvw_group.len());
            for (tile_rect, &src_idx) in rects.iter().zip(pvw_group.iter()) {
                if let (Some(peak_slot), Some(decay_slot)) = (
                    state.input_peak.get(src_idx),
                    state.input_decay.get(src_idx),
                ) {
                    let peak = peak_slot.load(Ordering::Relaxed);
                    let decay = decay_slot.load(Ordering::Relaxed);
                    draw_vu_meter(cr, tile_rect, peak, decay, layout.scale);
                }
            }
        }

        // PGM meters:
        //  - single source: one meter in the full PGM rect, driven by the
        //    dedicated pgm_audio_in port (master PGM mix from the audio mixer).
        //  - multi-source group: per-tile meters, each showing the
        //    corresponding source's own audio input.
        if pgm_group.len() <= 1 {
            let pgm_peak = state.pgm_peak.load(Ordering::Relaxed);
            let pgm_decay = state.pgm_decay.load(Ordering::Relaxed);
            draw_vu_meter(cr, &layout.pgm_rect, pgm_peak, pgm_decay, layout.scale);
        } else {
            let rects = super::layout::compute_group_sub_rects(&layout.pgm_rect, pgm_group.len());
            for (tile_rect, &src_idx) in rects.iter().zip(pgm_group.iter()) {
                if let (Some(peak_slot), Some(decay_slot)) = (
                    state.input_peak.get(src_idx),
                    state.input_decay.get(src_idx),
                ) {
                    let peak = peak_slot.load(Ordering::Relaxed);
                    let decay = decay_slot.load(Ordering::Relaxed);
                    draw_vu_meter(cr, tile_rect, peak, decay, layout.scale);
                }
            }
        }
    }

    // --- Input labels on thumbnails ---
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(layout.label_font_size);

    let sc = layout.scale;
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
            2.0 * sc,
            2.0 * sc,
        );
    }

    // --- PiP tile labels (e.g. "PiP 1") ---
    for (i, pos) in layout
        .pip_label_positions
        .iter()
        .take(layout.num_pips)
        .enumerate()
    {
        let label = format!("PiP {}", i + 1);
        draw_label_centered(
            cr,
            &label,
            pos.x,
            pos.y,
            0.0,
            0.0,
            0.0,
            0.6,
            2.0 * sc,
            2.0 * sc,
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
        4.0 * sc,
        2.0 * sc,
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
        4.0 * sc,
        2.0 * sc,
    );

    // --- Clock ---
    // "HH:MM:SS ABCD" — 8 time chars + space + up to 7 tz chars = 16 max
    let mut buf = [b' '; 16];
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (s / 10) as u8;
    buf[7] = b'0' + (s % 10) as u8;
    let mut tz_buf = [0u8; 7];
    let tz_len = state.tz_abbr_bytes(&mut tz_buf);
    buf[9..9 + tz_len].copy_from_slice(&tz_buf[..tz_len]);
    let total_len = 9 + tz_len;
    // SAFETY: buf contains only ASCII digits, colons, spaces, and ASCII tz abbreviation
    let clock_str = unsafe { std::str::from_utf8_unchecked(&buf[..total_len]) };

    cr.set_font_size(layout.header_font_size * 0.8);
    let clock_cx = layout.canvas_width / 2.0;
    let clock_y = layout.header_font_size * 1.2;

    draw_label_centered(
        cr,
        clock_str,
        clock_cx,
        clock_y,
        0.0,
        0.0,
        0.0,
        0.7,
        8.0 * sc,
        4.0 * sc,
    );

    // --- FTB indicator ---
    if ftb {
        let r = &layout.pgm_rect;
        let ftb_cx = r.x + r.w / 2.0;
        let ftb_cy = r.y + r.h / 2.0;
        cr.set_font_size(layout.header_font_size * 2.0);
        draw_label_centered(
            cr,
            "FTB",
            ftb_cx,
            ftb_cy,
            0.0,
            0.0,
            0.8,
            0.8,
            12.0 * sc,
            6.0 * sc,
        );
    }
}
