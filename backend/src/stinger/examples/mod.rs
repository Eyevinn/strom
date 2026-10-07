//! Example stinger clips, one per variant, rendered by Strom itself so they
//! carry no third-party licence. FFV1 in Matroska: lossless, alpha-capable,
//! and decodable wherever gst-libav is installed.
//!
//! - `strom-sweep.mkv` (classic): layered slanted panels whoosh in, cover the
//!   frame with the Strom name on the front panel, and peel away again.
//! - `strom-blade.mkv` (track matte, side by side): a hot blade of light
//!   crosses the frame throwing sparks; the matte beside it turns the new
//!   source on right behind the light.
//! - `strom-ribbons.mkv` (track matte, stacked): colourful brush ribbons
//!   sweep across; the matte below them reveals the new source behind their
//!   heads.
//! - `strom-shards.mkv` (mask only): hexagonal tiles flip open in a wave.
//!
//! Every frame is drawn with cairo on the CPU. Moving clips average a few
//! sub-frame samples per frame for motion blur. Randomness comes from a
//! seeded generator, so every render is the same.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::path::{Path, PathBuf};
use tracing::info;

mod blade;
mod draw;
mod ribbons;
mod shards;
mod sweep;
#[cfg(test)]
mod tests;

use blade::{blade_matte, render_blade, BLADE_FRAMES};
use ribbons::{render_ribbons, ribbons_matte};
use shards::render_shards;
use sweep::render_sweep;

/// Sub-directory of the media directory the examples go in.
pub const EXAMPLES_DIR: &str = "stingers";

const W: i32 = 1920;
const H: i32 = 1080;
const FPS: i32 = 30;

/// Fraction of a frame the virtual shutter is open for motion blur.
const SHUTTER: f64 = 0.5;

/// One example clip.
pub struct Example {
    pub file_name: &'static str,
    frames: usize,
    /// Size of a whole frame: twice `W` wide for a side-by-side clip, twice
    /// `H` high for a stacked one.
    width: i32,
    height: i32,
    grey: bool,
    /// Sub-frame samples averaged into each frame; 1 for no motion blur.
    samples: usize,
    /// Draws the graphic (or the whole mask) at a time.
    render: fn(&cairo::Context, f64),
    /// Draws a track matte's matte half, opaque grey, once per frame: it is
    /// not motion blurred, so it is not drawn per sample.
    matte: Option<fn(&cairo::Context, f64)>,
}

/// The examples Strom ships, in the order the playlist gets them: the
/// classic first (it works on every mixer), then the two track mattes next to
/// each other, then the mask.
pub fn examples() -> [Example; 4] {
    [
        Example {
            file_name: "strom-sweep.mkv",
            frames: 45,
            width: W,
            height: H,
            grey: false,
            samples: 4,
            render: render_sweep,
            matte: None,
        },
        Example {
            file_name: "strom-blade.mkv",
            frames: BLADE_FRAMES,
            width: 2 * W,
            height: H,
            grey: false,
            samples: 4,
            render: render_blade,
            matte: Some(blade_matte),
        },
        Example {
            file_name: "strom-ribbons.mkv",
            frames: 42,
            width: W,
            height: 2 * H,
            grey: false,
            samples: 3,
            render: render_ribbons,
            matte: Some(ribbons_matte),
        },
        Example {
            file_name: "strom-shards.mkv",
            frames: 36,
            width: W,
            height: H,
            grey: true,
            samples: 1,
            render: render_shards,
            matte: None,
        },
    ]
}

/// Write every example into `media_dir/stingers`, replacing older copies.
/// Returns the paths relative to `media_dir`.
pub fn write_examples(media_dir: &Path) -> Result<Vec<String>, String> {
    let dir = media_dir.join(EXAMPLES_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    // One thread per clip: each renders and encodes on its own, so the
    // whole set takes as long as the slowest clip.
    let all = examples();
    let results: Vec<Result<PathBuf, String>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = all
            .iter()
            .map(|example| {
                let path = dir.join(example.file_name);
                scope.spawn(move || write_example(example, &path))
            })
            .collect();
        jobs.into_iter()
            .map(|job| {
                job.join()
                    .unwrap_or_else(|_| Err("example render panicked".to_string()))
            })
            .collect()
    });
    for result in results {
        result?;
    }
    Ok(all
        .iter()
        .map(|example| format!("{}/{}", EXAMPLES_DIR, example.file_name))
        .collect())
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
                .field("height", example.height)
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
            let data = render_frame(example, example.t(i))?;
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

