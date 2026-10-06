//! Example stinger clips, one per variant, rendered by Strom itself so they
//! carry no third-party licence. FFV1 in Matroska: lossless, alpha-capable,
//! and decodable wherever gst-libav is installed.
//!
//! - `strom-sweep.mkv` (classic): a slanted panel of bands with the Strom
//!   name sweeps across and covers the frame half way through.
//! - `strom-blade.mkv` (track matte, side by side): a glowing blade crosses
//!   the frame; the matte beside it turns the new source on behind the blade.
//! - `strom-shards.mkv` (mask only): diamonds open from the left, revealing
//!   the new source.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::path::{Path, PathBuf};
use tracing::info;

/// Sub-directory of the media directory the examples go in.
pub const EXAMPLES_DIR: &str = "stingers";

const W: i32 = 1920;
const H: i32 = 1080;
const FPS: i32 = 30;

/// One example clip.
pub struct Example {
    pub file_name: &'static str,
    frames: usize,
    /// Width of a whole frame: twice `W` for a side-by-side clip.
    width: i32,
    grey: bool,
    render: fn(&cairo::Context, f64),
}

/// The examples Strom ships, in the order the playlist gets them.
pub fn examples() -> [Example; 3] {
    [
        Example {
            file_name: "strom-sweep.mkv",
            frames: 45,
            width: W,
            grey: false,
            render: render_sweep,
        },
        Example {
            file_name: "strom-blade.mkv",
            frames: 36,
            width: 2 * W,
            grey: false,
            render: render_blade,
        },
        Example {
            file_name: "strom-shards.mkv",
            frames: 36,
            width: W,
            grey: true,
            render: render_shards,
        },
    ]
}

/// Write every example into `media_dir/stingers`, replacing older copies.
/// Returns the paths relative to `media_dir`.
pub fn write_examples(media_dir: &Path) -> Result<Vec<String>, String> {
    let dir = media_dir.join(EXAMPLES_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut written = Vec::new();
    for example in examples() {
        let path = dir.join(example.file_name);
        write_example(&example, &path)?;
        written.push(format!("{}/{}", EXAMPLES_DIR, example.file_name));
    }
    Ok(written)
}

/// Render and encode one example to `path`.
pub fn write_example(example: &Example, path: &Path) -> Result<PathBuf, String> {
    let started = std::time::Instant::now();
    let format = if example.grey { "GRAY8" } else { "BGRA" };
    let pipeline = gst::Pipeline::new();
    let appsrc = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", format)
                .field("width", example.width)
                .field("height", H)
                .field("framerate", gst::Fraction::new(FPS, 1))
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .build(),
        )
        .format(gst::Format::Time)
        .is_live(false)
        .block(true)
        .max_buffers(4)
        .build();
    let encoder = gst::ElementFactory::make("avenc_ffv1")
        .build()
        .map_err(|e| format!("avenc_ffv1 (gst-libav): {e}"))?;
    let mux = gst::ElementFactory::make("matroskamux")
        .build()
        .map_err(|e| format!("matroskamux: {e}"))?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_string_lossy().to_string())
        .build()
        .map_err(|e| format!("filesink: {e}"))?;
    pipeline
        .add_many([appsrc.upcast_ref(), &encoder, &mux, &sink])
        .map_err(|e| e.to_string())?;
    gst::Element::link_many([appsrc.upcast_ref(), &encoder, &mux, &sink])
        .map_err(|e| e.to_string())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("start encoder: {e:?}"))?;

    let frame_ns = 1_000_000_000u64 / FPS as u64;
    let push = || -> Result<(), String> {
        for i in 0..example.frames {
            let t = i as f64 / (example.frames - 1).max(1) as f64;
            let data = render_frame(example, t)?;
            let mut buffer = gst::Buffer::from_mut_slice(data);
            {
                let b = buffer.get_mut().ok_or("buffer not writable")?;
                b.set_pts(gst::ClockTime::from_nseconds(i as u64 * frame_ns));
                b.set_duration(gst::ClockTime::from_nseconds(frame_ns));
            }
            appsrc
                .push_buffer(buffer)
                .map_err(|e| format!("encode: {e:?}"))?;
        }
        appsrc
            .end_of_stream()
            .map_err(|e| format!("end of stream: {e:?}"))?;
        Ok(())
    };
    let pushed = push();
    let bus = pipeline.bus().ok_or("no bus")?;
    let done = pushed.and_then(|()| {
        match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(m) if matches!(m.view(), gst::MessageView::Eos(_)) => Ok(()),
            Some(m) => Err(format!("encode failed: {:?}", m.view())),
            None => Err("encode timed out".to_string()),
        }
    });
    let _ = pipeline.set_state(gst::State::Null);
    done?;
    info!(
        "Wrote example stinger {} ({} frames) in {} ms",
        path.display(),
        example.frames,
        started.elapsed().as_millis()
    );
    Ok(path.to_path_buf())
}

