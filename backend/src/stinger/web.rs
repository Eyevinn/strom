//! Page stingers: an HTML Input whose page plays the stinger.
//!
//! A clip's first frame lands where the take pins it. A page starts its
//! animation when the take changes its URL fragment (`hashchange`), and
//! Chromium paints its first frame some time after that. So a page take
//! raises the graphic pad first (the page is transparent at rest), triggers
//! the page, watches the page's own output for its first new frame, and
//! plans the take from there ([`WebAnchor`], [`web_stinger_start`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::stinger::{
    StingerLayout, StingerVariant, WEB_STINGER_CUT_POINT_PROPERTY, WEB_STINGER_DURATION_PROPERTY,
    WEB_STINGER_MIX_PROPERTY,
};
use strom_types::PropertyValue;

use super::{ClipPlan, FrameGrid};

/// How soon after a take a page's first frame must arrive to be trusted as
/// the start of its animation. See [`web_stinger_start`].
pub const PAGE_FIRST_FRAME_GRACE: Duration = Duration::from_millis(100);

/// Delay from a take to the timestamp a page's first animation frame would
/// carry, used when the page paints nothing prompt to time from. Measured on
/// cefsrc's own output: about 28 ms on the fixed-rate production gstcefsrc
/// (Linux, software CEF). About 20 ms on one that emits frames as Chromium
/// paints, not measured on this probe.
pub const PAGE_FIRST_FRAME_DELAY_PAINTED: Duration = Duration::from_millis(20);
pub const PAGE_FIRST_FRAME_DELAY_FIXED_RATE: Duration = Duration::from_millis(30);

/// gstcefsrc property present only on builds that emit frames as Chromium
/// paints, rather than at a fixed rate.
pub const VARIABLE_RATE_PROPERTY: &str = "max-video-framerate";

/// The URL fragment a take sets to trigger a page.
pub fn take_fragment(token: u64) -> String {
    format!("strom-take-{token}")
}

/// A stinger page's URL cannot carry a fragment of its own: a take replaces
/// it, which would move a page that routes by its fragment off its route.
pub fn check_page_url(url: &str) -> Result<(), String> {
    match url.split_once('#') {
        Some((_, fragment)) if !fragment.is_empty() => Err(format!(
            "a stinger page's URL cannot have a #fragment ('#{}'): a take sets the fragment \
             to trigger the page",
            fragment
        )),
        _ => Ok(()),
    }
}

/// A page stinger's settings, from its HTML Input block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebStingerSettings {
    pub duration_ms: u64,
    /// 0 = the middle of the duration.
    pub cut_point_ms: u64,
    pub mix_ms: u64,
}

impl WebStingerSettings {
    pub fn from_properties(props: &HashMap<String, PropertyValue>) -> Self {
        let ms = |name: &str| match props.get(name) {
            Some(PropertyValue::UInt(v)) => *v,
            Some(PropertyValue::Int(v)) if *v > 0 => *v as u64,
            _ => 0,
        };
        Self {
            duration_ms: ms(WEB_STINGER_DURATION_PROPERTY),
            cut_point_ms: ms(WEB_STINGER_CUT_POINT_PROPERTY),
            mix_ms: ms(WEB_STINGER_MIX_PROPERTY),
        }
    }

    /// The earliest cut point a page take can program in time on a mixer
    /// with frames of `frame_ns`: the take waits up to the grace period for
    /// the page's first frame, and needs a couple of frames to program the
    /// mixer after that.
    pub fn min_cut_point_ms(frame_ns: u64) -> u64 {
        (PAGE_FIRST_FRAME_GRACE.as_nanos() as u64 + 2 * frame_ns).div_ceil(1_000_000)
    }

    /// How a take plays this page on a mixer with frames of `frame_ns`. A
    /// page is a classic stinger, premultiplied as Chromium renders it.
    pub fn plan(&self, frame_ns: u64) -> Result<ClipPlan, String> {
        if self.duration_ms == 0 {
            return Err("the stinger page has no duration (set Stinger Duration)".to_string());
        }
        let cut = if self.cut_point_ms == 0 {
            self.duration_ms / 2
        } else {
            self.cut_point_ms
        };
        if cut >= self.duration_ms {
            return Err(format!(
                "the stinger page's cut point ({} ms) must come before its end ({} ms)",
                cut, self.duration_ms
            ));
        }
        let min = Self::min_cut_point_ms(frame_ns);
        if cut < min {
            return Err(format!(
                "the stinger page's cut point ({} ms) is too early: a page take needs at \
                 least {} ms to find the page's first frame and program the cut",
                cut, min
            ));
        }
        Ok(ClipPlan {
            variant: StingerVariant::Classic,
            downgraded_from: None,
            layout: StingerLayout::Classic,
            duration_ms: self.duration_ms,
            cut_point_ms: Some(cut),
            mix_ms: self.mix_ms.min(self.duration_ms - cut),
            premultiplied: true,
            invert_matte: false,
        })
    }
}