impl Example {
    /// Clip time (0..1) of frame `i`.
    fn t(&self, i: usize) -> f64 {
        i as f64 / (self.frames - 1).max(1) as f64
    }
}

/// Render frame `t` (0..1) into a premultiplied cairo surface, averaging the
/// example's sub-frame samples over a shutter that closes at `t`.
fn render_surface(example: &Example, t: f64) -> Result<cairo::ImageSurface, String> {
    let create = || {
        cairo::ImageSurface::create(cairo::Format::ARgb32, example.width, example.height)
            .map_err(|e| format!("cairo surface: {e}"))
    };
    let surface = create()?;
    let n = example.samples.max(1);
    let shutter = SHUTTER / (example.frames - 1).max(1) as f64;
    let draw_matte = |surface: &cairo::ImageSurface| -> Result<(), String> {
        if let Some(matte) = example.matte {
            let cr = cairo::Context::new(surface).map_err(|e| format!("cairo: {e}"))?;
            // Half way through the shutter, where the blurred graphic is.
            let tm = if n == 1 { t } else { t - shutter / 2.0 };
            matte(&cr, tm.clamp(0.0, 1.0));
        }
        surface.flush();
        Ok(())
    };
    if n == 1 {
        let cr = cairo::Context::new(&surface).map_err(|e| format!("cairo: {e}"))?;
        (example.render)(&cr, t);
        drop(cr);
        draw_matte(&surface)?;
        return Ok(surface);
    }
    // A track matte's graphic stays in its half, top left of the frame, so
    // the samples need only that much.
    let sub = if example.matte.is_some() {
        cairo::ImageSurface::create(cairo::Format::ARgb32, W, H)
            .map_err(|e| format!("cairo surface: {e}"))?
    } else {
        create()?
    };
    let acc = cairo::Context::new(&surface).map_err(|e| format!("cairo: {e}"))?;
    acc.set_operator(cairo::Operator::Add);
    for k in 0..n {
        {
            let cr = cairo::Context::new(&sub).map_err(|e| format!("cairo: {e}"))?;
            cr.set_operator(cairo::Operator::Clear);
            let _ = cr.paint();
            cr.set_operator(cairo::Operator::Over);
            let ts = (t - shutter * k as f64 / n as f64).clamp(0.0, 1.0);
            (example.render)(&cr, ts);
        }
        sub.flush();
        // Each sample adds 1/n of itself. For n up to 5, n shares of an
        // opaque pixel add up to exactly 255 (or saturate there), so what
        // every sample covers stays fully covered.
        let _ = acc.set_source_surface(&sub, 0.0, 0.0);
        let _ = acc.paint_with_alpha(1.0 / n as f64);
    }
    drop(acc);
    draw_matte(&surface)?;
    Ok(surface)
}

/// Render frame `t` (0..1) as tightly packed BGRA with straight alpha, or
/// GRAY8 for a grey example.
fn render_frame(example: &Example, t: f64) -> Result<Vec<u8>, String> {
    let mut surface = render_surface(example, t)?;
    let stride = surface.stride() as usize;
    let w = example.width as usize;
    let h = example.height as usize;
    let data = surface.data().map_err(|e| format!("cairo data: {e}"))?;
    if example.grey {
        let mut out = vec![0u8; w * h];
        for (row, src) in out.chunks_exact_mut(w).zip(data.chunks(stride)) {
            // Cairo's ARGB32 is native-endian BGRA in memory; a grey
            // render has equal channels, so blue is the level.
            for (o, px) in row.iter_mut().zip(src.as_chunks::<4>().0) {
                *o = px[0];
            }
        }
        return Ok(out);
    }
    let mut out = vec![0u8; w * h * 4];
    for (row, src) in out.chunks_exact_mut(w * 4).zip(data.chunks(stride)) {
        for (o, px) in row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<4>().0)
        {
            let a = px[3] as u32;
            // Cairo premultiplies; the clips carry straight alpha.
            match a {
                0 => {}
                255 => *o = *px,
                _ => {
                    for c in 0..3 {
                        o[c] = ((px[c] as u32 * 255 + a / 2) / a).min(255) as u8;
                    }
                    o[3] = a as u8;
                }
            }
        }
    }
    Ok(out)
}
