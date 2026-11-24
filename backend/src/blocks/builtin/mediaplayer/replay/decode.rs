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

/// Pick the best available H.264 decoder. `STROM_REPLAY_DECODER` overrides
/// the choice (diagnostics), when that element exists.
pub fn pick_decoder() -> &'static str {
    if let Some(name) = strom_types::env::var_opt("STROM_REPLAY_DECODER") {
        if gst::ElementFactory::find(&name).is_some() {
            return Box::leak(name.into_boxed_str());
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
}

/// Decode one segment held in memory.
pub fn decode_bytes(
    bytes: &[u8],
    container: Option<&str>,
    decoder: &str,
) -> Result<Decoded, String> {
    let (demux, src_caps) = demux_for(container);
    let desc = format!(
        "appsrc name=src format=bytes is-live=false caps=\"{}\" ! {} ! h264parse ! {} ! \
         videoconvert ! video/x-raw,format=NV12 ! appsink name=sink sync=false",
        src_caps, demux, decoder
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or("no appsrc")?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("no appsink")?;
    // A pipeline outside a flow: give its pool threads normal scheduling.
    crate::gst::thread_priority::reset_streaming_threads(&pipeline);
    // The whole object as one buffer, then end of stream.
    src.set_max_bytes(bytes.len() as u64 + 1);
    src.push_buffer(gst::Buffer::from_slice(bytes.to_vec()))
        .map_err(|e| format!("push: {:?}", e))?;
    let _ = src.end_of_stream();
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    let mut frames = Vec::with_capacity(64);
    let mut caps = None;
    while let Ok(sample) = sink.pull_sample() {
        if caps.is_none() {
            caps = sample.caps_owned();
        }
        if let Some(b) = sample.buffer_owned() {
            if let Some(pts) = b.pts() {
                frames.push((pts.nseconds(), b));
            }
        }
    }
    let err = pipeline.bus().and_then(|bus| {
        bus.pop_filtered(&[gst::MessageType::Error])
            .map(|m| match m.view() {
                gst::MessageView::Error(e) => e.error().to_string(),
                _ => String::new(),
            })
    });
    let _ = pipeline.set_state(gst::State::Null);
    if let Some(e) = err {
        return Err(e);
    }
    let caps = caps.ok_or("no frames decoded")?;
    Ok(Decoded { frames, caps })
}

/// Place decoded frames on the grid: a frame's TAI is the segment's first
/// sample TAI plus its PTS distance from the earliest PTS in the object.
/// Frames outside the segment's timerange (partial use of an object) are
/// dropped. Container PTS is never taken as TAI: the segment timerange is the
/// link between the two (TS PTS wraps at 33 bits, demuxers rebase it).
pub fn map_to_grid<T>(seg: &Segment, grid: &FrameGrid, frames: Vec<(u64, T)>) -> Vec<(i64, T)> {
    let Some(min_pts) = frames.iter().map(|(p, _)| *p).min() else {
        return Vec::new();
    };
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
        let out = map_to_grid(&s, &grid, frames);
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
        let out = map_to_grid(&s, &grid, frames);
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
        let out = map_to_grid(&s, &grid, frames);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], (k0, 50));
        assert_eq!(out[49], (k0 + 49, 99));
    }

    #[test]
    fn empty_input() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let out = map_to_grid::<u8>(&seg(BASE, BASE + 1), &grid, vec![]);
        assert!(out.is_empty());
    }
}