/// Where a page stinger's animation starts, in the mixer's timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebStart {
    /// Too soon to tell: keep waiting for the page's first frame.
    Waiting,
    /// The page's first frame arrived promptly, and its timestamp is the
    /// start.
    FirstFrame(u64),
    /// No prompt frame, so the start is estimated from when the take changed
    /// the URL. The page may have painted nothing visible at first, or be
    /// slow.
    FromTake(u64),
}

/// Decide where a page stinger's animation starts.
///
/// Chromium paints only when pixels change, so a page's first frame marks
/// the start of its animation only if the animation changes something on
/// screen straight away. One that opens on invisible frames (an element
/// moving in from off frame, a fade from nothing, a delayed start) delivers
/// its first frame late, and anchoring on it would cut late by as much. Its
/// own clock started at `hashchange`, so a frame stamped more than
/// `grace_ns` after the take is ignored in favour of the take plus the usual
/// delivery delay.
///
/// All times are running times in ns. `first_frame_ns` is the first frame
/// of new content stamped at or after `taken_at_ns`. `output_ns` is the
/// newest timestamp seen on the output, repeats included: the grace period
/// ends when the output has moved past it, not when wall time has, because
/// a frame can be held between being painted and reaching the output.
pub fn web_stinger_start(
    first_frame_ns: Option<u64>,
    taken_at_ns: u64,
    output_ns: Option<u64>,
    grace_ns: u64,
    delay_ns: u64,
) -> WebStart {
    let deadline = taken_at_ns.saturating_add(grace_ns);
    match first_frame_ns {
        Some(frame) if frame <= deadline => WebStart::FirstFrame(frame),
        _ if output_ns.is_some_and(|out| out > deadline) => {
            WebStart::FromTake(taken_at_ns.saturating_add(delay_ns))
        }
        _ => WebStart::Waiting,
    }
}

/// The output frame a page frame stamped `first_frame_ns` first goes on
/// air in, for page frames `page_frame_ns` long.
///
/// For each output frame the mixer drops the queued buffers that end before
/// the frame starts and takes the oldest one left. Page frames at least an
/// output frame long are taken in the output frame their timestamp falls
/// in. Shorter ones (a page faster than the mixer) are taken from the first
/// output frame that starts inside them, so a page frame that starts and
/// ends within one output frame is dropped and its successor goes on air on
/// the next one. Which side of an output frame a page frame lands on is a
/// phase fixed when gstcefsrc starts, and differs from run to run.
pub fn web_first_output_frame(first_frame_ns: u64, page_frame_ns: u64, grid: FrameGrid) -> u64 {
    let unit = 1_000_000_000u128 * grid.den as u128;
    let mut n = (first_frame_ns as u128 * grid.num as u128 / unit) as u64;
    // `pts` rounds to the nanosecond, so step to the exact frame either side.
    while n > 0 && grid.pts(n) > first_frame_ns {
        n -= 1;
    }
    while grid.pts(n + 1) <= first_frame_ns {
        n += 1;
    }
    if page_frame_ns < grid.frame_ns() && grid.pts(n) < first_frame_ns {
        n += 1;
    }
    grid.pts(n)
}

