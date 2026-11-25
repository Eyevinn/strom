//! Decoding of one segment's coded bytes into raw frames.
//!
//! One short-lived pipeline per segment, fed from memory through `appsrc`
//! (the spike's model; a pool of long-lived decoders comes later). The demuxer
//! is chosen from the container type; frames come out as NV12 in presentation
//! order with their container PTS, which [`map_to_grid`] turns into frames on
//! the group's TAI grid.

use super::grid::FrameGrid;
use super::source::Segment;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Pick the best available H.264 decoder. `STROM_REPLAY_DECODER` overrides
/// the choice (diagnostics), when that element exists.
pub fn pick_decoder() -> &'static str {
    static OVERRIDE: OnceLock<Option<String>> = OnceLock::new();
    let wanted = OVERRIDE.get_or_init(|| strom_types::env::var_opt("STROM_REPLAY_DECODER"));
    if let Some(name) = wanted {
        if gst::ElementFactory::find(name).is_some() {
            return name;
        }
    }
    // vtdec_hw comes after avdec_h264: in replay it sometimes drops the
    // last frame of a segment (docs/UPSTREAM_NOTES.md), and per-segment
    // decodes were slower with it (seek outside cache p50 235 vs 134 ms).
    for name in ["nvh264dec", "vah264dec", "avdec_h264", "vtdec_hw"] {
        if gst::ElementFactory::find(name).is_some() {
            return name;
        }
    }
    "decodebin"
}

/// Demuxer and appsrc caps for a container MIME type.
fn demux_for(container: Option<&str>) -> (&'static str, &'static str) {
    match container {
        Some("video/mp4") | Some("video/quicktime") => ("qtdemux", "video/quicktime"),
        // MPEG-TS is the default: the spike material and TAMS Output's muxed mode.
        _ => (
            "tsdemux",
            "video/mpegts,systemstream=(boolean)true,packetsize=(int)188",
        ),
    }
}

/// Decoded frames with their container PTS, and the output caps.
pub struct Decoded {
    pub frames: Vec<(u64, gst::Buffer)>,
    pub caps: gst::Caps,
    /// Earliest PTS among the demuxed access units, before decoding: the
    /// object's first sample, even when the decoder drops leading pictures.
    pub first_pts: Option<u64>,
}

/// How long a decode may take before it is given up: five times the
/// segment's duration, at least 5 s and at most 60 s.
pub fn decode_timeout(segment_ns: i64) -> Duration {
    Duration::from_nanos((segment_ns.max(0) as u64).saturating_mul(5))
        .clamp(Duration::from_secs(5), Duration::from_secs(60))
}

