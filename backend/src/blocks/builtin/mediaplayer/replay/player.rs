//! Replay player engine: index + coded-bytes cache + frame store + output tick.
//!
//! - The fetcher ([`super::cache`]) keeps the index and the coded bytes around
//!   the group cursor, asynchronously.
//! - Decode workers turn whole segments whose bytes have arrived into decoded
//!   frames placed on the group's TAI frame grid.
//! - A scheduler keeps the segments around the cursor decoded, ahead in the
//!   direction of travel, and evicts the rest.
//! - The output tick runs on the flow clock, one frame period at a time. Each
//!   tick asks the group timeline for the frame at that running time and
//!   pushes the cached frame, restamped with the tick's running time and
//!   carrying its TAI as a reference timestamp. Pause repeats a frame, slow
//!   motion repeats frames, reverse walks backwards: the output never stops,
//!   flushes or seeks, and never waits on the network.

use super::backoff::{worth_warning, Backoff};
use super::cache::{by_urgency, Fetcher, SegmentStore};
use super::decode::{caps_frame_rate, decode_bytes, decode_timeout, map_to_grid, pick_decoder};
use super::source::{Segment, SourceIndex};
use super::timeline::{PlayerStats, ReplayTimeline};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const S: i64 = 1_000_000_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Decoded frames by absolute frame number, and which frames each decoded
/// segment contributed (for eviction).
#[derive(Default)]
struct FrameStore {
    frames: BTreeMap<i64, gst::Buffer>,
    segments: HashMap<i64, (i64, i64)>,
}

pub struct ReplayPlayer {
    pub block_id: String,
    timeline: Arc<ReplayTimeline>,
    pub stats: Arc<PlayerStats>,
    fetcher: Arc<Fetcher>,
    store: Arc<SegmentStore>,
    appsrc: gst::glib::WeakRef<gst_app::AppSrc>,
    decoded: Mutex<FrameStore>,
    inflight: Mutex<HashSet<i64>>,
    /// Segments whose decode failed (or gave no frames), retried with backoff.
    failed: Mutex<Backoff<i64>>,
    caps: Mutex<Option<gst::Caps>>,
    decoder: &'static str,
    workers: usize,
    /// Decoded window around the cursor, in its direction of travel.
    decode_behind_ns: i64,
    decode_ahead_ns: i64,
    /// Output frame rate; `None` = the source's own rate.
    output_rate: Option<(u32, u32)>,
    /// Read the bitstrip and burned-in time of pushed frames
    /// (STROM_REPLAY_BITSTRIP=1).
    read_strip: bool,
    started: AtomicBool,
    stop: Arc<AtomicBool>,
}

impl ReplayPlayer {
    pub fn new(
        block_id: &str,
        source: Arc<dyn SourceIndex>,
        timeline: Arc<ReplayTimeline>,
        appsrc: &gst_app::AppSrc,
        cache_ahead_s: u64,
        prefetch_s: u64,
        output_rate: Option<(u32, u32)>,
    ) -> Arc<Self> {
        let stats = Arc::new(PlayerStats::new(block_id));
        timeline.register_player(&stats);
        let decoder = pick_decoder();
        info!("Replay player {}: decoder {}", block_id, decoder);
        let fetcher = Arc::new(Fetcher::new(
            block_id,
            source,
            Arc::clone(&timeline),
            Arc::clone(&stats),
            prefetch_s.max(cache_ahead_s + 2),
        ));
        Arc::new(Self {
            block_id: block_id.to_string(),
            store: Arc::clone(&fetcher.store),
            fetcher,
            timeline,
            stats,
            appsrc: appsrc.downgrade(),
            decoded: Mutex::new(FrameStore::default()),
            inflight: Mutex::new(HashSet::new()),
            failed: Mutex::new(Backoff::new(
                Duration::from_millis(250),
                Duration::from_secs(10),
            )),
            caps: Mutex::new(None),
            decoder,
            workers: 2,
            decode_behind_ns: 2 * S,
            decode_ahead_ns: cache_ahead_s.max(1) as i64 * S,
            output_rate,
            read_strip: strom_types::env::var_opt("STROM_REPLAY_BITSTRIP")
                .is_some_and(|v| v == "1" || v == "true"),
            started: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start the threads, once, when the flow starts.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        Arc::clone(&self.fetcher).spawn(Arc::clone(&self.stop));
        let (tx, rx) = mpsc::channel::<Arc<Segment>>();
        let rx = Arc::new(Mutex::new(rx));
        for w in 0..self.workers {
            let me = Arc::clone(self);
            let rx = Arc::clone(&rx);
            std::thread::Builder::new()
                .name(format!("replay-dec-{}-{}", self.block_id, w))
                .spawn(move || me.decode_worker(rx))
                .ok();
        }
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("replay-sched-{}", self.block_id))
            .spawn(move || me.scheduler(tx))
            .ok();
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("replay-tick-{}", self.block_id))
            .spawn(move || me.tick_loop())
            .ok();
    }