/// Watches a page's output for its first new frame after a take, and for
/// paints once the take's page has gone back to rest.
///
/// On a fixed-rate gstcefsrc, wait for [`WebAnchor::ready`] before changing
/// the URL, then call [`WebAnchor::taken`]. Dropping it removes the probe.
pub struct WebAnchor {
    /// Weak, so a take in flight never keeps a stopped flow's pipeline alive.
    pad: gst::glib::WeakRef<gst::Pad>,
    probe: Option<gst::PadProbeId>,
    /// Buffers seen before the take.
    before_take: Arc<AtomicU32>,
    /// Running time at which the take changed the URL; `u64::MAX` before.
    taken_at_ns: Arc<AtomicU64>,
    /// Newest timestamp seen on the output, repeats included.
    output_pts: Arc<AtomicU64>,
    /// Timestamp of the first frame of new content after the take.
    first_pts: Arc<AtomicU64>,
    /// From this timestamp the page should be at rest; `u64::MAX` before
    /// it is known.
    rest_from: Arc<AtomicU64>,
    /// The window ends here, before anything the next take paints.
    rest_until: Arc<AtomicU64>,
    /// Fresh paints stamped in `[rest_from, rest_until)`.
    rest_paints: Arc<AtomicU32>,
    /// Assumed take-to-paint delay when the page paints nothing promptly.
    pub delay: Duration,
}

impl WebAnchor {
    /// Watch `pad`, a `cefsrc` src pad. cefsrc stamps its frames with the
    /// running time it makes them at, which every element down to the mixer
    /// keeps. `paint_only` is a gstcefsrc that sends a buffer only when the
    /// page paints.
    pub fn watch(pad: &gst::Pad, delay: Duration, paint_only: bool) -> Option<Self> {
        let before_take = Arc::new(AtomicU32::new(0));
        let taken_at_ns = Arc::new(AtomicU64::new(u64::MAX));
        let output_pts = Arc::new(AtomicU64::new(u64::MAX));
        let first_pts = Arc::new(AtomicU64::new(u64::MAX));
        let rest_from = Arc::new(AtomicU64::new(u64::MAX));
        let rest_until = Arc::new(AtomicU64::new(u64::MAX));
        let rest_paints = Arc::new(AtomicU32::new(0));
        let last_memory = AtomicUsize::new(0);
        let (p_before, p_taken, p_output, p_first) = (
            Arc::clone(&before_take),
            Arc::clone(&taken_at_ns),
            Arc::clone(&output_pts),
            Arc::clone(&first_pts),
        );
        let (p_rest_from, p_rest_until, p_rest) = (
            Arc::clone(&rest_from),
            Arc::clone(&rest_until),
            Arc::clone(&rest_paints),
        );
        // A per-buffer probe for the length of one take: a few atomic loads,
        // stores and compares per buffer. Only new content counts: a
        // GAP-flagged repeat, or a fixed-rate gstcefsrc re-sending its
        // current frame, shares the previous buffer's memory; a fresh paint
        // is a new allocation. A fixed-rate gstcefsrc's first buffer is only
        // the baseline, whenever it comes; a paint-only one sends nothing at
        // rest, so its first buffer is new.
        let probe = pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let memory = if buffer.n_memory() > 0 {
                buffer.peek_memory(0).as_ptr() as usize
            } else {
                0
            };
            let previous = last_memory.swap(memory, Ordering::Relaxed);
            let Some(pts) = buffer.pts().map(|t| t.nseconds()) else {
                return gst::PadProbeReturn::Ok;
            };
            let fresh = memory != previous
                && (previous != 0 || paint_only)
                && !buffer.flags().contains(gst::BufferFlags::GAP);
            let taken_at = p_taken.load(Ordering::Relaxed);
            if taken_at == u64::MAX {
                p_before.fetch_add(1, Ordering::Relaxed);
                return gst::PadProbeReturn::Ok;
            }
            p_output.store(pts, Ordering::Relaxed);
            if pts >= taken_at && fresh {
                let _ =
                    p_first.compare_exchange(u64::MAX, pts, Ordering::Relaxed, Ordering::Relaxed);
            }
            if fresh
                && pts >= p_rest_from.load(Ordering::Relaxed)
                && pts < p_rest_until.load(Ordering::Relaxed)
            {
                p_rest.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        })?;
        Some(Self {
            pad: pad.downgrade(),
            probe: Some(probe),
            before_take,
            taken_at_ns,
            output_pts,
            first_pts,
            rest_from,
            rest_until,
            rest_paints,
            delay,
        })
    }

    /// Wait, up to `limit`, for a fixed-rate gstcefsrc's first buffer, so a
    /// repeat of it after the take is not taken for a paint.
    pub async fn ready(&self, limit: Duration) {
        let until = std::time::Instant::now() + limit;
        while self.before_take.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Count paints stamped in `[from_ns, until_ns)`, when the page should
    /// be at rest again. An empty window counts nothing.
    pub fn expect_rest(&self, from_ns: u64, until_ns: u64) {
        self.rest_until.store(until_ns, Ordering::Relaxed);
        self.rest_from.store(from_ns, Ordering::Relaxed);
    }

    /// Whether the page kept painting in the [`Self::expect_rest`] window:
    /// two new frames or more. A page that keeps painting at rest makes its
    /// own rest paints look like the start of the next take's animation. A
    /// single paint is a page clearing itself as it ends.
    pub fn paints_at_rest(&self) -> bool {
        self.rest_paints.load(Ordering::Relaxed) > 1
    }

    /// Record that the take changed the page's URL at running time `now_ns`.
    pub fn taken(&self, now_ns: u64) {
        self.taken_at_ns.store(now_ns, Ordering::Relaxed);
    }

    pub fn taken_at_ns(&self) -> Option<u64> {
        load(&self.taken_at_ns)
    }

    /// The first new frame's timestamp, once one has arrived.
    pub fn first_frame_ns(&self) -> Option<u64> {
        load(&self.first_pts)
    }

    /// Where the page's animation started, once that can be told.
    pub fn start(&self) -> WebStart {
        let Some(taken_at) = self.taken_at_ns() else {
            return WebStart::Waiting;
        };
        web_stinger_start(
            self.first_frame_ns(),
            taken_at,
            load(&self.output_pts),
            PAGE_FIRST_FRAME_GRACE.as_nanos() as u64,
            self.delay.as_nanos() as u64,
        )
    }
}

