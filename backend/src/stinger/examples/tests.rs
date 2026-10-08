//! Checks on the rendered example frames, and contact sheets for a look by
//! eye.

use super::blade::{blade_x, BLADE_MATTE_LAG};
use super::draw::{hex, Rgb};
use super::*;

fn alpha_at(frame: &[u8], width: i32, x: i32, y: i32) -> u8 {
    frame[((y * width + x) * 4 + 3) as usize]
}

fn example(name: &str) -> Example {
    examples()
        .into_iter()
        .find(|e| e.file_name == name)
        .expect(name)
}

#[test]
fn the_examples_are_one_of_each_layout() {
    let shapes: Vec<_> = examples()
        .iter()
        .map(|e| (e.file_name, e.width, e.height, e.grey))
        .collect();
    assert_eq!(
        shapes,
        [
            ("strom-sweep.mkv", W, H, false),
            ("strom-blade.mkv", 2 * W, H, false),
            ("strom-ribbons.mkv", W, 2 * H, false),
            ("strom-shards.mkv", W, H, true),
        ]
    );
    assert_eq!(examples()[1].frames, BLADE_FRAMES);
}

/// Motion blur averages samples; where every sample is opaque the
/// average must stay exactly opaque, or the classic never covers.
#[test]
fn averaged_samples_keep_full_cover() {
    for n in 1..=5 {
        let ex = Example {
            file_name: "test",
            frames: 10,
            width: 64,
            height: 32,
            grey: false,
            samples: n,
            render: |cr, _| {
                cr.set_source_rgb(0.2, 0.4, 0.9);
                let _ = cr.paint();
            },
            matte: None,
        };
        let f = render_frame(&ex, 0.5).unwrap();
        assert!(f.chunks(4).all(|p| p[3] == 255), "{n} samples");
    }
}

#[test]
fn the_sweep_covers_the_whole_frame_in_the_middle_and_nothing_at_the_ends() {
    let ex = example("strom-sweep.mkv");
    let mid = ex.frames / 2;
    let f = render_frame(&ex, ex.t(mid)).unwrap();
    let uncovered = f.chunks(4).filter(|p| p[3] != 255).count();
    assert_eq!(uncovered, 0, "frame {mid}: {uncovered} pixels not covered");
    for t in [0.0, 1.0] {
        let f = render_frame(&ex, t).unwrap();
        let visible = f.chunks(4).filter(|p| p[3] != 0).count();
        assert_eq!(visible, 0, "t={t}: {visible} pixels drawn");
    }
}

/// The matte half of a track-matte frame: its left and top corner.
fn matte_origin(ex: &Example) -> (usize, usize) {
    if ex.width > W {
        (W as usize, 0)
    } else {
        (0, H as usize)
    }
}

/// The graphic stays in its half: a matte pixel is always grey and
/// opaque, never tinted by the glow, sparks or ribbons next to it.
fn assert_matte_clean(ex: &Example, i: usize, f: &[u8]) {
    let (w, h) = (ex.width as usize, ex.height as usize);
    let (x0, y0) = matte_origin(ex);
    for y in (y0..h).step_by(9) {
        for x in (x0..w).step_by(7) {
            let px = &f[(y * w + x) * 4..(y * w + x) * 4 + 4];
            assert!(
                px[0] == px[1] && px[1] == px[2] && px[3] == 255,
                "{} frame {i}: colour in the matte at {x},{y}: {px:?}",
                ex.file_name
            );
        }
    }
}

fn matte_mean(ex: &Example, f: &[u8]) -> f64 {
    let w = ex.width as usize;
    let (x0, y0) = matte_origin(ex);
    let (mut sum, mut n) = (0.0, 0.0);
    for y in (y0..y0 + H as usize).step_by(5) {
        for x in (x0..x0 + W as usize).step_by(5) {
            sum += f[(y * w + x) * 4] as f64;
            n += 1.0;
        }
    }
    sum / n
}

