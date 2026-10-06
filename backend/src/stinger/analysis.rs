//! Decode a stinger clip once and read what a take needs from it: its shape
//! and alpha (the layout), where its graphic covers the frame (a classic
//! clip's cut point) and where its matte switches (a track-matte clip played
//! as classic on the software mixer).
//!
//! Runs off the streaming path, as fast as the decoder goes, sampling a sparse
//! grid of each frame. Results are cached per file and program shape.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use strom_types::stinger::{StingerClipInfo, StingerLayout};
use tracing::{debug, info};

/// Every how many pixels a frame is sampled, both ways.
const SAMPLE_STEP: usize = 8;

/// Longest a single analysis may take.
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(60);

/// An alpha above this counts as covering.
const OPAQUE: u8 = 230;

/// How far a matte's mean may move and still count as standing still.
const MATTE_STILL: f32 = 0.02;

/// Per frame: alpha coverage and matte brightness of each region a layout
/// can use, so the layout can be decided once every frame is in.
#[derive(Debug, Clone, Copy, Default)]
struct FrameStats {
    pts_ns: u64,
    cover_full: f32,
    cover_left: f32,
    cover_top: f32,
    luma_full: f32,
    luma_right: f32,
    luma_bottom: f32,
    /// Mean colour difference between channels; ~0 for a grey frame.
    chroma: f32,
}

type CacheKey = (String, u64, u64, i64);

static CACHE: LazyLock<Mutex<HashMap<CacheKey, StingerClipInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_key(uri: &str, pgm_aspect: f64) -> CacheKey {
    let (mtime, len) = gst::glib::filename_from_uri(uri)
        .ok()
        .and_then(|(path, _)| std::fs::metadata(path).ok())
        .map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, m.len())
        })
        .unwrap_or((0, 0));
    (
        uri.to_string(),
        mtime,
        len,
        (pgm_aspect * 1000.0).round() as i64,
    )
}

/// The analysis of `uri` for a program of `pgm_aspect`, if it has run and the
/// file has not changed since.
pub fn cached(uri: &str, pgm_aspect: f64) -> Option<StingerClipInfo> {
    let key = cache_key(uri, pgm_aspect);
    CACHE.lock().ok()?.get(&key).cloned()
}

/// Analyse `uri`, or return the cached result. Blocks while decoding.
pub fn analyze_cached(uri: &str, pgm_aspect: f64) -> Result<StingerClipInfo, String> {
    if let Some(info) = cached(uri, pgm_aspect) {
        return Ok(info);
    }
    let info = analyze(uri, pgm_aspect)?;
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(cache_key(uri, pgm_aspect), info.clone());
    }
    Ok(info)
}

/// Decode `uri` and measure it. Blocks while decoding.
pub fn analyze(uri: &str, pgm_aspect: f64) -> Result<StingerClipInfo, String> {
    let started = Instant::now();
    let pipeline = gst::Pipeline::new();
    let decodebin = gst::ElementFactory::make("uridecodebin3")
        .property("uri", uri)
        .property("caps", gst::Caps::builder("video/x-raw").build())
        .build()
        .map_err(|e| format!("uridecodebin3: {e}"))?;
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|e| format!("videoconvert: {e}"))?;
    let appsink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build(),
        )
        .sync(false)
        .max_buffers(4)
        .build();
    pipeline
        .add_many([&decodebin, &convert, appsink.upcast_ref()])
        .map_err(|e| format!("add: {e}"))?;
    convert.link(&appsink).map_err(|e| format!("link: {e}"))?;

    // The decoder's own output format says whether the clip carries alpha;
    // after the converter every frame is RGBA either way.
    let decoded_alpha: std::sync::Arc<Mutex<Option<bool>>> = Default::default();
    {
        let convert_weak = convert.downgrade();
        let decoded_alpha = decoded_alpha.clone();
        decodebin.connect_pad_added(move |_, pad| {
            let Some(convert) = convert_weak.upgrade() else {
                return;
            };
            let Some(sink) = convert.static_pad("sink") else {
                return;
            };
            if sink.is_linked() {
                return;
            }
            if pad.link(&sink).is_err() {
                return;
            }
            let alpha = decoded_alpha.clone();
            sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::EventView::Caps(caps)) = info.event().map(|e| e.view()) {
                    if let Ok(vinfo) = gst_video::VideoInfo::from_caps(caps.caps()) {
                        if let Ok(mut a) = alpha.lock() {
                            *a = Some(vinfo.format_info().has_alpha());
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });
        });
    }

    // Down to NULL on every way out, a failed start included: a decodebin
    // left half started keeps its threads, and the next decoder anyone in
    // the process autoplugs can wait on them.
    struct ShutDown(gst::Pipeline);
    impl Drop for ShutDown {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let pipeline = ShutDown(pipeline);
    pipeline
        .0
        .set_state(gst::State::Playing)
        .map_err(|e| format!("start analysis: {e:?}"))?;

    let result = collect(&appsink, &pipeline.0, started);
    drop(pipeline);
    let (frames, width, height, fps) = result?;
    let has_alpha = decoded_alpha.lock().ok().and_then(|a| *a).unwrap_or(false);
    let info = summarize(
        &frames,
        width,
        height,
        fps,
        has_alpha,
        pgm_aspect,
        started.elapsed(),
    )?;
    info!(
        "Stinger clip {}: {}x{}, {} frames, {} ms, alpha={}, layout={:?}, analysed in {} ms",
        uri,
        info.width,
        info.height,
        info.frames,
        info.duration_ms,
        info.has_alpha,
        info.detected_layout,
        info.analysis_ms
    );
    Ok(info)
}

