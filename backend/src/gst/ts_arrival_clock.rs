//! Arrival-anchored timestamps for a demuxed MPEG-TS stream.
//!
//! With `ignore-pcr=true`, `tsdemux` maps the sender's PTS onto running time
//! from a single reference: the arrival of the first PES it parsed. It never
//! takes a new one. Any error in that first sample lasts for the whole
//! connection: a caller that encoded before it connected is stamped seconds
//! into the future, a sender that stalls and catches up is stamped late, and a
//! muxer that sends data ahead of its PTS (ffmpeg's default) adds that lead as
//! latency. PCR would track the sender's clock, but too many senders get PCR
//! wrong to rely on it.
//!
//! This keeps tsdemux's PTS spacing and replaces only its anchor, from when the
//! data actually arrives. Data cannot arrive before it was sent, so for each
//! stream the buffer with the largest *lead* (its running time minus its
//! arrival) in a sliding window is the best estimate of where the sender's
//! timeline sits against ours. The stream whose best lead is smallest (it
//! arrives latest for its timestamps, as AAC packed several frames per PES
//! does) sets one offset for every stream, so A/V sync is kept:
//!
//! ```text
//! offset = margin - min over streams of (best lead in window)
//! ```
//!
//! Rules on top:
//! - A stream whose best lead is more than [`MAX_STREAM_SPREAD_NS`] behind the
//!   earliest stream's is still on stale data (the start of an encode a caller
//!   sent before going live) and does not count. MPEG-TS bounds how long data
//!   may wait for its PTS to about a second.
//! - Buffers that stay late with the current offset re-anchor every stream at
//!   once, when the lateness is confirmed: late buffers spanning
//!   [`LATE_CONFIRM_NS`] of the stream's timeline, with no buffer on time in
//!   between. The whole stream really is later then (a stall, a slower sender,
//!   a new caller), and moving output forward in time is safe. A stale tail is
//!   not confirmed: live buffers on time arrive in between.
//! - For a second after a caller change every late buffer re-anchors at once:
//!   the new caller's timeline is new, and tsdemux may flush a PES left over
//!   from the last caller with the new caller's reference, one buffer stamped
//!   far off.
//! - A pad's output never steps backwards. A smaller offset is applied only as
//!   far as it keeps this buffer at or after the previous one.
//!
//! The offset is applied with `gst_pad_set_offset` on each tsdemux source pad,
//! only when it changes by [`APPLY_STEP_NS`] or more (or a buffer is late), so
//! downstream sees a new segment only then. The per-buffer path uses atomics
//! only: no allocation, no clock reads, and no locks except the pad's own
//! object lock inside `set_offset` when the offset changes. Arrival time is
//! `srtsrc`'s own buffer timestamp, which tracks arrival within a few
//! milliseconds.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

/// Length of one window bucket.
const BUCKET_NS: u64 = 500_000_000;
/// Buckets per window: the window covers the last 1.5 to 2 s.
const BUCKETS: usize = 4;
/// Streams in one transport stream. More pads than this go uncorrected.
const MAX_STREAMS: usize = 16;
/// How far apart streams may arrive relative to their PTS. The MPEG-TS
/// decoder model allows data to wait up to about a second for its PTS.
pub const MAX_STREAM_SPREAD_NS: i64 = 1_000_000_000;
/// Smallest change of the offset that is applied (except for late buffers).
pub const APPLY_STEP_NS: i64 = 5_000_000;
/// How much of the stream's timeline must arrive late, with nothing on time
/// in between, before the stream is re-anchored later.
pub const LATE_CONFIRM_NS: i64 = 100_000_000;
/// After a caller change, how long a late buffer re-anchors at once.
const CALLER_TRUST_NS: u64 = 1_000_000_000;

const NONE_U64: u64 = u64::MAX;
const NONE_I64: i64 = i64::MIN;

