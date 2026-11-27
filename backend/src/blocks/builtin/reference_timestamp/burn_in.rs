//! Element that burns a buffer's reference timestamp into the picture, as UTC
//! text and as a machine-readable code row.
//!
//! It renders the same reference timestamp TAMS Output anchors on (the first
//! `timestamp/x-tai`, `x-ptp`, `x-unix` or `x-ntp` meta), converted to UTC, so a
//! frame read back from a store can be checked against the time the store gives
//! it, by eye and automatically. Frames without such a meta pass untouched.
//!
//! Layout (all sizes derive from the frame size, see [`Layout`]): a black band
//! across the full width, holding
//! - a code row of [`CODE_CELLS`] cells, left to right: bit `i` (LSB first) of the
//!   UTC time in microseconds since 1970 for `i < CODE_BITS`, then a fixed
//!   alternating sync pattern (1, 0, 1, ...). White is 1, black is 0;
//! - below it the text `YYYY-MM-DD HH:MM:SS.mmm UTC` (milliseconds truncated).
//!
//! Only the 8-bit luma plane carries the drawing; the band's chroma is set
//! neutral. Accepts 8-bit planar and semi-planar YUV.

use crate::tams::tai_mapping::{reference_tai_ns, HostTai};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::subclass::prelude::*;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use gstreamer_video::subclass::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Cells in the code row.
pub const CODE_CELLS: u32 = 64;
/// Cells that carry the time (UTC microseconds since 1970, LSB first).
pub const CODE_BITS: u32 = 52;

const WHITE: u8 = 235;
const BLACK: u8 = 16;
const NEUTRAL_CHROMA: u8 = 128;

/// Where the band, the code row and the text go in a `width` x `height` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub band_y: u32,
    pub band_h: u32,
    pub code_y: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    pub text_x: u32,
    pub text_y: u32,
    /// Pixels per font dot.
    pub scale: u32,
}

impl Layout {
    pub fn for_frame(width: u32, height: u32) -> Self {
        let cell_w = (width / CODE_CELLS).max(1);
        let cell_h = (height / 40).max(4);
        let scale = (height / 180).max(1);
        let pad = scale * 2;
        let code_y = height / 12;
        let text_y = code_y + cell_h + pad;
        let band_y = code_y.saturating_sub(pad);
        let band_end = (text_y + GLYPH_H * scale + pad).min(height);
        Self {
            band_y,
            band_h: band_end.saturating_sub(band_y),
            code_y,
            cell_w,
            cell_h,
            text_x: pad,
            text_y,
            scale,
        }
    }
}

/// The value of each code cell for a time in UTC microseconds.
pub fn code_cells(utc_us: u64) -> [bool; CODE_CELLS as usize] {
    let mut cells = [false; CODE_CELLS as usize];
    for (i, cell) in cells.iter_mut().enumerate() {
        let i = i as u32;
        *cell = if i < CODE_BITS {
            (utc_us >> i) & 1 == 1
        } else {
            (i - CODE_BITS).is_multiple_of(2)
        };
    }
    cells
}

/// Decode a code row back to UTC microseconds. `None` when the sync pattern does
/// not match.
pub fn decode_cells(cells: &[bool]) -> Option<u64> {
    if cells.len() != CODE_CELLS as usize {
        return None;
    }
    let sync_ok = cells[CODE_BITS as usize..]
        .iter()
        .enumerate()
        .all(|(i, &c)| c == (i % 2 == 0));
    if !sync_ok {
        return None;
    }
    Some(
        cells[..CODE_BITS as usize]
            .iter()
            .enumerate()
            .fold(0u64, |v, (i, &c)| v | ((c as u64) << i)),
    )
}

/// `YYYY-MM-DD HH:MM:SS.mmm UTC` for UTC nanoseconds since 1970.
pub fn utc_text(utc_ns: u64) -> String {
    match i64::try_from(utc_ns) {
        Ok(ns) => format!(
            "{} UTC",
            strom_types::tai::format_calendar_ms(ns).replacen('T', " ", 1)
        ),
        Err(_) => String::from("----------"),
    }
}

const GLYPH_W: u32 = 5;
const GLYPH_H: u32 = 7;
/// Advance per character, in font dots.
const ADVANCE: u32 = GLYPH_W + 1;

/// 5x7 glyphs, one byte per row, bit 4 is the leftmost dot.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        _ => [0; 7],
    }
}

/// How a format stores chroma: (plane, horizontal and vertical subsampling shift,
/// bytes per chroma sample in that plane).
fn chroma_planes(format: gst_video::VideoFormat) -> &'static [(u32, u32, u32, u32)] {
    use gst_video::VideoFormat as F;
    match format {
        F::I420 | F::Yv12 => &[(1, 1, 1, 1), (2, 1, 1, 1)],
        F::Y42b => &[(1, 1, 0, 1), (2, 1, 0, 1)],
        F::Y444 => &[(1, 0, 0, 1), (2, 0, 0, 1)],
        F::Nv12 | F::Nv21 => &[(1, 1, 1, 2)],
        _ => &[],
    }
}