/// For both track mattes: the fill never paints into the matte, the
/// matte goes from strictly black to strictly white without going back,
/// and nothing of the graphic is left on screen at either end.
#[test]
fn the_track_mattes_switch_cleanly_and_stay_in_their_half() {
    for name in ["strom-blade.mkv", "strom-ribbons.mkv"] {
        let ex = example(name);
        // Every frame at full size costs time in a debug build, next to
        // timing tests: a spread of frames, both ends included.
        let mut means = Vec::new();
        for i in (0..ex.frames).step_by(4).chain([ex.frames - 1]) {
            let f = render_frame(&ex, ex.t(i)).unwrap();
            assert_matte_clean(&ex, i, &f);
            means.push(matte_mean(&ex, &f));
            if i != 0 && i != ex.frames - 1 {
                continue;
            }
            let w = ex.width as usize;
            let (mx, my) = matte_origin(&ex);
            let want = if i == 0 { 0 } else { 255 };
            for y in 0..H as usize {
                for x in 0..W as usize {
                    let m = f[((y + my) * w + x + mx) * 4];
                    assert_eq!(m, want, "{name} frame {i}: matte {m} at {x},{y}");
                    let a = f[(y * w + x) * 4 + 3];
                    assert_eq!(a, 0, "{name} frame {i}: graphic left at {x},{y}");
                }
            }
        }
        assert!(
            means.windows(2).all(|m| m[1] >= m[0] - 0.5),
            "{name}: matte goes back: {means:?}"
        );
    }
}

#[test]
fn the_blade_matte_is_white_behind_the_blade_and_black_ahead() {
    let ex = example("strom-blade.mkv");
    let i = (0..ex.frames)
        .find(|i| blade_x(ex.t(*i)) > W as f64 / 2.0)
        .unwrap();
    let t = ex.t(i);
    let f = render_frame(&ex, t).unwrap();
    let w = 2 * W as usize;
    let row = (H / 2) as usize * w;
    let bx = blade_x(t) as usize;
    let behind = W as usize + bx - 250;
    let ahead = W as usize + bx + 250;
    assert_eq!(f[(row + behind) * 4], 255, "behind the blade is not white");
    assert_eq!(f[(row + ahead) * 4], 0, "ahead of the blade is not black");
    // The light covers the matte's switch, motion blurred as it is.
    let switch = (blade_x(t - SHUTTER / 2.0 / (ex.frames - 1) as f64) - BLADE_MATTE_LAG) as i32;
    let a = alpha_at(&f, 2 * W, switch, H / 2);
    assert!(a > 180, "alpha {a} over the matte switch");
}

#[test]
fn the_shards_mask_opens_from_black_to_white_and_never_closes() {
    let ex = example("strom-shards.mkv");
    let mean = |f: &[u8]| f.iter().map(|v| *v as f64).sum::<f64>() / f.len() as f64;
    let first = render_frame(&ex, 0.0).unwrap();
    assert!(first.iter().all(|v| *v == 0), "first frame not black");
    let last = render_frame(&ex, 1.0).unwrap();
    assert!(last.iter().all(|v| *v == 255), "last frame not white");
    let mut prev = 0.0;
    for i in (0..ex.frames).step_by(3) {
        let m = mean(&render_frame(&ex, ex.t(i)).unwrap());
        assert!(m >= prev, "frame {i}: mean {m} below {prev}");
        prev = m;
    }
}

/// Contact sheets and on-air previews of every example, for a look by
/// eye: `STROM_STINGER_SHEETS_DIR=... cargo test --lib -- --ignored
/// contact_sheets`.
#[test]
#[ignore]
fn contact_sheets() {
    let dir = std::env::var("STROM_STINGER_SHEETS_DIR").expect("set STROM_STINGER_SHEETS_DIR");
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    gst::init().unwrap();
    for ex in examples() {
        let stem = ex.file_name.trim_end_matches(".mkv");
        let started = std::time::Instant::now();
        for i in 0..ex.frames {
            render_frame(&ex, ex.t(i)).unwrap();
        }
        println!(
            "{stem}: {} frames rendered in {} ms",
            ex.frames,
            started.elapsed().as_millis()
        );
        write_png(&dir.join(format!("{stem}-sheet.png")), sheet(&ex));
        write_png(&dir.join(format!("{stem}-preview.png")), preview(&ex));
    }
}

