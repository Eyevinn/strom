//! PGM graphics overlay — mixer-state-driven decorations on the program
//! output (currently: zone borders).
//!
//! Borders are a function of the mixer's own live geometry: boxes move with
//! zone morphs, takes and punch-ins, all driven by control bindings on the
//! compositor pads. No external graphics source could stay in sync with
//! that, so the mixer draws them itself. External graphics (lower thirds,
//! bugs) stay on DSK — the overlay pad sits below the DSK stack.
//!
//! Rendering follows the multiview overlay pattern (cairo → appsrc → one
//! compositor pad), with one key difference: geometry is read **live from
//! the dist compositor's sink pads** each tick (property reads on the timer
//! thread — never a buffer probe), so borders track animations frame by
//! frame, fade with their box's alpha, and need no knowledge of the
//! transition engine.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tracing::{debug, warn};

use super::overlay::{self, VisionMixerOverlayState};

/// How far ahead of the compositor's current position border geometry is
/// evaluated. An overlay frame pushed now is composited 1–3 output frames
/// later; sampling the pads' *current* values would draw the border where
/// the box was, trailing it during morphs/takes. Instead the control
/// bindings driving the animation are evaluated at (position + lead) so the
/// border lands where the box will be at composite time.
pub(crate) const BORDER_LEAD_MS: u64 = 66;

/// Read a pad's geometry + alpha as they will be at `eval_t`: animated
/// properties are evaluated through their control bindings at that
/// timestamp; properties without a binding read their current value.
pub(crate) fn pad_geometry_at(
    pad: &gst::Pad,
    eval_t: Option<gst::ClockTime>,
) -> (i32, i32, i32, i32, f64) {
    fn int_at(pad: &gst::Pad, prop: &str, t: Option<gst::ClockTime>) -> i32 {
        if let (Some(t), Some(binding)) = (t, pad.control_binding(prop)) {
            if let Some(v) = gst::prelude::ControlBindingExt::value(&binding, t) {
                if let Ok(v) = v.get::<i32>() {
                    return v;
                }
            }
        }
        pad.property::<i32>(prop)
    }
    fn f64_at(pad: &gst::Pad, prop: &str, t: Option<gst::ClockTime>) -> f64 {
        if let (Some(t), Some(binding)) = (t, pad.control_binding(prop)) {
            if let Some(v) = gst::prelude::ControlBindingExt::value(&binding, t) {
                if let Ok(v) = v.get::<f64>() {
                    return v;
                }
            }
        }
        pad.property::<f64>(prop)
    }
    (
        int_at(pad, "xpos", eval_t),
        int_at(pad, "ypos", eval_t),
        int_at(pad, "width", eval_t),
        int_at(pad, "height", eval_t),
        f64_at(pad, "alpha", eval_t),
    )
}

/// One border rectangle to draw: the box's live rect, its style and the
/// box's current alpha (borders fade with their box).
#[derive(Debug, Clone, PartialEq)]
struct BorderDraw {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    width: f64,
    rgba: (f64, f64, f64, f64),
    pad_alpha: f64,
}

pub struct PgmOverlayRenderer {
    appsrc: gst_app::AppSrc,
    caps: gst::Caps,
    state: Arc<VisionMixerOverlayState>,
    /// The dist compositor, for live pad-geometry reads. Weak — the renderer
    /// is reachable from a global registry and must not keep the pipeline
    /// alive.
    mixer: glib::WeakRef<gst::Element>,
    width: i32,
    height: i32,
    surface: Option<cairo::ImageSurface>,
    /// Last rendered pixel data for cheap re-pushes (Arc bump, no copy).
    last_data: Option<Arc<[u8]>>,
    /// Hash of the last drawn border list (empty list hashes too) — geometry
    /// reads happen every tick, drawing only when something moved.
    last_hash: u64,
}

// SAFETY: accessed via Mutex from the timer thread and API thread. Cairo
// surfaces are not Send/Sync but exclusive Mutex access is safe — same
// pattern as the multiview OverlayRenderer.
unsafe impl Send for PgmOverlayRenderer {}
unsafe impl Sync for PgmOverlayRenderer {}

impl PgmOverlayRenderer {
    pub fn new(
        appsrc: gst_app::AppSrc,
        caps: gst::Caps,
        state: Arc<VisionMixerOverlayState>,
        mixer: glib::WeakRef<gst::Element>,
        width: i32,
        height: i32,
    ) -> Self {
        Self {
            appsrc,
            caps,
            state,
            mixer,
            width,
            height,
            surface: None,
            last_data: None,
            last_hash: u64::MAX,
        }
    }