/// One tsdemux source pad.
struct Stream {
    in_use: AtomicBool,
    /// From its SEGMENT event: where running time counts from (start plus the
    /// segment's own offset), and base without our pad offset.
    seg_origin: AtomicU64,
    seg_raw_base: AtomicI64,
    /// Bucket `epoch + 1` (0: empty) and the largest lead seen in it.
    bucket_epoch: [AtomicU64; BUCKETS],
    bucket_lead: [AtomicI64; BUCKETS],
    /// The offset in effect on the pad.
    applied: AtomicI64,
    /// Running time of the last buffer out, with the offset.
    last_out: AtomicI64,
}

impl Stream {
    fn new() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            seg_origin: AtomicU64::new(NONE_U64),
            seg_raw_base: AtomicI64::new(NONE_I64),
            bucket_epoch: Default::default(),
            bucket_lead: Default::default(),
            applied: AtomicI64::new(0),
            last_out: AtomicI64::new(NONE_I64),
        }
    }

    fn reset(&self) {
        self.seg_origin.store(NONE_U64, Ordering::Relaxed);
        self.seg_raw_base.store(NONE_I64, Ordering::Relaxed);
        self.clear_window();
        self.applied.store(0, Ordering::Relaxed);
        self.last_out.store(NONE_I64, Ordering::Relaxed);
    }

    fn clear_window(&self) {
        for e in &self.bucket_epoch {
            e.store(0, Ordering::Relaxed);
        }
    }

    fn record(&self, epoch: u64, lead: i64) {
        let i = (epoch % BUCKETS as u64) as usize;
        if self.bucket_epoch[i].load(Ordering::Relaxed) != epoch + 1 {
            self.bucket_epoch[i].store(epoch + 1, Ordering::Relaxed);
            self.bucket_lead[i].store(lead, Ordering::Relaxed);
        } else if lead > self.bucket_lead[i].load(Ordering::Relaxed) {
            self.bucket_lead[i].store(lead, Ordering::Relaxed);
        }
    }

    /// Largest lead in the window ending at `epoch`, if the stream has any.
    fn best_lead(&self, epoch: u64) -> Option<i64> {
        let mut best = None;
        for i in 0..BUCKETS {
            let e = self.bucket_epoch[i].load(Ordering::Relaxed);
            if e == 0 || e - 1 + (BUCKETS as u64) <= epoch {
                continue;
            }
            let lead = self.bucket_lead[i].load(Ordering::Relaxed);
            best = Some(best.map_or(lead, |b: i64| b.max(lead)));
        }
        best
    }
}

/// Shared state for one demuxer: the latest arrival and its streams.
pub struct ArrivalClock {
    margin_ns: i64,
    last_arrival: AtomicU64,
    /// Lowest raw running time in the current run of late buffers, over all
    /// streams. `NONE_I64` when the last buffer was on time.
    late_since: AtomicI64,
    /// Until this arrival time, late buffers re-anchor without waiting for
    /// confirmation (set on a caller change).
    trust_late_until: AtomicU64,
    streams: [Stream; MAX_STREAMS],
}

impl ArrivalClock {
    /// `margin_ns`: how far ahead of arrival the latest-arriving stream's
    /// earliest buffers are stamped. Covers jitter within a stream.
    pub fn new(margin_ns: i64) -> Self {
        Self {
            margin_ns,
            last_arrival: AtomicU64::new(NONE_U64),
            late_since: AtomicI64::new(NONE_I64),
            trust_late_until: AtomicU64::new(0),
            streams: std::array::from_fn(|_| Stream::new()),
        }
    }

    /// An input buffer arrived, stamped `arrival_ns` (running time).
    pub fn on_arrival(&self, arrival_ns: u64) {
        self.last_arrival.store(arrival_ns, Ordering::Relaxed);
    }

    /// A new caller connected, or the last one left. tsdemux takes a fresh
    /// reference for the next caller, which may sit behind the last one's, so
    /// the next late buffer is a new timeline, not a stale tail.
    pub fn on_caller_change(&self) {
        let now = self.last_arrival.load(Ordering::Relaxed);
        let now = if now == NONE_U64 { 0 } else { now };
        self.trust_late_until
            .store(now.saturating_add(CALLER_TRUST_NS), Ordering::Relaxed);
    }