/// Render frame `t` (0..1) as tightly packed BGRA with straight alpha, or
/// GRAY8 for a grey example.
fn render_frame(example: &Example, t: f64) -> Result<Vec<u8>, String> {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, example.width, H)
        .map_err(|e| format!("cairo surface: {e}"))?;
    {
        let cr = cairo::Context::new(&surface).map_err(|e| format!("cairo: {e}"))?;
        (example.render)(&cr, t);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let w = example.width as usize;
    let h = H as usize;
    let data = surface.data().map_err(|e| format!("cairo data: {e}"))?;
    if example.grey {
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                // Cairo's ARGB32 is native-endian BGRA in memory; a grey
                // render has equal channels, so blue is the level.
                out[y * w + x] = data[y * stride + x * 4];
            }
        }
        return Ok(out);
    }
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = &data[y * stride + x * 4..y * stride + x * 4 + 4];
            let a = px[3];
            let o = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            // Cairo premultiplies; the clips carry straight alpha.
            if a == 0 {
                o.copy_from_slice(&[0, 0, 0, 0]);
            } else {
                for c in 0..3 {
                    o[c] = ((px[c] as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                }
                o[3] = a;
            }
        }
    }
    Ok(out)
}

fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Classic: a slanted panel of four bands crosses the frame, with the Strom
/// name riding on it, covering everything around the middle of the clip.
fn render_sweep(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let slant = h * 0.45;
    // The panel is wide enough to cover the frame while centred on it.
    let panel = w + slant * 2.0;
    let travel = w + panel + slant;
    // Fast in, a hold of full cover in the middle, fast out.
    let p = if t < 0.4 {
        ease_in_out(t / 0.4) * 0.5
    } else if t < 0.6 {
        0.5
    } else {
        0.5 + ease_in_out((t - 0.6) / 0.4) * 0.5
    };
    let left = -panel - slant + travel * p;
    let bands = [
        (0.04, (0.95, 0.95, 0.97)),
        (0.30, (0.05, 0.23, 0.36)),
        (0.36, (0.12, 0.56, 0.75)),
        (0.30, (0.21, 0.76, 0.63)),
    ];
    let mut x = left;
    for (share, (r, g, b)) in bands {
        let bw = panel * share;
        cr.set_source_rgba(r, g, b, 1.0);
        cr.move_to(x + slant, 0.0);
        cr.line_to(x + slant + bw + 1.0, 0.0);
        cr.line_to(x + bw + 1.0, h);
        cr.line_to(x, h);
        cr.close_path();
        let _ = cr.fill();
        x += bw;
    }
    // Leading and trailing edges in white.
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.set_line_width(10.0);
    for edge in [left + panel, left] {
        cr.move_to(edge + slant, 0.0);
        cr.line_to(edge, h);
    }
    let _ = cr.stroke();
    // The name, centred on the panel.
    cr.select_font_face("Sans", cairo::FontSlant::Italic, cairo::FontWeight::Bold);
    cr.set_font_size(h * 0.22);
    if let Ok(ext) = cr.text_extents("STROM") {
        let cx = left + slant / 2.0 + panel / 2.0;
        cr.move_to(
            cx - ext.width() / 2.0 - ext.x_bearing(),
            h / 2.0 - ext.height() / 2.0 - ext.y_bearing(),
        );
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        let _ = cr.show_text("STROM");
    }
}