    /// Collect the borders to draw from current state + live pad geometry.
    fn collect_borders(&self) -> Vec<BorderDraw> {
        let Some(p) = self.state.pgm_pip() else {
            return Vec::new();
        };
        let Some(mixer) = self.mixer.upgrade() else {
            return Vec::new();
        };
        let zones = self.state.pip_zones(p);
        // Evaluate animated geometry at composite time, not sampling time —
        // otherwise borders trail their boxes during morphs/takes.
        let eval_t = mixer
            .query_position::<gst::ClockTime>()
            .map(|pos| pos + gst::ClockTime::from_mseconds(BORDER_LEAD_MS));
        let mut out = Vec::new();
        for zone in &zones {
            let Some(border) = &zone.border else { continue };
            if !border.is_visible() {
                continue;
            }
            let Some(rgba) = border.rgba() else { continue };
            let bw = border.clamped_width() as f64;
            for &input in zone.effective_sources() {
                let Some(pad) =
                    crate::gst::pipeline::effects::find_pad(&mixer, &format!("sink_{}", input))
                else {
                    continue;
                };
                let (x, y, w, h, pad_alpha) = pad_geometry_at(&pad, eval_t);
                if pad_alpha <= 0.01 {
                    continue;
                }
                out.push(BorderDraw {
                    x: x as f64,
                    y: y as f64,
                    w: w as f64,
                    h: h as f64,
                    width: bw,
                    rgba,
                    pad_alpha,
                });
            }
        }
        out
    }

    fn hash_borders(borders: &[BorderDraw]) -> u64 {
        let mut h: u64 = 0xB04DE4 ^ 0x9E3779B97F4A7C15;
        let mut mix = |v: u64| {
            h = h.rotate_left(13) ^ v.wrapping_mul(0x100000001B3);
        };
        for b in borders {
            mix(b.x.to_bits());
            mix(b.y.to_bits());
            mix(b.w.to_bits());
            mix(b.h.to_bits());
            mix(b.width.to_bits());
            mix(b.rgba.0.to_bits());
            mix(b.rgba.1.to_bits());
            mix(b.rgba.2.to_bits());
            mix(b.rgba.3.to_bits());
            mix(b.pad_alpha.to_bits());
        }
        mix(borders.len() as u64);
        h
    }

    /// Read live geometry, redraw if anything moved, always keep the
    /// compositor fed (re-pushing the last frame when nothing changed).
    pub fn render_if_dirty(&mut self) -> bool {
        let borders = self.collect_borders();
        let hash = Self::hash_borders(&borders);
        if hash == self.last_hash {
            return self.repush_last();
        }
        let pushed = self.push_frame(&borders);
        if pushed {
            self.last_hash = hash;
        }
        pushed
    }

    fn push_frame(&mut self, borders: &[BorderDraw]) -> bool {
        let mut surface = self
            .surface
            .take()
            .filter(|s| s.width() == self.width && s.height() == self.height)
            .unwrap_or_else(|| {
                cairo::ImageSurface::create(cairo::Format::ARgb32, self.width, self.height)
                    .expect("failed to create pgm overlay surface")
            });

        {
            let cr =
                cairo::Context::new(&surface).expect("failed to create pgm overlay cairo context");
            cr.set_operator(cairo::Operator::Clear);
            let _ = cr.paint();
            cr.set_operator(cairo::Operator::Over);
            for b in borders {
                // R↔B swapped on purpose: cairo's BGRA memory layout then
                // lands as correct RGBA in the buffer (same trick as the
                // multiview overlay).
                let (r, g, bl, a) = b.rgba;
                cr.set_source_rgba(bl, g, r, a * b.pad_alpha);
                cr.set_line_width(b.width);
                // Stroke centered on the rect expanded by width/2 → the band
                // covers [edge, edge + width] outward from the box on every
                // side, hugging the picture without covering it.
                cr.rectangle(
                    b.x - b.width / 2.0,
                    b.y - b.width / 2.0,
                    b.w + b.width,
                    b.h + b.width,
                );
                let _ = cr.stroke();
            }
        }

        let row_bytes = self.width as usize * 4;
        let buf_size = row_bytes * self.height as usize;
        let cairo_stride = surface.stride() as usize;

        let pushed = (|| -> Option<()> {
            let data = surface.data().ok()?;
            let mut pixel_data = vec![0u8; buf_size];
            if cairo_stride == row_bytes {
                pixel_data[..buf_size].copy_from_slice(&data[..buf_size]);
            } else {
                for y in 0..self.height as usize {
                    let src = y * cairo_stride;
                    let d = y * row_bytes;
                    pixel_data[d..d + row_bytes].copy_from_slice(&data[src..src + row_bytes]);
                }
            }
            let shared: Arc<[u8]> = pixel_data.into();
            self.last_data = Some(shared.clone());
            let buffer = gst::Buffer::from_slice(shared);
            let sample = gst::Sample::builder()
                .buffer(&buffer)
                .caps(&self.caps)
                .build();
            self.appsrc.push_sample(&sample).ok()?;
            Some(())
        })()
        .is_some();

        self.surface = Some(surface);
        pushed
    }