/// Decode one segment held in memory. Fails on a pipeline error, or when the
/// stream has not ended within `timeout`: a stream that errors before its end
/// (an unlinkable codec, no video stream, garbage) never reaches the appsink
/// as end of stream, so waiting for it alone would block for ever.
pub fn decode_bytes(
    bytes: Arc<Vec<u8>>,
    container: Option<&str>,
    decoder: &str,
    timeout: Duration,
) -> Result<Decoded, String> {
    let (demux, src_caps) = demux_for(container);
    let desc = format!(
        "appsrc name=src format=bytes is-live=false caps=\"{}\" ! {} ! h264parse name=parse ! {} ! \
         videoconvert ! video/x-raw,format=NV12 ! appsink name=sink sync=false",
        src_caps, demux, decoder
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let result = run_decode(&pipeline, bytes, timeout);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Segment bytes shared with the cache, handed to GStreamer without a copy.
struct SharedBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

fn run_decode(
    pipeline: &gst::Pipeline,
    bytes: Arc<Vec<u8>>,
    timeout: Duration,
) -> Result<Decoded, String> {
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or("no appsrc")?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("no appsink")?;
    let bus = pipeline.bus().ok_or("no bus")?;
    // Earliest PTS the parser passes on (per buffer: an atomic only).
    let first_pts = Arc::new(AtomicU64::new(u64::MAX));
    if let Some(pad) = pipeline.by_name("parse").and_then(|p| p.static_pad("src")) {
        let first = Arc::clone(&first_pts);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                first.fetch_min(pts.nseconds(), Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }
    // A pipeline outside a flow: give its pool threads normal scheduling.
    crate::gst::thread_priority::reset_streaming_threads(pipeline);
    // The whole object as one buffer, then end of stream.
    src.set_max_bytes(bytes.len() as u64 + 1);
    src.push_buffer(gst::Buffer::from_slice(SharedBytes(bytes)))
        .map_err(|e| format!("push: {:?}", e))?;
    let _ = src.end_of_stream();
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    let bus_error = || {
        bus.pop_filtered(&[gst::MessageType::Error])
            .map(|m| match m.view() {
                gst::MessageView::Error(e) => e.error().to_string(),
                _ => String::new(),
            })
    };
    let deadline = Instant::now() + timeout;
    let mut frames = Vec::with_capacity(64);
    let mut caps = None;
    loop {
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
            if caps.is_none() {
                caps = sample.caps_owned();
            }
            if let Some(b) = sample.buffer_owned() {
                if let Some(pts) = b.pts() {
                    frames.push((pts.nseconds(), b));
                }
            }
            continue;
        }
        if sink.is_eos() {
            break;
        }
        if let Some(e) = bus_error() {
            return Err(e);
        }
        if Instant::now() >= deadline {
            return Err(format!("no end of stream within {:?}", timeout));
        }
    }
    if let Some(e) = bus_error() {
        return Err(e);
    }
    let caps = caps.ok_or("no frames decoded")?;
    let first_pts = Some(first_pts.load(Ordering::Relaxed)).filter(|p| *p != u64::MAX);
    Ok(Decoded {
        frames,
        caps,
        first_pts,
    })
}

/// Place decoded frames on the grid: a frame's TAI is the segment's first
/// sample TAI plus its PTS distance from the object's first PTS. That is
/// `first_pts` (the earliest demuxed PTS) when known, so pictures the decoder
/// drops at the start (open GOP, a segment not starting on a keyframe) do not
/// shift the rest; else the earliest decoded PTS. Frames outside the
/// segment's timerange (partial use of an object) are dropped. Container PTS
/// is never taken as TAI: the segment timerange is the link between the two
/// (TS PTS wraps at 33 bits, demuxers rebase it).
pub fn map_to_grid<T>(
    seg: &Segment,
    grid: &FrameGrid,
    frames: Vec<(u64, T)>,
    first_pts: Option<u64>,
) -> Vec<(i64, T)> {
    let Some(decoded_min) = frames.iter().map(|(p, _)| *p).min() else {
        return Vec::new();
    };
    let min_pts = first_pts.map_or(decoded_min, |p| p.min(decoded_min));
    let half = (grid.frame_ns() / 2.0) as i64;
    let base = seg.first_sample_tai();
    let mut out: Vec<(i64, T)> = frames
        .into_iter()
        .filter_map(|(pts, f)| {
            let tai = base + (pts - min_pts) as i64;
            (tai >= seg.start_ns - half && tai < seg.end_ns - half)
                .then(|| (grid.nearest_frame(tai), f))
        })
        .collect();
    out.sort_by_key(|(k, _)| *k);
    out.dedup_by_key(|(k, _)| *k);
    out
}

/// Frame rate from decoded caps, if fixed and non-zero.
pub fn caps_frame_rate(caps: &gst::Caps) -> Option<(u32, u32)> {
    let s = caps.structure(0)?;
    let f = s.get::<gst::Fraction>("framerate").ok()?;
    (f.numer() > 0 && f.denom() > 0).then(|| (f.numer() as u32, f.denom() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::source::Location;

    const BASE: i64 = 1_800_000_000_000_000_000;

    fn seg(start: i64, end: i64) -> Segment {
        Segment {
            start_ns: start,
            end_ns: end,
            ts_offset_ns: 0,
            object_start_ns: None,
            object_id: "o".into(),
            location: Location::Urls(vec![]),
        }
    }

    #[test]
    fn frames_map_from_segment_start_not_pts() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // Container PTS starts at an arbitrary 3600 s (tsdemux rebasing).
        let pts0 = 3_600_000_000_000u64;
        let frames: Vec<(u64, u32)> = (0..50).map(|i| (pts0 + i * 20_000_000, i as u32)).collect();
        let s = seg(BASE + 1_000_000_000, BASE + 2_000_000_000);
        let out = map_to_grid(&s, &grid, frames, None);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], (k0 + 50, 0));
        assert_eq!(out[49], (k0 + 99, 49));
    }

    #[test]
    fn jittered_pts_land_on_the_same_frames() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        // 90 kHz rounding: 20 ms = 1800 ticks, but 59.94-style drift of a few
        // µs must not move a frame.
        let frames: Vec<(u64, u32)> = (0..50)
            .map(|i| {
                (
                    i * 20_000_000 + if i % 2 == 0 { 11_111 } else { 0 },
                    i as u32,
                )
            })
            .collect();
        let s = seg(BASE, BASE + 1_000_000_000);
        let out = map_to_grid(&s, &grid, frames, None);
        let ks: Vec<i64> = out
            .iter()
            .map(|(k, _)| k - grid.floor_frame(BASE))
            .collect();
        assert_eq!(ks, (0..50).collect::<Vec<i64>>());
    }

    #[test]
    fn partial_object_use_drops_frames_outside_timerange() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // Object holds 2 s; the segment uses its second half, retimed by
        // ts_offset so that object time 1 s is flow time BASE.
        let mut s = seg(BASE, BASE + 1_000_000_000);
        s.ts_offset_ns = BASE - 1_000_000_000;
        s.object_start_ns = Some(0);
        let frames: Vec<(u64, u32)> = (0..100).map(|i| (i * 20_000_000, i as u32)).collect();
        let out = map_to_grid(&s, &grid, frames, None);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], (k0, 50));
        assert_eq!(out[49], (k0 + 49, 99));
    }

    #[test]
    fn dropped_leading_pictures_do_not_shift_the_segment() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // The object starts at PTS 0, but the decoder dropped its first two
        // pictures (leading B-frames of an open GOP).
        let frames: Vec<(u64, u32)> = (2..50).map(|i| (i * 20_000_000, i as u32)).collect();
        let s = seg(BASE, BASE + 1_000_000_000);
        let out = map_to_grid(&s, &grid, frames, Some(0));
        assert_eq!(out.len(), 48);
        assert_eq!(out[0], (k0 + 2, 2));
        assert_eq!(out[47], (k0 + 49, 49));
    }

    #[test]
    fn empty_input() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let out = map_to_grid::<u8>(&seg(BASE, BASE + 1), &grid, vec![], None);
        assert!(out.is_empty());
    }

    /// MPEG-TS of `frames` frames of 320x240 at 25 fps, encoded with
    /// `encoder` (with B-frames for H.264).
    pub(super) fn test_ts(encoder: &str, frames: u32) -> Vec<u8> {
        gst::init().unwrap();
        let (enc, parse) = match encoder {
            "x265enc" => ("x265enc key-int-max=25", "h265parse"),
            _ => ("x264enc bframes=2 key-int-max=25", "h264parse"),
        };
        let desc = format!(
            "videotestsrc num-buffers={} ! video/x-raw,width=320,height=240,framerate=25/1 ! \
             {} ! {} ! mpegtsmux ! appsink name=sink sync=false",
            frames, enc, parse
        );
        let p = gst::parse::launch(&desc)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let sink = p
            .by_name("sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let mut out = Vec::new();
        while let Ok(s) = sink.pull_sample() {
            out.extend_from_slice(&s.buffer().unwrap().map_readable().unwrap());
        }
        p.set_state(gst::State::Null).unwrap();
        out
    }

    /// Run a decode on another thread; `None` if it has not returned in 30 s.
    fn decode_in_thread(bytes: Vec<u8>, timeout: Duration) -> Option<Result<Decoded, String>> {
        gst::init().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(decode_bytes(Arc::new(bytes), None, pick_decoder(), timeout));
        });
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    #[test]
    fn h264_ts_decodes() {
        let d = decode_in_thread(test_ts("x264enc", 25), Duration::from_secs(10))
            .expect("decode returned")
            .expect("decode ok");
        assert_eq!(d.frames.len(), 25);
        let decoded_min = d.frames.iter().map(|f| f.0).min();
        assert_eq!(d.first_pts, decoded_min);
        assert_eq!(caps_frame_rate(&d.caps), Some((25, 1)));
    }

    #[test]
    fn garbage_fails_instead_of_hanging() {
        let bytes: Vec<u8> = (0..188 * 200u32).map(|i| (i * 7919 % 251) as u8).collect();
        let r = decode_in_thread(bytes, Duration::from_secs(5)).expect("decode returned");
        assert!(r.is_err());
    }

    #[test]
    fn empty_input_fails_instead_of_hanging() {
        let r = decode_in_thread(Vec::new(), Duration::from_secs(5)).expect("decode returned");
        assert!(r.is_err());
    }

    #[test]
    fn h265_ts_fails_instead_of_hanging() {
        // tsdemux's H.265 pad cannot link to h264parse: the stream stops
        // before its end of stream reaches the appsink.
        let r = decode_in_thread(test_ts("x265enc", 25), Duration::from_secs(5))
            .expect("decode returned");
        assert!(r.is_err());
    }

    #[test]
    fn timeout_scales_with_segment_length() {
        assert_eq!(decode_timeout(1_000_000_000), Duration::from_secs(5));
        assert_eq!(decode_timeout(10_000_000_000), Duration::from_secs(50));
        assert_eq!(decode_timeout(i64::MAX), Duration::from_secs(60));
        assert_eq!(decode_timeout(-1), Duration::from_secs(5));
    }
}