/// Track matte, side by side: a glowing blade on the left half, and on the
/// right half the matte that is white behind it and black ahead of it.
fn render_blade(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    let slant = h * 0.25;
    let p = ease_in_out(t);
    // Where the blade crosses the middle row of the frame.
    let bx = -slant - 120.0 + (w + 2.0 * slant + 240.0) * p;
    let blade_x = |y: f64| bx + (h / 2.0 - y) * slant / (h / 2.0);

    // Fill: glow, trailing streaks and a white core.
    for (half, alpha) in [(90.0, 0.18), (45.0, 0.35), (20.0, 0.7)] {
        cr.set_source_rgba(0.35, 0.85, 1.0, alpha);
        cr.move_to(blade_x(0.0) - half, 0.0);
        cr.line_to(blade_x(0.0) + half, 0.0);
        cr.line_to(blade_x(h) + half, h);
        cr.line_to(blade_x(h) - half, h);
        cr.close_path();
        let _ = cr.fill();
    }
    for i in 0..9 {
        let y = h * (i as f64 + 0.5) / 9.0;
        let len = 160.0 + 90.0 * ((i * 37 % 11) as f64);
        let x = blade_x(y);
        let grad = cairo::LinearGradient::new(x - len, y, x, y);
        grad.add_color_stop_rgba(0.0, 0.35, 0.85, 1.0, 0.0);
        grad.add_color_stop_rgba(1.0, 0.75, 0.95, 1.0, 0.8);
        let _ = cr.set_source(&grad);
        cr.set_line_width(4.0);
        cr.move_to(x - len, y);
        cr.line_to(x, y);
        let _ = cr.stroke();
    }
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.move_to(blade_x(0.0) - 6.0, 0.0);
    cr.line_to(blade_x(0.0) + 6.0, 0.0);
    cr.line_to(blade_x(h) + 6.0, h);
    cr.line_to(blade_x(h) - 6.0, h);
    cr.close_path();
    let _ = cr.fill();

    // Matte, right half: opaque grey, white up to the blade with a soft edge
    // under it so no hard line shows past the glow. Sheared so that lines of
    // constant x follow the blade's slant.
    let _ = cr.save();
    cr.rectangle(w, 0.0, w, h);
    cr.clip();
    cr.set_source_rgba(0.0, 0.0, 0.0, 1.0);
    let _ = cr.paint();
    let k = slant / (h / 2.0);
    cr.transform(cairo::Matrix::new(1.0, 0.0, -k, 1.0, w + k * h / 2.0, 0.0));
    let soft = 30.0;
    let grad = cairo::LinearGradient::new(bx - soft, 0.0, bx + soft, 0.0);
    grad.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 1.0);
    grad.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 1.0);
    let _ = cr.set_source(&grad);
    cr.rectangle(-4.0 * w, 0.0, 8.0 * w, h);
    let _ = cr.fill();
    let _ = cr.restore();
}

/// Mask only: diamonds open across the frame from the left, white where the
/// new source shows.
fn render_shards(cr: &cairo::Context, t: f64) {
    let (w, h) = (W as f64, H as f64);
    cr.set_source_rgb(0.0, 0.0, 0.0);
    let _ = cr.paint();
    cr.set_source_rgb(1.0, 1.0, 1.0);
    let cell = 120.0;
    let cols = (w / cell).ceil() as i32 + 1;
    let rows = (h / cell).ceil() as i32 + 1;
    for row in 0..rows {
        for col in 0..cols {
            let cx = col as f64 * cell + if row % 2 == 0 { 0.0 } else { cell / 2.0 };
            let cy = row as f64 * cell;
            // A wave from the left, slightly bowed at the middle row.
            let delay = (cx / w) * 0.6 + ((cy - h / 2.0).abs() / h) * 0.15;
            let local = ((t - delay) / 0.3).clamp(0.0, 1.0);
            let r = ease_in_out(local) * cell * 0.8;
            if r <= 0.0 {
                continue;
            }
            cr.move_to(cx, cy - r);
            cr.line_to(cx + r, cy);
            cr.line_to(cx, cy + r);
            cr.line_to(cx - r, cy);
            cr.close_path();
            let _ = cr.fill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(frame: &[u8], width: i32, x: i32, y: i32) -> u8 {
        frame[((y * width + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_sweep_covers_the_frame_in_the_middle_and_nothing_at_the_ends() {
        let ex = &examples()[0];
        let mid = render_frame(ex, 0.5).unwrap();
        for (x, y) in [(10, 10), (W / 2, H / 2), (W - 10, H - 10)] {
            assert_eq!(alpha_at(&mid, W, x, y), 255, "uncovered at {x},{y}");
        }
        let start = render_frame(ex, 0.0).unwrap();
        assert_eq!(alpha_at(&start, W, W / 2, H / 2), 0);
    }

    #[test]
    fn the_blade_matte_is_white_behind_the_blade_and_black_ahead() {
        let ex = &examples()[1];
        let f = render_frame(ex, 0.5).unwrap();
        let w = 2 * W;
        // Matte half: left of the blade white, right black, both opaque.
        let behind = (W + 50) as usize;
        let ahead = (2 * W - 50) as usize;
        let row = (H / 2) as usize * w as usize;
        assert!(f[(row + behind) * 4] > 240, "behind the blade is not white");
        assert!(f[(row + ahead) * 4] < 15, "ahead of the blade is not black");
        assert_eq!(f[(row + ahead) * 4 + 3], 255);
        // Fill half: transparent away from the blade.
        assert_eq!(alpha_at(&f, w, W - 20, 20), 0);
    }

    #[test]
    fn the_shards_mask_opens_from_black_to_white() {
        let ex = &examples()[2];
        let first = render_frame(ex, 0.0).unwrap();
        let last = render_frame(ex, 1.0).unwrap();
        let mean = |f: &[u8]| f.iter().map(|v| *v as f64).sum::<f64>() / f.len() as f64;
        assert!(mean(&first) < 1.0);
        assert!(mean(&last) > 254.0);
    }
}