    fn repush_last(&self) -> bool {
        if let Some(ref data) = self.last_data {
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

    fn appsrc_state(&self) -> gst::State {
        self.appsrc.current_state()
    }
}

/// Global registry, keyed by block instance ID (mirrors the multiview
/// overlay renderer registry).
fn pgm_overlay_renderers() -> &'static Mutex<HashMap<String, Arc<Mutex<PgmOverlayRenderer>>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, Arc<Mutex<PgmOverlayRenderer>>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_pgm_overlay_renderer(block_id: &str, renderer: Arc<Mutex<PgmOverlayRenderer>>) {
    if let Ok(mut map) = pgm_overlay_renderers().lock() {
        map.insert(block_id.to_string(), renderer);
    }
}

pub fn get_pgm_overlay_renderer(block_id: &str) -> Option<Arc<Mutex<PgmOverlayRenderer>>> {
    pgm_overlay_renderers().lock().ok()?.get(block_id).cloned()
}

pub fn unregister_pgm_overlay_renderer(block_id: &str) {
    if let Ok(mut map) = pgm_overlay_renderers().lock() {
        map.remove(block_id);
    }
}

/// Start the PGM overlay push timer at the PGM framerate. Geometry reads +
/// the dirty hash run every tick; cairo only redraws when something moved.
/// The thread stops when the renderer is unregistered (flow stop).
pub fn start_pgm_overlay_timer(
    block_id: String,
    renderer: Arc<Mutex<PgmOverlayRenderer>>,
    pgm_framerate: (i32, i32),
) {
    let frame_interval = std::time::Duration::from_nanos(
        (pgm_framerate.1 as u64 * 1_000_000_000) / pgm_framerate.0.max(1) as u64,
    );
    std::thread::Builder::new()
        .name(format!(
            "pgm-overlay-{}",
            &block_id[..8.min(block_id.len())]
        ))
        .spawn(move || {
            debug!("PGM overlay timer started for {}", block_id);
            // Wait for the pipeline to reach PLAYING before the first push.
            let ready = loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if get_pgm_overlay_renderer(&block_id).is_none() {
                    break false;
                }
                if let Ok(r) = renderer.lock() {
                    if r.appsrc_state() == gst::State::Playing {
                        break true;
                    }
                }
            };
            if !ready {
                debug!("PGM overlay timer exiting early (renderer unregistered)");
                return;
            }
            if let Ok(mut r) = renderer.lock() {
                r.render_if_dirty();
            }
            let mut next_tick = std::time::Instant::now();
            loop {
                next_tick += frame_interval;
                let now = std::time::Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                } else if now - next_tick > frame_interval {
                    next_tick = now;
                }
                if get_pgm_overlay_renderer(&block_id).is_none() {
                    debug!("PGM overlay timer stopping for {}", block_id);
                    break;
                }
                if let Ok(mut r) = renderer.lock() {
                    r.render_if_dirty();
                } else {
                    warn!("PGM overlay timer: mutex poisoned for {}", block_id);
                    break;
                }
            }
        })
        .expect("failed to spawn pgm overlay timer thread");
}

/// Convenience used by the builders: create the renderer, register it, and
/// schedule the timer at element-setup time (pipeline start).
#[allow(clippy::too_many_arguments)]
pub fn setup_pgm_overlay_renderer(
    block_id: &str,
    appsrc: &gst_app::AppSrc,
    caps: &gst::Caps,
    state: Arc<VisionMixerOverlayState>,
    mixer: glib::WeakRef<gst::Element>,
    width: i32,
    height: i32,
    pgm_framerate: (i32, i32),
    ctx: &crate::blocks::BlockBuildContext,
) {
    let renderer = Arc::new(Mutex::new(PgmOverlayRenderer::new(
        appsrc.clone(),
        caps.clone(),
        state,
        mixer,
        width,
        height,
    )));
    register_pgm_overlay_renderer(block_id, Arc::clone(&renderer));

    let block_id = block_id.to_string();
    ctx.register_element_setup(Box::new(move |_flow_id, _events| {
        start_pgm_overlay_timer(block_id, renderer, pgm_framerate);
    }));
}

// Re-export so callers can reach overlay state helpers through one path if
// they only import this module.
#[allow(unused_imports)]
pub use overlay::get_overlay_state;