type Collected = (Vec<FrameStats>, u32, u32, (i32, i32));

fn collect(
    appsink: &gst_app::AppSink,
    pipeline: &gst::Pipeline,
    started: Instant,
) -> Result<Collected, String> {
    let bus = pipeline.bus().ok_or("no bus")?;
    let mut frames = Vec::new();
    let mut shape: Option<(u32, u32, (i32, i32))> = None;
    loop {
        if started.elapsed() > ANALYSIS_TIMEOUT {
            return Err(format!("analysis took longer than {:?}", ANALYSIS_TIMEOUT));
        }
        if let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = msg.view() {
                return Err(format!("cannot decode the clip: {}", e.error()));
            }
        }
        match appsink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            Some(sample) => {
                let caps = sample.caps().ok_or("frame without caps")?;
                let vinfo = gst_video::VideoInfo::from_caps(caps).map_err(|e| e.to_string())?;
                if shape.is_none() {
                    let fps = vinfo.fps();
                    shape = Some((vinfo.width(), vinfo.height(), (fps.numer(), fps.denom())));
                }
                let buffer = sample.buffer().ok_or("sample without buffer")?;
                let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &vinfo)
                    .map_err(|_| "unreadable frame".to_string())?;
                frames.push(measure(&frame, buffer.pts().map_or(0, |p| p.nseconds())));
            }
            None if appsink.is_eos() => break,
            None => {}
        }
    }
    let (w, h, fps) = shape.ok_or("the clip has no video frames")?;
    debug!("Stinger analysis: {} frames of {}x{}", frames.len(), w, h);
    Ok((frames, w, h, fps))
}

fn measure(frame: &gst_video::VideoFrameRef<&gst::BufferRef>, pts_ns: u64) -> FrameStats {
    let w = frame.width() as usize;
    let h = frame.height() as usize;
    let stride = frame.plane_stride()[0] as usize;
    let Ok(data) = frame.plane_data(0) else {
        return FrameStats {
            pts_ns,
            ..Default::default()
        };
    };
    #[derive(Default)]
    struct Acc {
        n: u32,
        cover: u32,
        luma: f32,
    }
    let (mut full, mut left, mut right, mut top, mut bottom) = (
        Acc::default(),
        Acc::default(),
        Acc::default(),
        Acc::default(),
        Acc::default(),
    );
    let mut chroma = 0f32;
    for y in (0..h).step_by(SAMPLE_STEP) {
        let row = &data[y * stride..];
        for x in (0..w).step_by(SAMPLE_STEP) {
            let px = &row[x * 4..x * 4 + 4];
            let (r, g, b, a) = (px[0] as f32, px[1] as f32, px[2] as f32, px[3]);
            let luma = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
            let covers = (a >= OPAQUE) as u32;
            chroma += ((r - g).abs() + (g - b).abs()) / 510.0;
            for (acc, inside) in [
                (&mut full, true),
                (&mut left, x < w / 2),
                (&mut right, x >= w / 2),
                (&mut top, y < h / 2),
                (&mut bottom, y >= h / 2),
            ] {
                if inside {
                    acc.n += 1;
                    acc.cover += covers;
                    acc.luma += luma;
                }
            }
        }
    }
    let cover = |a: &Acc| a.cover as f32 / a.n.max(1) as f32;
    let luma = |a: &Acc| a.luma / a.n.max(1) as f32;
    FrameStats {
        pts_ns,
        cover_full: cover(&full),
        cover_left: cover(&left),
        cover_top: cover(&top),
        luma_full: luma(&full),
        luma_right: luma(&right),
        luma_bottom: luma(&bottom),
        chroma: chroma / full.n.max(1) as f32,
    }
}