    fn alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    // --- decoding -----------------------------------------------------------

    fn decode_worker(self: Arc<Self>, rx: Arc<Mutex<mpsc::Receiver<Arc<Segment>>>>) {
        while self.alive() {
            let job = lock(&rx).recv_timeout(Duration::from_millis(100));
            let seg = match job {
                Ok(s) => s,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match self.decode_one(&seg) {
                Ok(()) => lock(&self.failed).succeed(&seg.start_ns),
                Err(e) => {
                    let attempts = lock(&self.failed).fail(seg.start_ns, Instant::now());
                    if worth_warning(attempts) {
                        warn!(
                            "Replay {}: segment {} not decoded (attempt {}): {}",
                            self.block_id, seg.object_id, attempts, e
                        );
                    } else {
                        debug!(
                            "Replay {}: segment {} not decoded (attempt {}): {}",
                            self.block_id, seg.object_id, attempts, e
                        );
                    }
                }
            }
            lock(&self.inflight).remove(&seg.start_ns);
        }
    }

    /// Decode one segment into the frame store. An error means the segment
    /// gave no usable frames; it is retried later with backoff.
    fn decode_one(&self, seg: &Segment) -> Result<(), String> {
        // Evicted meanwhile: nothing to do, it is sent again when refetched.
        let Some(bytes) = self.store.bytes(seg.start_ns) else {
            return Ok(());
        };
        let container = self.store.info().and_then(|i| i.container);
        let started = Instant::now();
        let result = decode_bytes(
            bytes,
            container.as_deref(),
            self.decoder,
            decode_timeout(seg.duration_ns()),
        );
        let ms = started.elapsed().as_millis() as u64;
        let decoded = result?;
        // No frame rate from the index: take it from the stream.
        let grid = self.stats.grid().or_else(|| {
            let (n, d) = caps_frame_rate(&decoded.caps)?;
            self.stats.init_grid(n, d, seg.first_sample_tai())
        });
        let grid = grid.ok_or("no frame rate known")?;
        let n_decoded = decoded.frames.len();
        let frames = map_to_grid(seg, &grid, decoded.frames, decoded.first_pts);
        if frames.is_empty() {
            return Err(format!(
                "none of {} decoded frames fall inside the segment's timerange",
                n_decoded
            ));
        }
        let expected = ((seg.end_ns - seg.start_ns) as f64 / grid.frame_ns()).round() as usize;
        if frames.len() != expected {
            let first = grid.ceil_frame(seg.start_ns);
            let have: HashSet<i64> = frames.iter().map(|f| f.0).collect();
            let missing: Vec<i64> = (first..first + expected as i64)
                .filter(|k| !have.contains(k))
                .map(|k| k - first)
                .collect();
            warn!(
                "Replay {}: segment {} gave {} frames ({} decoded), timerange holds {}, \
                 missing offsets {:?}",
                self.block_id,
                seg.object_id,
                frames.len(),
                n_decoded,
                expected,
                missing
            );
        }
        debug!(
            "Replay {}: segment {} decoded in {} ms",
            self.block_id, seg.object_id, ms
        );
        {
            // The output caps are those of the first decoded segment; a
            // segment in another layout would go out under the wrong caps.
            let mut caps = lock(&self.caps);
            match caps.as_ref() {
                Some(out) if !same_layout(out, &decoded.caps) => {
                    return Err(format!(
                        "decoded as {}, the output is {}; not shown",
                        decoded.caps, out
                    ));
                }
                Some(_) => {}
                None => *caps = Some(decoded.caps),
            }
        }
        if let (Some(lo), Some(hi)) = (frames.first().map(|f| f.0), frames.last().map(|f| f.0)) {
            let mut store = lock(&self.decoded);
            store.segments.insert(seg.start_ns, (lo, hi));
            store.frames.extend(frames);
        }
        self.stats.decodes.fetch_add(1, Ordering::Relaxed);
        self.stats.last_decode_ms.store(ms, Ordering::Relaxed);
        Ok(())
    }

    fn scheduler(self: Arc<Self>, tx: mpsc::Sender<Arc<Segment>>) {
        while self.alive() {
            let rt = self.timeline.now_rt();
            if let Some((cursor, speed)) = self.timeline.cursor_tai(rt) {
                let forward = speed >= 0.0;
                let (lo, hi) = if forward {
                    (
                        cursor - self.decode_behind_ns,
                        cursor + self.decode_ahead_ns,
                    )
                } else {
                    (
                        cursor - self.decode_ahead_ns,
                        cursor + self.decode_behind_ns,
                    )
                };
                let wanted = by_urgency(&self.store.segments_in(lo, hi), cursor, forward);
                {
                    let decoded = lock(&self.decoded);
                    let mut inflight = lock(&self.inflight);
                    let mut failed = lock(&self.failed);
                    let keys: HashSet<i64> = wanted.iter().map(|s| s.start_ns).collect();
                    failed.retain(|k| keys.contains(k));
                    let jobs = pick_decodes(
                        &wanted,
                        |k| decoded.segments.contains_key(&k) || self.store.bytes(k).is_none(),
                        &mut inflight,
                        &failed,
                        self.workers,
                        Instant::now(),
                    );
                    for s in jobs {
                        if tx.send(s).is_err() {
                            return;
                        }
                    }
                }
                let keep: HashSet<i64> = wanted.iter().map(|s| s.start_ns).collect();
                let mut store = lock(&self.decoded);
                let evict: Vec<(i64, (i64, i64))> = store
                    .segments
                    .iter()
                    .filter(|(k, _)| !keep.contains(k))
                    .map(|(k, r)| (*k, *r))
                    .collect();
                for (k, (a, b)) in evict {
                    store.segments.remove(&k);
                    let doomed: Vec<i64> = store.frames.range(a..=b).map(|(f, _)| *f).collect();
                    for f in doomed {
                        store.frames.remove(&f);
                    }
                }
                self.stats
                    .cached_segments
                    .store(store.segments.len() as u64, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn frame_at(&self, frame: i64) -> Option<gst::Buffer> {
        lock(&self.decoded).frames.get(&frame).cloned()
    }

    // --- output -------------------------------------------------------------

    fn tick_loop(self: Arc<Self>) {
        // Wait for the flow to play and for the first frames to exist.
        let (appsrc, clock, base, grid) = loop {
            if !self.alive() {
                return;
            }
            let Some(appsrc) = self.appsrc.upgrade() else {
                self.halt();
                return;
            };
            let has_caps = lock(&self.caps).is_some();
            if appsrc.current_state() == gst::State::Playing {
                if let (Some(clock), Some(base)) = (appsrc.clock(), appsrc.base_time()) {
                    // The group's "now" exists as soon as the flow plays, so
                    // the cursor (and prefetch) can start before any decode.
                    self.timeline.set_clock_source(appsrc.upcast_ref());
                    if let (true, Some(grid)) = (has_caps, self.stats.grid()) {
                        break (appsrc, clock, base, grid);
                    }
                }
            }
            drop(appsrc);
            std::thread::sleep(Duration::from_millis(10));
        };

        // Output rate: as configured, else the source's own rate.
        let (on, od_) = self.output_rate.unwrap_or((grid.fps_n, grid.fps_d));
        self.stats.set_output_rate(on, od_);
        let mut caps = lock(&self.caps).clone().expect("caps checked above");
        caps.make_mut()
            .set("framerate", gst::Fraction::new(on as i32, od_ as i32));
        let video_info = gst_video::VideoInfo::from_caps(&caps).ok();
        appsrc.set_caps(Some(&caps));
        info!(
            "Replay {}: output running, source {}/{} fps, caps {}",
            self.block_id, grid.fps_n, grid.fps_d, caps
        );
        let weak = appsrc.downgrade();
        drop(appsrc);
        let tai_caps = gst::Caps::builder("timestamp/x-tai").build();

        // Output tick period.
        let fd = 1e9 * od_ as f64 / on as f64;
        let now_rt = || clock.time().nseconds() as i64 - base.nseconds() as i64;
        let mut k = (now_rt() as f64 / fd).ceil() as i64;
        let mut last: Option<(i64, gst::Buffer)> = None;
        let mut last_hit: Option<i64> = None;

        while self.alive() {
            let rt = (k as f64 * fd) as i64;
            let id =
                clock.new_single_shot_id(base + gst::ClockTime::from_nseconds(rt.max(0) as u64));
            let _ = id.wait();

            let Some(appsrc) = weak.upgrade() else {
                break;
            };
            if appsrc.current_state() == gst::State::Null {
                break;
            }
            // Woke up late: skip to the current tick instead of bursting.
            let now = now_rt();
            if now > rt + 2 * fd as i64 {
                self.stats.late_ticks.fetch_add(1, Ordering::Relaxed);
                k = (now as f64 / fd).ceil() as i64;
                continue;
            }

            // The group's time at this tick, and this angle's frame for it.
            let want = self.timeline.frame_on(&grid, rt);
            if let Some((f, _)) = want {
                self.stats.wanted_frame.store(f, Ordering::Relaxed);
            }
            let hit = want.and_then(|(f, t)| self.frame_at(f).map(|b| (f, t, b)));
            let hit = match hit {
                Some((f, t, b)) => {
                    let new_picture = last.as_ref().is_none_or(|(lf, _)| *lf != f);
                    if self.read_strip && new_picture {
                        if let Some(info) = &video_info {
                            self.check_picture(&b, info, &grid, f, t);
                        }
                    }
                    if let Some(lf) = last_hit {
                        let d = (f - lf).abs();
                        if d == 0 {
                            self.stats.repeated_ticks.fetch_add(1, Ordering::Relaxed);
                        } else if d > 1 {
                            self.stats
                                .skipped_frames
                                .fetch_add(d as u64 - 1, Ordering::Relaxed);
                        }
                    }
                    last_hit = Some(f);
                    last = Some((f, b));
                    self.stats.shown_frame.store(f, Ordering::Relaxed);
                    Some(t)
                }
                None => {
                    self.stats.misses.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            if let Some((f, b)) = &last {
                let mut out = b.copy();
                if let Some(o) = out.get_mut() {
                    o.set_pts(gst::ClockTime::from_nseconds(rt as u64));
                    o.set_dts(gst::ClockTime::NONE);
                    o.set_duration(gst::ClockTime::from_nseconds(fd as u64));
                    // The capture time of the source frame shown.
                    let tai = grid.tai_of_frame(*f);
                    if tai >= 0 {
                        gst::ReferenceTimestampMeta::add(
                            o,
                            &tai_caps,
                            gst::ClockTime::from_nseconds(tai as u64),
                            gst::ClockTime::from_nseconds(grid.frame_ns() as u64),
                        );
                    }
                }
                let pushed = appsrc.push_buffer(out);
                if let Some(t) = hit {
                    self.timeline.note_pushed(rt, t);
                }
                if let Err(e) = pushed {
                    debug!("Replay {}: push stopped: {:?}", self.block_id, e);
                    break;
                }
            }
            k += 1;
        }
        info!("Replay {}: output stopped", self.block_id);
        self.halt();
    }

    /// Diagnostics on a new pushed picture of the test material: the bitstrip
    /// (frame number from the group origin, same-rate material) and the
    /// burned-in capture time, which must hold the group time `t` of the tick.
    fn check_picture(
        &self,
        b: &gst::Buffer,
        info: &gst_video::VideoInfo,
        grid: &super::grid::FrameGrid,
        f: i64,
        t: i64,
    ) {
        let n = read_bitstrip(b, info).map_or(-1, i64::from);
        self.stats.strip_frame.store(n, Ordering::Relaxed);
        if let Some(origin) = self.timeline.origin_on(grid) {
            if n != f - origin {
                self.stats.strip_mismatches.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(us) = read_code_utc_us(b, info) {
            self.stats.code_utc_us.store(us as i64, Ordering::Relaxed);
            self.stats.time_checked.fetch_add(1, Ordering::Relaxed);
            let captured = us as i64 * 1000 + strom_types::tai::TAI_MINUS_UTC_NS as i64;
            if !holds(captured, grid.frame_ns() as i64, t) {
                self.stats.time_errors.fetch_add(1, Ordering::Relaxed);
                debug!(
                    "Replay {}: picture captured at {} does not hold group time {} ({} ns)",
                    self.block_id,
                    captured,
                    t,
                    t - captured
                );
            }
        }
    }
}

/// The segments to send to the decode workers, most urgent first: not done
/// (decoded, or no bytes yet), not in flight, and not waiting out a failure.
/// Marks them in flight; at most `workers` are in flight at once.
fn pick_decodes(
    wanted: &[Arc<Segment>],
    done: impl Fn(i64) -> bool,
    inflight: &mut HashSet<i64>,
    failed: &Backoff<i64>,
    workers: usize,
    now: Instant,
) -> Vec<Arc<Segment>> {
    let mut out = Vec::new();
    for s in wanted {
        if inflight.len() >= workers {
            break;
        }
        let key = s.start_ns;
        if inflight.contains(&key) || !failed.ready(&key, now) || done(key) {
            continue;
        }
        inflight.insert(key);
        out.push(Arc::clone(s));
    }
    out
}

/// Whether frames in caps `b` can go out under caps `a`: same format, size
/// and interlacing (frame rate and colorimetry aside).
fn same_layout(a: &gst::Caps, b: &gst::Caps) -> bool {
    match (
        gst_video::VideoInfo::from_caps(a),
        gst_video::VideoInfo::from_caps(b),
    ) {
        (Ok(a), Ok(b)) => {
            a.format() == b.format()
                && a.width() == b.width()
                && a.height() == b.height()
                && a.interlace_mode() == b.interlace_mode()
        }
        _ => a == b,
    }
}

/// Whether a frame captured at `captured` (TAI) with period `period` is the
/// one on screen at group time `t`: it started at or before `t` and the next
/// had not started. 1 ms of slack for timestamp rounding in the store.
fn holds(captured: i64, period: i64, t: i64) -> bool {
    const SLACK: i64 = 1_000_000;
    captured <= t + SLACK && t < captured + period + SLACK
}

impl Drop for ReplayPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Read the spike test material's bitstrip: 20 blocks of 48x32 px at the top left of
/// a 1920-wide picture, white = 1, LSB first. Diagnostics only.
fn read_bitstrip(buf: &gst::Buffer, info: &gst_video::VideoInfo) -> Option<u32> {
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf.as_ref(), info).ok()?;
    let y = frame.plane_data(0).ok()?;
    let stride = info.stride()[0] as usize;
    let sx = info.width() as f64 / 1920.0;
    let row = (16.0 * sx) as usize;
    let mut n = 0u32;
    for i in 0..20 {
        let x = ((i as f64 + 0.5) * 48.0 * sx) as usize;
        if *y.get(row * stride + x)? > 128 {
            n |= 1 << i;
        }
    }
    Some(n)
}

/// Read the burned-in UTC code row of the Reference Timestamp block
/// (`burn_in`), in microseconds. `None` when the picture has none.
/// Diagnostics only.
fn read_code_utc_us(buf: &gst::Buffer, info: &gst_video::VideoInfo) -> Option<u64> {
    use crate::blocks::builtin::reference_timestamp::burn_in::{decode_cells, Layout, CODE_CELLS};
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buf.as_ref(), info).ok()?;
    let y = frame.plane_data(0).ok()?;
    let stride = info.stride()[0] as usize;
    let l = Layout::for_frame(info.width(), info.height());
    let row = (l.code_y + l.cell_h / 2) as usize;
    let cells: Vec<bool> = (0..CODE_CELLS)
        .map(|c| {
            let x = (c * l.cell_w + l.cell_w / 2) as usize;
            y.get(row * stride + x).is_some_and(|v| *v > 128)
        })
        .collect();
    decode_cells(&cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::source::Location;

    fn seg(i: i64) -> Arc<Segment> {
        Arc::new(Segment {
            start_ns: i * S,
            end_ns: (i + 1) * S,
            ts_offset_ns: 0,
            object_start_ns: None,
            object_id: i.to_string(),
            location: Location::Urls(vec![]),
        })
    }

    #[test]
    fn a_failed_decode_waits_before_it_is_sent_again() {
        let wanted: Vec<_> = (0..3).map(seg).collect();
        let mut failed = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));
        let t0 = Instant::now();
        let mut inflight = HashSet::new();
        let keys = |v: Vec<Arc<Segment>>| v.iter().map(|s| s.start_ns / S).collect::<Vec<_>>();

        let first = pick_decodes(&wanted, |_| false, &mut inflight, &failed, 2, t0);
        assert_eq!(keys(first), vec![0, 1]);
        // Segment 0 fails and leaves flight; 1 is still in flight.
        inflight.remove(&0);
        failed.fail(0, t0);
        let next = pick_decodes(&wanted, |_| false, &mut inflight, &failed, 2, t0);
        assert_eq!(
            keys(next),
            vec![2],
            "the failed segment is not retried at once"
        );
        inflight.clear();
        let later = t0 + Duration::from_millis(250);
        let retry = pick_decodes(&wanted, |_| false, &mut inflight, &failed, 2, later);
        assert_eq!(keys(retry), vec![0, 1]);
    }

    #[test]
    fn segments_in_another_layout_are_told_apart() {
        gst::init().unwrap();
        let caps = |w: i32, rate: i32| {
            gst::Caps::builder("video/x-raw")
                .field("format", "NV12")
                .field("width", w)
                .field("height", 720)
                .field("framerate", gst::Fraction::new(rate, 1))
                .build()
        };
        assert!(same_layout(&caps(1280, 50), &caps(1280, 25)));
        assert!(!same_layout(&caps(1280, 50), &caps(1920, 50)));
    }

    #[test]
    fn done_segments_are_not_sent() {
        let wanted: Vec<_> = (0..3).map(seg).collect();
        let failed = Backoff::new(Duration::from_millis(250), Duration::from_secs(10));
        let mut inflight = HashSet::new();
        let got = pick_decodes(
            &wanted,
            |k| k != S,
            &mut inflight,
            &failed,
            2,
            Instant::now(),
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].start_ns, S);
    }

    #[test]
    fn a_frame_holds_the_instants_of_its_period() {
        let p = 20_000_000;
        assert!(holds(1_000, p, 1_000));
        assert!(holds(1_000, p, 1_000 + p - 1));
        // The next frame has started (beyond the slack).
        assert!(!holds(1_000, p, 1_000 + p + 2_000_000));
        // Captured after the group time: a frame from the future.
        assert!(!holds(1_000 + 5_000_000, p, 1_000));
    }
}
