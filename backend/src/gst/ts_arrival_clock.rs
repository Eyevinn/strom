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
//! This keeps tsdemux's timestamp spacing and replaces only its anchor, from
//! when the data actually arrives. Data cannot arrive before it was sent, so
//! for each stream the buffer with the largest *lead* (its running time minus
//! its deadline) in a sliding window is the best estimate of where the
//! sender's timeline sits against ours. The stream whose best lead is smallest
//! (it arrives latest for its timestamps, as AAC packed several frames per PES
//! does) sets one offset for every stream, so A/V sync is kept:
//!
//! ```text
//! offset = margin - min over streams of (best lead in window)
//! ```
//!
//! Lead is measured against the *deadline*, not the raw arrival: the arrival
//! in pipeline running time minus the latency the source and tsdemux report
//! upstream (`budget`). Downstream already waits that long for every buffer,
//! so a buffer may leave tsdemux up to `budget` behind the clock and still be
//! on time. Measured against the raw arrival, the margin landed on top of the
//! reported SRT latency and the two were paid twice (#1265). The budget is
//! what the pipeline counts with, the configured SRT latency, not what SRT
//! negotiated with the peer: srtsrc reports the configured value.
//!
//! Lead is taken in *decode order*: from the buffer's DTS where it has one,
//! else its PTS. Buffers leave tsdemux in decode order, so that is when each
//! one is due. With B-frames, a P-frame's PTS lead is the largest of its
//! group: anchored on PTS, every B-frame after it missed its deadline by 20 to
//! 60 ms, and the on-time P-frames in between kept the lateness from being
//! confirmed. DTS is never after PTS, so a buffer on time by its DTS is on
//! time by its PTS too.
//!
//! The arrival is the pipeline's running time when `srtsrc` pushes the
//! buffer, not `srtsrc`'s stamp. srtsrc stamps `now - (srt_time - srctime)`:
//! close to delivery while data flows evenly, but the burst a caller sends on
//! connecting is stamped up to its send time, more than a second behind the
//! clock (measured with libsrt 1.5.7). The running time comes from `Instant`,
//! calibrated against the pipeline clock on the first buffer and then on the
//! first buffer after each [`CALIBRATE_INTERVAL_NS`] of `Instant` time.
//!
//! Rules on top:
//! - A stream whose best lead is more than [`MAX_STREAM_SPREAD_NS`] behind the
//!   earliest stream's is still on stale data (the start of an encode a caller
//!   sent before going live) and does not count. MPEG-TS bounds how long data
//!   may wait for its PTS to about a second.
//! - Buffers that stay late with the current offset re-anchor every stream at
//!   once, when the lateness is confirmed: late buffers of one stream spanning
//!   [`LATE_CONFIRM_NS`] of that stream's own timeline, with no buffer of any
//!   stream on time in between. The whole stream really is later then (a
//!   stall, a slower sender, a new caller), and moving output forward in time
//!   is safe. A stale tail is not confirmed: live buffers on time arrive in
//!   between. Each stream counts its own span: streams' running times differ
//!   by up to [`MAX_STREAM_SPREAD_NS`], so one late buffer of each could
//!   otherwise span 100 ms between them.
//! - For a second from the first arrival after a caller change, every late
//!   buffer re-anchors at once: the new caller's timeline is new, and tsdemux
//!   may flush a PES left over from the last caller with the new caller's
//!   reference, one buffer stamped far off.
//! - A pad's output never steps backwards. A smaller offset is applied only as
//!   far as it keeps this buffer at or after the previous one, in decode order.
//!   Video with B-frames goes out with its PTS stepping back at every B-frame;
//!   compared by PTS, each one pushed the video offset up and it never came
//!   back down, which put video behind audio.
//! - A slot a removed pad released can be claimed by a new pad while the old
//!   pad's probes still run (a PMT change). Each claim has its own generation
//!   ([`StreamId`]), and calls made with an older one are ignored.
//!
//! The offset is applied with `gst_pad_set_offset` on each tsdemux source pad,
//! only when it changes by [`APPLY_STEP_NS`] or more (or a buffer is late), so
//! downstream sees a new segment only then. The per-buffer path uses atomics
//! and one `Instant` read: no allocation and no locks, except the pad's own
//! object lock inside `set_offset` when the offset changes, and the
//! calibration. Calibrating reads the pipeline's running time
//! (`parent_element` and `current_running_time`, which take object locks) in
//! the `srtsrc` buffer probe. That is a deliberate exception to the rule
//! against locks in a buffer probe, bounded to at most once per
//! [`CALIBRATE_INTERVAL_NS`] (2 Hz) per input: calibrating on a buffer count
//! instead left an audio-only stream at ~12 packets/s uncalibrated for ~20 s.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
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
/// How much of one stream's timeline must arrive late, with nothing on time
/// in between, before every stream is re-anchored later.
pub const LATE_CONFIRM_NS: i64 = 100_000_000;
/// From the first arrival after a caller change, how long a late buffer
/// re-anchors at once.
const CALLER_TRUST_NS: i64 = 1_000_000_000;
/// `Instant` time between two calibrations against the pipeline clock.
/// Reading the pipeline clock takes object locks, so at most 2 Hz.
pub const CALIBRATE_INTERVAL_NS: i64 = 500_000_000;