fn summarize(
    frames: &[FrameStats],
    width: u32,
    height: u32,
    fps: (i32, i32),
    has_alpha: bool,
    pgm_aspect: f64,
    took: Duration,
) -> Result<StingerClipInfo, String> {
    let first = frames.first().ok_or("the clip has no video frames")?;
    let looks_grey = frames.iter().all(|f| f.chroma < 0.02);
    let layout = super::detect_layout(width, height, has_alpha, looks_grey, pgm_aspect);

    let t0 = first.pts_ns;
    let at = |i: usize| (frames[i].pts_ns.saturating_sub(t0)) / 1_000_000;
    let frame_ns = if fps.0 > 0 && fps.1 > 0 {
        1_000_000_000u64 * fps.1 as u64 / fps.0 as u64
    } else {
        frames
            .get(1)
            .map(|f| f.pts_ns.saturating_sub(t0))
            .unwrap_or(33_333_333)
    };
    let duration_ms =
        (frames.last().map_or(0, |f| f.pts_ns.saturating_sub(t0)) + frame_ns) / 1_000_000;

    let cover: Option<Vec<f32>> = match layout {
        StingerLayout::Classic if has_alpha => Some(frames.iter().map(|f| f.cover_full).collect()),
        StingerLayout::SideBySide if has_alpha => {
            Some(frames.iter().map(|f| f.cover_left).collect())
        }
        StingerLayout::Stacked if has_alpha => Some(frames.iter().map(|f| f.cover_top).collect()),
        _ => None,
    };
    let matte: Option<Vec<f32>> = match layout {
        StingerLayout::SideBySide => Some(frames.iter().map(|f| f.luma_right).collect()),
        StingerLayout::Stacked => Some(frames.iter().map(|f| f.luma_bottom).collect()),
        StingerLayout::MaskOnly => Some(frames.iter().map(|f| f.luma_full).collect()),
        _ => None,
    };

    let (cover_peak_ms, cover_peak) = match cover.as_deref().and_then(plateau_middle) {
        Some((i, peak)) => (Some(at(i)), peak),
        None => (None, 0.0),
    };
    let (matte_midpoint_ms, matte_start_ms, matte_end_ms) = match matte.as_deref() {
        Some(m) => {
            let (mid, start, end) = matte_marks(m);
            (mid.map(at), start.map(at), end.map(at))
        }
        None => (None, None, None),
    };

    Ok(StingerClipInfo {
        width,
        height,
        framerate_num: fps.0,
        framerate_den: fps.1,
        frames: frames.len() as u32,
        duration_ms,
        has_alpha,
        detected_layout: layout,
        cover_peak_ms,
        cover_peak,
        matte_midpoint_ms,
        matte_start_ms,
        matte_end_ms,
        analysis_ms: took.as_millis() as u64,
    })
}

/// The middle of the first run of frames at the highest coverage, and that
/// coverage. A graphic that holds full cover for a while is cut under in the
/// middle of the hold, furthest from both edges.
fn plateau_middle(cover: &[f32]) -> Option<(usize, f32)> {
    let peak = cover.iter().copied().fold(f32::MIN, f32::max);
    let first = cover.iter().position(|c| *c >= peak - 0.005)?;
    let len = cover[first..]
        .iter()
        .take_while(|c| **c >= peak - 0.005)
        .count();
    Some((first + len.saturating_sub(1) / 2, peak))
}

/// Where a matte crosses half way from where it starts, where it starts
/// moving, and where it stops.
fn matte_marks(m: &[f32]) -> (Option<usize>, Option<usize>, Option<usize>) {
    let (Some(&first), Some(&last)) = (m.first(), m.last()) else {
        return (None, None, None);
    };
    let mid = m.iter().position(|v| (*v >= 0.5) != (first >= 0.5));
    let start = m.iter().position(|v| (*v - first).abs() > MATTE_STILL);
    let end = start.and_then(|s| {
        m[s..]
            .iter()
            .position(|v| (*v - last).abs() <= MATTE_STILL)
            .map(|i| i + s)
    });
    (mid, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_peak_is_cut_under_in_its_middle() {
        assert_eq!(
            plateau_middle(&[0.0, 0.5, 1.0, 1.0, 1.0, 0.4]),
            Some((3, 1.0))
        );
        assert_eq!(plateau_middle(&[0.2, 0.9, 0.3]), Some((1, 0.9)));
        assert_eq!(plateau_middle(&[]), None);
    }

    #[test]
    fn matte_marks_find_the_switch_either_way_round() {
        let rising = [0.0, 0.0, 0.1, 0.4, 0.6, 0.9, 1.0, 1.0];
        assert_eq!(matte_marks(&rising), (Some(4), Some(2), Some(6)));
        let falling = [1.0, 1.0, 0.7, 0.3, 0.0];
        assert_eq!(matte_marks(&falling), (Some(3), Some(2), Some(4)));
        assert_eq!(matte_marks(&[0.0, 0.0]), (None, None, None));
    }
}
