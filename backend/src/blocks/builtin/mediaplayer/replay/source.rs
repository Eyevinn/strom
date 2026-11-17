//! Segment index for replay mode (spike).
//!
//! Reads a TAMS-like `index.json` from a directory of MPEG-TS segments. Every
//! angle of a replay group shares one timeline, so a frame number means the
//! same instant on every angle.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct RawRate {
    numerator: u32,
    denominator: u32,
}

#[derive(Deserialize)]
struct RawSegment {
    object: String,
    start_ns: u64,
    end_ns: u64,
}

#[derive(Deserialize)]
struct RawIndex {
    frame_rate: RawRate,
    tai_base_ns: u64,
    segments: Vec<RawSegment>,
}

/// One segment file and the frames it holds on the shared timeline.
#[derive(Debug, Clone)]
pub struct Segment {
    pub path: PathBuf,
    pub first_frame: u64,
    pub frames: u64,
}

/// All segments of one angle, ordered by time.
#[derive(Debug)]
pub struct SegmentIndex {
    pub fps_n: i32,
    pub fps_d: i32,
    /// Nanoseconds per frame.
    pub frame_ns: f64,
    pub segments: Vec<Segment>,
    pub total_frames: u64,
}

impl SegmentIndex {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("index.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let raw: RawIndex = serde_json::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {}", path.display(), e))?;
        if raw.frame_rate.numerator == 0 || raw.frame_rate.denominator == 0 {
            return Err("index.json: frame_rate must be non-zero".to_string());
        }
        let frame_ns = 1e9 * raw.frame_rate.denominator as f64 / raw.frame_rate.numerator as f64;
        let mut segments = Vec::with_capacity(raw.segments.len());
        for s in raw.segments {
            let first_frame =
                ((s.start_ns.saturating_sub(raw.tai_base_ns)) as f64 / frame_ns).round() as u64;
            let frames = ((s.end_ns.saturating_sub(s.start_ns)) as f64 / frame_ns).round() as u64;
            segments.push(Segment {
                path: dir.join(&s.object),
                first_frame,
                frames,
            });
        }
        segments.sort_by_key(|s| s.first_frame);
        let total_frames = segments
            .last()
            .map(|s| s.first_frame + s.frames)
            .unwrap_or(0);
        if total_frames == 0 {
            return Err(format!("{}: no segments", path.display()));
        }
        Ok(Self {
            fps_n: raw.frame_rate.numerator as i32,
            fps_d: raw.frame_rate.denominator as i32,
            frame_ns,
            segments,
            total_frames,
        })
    }

    /// Index of the segment holding `frame`, if any.
    pub fn segment_of(&self, frame: u64) -> Option<usize> {
        let i = self.segments.partition_point(|s| s.first_frame <= frame);
        if i == 0 {
            return None;
        }
        let s = &self.segments[i - 1];
        (frame < s.first_frame + s.frames).then_some(i - 1)
    }
}