const NONE_U64: u64 = u64::MAX;
const NONE_I64: i64 = i64::MIN;

/// A stream slot as one pad claimed it. The generation tells this claim from
/// a later claim of the same slot by another pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamId {
    slot: usize,
    generation: u64,
}

/// One tsdemux source pad.
struct Stream {
    in_use: AtomicBool,
    /// Bumped on every claim of the slot.
    generation: AtomicU64,
    /// From its SEGMENT event: where running time counts from (start plus the
    /// segment's own offset), and base without our pad offset.
    seg_origin: AtomicU64,
    seg_raw_base: AtomicI64,
    /// Bucket `epoch + 1` (0: empty) and the largest lead seen in it.
    bucket_epoch: [AtomicU64; BUCKETS],
    bucket_lead: [AtomicI64; BUCKETS],
    /// The offset in effect on the pad.
    applied: AtomicI64,
    /// Decode-order running time (DTS, else PTS) of the last buffer out, with
    /// the offset.
    last_out: AtomicI64,
    /// Lowest decode-order raw running time in this stream's current run of
    /// late buffers. `NONE_I64` when no run is open.
    late_since: AtomicI64,
}

impl Stream {
    fn new() -> Self {
        Self {
            in_use: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            seg_origin: AtomicU64::new(NONE_U64),
            seg_raw_base: AtomicI64::new(NONE_I64),
            bucket_epoch: Default::default(),
            bucket_lead: Default::default(),
            applied: AtomicI64::new(0),
            last_out: AtomicI64::new(NONE_I64),
            late_since: AtomicI64::new(NONE_I64),
        }
    }

    fn reset(&self) {
        self.seg_origin.store(NONE_U64, Ordering::Relaxed);
        self.seg_raw_base.store(NONE_I64, Ordering::Relaxed);
        self.clear_window();
        self.applied.store(0, Ordering::Relaxed);
        self.last_out.store(NONE_I64, Ordering::Relaxed);
        self.late_since.store(NONE_I64, Ordering::Relaxed);
    }

    fn clear_window(&self) {
        for e in &self.bucket_epoch {
            e.store(0, Ordering::Relaxed);
        }
    }