    /// Take a stream slot for a new pad. `None` when all are taken.
    pub fn claim(&self) -> Option<usize> {
        self.streams
            .iter()
            .position(|s| {
                s.in_use
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            })
            .inspect(|&i| self.streams[i].reset())
    }

    pub fn release(&self, slot: usize) {
        self.streams[slot].in_use.store(false, Ordering::Relaxed);
    }

    /// A SEGMENT event on the stream's pad, as it leaves the pad: the pad's
    /// current offset `pad_offset_ns` is already in it. GStreamer adds a
    /// positive offset to `base`; a negative one larger than `base` zeroes
    /// `base` and moves the segment's own `offset` instead. Either way the
    /// running time is `pts - (start + offset) + base`, and without the pad
    /// offset it is that minus `pad_offset_ns`.
    pub fn on_segment(
        &self,
        slot: usize,
        start_ns: u64,
        segment_offset_ns: u64,
        base_ns: u64,
        pad_offset_ns: i64,
    ) {
        let s = &self.streams[slot];
        s.seg_origin.store(
            start_ns.saturating_add(segment_offset_ns),
            Ordering::Relaxed,
        );
        s.seg_raw_base
            .store(base_ns as i64 - pad_offset_ns, Ordering::Relaxed);
    }

    /// Running time of `pts_ns` on this stream as tsdemux stamped it.
    pub fn raw_running_time(&self, slot: usize, pts_ns: u64) -> Option<i64> {
        let s = &self.streams[slot];
        let origin = s.seg_origin.load(Ordering::Relaxed);
        let base = s.seg_raw_base.load(Ordering::Relaxed);
        if origin == NONE_U64 || base == NONE_I64 || pts_ns < origin {
            return None;
        }
        Some((pts_ns - origin) as i64 + base)
    }

    /// A buffer at raw running time `raw_rt` is leaving the stream's pad.
    /// Returns the offset to apply to the pad before it goes, if it changes.
    pub fn on_buffer(&self, slot: usize, raw_rt: i64) -> Option<i64> {
        let now = self.last_arrival.load(Ordering::Relaxed);
        if now == NONE_U64 {
            return None;
        }
        let s = &self.streams[slot];
        let lead = raw_rt - now as i64;
        let epoch = now / BUCKET_NS;
        let applied = s.applied.load(Ordering::Relaxed);
        let late = lead + applied < 0;
        if late {
            let since = match self.late_since.load(Ordering::Relaxed) {
                NONE_I64 => raw_rt,
                since => since.min(raw_rt),
            };
            self.late_since.store(since, Ordering::Relaxed);
            let trusted = now < self.trust_late_until.load(Ordering::Relaxed);
            if trusted || raw_rt - since >= LATE_CONFIRM_NS {
                for other in &self.streams {
                    other.clear_window();
                }
                self.late_since.store(NONE_I64, Ordering::Relaxed);
            }
        } else {
            self.late_since.store(NONE_I64, Ordering::Relaxed);
        }
        s.record(epoch, lead);

        let anchor = self.anchor(epoch)?;
        let mut wanted = self.margin_ns - anchor;
        let last_out = s.last_out.load(Ordering::Relaxed);
        if last_out != NONE_I64 {
            wanted = wanted.max(last_out - raw_rt);
        }
        let change = (wanted - applied).abs() >= APPLY_STEP_NS || (late && wanted > applied);
        let offset = if change { wanted } else { applied };
        s.last_out.store(raw_rt + offset, Ordering::Relaxed);
        if change {
            s.applied.store(offset, Ordering::Relaxed);
            Some(offset)
        } else {
            None
        }
    }