fn load(a: &AtomicU64) -> Option<u64> {
    match a.load(Ordering::Relaxed) {
        u64::MAX => None,
        v => Some(v),
    }
}

impl Drop for WebAnchor {
    fn drop(&mut self) {
        if let (Some(probe), Some(pad)) = (self.probe.take(), self.pad.upgrade()) {
            pad.remove_probe(probe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn a_prompt_first_frame_is_the_start() {
        assert_eq!(
            web_stinger_start(
                Some(1_030 * MS),
                1_000 * MS,
                Some(1_030 * MS),
                100 * MS,
                20 * MS
            ),
            WebStart::FirstFrame(1_030 * MS)
        );
        // On the deadline still counts.
        assert_eq!(
            web_stinger_start(
                Some(1_100 * MS),
                1_000 * MS,
                Some(1_200 * MS),
                100 * MS,
                20 * MS
            ),
            WebStart::FirstFrame(1_100 * MS)
        );
    }

    #[test]
    fn a_late_first_frame_falls_back_to_the_take() {
        assert_eq!(
            web_stinger_start(
                Some(1_300 * MS),
                1_000 * MS,
                Some(1_300 * MS),
                100 * MS,
                20 * MS
            ),
            WebStart::FromTake(1_020 * MS)
        );
        // No frame at all, once the output has moved past the grace period.
        assert_eq!(
            web_stinger_start(None, 1_000 * MS, Some(1_101 * MS), 100 * MS, 120 * MS),
            WebStart::FromTake(1_120 * MS)
        );
    }

    #[test]
    fn it_waits_while_the_output_is_inside_the_grace_period() {
        assert_eq!(
            web_stinger_start(None, 1_000 * MS, Some(1_050 * MS), 100 * MS, 20 * MS),
            WebStart::Waiting
        );
        assert_eq!(
            web_stinger_start(None, 1_000 * MS, None, 100 * MS, 20 * MS),
            WebStart::Waiting
        );
    }

    #[test]
    fn a_page_frame_goes_on_air_in_the_output_frame_it_falls_in() {
        let grid = FrameGrid::new(30, 1).unwrap();
        let f = grid.frame_ns();
        let on_air = |t| web_first_output_frame(t, f, grid);
        assert_eq!(on_air(grid.pts(10) + f / 3), grid.pts(10));
        assert_eq!(on_air(grid.pts(10) + 2 * f / 3), grid.pts(10));
        assert_eq!(on_air(grid.pts(10)), grid.pts(10));
        assert_eq!(on_air(grid.pts(11) - 1), grid.pts(10));
        // Frame 2 of 1/30 s is stamped 66_666_667: a buffer a nanosecond
        // before it starts inside frame 1.
        assert_eq!(grid.pts(2), 66_666_667);
        assert_eq!(on_air(66_666_666), grid.pts(1));
        // An hour in, the grid's own frame is still the one returned.
        let n = 30 * 3600;
        assert_eq!(on_air(grid.pts(n) + 1), grid.pts(n));
        // A slower page too: its frames span more than one output frame.
        assert_eq!(
            web_first_output_frame(grid.pts(10) + 2 * f / 3, 2 * f, grid),
            grid.pts(10)
        );
        let ntsc = FrameGrid::new(30000, 1001).unwrap();
        let nf = ntsc.frame_ns();
        assert_eq!(
            web_first_output_frame(ntsc.pts(7) - 5, nf, ntsc),
            ntsc.pts(6)
        );
        assert_eq!(web_first_output_frame(ntsc.pts(7), nf, ntsc), ntsc.pts(7));
    }

    #[test]
    fn a_page_faster_than_the_mixer_goes_on_air_from_the_next_output_frame() {
        // Measured: a 60 fps page on a 30 fps mixer, stamped 8.3 or 24.6 ms
        // into an output frame, first showed on the next one.
        let grid = FrameGrid::new(30, 1).unwrap();
        let half = 16_666_667;
        for into in [8_300_000, 24_600_000] {
            assert_eq!(
                web_first_output_frame(grid.pts(60) + into, half, grid),
                grid.pts(61)
            );
        }
        // Stamped on an output frame, it is taken there.
        assert_eq!(
            web_first_output_frame(grid.pts(60), half, grid),
            grid.pts(60)
        );
    }

    fn settings(duration_ms: u64, cut_point_ms: u64, mix_ms: u64) -> WebStingerSettings {
        WebStingerSettings {
            duration_ms,
            cut_point_ms,
            mix_ms,
        }
    }

    const FRAME_30: u64 = 33_333_333;

    #[test]
    fn the_cut_point_must_leave_time_to_find_the_page_and_program_the_cut() {
        assert_eq!(WebStingerSettings::min_cut_point_ms(FRAME_30), 167);
        let err = settings(1_000, 150, 0).plan(FRAME_30).unwrap_err();
        assert!(err.contains("too early") && err.contains("167 ms"), "{err}");
        assert!(settings(1_000, 167, 0).plan(FRAME_30).is_ok());
        // At 60 fps the frames are shorter, so the floor is lower.
        assert_eq!(WebStingerSettings::min_cut_point_ms(16_666_667), 134);
    }

    #[test]
    fn the_cut_point_must_come_before_the_end() {
        let err = settings(1_000, 1_000, 0).plan(FRAME_30).unwrap_err();
        assert!(err.contains("before its end"), "{err}");
        assert!(settings(1_000, 1_500, 0).plan(FRAME_30).is_err());
        assert!(settings(1_000, 999, 0).plan(FRAME_30).is_ok());
    }

    #[test]
    fn a_page_needs_a_duration() {
        let err = settings(0, 500, 0).plan(FRAME_30).unwrap_err();
        assert!(err.contains("no duration"), "{err}");
    }

    #[test]
    fn a_page_plans_as_a_premultiplied_classic_stinger() {
        let plan = settings(1_200, 0, 0).plan(FRAME_30).unwrap();
        assert_eq!(plan.variant, StingerVariant::Classic);
        assert_eq!(plan.layout, StingerLayout::Classic);
        assert!(plan.premultiplied);
        assert_eq!(plan.cut_point_ms, Some(600), "0 takes the middle");
        assert_eq!(plan.mix_ms, 0);
        // A mix runs out with the page, not past it.
        let plan = settings(1_000, 700, 500).plan(FRAME_30).unwrap();
        assert_eq!(plan.mix_ms, 300);
    }

    /// A src pad linked to a sink that takes anything, to drive the probe.
    fn pads() -> (gst::Pad, gst::Pad) {
        gst::init().unwrap();
        let src = gst::Pad::builder(gst::PadDirection::Src).build();
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .chain_function(|_, _, _| Ok(gst::FlowSuccess::Ok))
            .build();
        src.link(&sink).unwrap();
        sink.set_active(true).unwrap();
        src.set_active(true).unwrap();
        (src, sink)
    }

    fn buffer(pts_ms: u64) -> gst::Buffer {
        let mut b = gst::Buffer::with_size(16).unwrap();
        b.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(pts_ms));
        b
    }

    /// The same frame again, as a fixed-rate gstcefsrc re-sends it.
    fn repeat(of: &gst::Buffer, pts_ms: u64) -> gst::Buffer {
        let mut b = of.copy();
        b.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(pts_ms));
        b
    }

    #[test]
    fn a_repeat_after_the_take_is_not_the_first_frame() {
        let (src, _sink) = pads();
        let anchor = WebAnchor::watch(&src, PAGE_FIRST_FRAME_DELAY_FIXED_RATE, false).unwrap();
        let rest = buffer(0);
        let _ = src.push(rest.clone());
        let _ = src.push(repeat(&rest, 33));
        anchor.taken(50 * MS);
        let _ = src.push(repeat(&rest, 66));
        assert_eq!(anchor.first_frame_ns(), None);
        let paint = buffer(100);
        let _ = src.push(paint.clone());
        assert_eq!(anchor.first_frame_ns(), Some(100 * MS));
        drop((rest, paint));
    }

    #[test]
    fn a_fixed_rate_page_silent_until_the_take_starts_on_its_first_paint() {
        // A fixed-rate gstcefsrc that sent nothing before the take resumes
        // with a repeat of its rest frame: that is the baseline, not a paint.
        let (src, _sink) = pads();
        let anchor = WebAnchor::watch(&src, PAGE_FIRST_FRAME_DELAY_FIXED_RATE, false).unwrap();
        anchor.taken(50 * MS);
        let rest = buffer(60);
        let _ = src.push(rest.clone());
        assert_eq!(anchor.first_frame_ns(), None);
        let paint = buffer(93);
        let _ = src.push(paint.clone());
        assert_eq!(anchor.first_frame_ns(), Some(93 * MS));
        drop((rest, paint));
    }

    #[test]
    fn a_paint_only_page_starts_on_its_first_buffer() {
        // A gstcefsrc that sends only paints sends nothing while the page
        // rests, so the first buffer after the take is the page's start.
        let (src, _sink) = pads();
        let anchor = WebAnchor::watch(&src, PAGE_FIRST_FRAME_DELAY_PAINTED, true).unwrap();
        anchor.taken(50 * MS);
        let _ = src.push(buffer(70));
        assert_eq!(anchor.first_frame_ns(), Some(70 * MS));
    }

    #[test]
    fn a_page_painting_after_its_take_is_noticed() {
        // Kept alive, as gstcefsrc keeps its current frame while it makes
        // the next: a freed frame's memory could be handed out again.
        let mut kept = Vec::new();
        let (src, _sink) = pads();
        let anchor = WebAnchor::watch(&src, PAGE_FIRST_FRAME_DELAY_FIXED_RATE, false).unwrap();
        kept.push(buffer(0));
        let _ = src.push(kept[0].clone());
        anchor.taken(50 * MS);
        anchor.expect_rest(1_100 * MS, 1_300 * MS);
        // Painting during the animation is expected.
        kept.push(buffer(100));
        let _ = src.push(kept[1].clone());
        kept.push(buffer(1_000));
        let _ = src.push(kept[2].clone());
        // Back at rest: a closing paint, then repeats.
        kept.push(buffer(1_133));
        let _ = src.push(kept[3].clone());
        let _ = src.push(repeat(&kept[3], 1_166));
        assert!(
            !anchor.paints_at_rest(),
            "one closing paint is not painting at rest"
        );
        // The next take's paints are past the window.
        kept.push(buffer(1_300));
        let _ = src.push(kept[4].clone());
        assert!(!anchor.paints_at_rest());
        kept.push(buffer(1_200));
        let _ = src.push(kept[5].clone());
        assert!(anchor.paints_at_rest());
    }

    #[test]
    fn a_page_url_with_a_fragment_is_refused() {
        let err = check_page_url("https://example.com/graphics/#/stinger").unwrap_err();
        assert!(err.contains("#/stinger"), "{err}");
        assert!(check_page_url("https://example.com/graphics/").is_ok());
        assert!(check_page_url("https://example.com/graphics/#").is_ok());
    }

    #[test]
    fn settings_read_from_the_block() {
        let props: HashMap<String, PropertyValue> = [
            (WEB_STINGER_DURATION_PROPERTY, PropertyValue::UInt(1_000)),
            (WEB_STINGER_CUT_POINT_PROPERTY, PropertyValue::Int(400)),
            (WEB_STINGER_MIX_PROPERTY, PropertyValue::String("x".into())),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(
            WebStingerSettings::from_properties(&props),
            settings(1_000, 400, 0)
        );
    }
}