    fn end_late_run(&self) {
        if self.late_since.load(Ordering::Relaxed) != NONE_I64 {
            self.late_since.store(NONE_I64, Ordering::Relaxed);
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
    /// Latency reported upstream of the demuxer's output (SRT + tsdemux).
    budget_ns: i64,
    /// Running time at the last input buffer's arrival.
    last_arrival: AtomicU64,
    /// Origin of the monotonic time below.
    mono_origin: Instant,
    /// Calibration: a running time, and the monotonic time (ns since
    /// `mono_origin`) it was read at. `NONE_I64` until calibrated.
    calib_rt: AtomicI64,
    calib_mono: AtomicI64,
    /// Monotonic time from which the next input buffer calibrates.
    next_calib_mono: AtomicI64,
    /// A caller connected or left, and its trust window starts at the next
    /// arrival.
    caller_changed: AtomicBool,
    /// Until this deadline, late buffers re-anchor without waiting for
    /// confirmation (set from the first arrival after a caller change).
    trust_late_until: AtomicI64,
    streams: [Stream; MAX_STREAMS],
}

impl ArrivalClock {
    /// `margin_ns`: how far ahead of its deadline the latest-arriving stream's
    /// earliest buffers are stamped. Covers jitter within a stream.
    /// `budget_ns`: the latency reported upstream of the demuxer's output.
    pub fn new(margin_ns: i64, budget_ns: i64) -> Self {
        Self {
            margin_ns,
            budget_ns,
            last_arrival: AtomicU64::new(NONE_U64),
            mono_origin: Instant::now(),
            calib_rt: AtomicI64::new(NONE_I64),
            calib_mono: AtomicI64::new(0),
            next_calib_mono: AtomicI64::new(0),
            caller_changed: AtomicBool::new(false),
            trust_late_until: AtomicI64::new(NONE_I64),
            streams: std::array::from_fn(|_| Stream::new()),
        }
    }

    /// An input buffer stamped `stamp_ns` by srtsrc arrived. Its arrival is
    /// the running time now, from the calibration; before the first
    /// calibration, the stamp. Returns true when the caller should calibrate
    /// on this buffer ([`Self::calibrate`]): on the first buffer, and then
    /// once per [`CALIBRATE_INTERVAL_NS`].
    pub fn on_arrival(&self, stamp_ns: u64) -> bool {
        self.arrive(stamp_ns, self.mono_now())
    }

    /// The pipeline's running time now is `running_time_ns`.
    pub fn calibrate(&self, running_time_ns: u64) {
        self.calibrate_at(running_time_ns, self.mono_now());
    }

    fn mono_now(&self) -> i64 {
        self.mono_origin.elapsed().as_nanos() as i64
    }

    fn arrive(&self, stamp_ns: u64, mono: i64) -> bool {
        let calib_rt = self.calib_rt.load(Ordering::Relaxed);
        let calibrated = calib_rt != NONE_I64;
        let arrival = if calibrated {
            (calib_rt + mono - self.calib_mono.load(Ordering::Relaxed)).max(0) as u64
        } else {
            stamp_ns
        };
        self.last_arrival.store(arrival, Ordering::Relaxed);
        // Before the first calibration the arrival is only srtsrc's stamp,
        // which can lag the clock by a second: the calibration that follows
        // starts the trust window instead.
        if calibrated {
            self.start_pending_trust(arrival);
        }
        let calibrate = mono >= self.next_calib_mono.load(Ordering::Relaxed);
        if calibrate {
            // Also bounds the retries when the running time is unavailable.
            self.next_calib_mono
                .store(mono + CALIBRATE_INTERVAL_NS, Ordering::Relaxed);
        }
        calibrate
    }

    fn calibrate_at(&self, running_time_ns: u64, mono: i64) {
        self.calib_mono.store(mono, Ordering::Relaxed);
        self.calib_rt
            .store(running_time_ns as i64, Ordering::Relaxed);
        self.last_arrival.store(running_time_ns, Ordering::Relaxed);
        self.start_pending_trust(running_time_ns);
    }

    fn start_pending_trust(&self, arrival: u64) {
        if self.caller_changed.load(Ordering::Relaxed)
            && self.caller_changed.swap(false, Ordering::Relaxed)
        {
            self.trust_late_until.store(
                arrival as i64 - self.budget_ns + CALLER_TRUST_NS,
                Ordering::Relaxed,
            );
        }
    }

    /// The deadline the last input buffer sets: its arrival minus the
    /// reported budget. `None` before the first buffer.
    fn deadline(&self) -> Option<i64> {
        let arrival = self.last_arrival.load(Ordering::Relaxed);
        if arrival == NONE_U64 {
            return None;
        }
        Some(arrival as i64 - self.budget_ns)
    }

    /// A new caller connected, or the last one left. tsdemux takes a fresh
    /// reference for the next caller, which may sit behind the last one's, so
    /// the next late buffer is a new timeline, not a stale tail. The trust
    /// window starts at the next arrival, so a caller that connects before
    /// any data has arrived gets it too.
    pub fn on_caller_change(&self) {
        self.caller_changed.store(true, Ordering::Relaxed);
    }

    /// Take a stream slot for a new pad. `None` when all are taken.
    pub fn claim(&self) -> Option<StreamId> {
        let slot = self.streams.iter().position(|s| {
            s.in_use
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        })?;
        let s = &self.streams[slot];
        let generation = s.generation.fetch_add(1, Ordering::Relaxed) + 1;
        s.reset();
        Some(StreamId { slot, generation })
    }

    pub fn release(&self, id: StreamId) {
        if self.stream(id).is_some() {
            self.streams[id.slot].in_use.store(false, Ordering::Relaxed);
        }
    }

    /// The stream `id` claimed, if no later claim has taken its slot.
    fn stream(&self, id: StreamId) -> Option<&Stream> {
        let s = &self.streams[id.slot];
        (s.generation.load(Ordering::Relaxed) == id.generation).then_some(s)
    }

    /// A SEGMENT event on the stream's pad, as it leaves the pad: the pad's
    /// current offset `pad_offset_ns` is already in it. GStreamer adds a
    /// positive offset to `base`; a negative one larger than `base` zeroes
    /// `base` and moves the segment's own `offset` instead. Either way the
    /// running time is `pts - (start + offset) + base`, and without the pad
    /// offset it is that minus `pad_offset_ns`.
    pub fn on_segment(
        &self,
        id: StreamId,
        start_ns: u64,
        segment_offset_ns: u64,
        base_ns: u64,
        pad_offset_ns: i64,
    ) {
        let Some(s) = self.stream(id) else {
            return;
        };
        s.seg_origin.store(
            start_ns.saturating_add(segment_offset_ns),
            Ordering::Relaxed,
        );
        s.seg_raw_base
            .store(base_ns as i64 - pad_offset_ns, Ordering::Relaxed);
    }

    /// Running time of `ts_ns` on this stream as tsdemux stamped it. A
    /// timestamp before the segment's origin (the DTS of the first frames
    /// with B-frames) gives a running time below the segment's base, not
    /// none: it still orders the buffer.
    pub fn raw_running_time(&self, id: StreamId, ts_ns: u64) -> Option<i64> {
        let s = self.stream(id)?;
        let origin = s.seg_origin.load(Ordering::Relaxed);
        let base = s.seg_raw_base.load(Ordering::Relaxed);
        if origin == NONE_U64 || base == NONE_I64 {
            return None;
        }
        Some(ts_ns as i64 - origin.min(i64::MAX as u64) as i64 + base)
    }

    /// A buffer is leaving the stream's pad. `pts_rt` is its raw running time
    /// by PTS, `dts_rt` by DTS where it has one. Returns the offset to apply
    /// to the pad before it goes, if it changes.
    pub fn on_buffer(&self, id: StreamId, pts_rt: i64, dts_rt: Option<i64>) -> Option<i64> {
        let s = self.stream(id)?;
        let now = self.deadline()?;
        // Decode order: the buffer is due when it leaves, by its DTS.
        let order_rt = dts_rt.unwrap_or(pts_rt);
        let lead = order_rt - now;
        let epoch = (self.last_arrival.load(Ordering::Relaxed)) / BUCKET_NS;
        let applied = s.applied.load(Ordering::Relaxed);
        let late = lead + applied < 0;
        if late {
            let since = match s.late_since.load(Ordering::Relaxed) {
                NONE_I64 => order_rt,
                since => since.min(order_rt),
            };
            s.late_since.store(since, Ordering::Relaxed);
            let trusted = now < self.trust_late_until.load(Ordering::Relaxed);
            if trusted || order_rt - since >= LATE_CONFIRM_NS {
                for other in &self.streams {
                    other.clear_window();
                    other.end_late_run();
                }
            }
        } else {
            // A buffer on time, from any stream, ends every stream's run: the
            // late ones were a stale tail, not a later timeline.
            for other in &self.streams {
                other.end_late_run();
            }
        }
        s.record(epoch, lead);

        let anchor = self.anchor(epoch)?;
        let mut wanted = self.margin_ns - anchor;
        let last_out = s.last_out.load(Ordering::Relaxed);
        if last_out != NONE_I64 {
            wanted = wanted.max(last_out - order_rt);
        }
        let change = (wanted - applied).abs() >= APPLY_STEP_NS || (late && wanted > applied);
        let offset = if change { wanted } else { applied };
        s.last_out.store(order_rt + offset, Ordering::Relaxed);
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

/// A demuxer pad and the stream it claimed.
type PadSlot = (gst::glib::WeakRef<gst::Pad>, StreamId);

/// Record arrivals from `source_pad`, the pad feeding the demuxer (`srtsrc`'s
/// source pad). Its buffers carry `srtsrc`'s arrival timestamps.
pub fn watch_arrivals(source_pad: &gst::Pad, clock: &Arc<ArrivalClock>) {
    let clock = clock.clone();
    source_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
            return gst::PadProbeReturn::Ok;
        };
        if clock.on_arrival(pts.nseconds()) {
            // At most once per CALIBRATE_INTERVAL_NS: takes object locks.
            if let Some(now) = pad.parent_element().and_then(|e| e.current_running_time()) {
                clock.calibrate(now.nseconds());
            }
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
        let Some(id) = added.claim() else {
            warn!(
                "{}: more than {} demuxed streams, {} keeps tsdemux's timestamps",
                label_added,
                MAX_STREAMS,
                pad.name()
            );
            return;
        };
        if let Ok(mut s) = slots_added.lock() {
            s.push((pad.downgrade(), id));
        }
        debug!("{}: retiming {} from arrival", label_added, pad.name());

        let segments = added.clone();
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
            if let Some(gst::EventView::Segment(e)) = info.event().map(|e| e.view()) {
                if let Ok(seg) = e.segment().clone().downcast::<gst::ClockTime>() {
                    if seg.rate() == 1.0 {
                        segments.on_segment(
                            id,
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
            let Some(buffer) = info.buffer() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(pts_rt) = buffer
                .pts()
                .and_then(|pts| buffers.raw_running_time(id, pts.nseconds()))
            else {
                return gst::PadProbeReturn::Ok;
            };
            let dts_rt = buffer
                .dts()
                .and_then(|dts| buffers.raw_running_time(id, dts.nseconds()));
            if let Some(offset) = buffers.on_buffer(id, pts_rt, dts_rt) {
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
        s.retain(|(weak, id)| match weak.upgrade() {
            Some(p) if &p != pad => true,
            _ => {
                removed.release(*id);
                false
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// An input buffer arrives at `arrival_ms`, with the pipeline clock at the
    /// same running time: the monotonic time is the arrival, and calibration
    /// happens when the clock asks for it, as the srtsrc probe does.
    fn arrive(c: &ArrivalClock, arrival_ms: i64) {
        let a = arrival_ms * MS;
        if c.arrive(a as u64, a) {
            c.calibrate_at(a as u64, a);
        }
    }

    /// The stream currently claiming `slot`.
    fn id(c: &ArrivalClock, slot: usize) -> StreamId {
        StreamId {
            slot,
            generation: c.streams[slot].generation.load(Ordering::Relaxed),
        }
    }

    /// Feed buffers `(arrival_ms, slot, raw_rt_ms)`; return each buffer's
    /// lead after correction (its stamp minus its arrival), in ms.
    fn run(clock: &ArrivalClock, buffers: &[(i64, usize, i64)]) -> Vec<i64> {
        let mut offsets = [0i64; MAX_STREAMS];
        buffers
            .iter()
            .map(|&(arrival, slot, raw)| {
                arrive(clock, arrival);
                if let Some(o) = clock.on_buffer(id(clock, slot), raw * MS, None) {
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
        let c = ArrivalClock::new(margin_ms * MS, 0);
        assert_eq!(c.claim().map(|s| s.slot), Some(0));
        assert_eq!(c.claim().map(|s| s.slot), Some(1));
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
            arrive(&c, arrival);
            if let Some(o) = c.on_buffer(id(&c, slot), raw * MS, None) {
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
    fn the_latest_stream_spends_the_reported_budget_not_more() {
        // #1265: srtsrc stamps at delivery and reports 125 ms of SRT latency.
        // The latest stream (audio, 190 ms later than video for its PTS) is due
        // `margin` before its deadline, which is 125 ms after its arrival: lead
        // margin - budget = -25 ms, not +margin on top of the budget.
        let c = ArrivalClock::new(100 * MS, 125 * MS);
        assert_eq!(c.claim().map(|s| s.slot), Some(0));
        assert_eq!(c.claim().map(|s| s.slot), Some(1));
        let leads = run(&c, &av(0, 6, |s, _| if s == 0 { 0 } else { -190 }));
        let tail = &leads[leads.len() - 100..];
        let video: Vec<_> = tail.iter().step_by(2).collect();
        let audio: Vec<_> = tail.iter().skip(1).step_by(2).collect();
        assert!(
            audio.iter().all(|&&l| (-30..=-20).contains(&l)),
            "{audio:?}"
        );
        // One offset for both: video keeps its 190 ms over audio.
        assert!(
            video.iter().all(|&&l| (160..=170).contains(&l)),
            "{video:?}"
        );
        assert!(
            leads.iter().all(|&l| l >= -125),
            "a buffer missed its deadline"
        );
    }

    #[test]
    fn arrival_follows_the_clock_not_the_stamp() {
        // A caller's connect burst: srtsrc stamps it up to its send time, 1.3 s
        // behind the clock. Once calibrated, arrival is the clock's running
        // time, whatever the stamp says.
        let c = ArrivalClock::new(100 * MS, 125 * MS);
        assert!(c.arrive(500 * MS as u64, 0), "the first buffer calibrates");
        c.calibrate_at(500 * MS as u64, 0);
        assert!(!c.arrive(0, 40 * MS));
        assert_eq!(c.last_arrival.load(Ordering::Relaxed), 540 * MS as u64);
        assert!(!c.arrive(10_000 * MS as u64, 80 * MS));
        assert_eq!(c.last_arrival.load(Ordering::Relaxed), 580 * MS as u64);
        assert_eq!(c.deadline(), Some(455 * MS));
    }

    #[test]
    fn calibration_follows_time_not_buffer_count() {
        // Audio alone at ~12 packets/s: calibrating on a buffer count (one in
        // 256) left it uncalibrated for ~20 s. Calibrate on the first buffer
        // after each interval, however few buffers there are.
        let c = ArrivalClock::new(100 * MS, 0);
        assert!(c.arrive(0, 0), "the first buffer calibrates");
        c.calibrate_at(0, 0);
        assert!(!c.arrive(0, CALIBRATE_INTERVAL_NS - 1));
        assert!(c.arrive(0, CALIBRATE_INTERVAL_NS + 80 * MS));
        assert!(!c.arrive(0, CALIBRATE_INTERVAL_NS + 160 * MS));
        // A failed calibration is retried one interval later, not per buffer.
        assert!(c.arrive(0, 2 * CALIBRATE_INTERVAL_NS + 80 * MS));
        assert!(!c.arrive(0, 2 * CALIBRATE_INTERVAL_NS + 120 * MS));
    }

    #[test]
    fn b_frames_keep_the_offset_of_the_stream() {
        // H.264 with two B-frames goes out in decode order: PTS steps back at
        // every B-frame (I0 P3 B1 B2 P6 B4 B5 ...). Every video buffer must get
        // the same offset as audio. Compared by PTS, each B-frame raised video's
        // offset to the previous P-frame's time and the next P-frame lowered it:
        // B-frames were stamped at the wrong time, with a new segment each.
        let c = two_streams(100);
        let mut offsets = [0i64; MAX_STREAMS];
        let mut video_offsets = Vec::new();
        for k in 0..250i64 {
            let t = k * 40;
            let reorder = if k % 3 == 0 { 2 } else { -1 };
            let pts = t + reorder * 40 + 80;
            arrive(&c, t);
            if let Some(o) = c.on_buffer(id(&c, 0), pts * MS, Some(t * MS)) {
                offsets[0] = o;
            }
            if let Some(o) = c.on_buffer(id(&c, 1), t * MS, None) {
                offsets[1] = o;
            }
            if t >= 2000 {
                video_offsets.push((offsets[0] - offsets[1]) / MS);
            }
        }
        assert!(
            video_offsets.iter().all(|&d| d.abs() <= APPLY_STEP_NS / MS),
            "video offset minus audio offset per buffer (ms): {video_offsets:?}"
        );
    }

    #[test]
    fn small_changes_are_not_applied() {
        // ±2 ms jitter must not send a new segment every buffer.
        let c = two_streams(100);
        let buffers = av(0, 10, |_, t| (t / 40 % 2) * 4 - 2);
        let mut changes = 0;
        for &(arrival, slot, raw) in &buffers {
            arrive(&c, arrival);
            if c.on_buffer(id(&c, slot), raw * MS, None).is_some() {
                changes += 1;
            }
        }
        assert!(changes <= 4, "{changes} offset changes");
    }
    #[test]
    fn b_frames_are_on_time_when_video_sets_the_anchor() {
        // Video with two B-frames is the only stream, so it sets the anchor.
        // It goes out in decode order (I0 P3 B1 B2 P6 B4 B5 ...): a P-frame's
        // PTS lead is the largest of its group. Anchored on PTS, every B-frame
        // after it missed its deadline, and the on-time P-frames in between
        // kept that from being confirmed. Anchored on DTS, every buffer's PTS
        // stays at or ahead of its deadline.
        let c = ArrivalClock::new(20 * MS, 125 * MS);
        let v = c.claim().unwrap();
        let mut offset = 0;
        let mut misses = Vec::new();
        for k in 0..250i64 {
            let t = k * 40;
            let dts = t + 300;
            let pts = dts + if k % 3 == 0 { 120 } else { 0 };
            arrive(&c, t);
            if let Some(o) = c.on_buffer(v, pts * MS, Some(dts * MS)) {
                offset = o;
            }
            let lead = (pts * MS + offset - (t - 125) * MS) / MS;
            if lead < 0 {
                misses.push((t, lead));
            }
        }
        assert!(
            misses.is_empty(),
            "{} buffers missed their deadline by PTS (arrival ms, lead ms): {:?}",
            misses.len(),
            &misses[..misses.len().min(10)]
        );
    }

    #[test]
    fn late_buffers_of_two_streams_do_not_confirm_each_other() {
        // After a 500 ms stall, one late audio buffer, then one late video
        // buffer whose running time is 155 ms higher (streams' running times
        // differ by up to MAX_STREAM_SPREAD_NS). Neither stream has 100 ms of
        // its own timeline late, so nothing is confirmed.
        let c = two_streams(100);
        run(&c, &av(0, 2, |_, _| 0));
        arrive(&c, 2500);
        assert_eq!(c.on_buffer(id(&c, 1), 2000 * MS, None), None);
        assert_eq!(
            c.on_buffer(id(&c, 0), 2155 * MS, None),
            None,
            "re-anchored on one late buffer from each of two streams"
        );
    }

    #[test]
    fn the_first_caller_gets_the_trust_window() {
        // The caller connects before any data arrived (caller-added comes
        // first), and its first buffer is a leftover PES stamped 1.3 s ahead.
        // The trust window starts at the first arrival, so the live buffers
        // after it re-anchor at once instead of waiting for confirmation.
        let c = two_streams(100);
        c.on_caller_change();
        let mut buffers = vec![(0, 0, 1300)];
        buffers.extend(av(0, 4, |_, _| 0));
        let leads = run(&c, &buffers);
        assert!(leads[1..].iter().all(|&l| l >= 0), "{:?}", &leads[..20]);
    }

    #[test]
    fn a_reclaimed_slot_ignores_the_pad_that_released_it() {
        // A PMT change: the old pad is removed and its slot claimed by a new
        // pad, while the old pad's probes may still run.
        let c = ArrivalClock::new(100 * MS, 0);
        let old = c.claim().unwrap();
        c.on_segment(old, 0, 0, 0, 0);
        c.release(old);
        let new = c.claim().unwrap();
        assert_eq!(new.slot, old.slot);
        c.on_segment(new, 5000 * MS as u64, 0, 0, 0);
        c.on_segment(old, 0, 0, 0, 0);
        assert_eq!(
            c.raw_running_time(new, 5040 * MS as u64),
            Some(40 * MS),
            "the old pad's segment replaced the new pad's"
        );
        assert_eq!(c.raw_running_time(old, 40 * MS as u64), None);
        arrive(&c, 0);
        assert_eq!(c.on_buffer(old, 10_000 * MS, None), None);
        assert_eq!(c.streams[new.slot].best_lead(0), None);
        // Releasing the old claim again leaves the new one in use.
        c.release(old);
        assert!(c.streams[new.slot].in_use.load(Ordering::Relaxed));
    }

    #[test]
    fn a_timestamp_before_the_segment_start_has_a_running_time() {
        // With B-frames the first frames' DTS lie before the segment start.
        let c = ArrivalClock::new(100 * MS, 0);
        let s = c.claim().unwrap();
        c.on_segment(s, 1000 * MS as u64, 0, 200 * MS as u64, 0);
        assert_eq!(c.raw_running_time(s, 920 * MS as u64), Some(120 * MS));
    }
}