/// 12 frames in a 4x3 grid at a sixth of their size, over a checker
/// board so alpha shows.
fn sheet(ex: &Example) -> cairo::ImageSurface {
    let scale = 1.0 / 6.0;
    let (cw, ch) = (ex.width as f64 * scale, ex.height as f64 * scale);
    let (cols, rows, pad) = (4, 3, 8.0);
    let out = cairo::ImageSurface::create(
        cairo::Format::ARgb32,
        (cols as f64 * (cw + pad) + pad) as i32,
        (rows as f64 * (ch + pad) + pad) as i32,
    )
    .unwrap();
    let cr = cairo::Context::new(&out).unwrap();
    cr.set_source_rgb(0.1, 0.1, 0.1);
    let _ = cr.paint();
    for n in 0..cols * rows {
        let i = (n * (ex.frames - 1) + (cols * rows - 1) / 2) / (cols * rows - 1);
        let (x, y) = (
            pad + (n % cols) as f64 * (cw + pad),
            pad + (n / cols) as f64 * (ch + pad),
        );
        let _ = cr.save();
        cr.rectangle(x, y, cw, ch);
        cr.clip();
        let sq = 10.0;
        for yy in 0..(ch / sq) as i32 + 1 {
            for xx in 0..(cw / sq) as i32 + 1 {
                let v = if (xx + yy) % 2 == 0 { 0.55 } else { 0.4 };
                cr.set_source_rgb(v, v, v);
                cr.rectangle(x + xx as f64 * sq, y + yy as f64 * sq, sq, sq);
                let _ = cr.fill();
            }
        }
        let s = render_surface(ex, ex.t(i)).unwrap();
        cr.translate(x, y);
        cr.scale(scale, scale);
        let _ = cr.set_source_surface(&s, 0.0, 0.0);
        let _ = cr.paint();
        let _ = cr.restore();
        cr.set_source_rgb(1.0, 1.0, 0.0);
        cr.set_font_size(14.0);
        cr.move_to(x + 4.0, y + 16.0);
        let _ = cr.show_text(&format!("{i}"));
    }
    drop(cr);
    out
}

fn test_card(w: i32, h: i32, bg: Rgb, fg: Rgb, label: &str) -> cairo::ImageSurface {
    let s = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
    let cr = cairo::Context::new(&s).unwrap();
    cr.set_source_rgb(bg.0, bg.1, bg.2);
    let _ = cr.paint();
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.35);
    cr.set_line_width(1.0);
    for x in (0..w).step_by(30) {
        cr.move_to(x as f64 + 0.5, 0.0);
        cr.line_to(x as f64 + 0.5, h as f64);
    }
    for y in (0..h).step_by(30) {
        cr.move_to(0.0, y as f64 + 0.5);
        cr.line_to(w as f64, y as f64 + 0.5);
    }
    let _ = cr.stroke();
    cr.set_source_rgb(fg.0, fg.1, fg.2);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(h as f64 * 0.6);
    cr.move_to(w as f64 * 0.36, h as f64 * 0.72);
    let _ = cr.show_text(label);
    drop(cr);
    s
}