/// Draw the band for `utc_ns` into the planes of a frame.
fn draw(frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>, utc_ns: u64) {
    let width = frame.width();
    let height = frame.height();
    let format = frame.format();
    let layout = Layout::for_frame(width, height);
    let strides: Vec<usize> = frame.plane_stride().iter().map(|&s| s as usize).collect();

    if let Ok(luma) = frame.plane_data_mut(0) {
        let stride = strides[0];
        let mut fill = |x0: u32, y0: u32, w: u32, h: u32, v: u8| {
            let x1 = (x0 + w).min(width) as usize;
            for y in y0..(y0 + h).min(height) {
                let row = y as usize * stride;
                if let Some(r) = luma.get_mut(row + x0 as usize..row + x1) {
                    r.fill(v);
                }
            }
        };
        fill(0, layout.band_y, width, layout.band_h, BLACK);
        for (i, on) in code_cells(utc_ns / 1_000).iter().enumerate() {
            if *on {
                fill(
                    i as u32 * layout.cell_w,
                    layout.code_y,
                    layout.cell_w,
                    layout.cell_h,
                    WHITE,
                );
            }
        }
        let s = layout.scale;
        for (n, c) in utc_text(utc_ns).chars().enumerate() {
            let gx = layout.text_x + n as u32 * ADVANCE * s;
            for (row, bits) in glyph(c).iter().enumerate() {
                for col in 0..GLYPH_W {
                    if bits & (1 << (GLYPH_W - 1 - col)) != 0 {
                        fill(gx + col * s, layout.text_y + row as u32 * s, s, s, WHITE);
                    }
                }
            }
        }
    }

    for &(plane, xs, ys, bytes) in chroma_planes(format) {
        let stride = strides[plane as usize];
        let Ok(data) = frame.plane_data_mut(plane) else {
            continue;
        };
        let x1 = (((width + (1 << xs) - 1) >> xs) * bytes) as usize;
        let y0 = layout.band_y >> ys;
        let y1 = (layout.band_y + layout.band_h).div_ceil(1 << ys);
        for y in y0..y1 {
            let row = y as usize * stride;
            if let Some(r) = data.get_mut(row..row + x1) {
                r.fill(NEUTRAL_CHROMA);
            }
        }
    }
}

glib::wrapper! {
    pub struct TimestampBurnIn(ObjectSubclass<imp::TimestampBurnIn>)
        @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

impl TimestampBurnIn {
    pub fn new(name: &str) -> Self {
        glib::Object::builder().property("name", name).build()
    }
}

mod imp {
    use super::*;
    use std::sync::LazyLock;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "stromtimestampburnin",
            gst::DebugColorFlags::empty(),
            Some("Strom reference timestamp burn-in"),
        )
    });

    #[derive(Default)]
    pub struct TimestampBurnIn {
        /// TAI - UTC of this host, sampled on start.
        tai_minus_utc_ns: AtomicU64,
        warned_missing: AtomicBool,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TimestampBurnIn {
        const NAME: &'static str = "StromTimestampBurnIn";
        type Type = super::TimestampBurnIn;
        type ParentType = gst_video::VideoFilter;
    }

    impl ObjectImpl for TimestampBurnIn {}
    impl GstObjectImpl for TimestampBurnIn {}

    impl ElementImpl for TimestampBurnIn {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                use gst_video::VideoFormat as F;
                let caps = gst_video::VideoCapsBuilder::new()
                    .format_list([F::I420, F::Yv12, F::Nv12, F::Nv21, F::Y42b, F::Y444])
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .unwrap(),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for TimestampBurnIn {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn start(&self) -> Result<(), gst::ErrorMessage> {
            self.tai_minus_utc_ns
                .store(HostTai::new().tai_minus_utc_ns(), Ordering::Relaxed);
            self.warned_missing.store(false, Ordering::Relaxed);
            Ok(())
        }
    }

    impl VideoFilterImpl for TimestampBurnIn {
        fn transform_frame_ip(
            &self,
            frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let off = self.tai_minus_utc_ns.load(Ordering::Relaxed);
            let Some(tai_ns) = reference_tai_ns(frame.buffer(), off) else {
                if !self.warned_missing.swap(true, Ordering::Relaxed) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "buffer without a reference timestamp, nothing to burn in"
                    );
                }
                return Ok(gst::FlowSuccess::Ok);
            };
            draw(frame, tai_ns.saturating_sub(off));
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trips() {
        let us = 1_795_525_718_668_455u64;
        let cells = code_cells(us);
        assert_eq!(decode_cells(&cells), Some(us));
        let mut broken = cells;
        broken[CODE_BITS as usize] = false;
        assert_eq!(decode_cells(&broken), None);
    }

    #[test]
    fn text_truncates_to_milliseconds() {
        // 2026-11-24 14:08:38.680999 UTC
        assert_eq!(
            utc_text(1_795_529_318_680_999_000),
            "2026-11-24 14:08:38.680 UTC"
        );
        assert_eq!(utc_text(0), "1970-01-01 00:00:00.000 UTC");
    }

    #[test]
    fn layout_fits_the_frame() {
        for (w, h) in [(1920, 1080), (1280, 720), (320, 180), (64, 48)] {
            let l = Layout::for_frame(w, h);
            assert!(l.band_y + l.band_h <= h, "{w}x{h}: {l:?}");
            assert!(l.cell_w * CODE_CELLS <= w);
            assert!(l.code_y >= l.band_y && l.code_y + l.cell_h <= l.band_y + l.band_h);
        }
    }
}