    fn anchor(&self, epoch: u64) -> Option<i64> {
        let mut earliest = None;
        for s in &self.streams {
            if !s.in_use.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(lead) = s.best_lead(epoch) {
                earliest = Some(earliest.map_or(lead, |e: i64| e.max(lead)));
            }
        }
        let earliest = earliest?;
        let mut anchor = earliest;
        for s in &self.streams {
            if !s.in_use.load(Ordering::Relaxed) {
                continue;
            }
            if let Some(lead) = s.best_lead(epoch) {
                if lead >= earliest - MAX_STREAM_SPREAD_NS {
                    anchor = anchor.min(lead);
                }
            }
        }
        Some(anchor)
    }
}

/// A demuxer pad and the stream slot it holds.
type PadSlot = (gst::glib::WeakRef<gst::Pad>, usize);

/// Record arrivals from `source_pad`, the pad feeding the demuxer (`srtsrc`'s
/// source pad). Its buffers carry `srtsrc`'s arrival timestamps.
pub fn watch_arrivals(source_pad: &gst::Pad, clock: &Arc<ArrivalClock>) {
    let clock = clock.clone();
    source_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
            clock.on_arrival(pts.nseconds());
        }
        gst::PadProbeReturn::Ok
    });
}

/// Retime each `tsdemux` source pad from the arrivals [`watch_arrivals`]
/// records. Call when the demuxer is created, before it adds pads. The closures
/// hold the shared [`ArrivalClock`] and weak pad references only.
pub fn retime_demuxer(tsdemux: &gst::Element, clock: &Arc<ArrivalClock>, label: &str) {
    let slots: Arc<Mutex<Vec<PadSlot>>> = Default::default();

    let added = clock.clone();
    let slots_added = slots.clone();
    let label_added = label.to_string();
    tsdemux.connect_pad_added(move |_, pad| {
        let Some(slot) = added.claim() else {
            warn!(
                "{}: more than {} demuxed streams, {} keeps tsdemux's timestamps",
                label_added,
                MAX_STREAMS,
                pad.name()
            );
            return;
        };
        if let Ok(mut s) = slots_added.lock() {
            s.push((pad.downgrade(), slot));
        }
        debug!("{}: retiming {} from arrival", label_added, pad.name());

        let segments = added.clone();
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            if let Some(gst::EventView::Segment(e)) = info.event().map(|e| e.view()) {
                if let Ok(seg) = e.segment().clone().downcast::<gst::ClockTime>() {
                    if seg.rate() == 1.0 {
                        segments.on_segment(
                            slot,
                            seg.start().map_or(0, |t| t.nseconds()),
                            seg.offset().map_or(0, |t| t.nseconds()),
                            seg.base().map_or(0, |t| t.nseconds()),
                            pad.offset(),
                        );
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        let buffers = added.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(raw_rt) = buffers.raw_running_time(slot, pts.nseconds()) else {
                return gst::PadProbeReturn::Ok;
            };
            if let Some(offset) = buffers.on_buffer(slot, raw_rt) {
                pad.set_offset(offset);
            }
            gst::PadProbeReturn::Ok
        });
    });

    let removed = clock.clone();
    tsdemux.connect_pad_removed(move |_, pad| {
        let Ok(mut s) = slots.lock() else {
            return;
        };
        s.retain(|(weak, slot)| match weak.upgrade() {
            Some(p) if &p != pad => true,
            _ => {
                removed.release(*slot);
                false
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// Feed buffers `(arrival_ms, slot, raw_rt_ms)`; return each buffer's
    /// lead after correction (its stamp minus its arrival), in ms.
    fn run(clock: &ArrivalClock, buffers: &[(i64, usize, i64)]) -> Vec<i64> {
        let mut offsets = [0i64; MAX_STREAMS];
        buffers
            .iter()
            .map(|&(arrival, slot, raw)| {
                clock.on_arrival((arrival * MS) as u64);
                if let Some(o) = clock.on_buffer(slot, raw * MS) {
                    offsets[slot] = o;
                }
                (raw * MS + offsets[slot] - arrival * MS) / MS
            })
            .collect()
    }

    /// Two streams at 25 buffers/s for `secs`, from `t0_ms`. `lead(stream, t)`
    /// is how far ahead of arrival tsdemux stamps that buffer.
    fn av(t0_ms: i64, secs: i64, lead: impl Fn(usize, i64) -> i64) -> Vec<(i64, usize, i64)> {
        let mut v = Vec::new();
        for k in 0..secs * 25 {
            let t = t0_ms + k * 40;
            for slot in 0..2 {
                v.push((t, slot, t + lead(slot, t)));
            }
        }
        v
    }

    fn two_streams(margin_ms: i64) -> ArrivalClock {
        let c = ArrivalClock::new(margin_ms * MS);
        assert_eq!(c.claim(), Some(0));
        assert_eq!(c.claim(), Some(1));
        c
    }

    #[test]
    fn a_healthy_stream_is_stamped_margin_ahead_of_its_latest_stream() {
        // ffmpeg's default muxing: video ~505 ms ahead, audio ~350 ms (±15).
        let c = two_streams(100);
        let leads = run(
            &c,
            &av(0, 10, |s, t| {
                if s == 0 {
                    505
                } else {
                    350 + (t / 40 % 3) * 10 - 10
                }
            }),
        );
        let tail = &leads[leads.len() - 100..];
        let audio: Vec<_> = tail.iter().skip(1).step_by(2).collect();
        let video: Vec<_> = tail.iter().step_by(2).collect();
        assert!(audio.iter().all(|&&l| (80..=100).contains(&l)), "{audio:?}");
        // Video keeps its 155 ms over audio: one offset for both, A/V kept.
        assert!(
            video.iter().all(|&&l| (235..=255).contains(&l)),
            "{video:?}"
        );
    }

    #[test]
    fn a_stale_head_is_corrected_within_a_second() {
        // 2 s of encoding before the connect: the first 150 ms are the start of
        // the encode (tsdemux's reference), then the stream jumps to live.
        let c = two_streams(100);
        let leads = run(&c, &av(0, 6, |_, t| if t < 150 { 0 } else { 2000 }));
        let after: Vec<_> = leads[(1000 / 40) * 2..].to_vec();
        assert!(after.iter().all(|&l| (90..=110).contains(&l)), "{after:?}");
    }

    #[test]
    fn output_never_steps_backwards() {
        let c = two_streams(100);
        let buffers = av(0, 6, |_, t| if t < 150 { 0 } else { 2000 });
        let mut offsets = [0i64; MAX_STREAMS];
        let mut last = [i64::MIN; MAX_STREAMS];
        for &(arrival, slot, raw) in &buffers {
            c.on_arrival((arrival * MS) as u64);
            if let Some(o) = c.on_buffer(slot, raw * MS) {
                offsets[slot] = o;
            }
            let out = raw * MS + offsets[slot];
            assert!(out >= last[slot], "stream {slot} went back at {arrival} ms");
            last[slot] = out;
        }
    }

    #[test]
    fn a_stall_and_catch_up_is_late_only_until_confirmed() {
        // 700 ms without data at 4 s, then the backlog arrives late and the
        // sender catches up at 5 % over real time (ffmpeg's -re catch-up).
        let c = two_streams(100);
        let mut buffers = av(0, 4, |_, _| 350);
        for k in 0..150 {
            let raw = 4000 + k * 40;
            let arrival = (4700 + k * 38).max(raw - 350);
            for slot in 0..2 {
                buffers.push((arrival, slot, raw));
            }
        }
        let leads = run(&c, &buffers);
        // The backlog's first 100 ms of timeline (3 buffers a stream) wait for
        // confirmation; nothing after them is late.
        let late: Vec<usize> = (0..leads.len()).filter(|&i| leads[i] < 0).collect();
        assert!(late.len() <= 6, "{} late buffers", late.len());
        assert!(
            late.iter().all(|&i| i < 4 * 25 * 2 + 8),
            "late after the stall settled"
        );
    }

    #[test]
    fn a_stale_tail_does_not_undo_the_correction() {
        // Live media resumes on video before audio's last stale frames are out:
        // those audio frames go out late, but must not pull the anchor back.
        let c = two_streams(100);
        let mut buffers = av(0, 1, |_, _| 0);
        for k in 0..100 {
            let t = 1000 + k * 40;
            buffers.push((t, 0, t + 2000));
            // audio: stale for its first 8 buffers, then live
            buffers.push((t, 1, t + if k < 8 { 0 } else { 2000 }));
        }
        let leads = run(&c, &buffers);
        let video_after: Vec<_> = leads[2 * 25 + 4..].iter().step_by(2).collect();
        assert!(
            video_after.iter().all(|&&l| (90..=110).contains(&l)),
            "{video_after:?}"
        );
    }

    #[test]
    fn a_drifting_sender_is_followed() {
        // Sender clock 0.5 % fast for 60 s: tsdemux's lead grows by 300 ms.
        let c = two_streams(100);
        let leads = run(&c, &av(0, 60, |_, t| t / 200));
        let tail = &leads[leads.len() - 50..];
        assert!(tail.iter().all(|&l| (90..=110).contains(&l)), "{tail:?}");
    }

    #[test]
    fn a_new_caller_after_a_corrected_one_is_never_late() {
        // The first caller had a 2 s stale head, corrected. The next caller is
        // clean: tsdemux's fresh reference puts it 2 s behind the first's.
        let c = two_streams(100);
        let first = av(0, 4, |_, t| if t < 150 { 0 } else { 2000 });
        run(&c, &first);
        c.on_caller_change();
        let leads = run(&c, &av(4500, 4, |_, _| 0));
        assert!(leads.iter().all(|&l| l >= 0), "a buffer went out late");
        let tail = &leads[leads.len() - 50..];
        assert!(tail.iter().all(|&l| (90..=110).contains(&l)), "{tail:?}");
    }

    #[test]
    fn a_junk_buffer_at_a_caller_change_does_not_hold_the_correction() {
        // tsdemux flushes a PES left over from the last caller with the new
        // caller's reference: one video buffer stamped 1.3 s ahead.
        let c = two_streams(100);
        run(&c, &av(0, 4, |_, t| if t < 150 { 0 } else { 2000 }));
        c.on_caller_change();
        let mut buffers = vec![(4500, 0, 4500 + 1300)];
        buffers.extend(av(4500, 4, |_, _| 0));
        let leads = run(&c, &buffers);
        assert!(leads[1..].iter().all(|&l| l >= 0), "{:?}", &leads[..20]);
    }

    #[test]
    fn a_reference_that_jumps_back_unannounced_recovers_once_confirmed() {
        // As above, with no caller signal (`mode=caller`, the far end
        // restarted): late until 100 ms of the new timeline confirm it.
        let c = two_streams(100);
        run(&c, &av(0, 4, |_, t| if t < 150 { 0 } else { 2000 }));
        let leads = run(&c, &av(4500, 4, |_, _| 0));
        let after = &leads[(200 / 40) * 2..];
        assert!(after.iter().all(|&l| (90..=110).contains(&l)), "{after:?}");
    }

    #[test]
    fn a_stream_that_stops_no_longer_holds_the_anchor() {
        // Audio stops at 2 s; video keeps going 300 ms further ahead.
        let c = two_streams(100);
        let mut buffers = av(0, 2, |s, _| if s == 0 { 300 } else { 0 });
        for k in 0..100 {
            let t = 2000 + k * 40;
            buffers.push((t, 0, t + 300));
        }
        let leads = run(&c, &buffers);
        let tail = &leads[leads.len() - 20..];
        assert!(tail.iter().all(|&l| (90..=110).contains(&l)), "{tail:?}");
    }

    #[test]
    fn small_changes_are_not_applied() {
        // ±2 ms jitter must not send a new segment every buffer.
        let c = two_streams(100);
        let buffers = av(0, 10, |_, t| (t / 40 % 2) * 4 - 2);
        let mut changes = 0;
        for &(arrival, slot, raw) in &buffers {
            c.on_arrival((arrival * MS) as u64);
            if c.on_buffer(slot, raw * MS).is_some() {
                changes += 1;
            }
        }
        assert!(changes <= 4, "{changes} offset changes");
    }
}