/// What goes on air: A (blue) to B (orange) through the matte, the fill
/// on top; a classic cuts at its middle frame. 8 frames, 4x2.
fn preview(ex: &Example) -> cairo::ImageSurface {
    let (cw, ch) = (480, 270);
    let scale = cw as f64 / W as f64;
    let a = test_card(cw, ch, hex(0x0c1a3d), hex(0x7aa2ff), "A");
    let b = test_card(cw, ch, hex(0xf08a24), hex(0x5a2a00), "B");
    let (cols, rows, pad) = (4usize, 2usize, 8i32);
    let out = cairo::ImageSurface::create(
        cairo::Format::ARgb32,
        cols as i32 * (cw + pad) + pad,
        rows as i32 * (ch + pad) + pad,
    )
    .unwrap();
    let small = |s: &cairo::ImageSurface, ox: f64, oy: f64| {
        let t = cairo::ImageSurface::create(cairo::Format::ARgb32, cw, ch).unwrap();
        let cr = cairo::Context::new(&t).unwrap();
        cr.scale(scale, scale);
        let _ = cr.set_source_surface(s, -ox, -oy);
        let _ = cr.paint();
        drop(cr);
        t
    };
    let mut a = a;
    let mut b = b;
    let a_px = a.data().unwrap().to_vec();
    let b_px = b.data().unwrap().to_vec();
    let n = cols * rows;
    for k in 0..n {
        let i = (k * (ex.frames - 1) + (n - 1) / 2) / (n - 1);
        let t = ex.t(i);
        let full = render_surface(ex, t).unwrap();
        let mut fill = small(&full, 0.0, 0.0);
        let mut matte = if ex.grey {
            Some(small(&full, 0.0, 0.0))
        } else if ex.width > W {
            Some(small(&full, W as f64, 0.0))
        } else if ex.height > H {
            Some(small(&full, 0.0, H as f64))
        } else {
            None
        };
        let fill_px = fill.data().unwrap().to_vec();
        let matte_px = matte.as_mut().map(|m| m.data().unwrap().to_vec());
        let mut cell = cairo::ImageSurface::create(cairo::Format::ARgb32, cw, ch).unwrap();
        {
            let mut d = cell.data().unwrap();
            for p in 0..(cw * ch) as usize {
                let m = match &matte_px {
                    Some(m) => m[p * 4] as f64 / 255.0,
                    None => (t >= 0.5) as u8 as f64,
                };
                let fa = if ex.grey {
                    0.0
                } else {
                    fill_px[p * 4 + 3] as f64 / 255.0
                };
                for c in 0..3 {
                    let bg = a_px[p * 4 + c] as f64 * (1.0 - m) + b_px[p * 4 + c] as f64 * m;
                    let f = if ex.grey {
                        0.0
                    } else {
                        fill_px[p * 4 + c] as f64
                    };
                    d[p * 4 + c] = (f + bg * (1.0 - fa)).round().min(255.0) as u8;
                }
                d[p * 4 + 3] = 255;
            }
        }
        let cr = cairo::Context::new(&out).unwrap();
        let (x, y) = (
            pad + (k % cols) as i32 * (cw + pad),
            pad + (k / cols) as i32 * (ch + pad),
        );
        let _ = cr.set_source_surface(&cell, x as f64, y as f64);
        let _ = cr.paint();
        cr.set_source_rgb(1.0, 1.0, 0.0);
        cr.set_font_size(16.0);
        cr.move_to(x as f64 + 6.0, y as f64 + 20.0);
        let _ = cr.show_text(&format!("{i}"));
    }
    out
}

fn write_png(path: &std::path::Path, mut s: cairo::ImageSurface) {
    let (w, h) = (s.width(), s.height());
    let stride = s.stride() as usize;
    let data = s.data().unwrap();
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for row in data.chunks(stride).take(h as usize) {
        px.extend_from_slice(&row[..(w * 4) as usize]);
    }
    let pipeline = gst::parse::launch(&format!(
        "appsrc name=src caps=video/x-raw,format=BGRA,width={w},height={h},framerate=0/1 \
         ! videoconvert ! pngenc ! filesink name=sink"
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    // `location` as a property, not in the launch string: the parser reads a
    // Windows path's backslashes as escapes.
    pipeline
        .by_name("sink")
        .unwrap()
        .set_property("location", path.to_str().unwrap());
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    src.push_buffer(gst::Buffer::from_mut_slice(px)).unwrap();
    src.end_of_stream().unwrap();
    let bus = pipeline.bus().unwrap();
    bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).unwrap();
}
