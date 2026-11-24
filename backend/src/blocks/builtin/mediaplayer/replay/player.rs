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

use super::cache::{by_urgency, Fetcher, SegmentStore};
use super::decode::{caps_frame_rate, decode_bytes, map_to_grid, pick_decoder};
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
    caps: Mutex<Option<gst::Caps>>,
    decoder: &'static str,
    workers: usize,
    /// Decoded window around the cursor, in its direction of travel.
    decode_behind_ns: i64,
    decode_ahead_ns: i64,
    /// Read the bitstrip of pushed frames (STROM_REPLAY_BITSTRIP=1).
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
            caps: Mutex::new(None),
            decoder,
            workers: 2,
            decode_behind_ns: 2 * S,
            decode_ahead_ns: cache_ahead_s.max(1) as i64 * S,
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
            self.decode_one(&seg);
            lock(&self.inflight).remove(&seg.start_ns);
        }
    }

    fn decode_one(&self, seg: &Segment) {
        let Some(bytes) = self.store.bytes(seg.start_ns) else {
            return;
        };
        let container = self.store.info().and_then(|i| i.container);
        let started = Instant::now();
        let result = decode_bytes(&bytes, container.as_deref(), self.decoder);
        let ms = started.elapsed().as_millis() as u64;
        let decoded = match result {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    "Replay {}: segment {} failed: {}",
                    self.block_id, seg.object_id, e
                );
                return;
            }
        };
        // No frame rate from the index: take it from the stream.
        let grid = self.timeline.grid().or_else(|| {
            let (n, d) = caps_frame_rate(&decoded.caps)?;
            self.timeline.ensure_grid(n, d, seg.first_sample_tai())
        });
        let Some(grid) = grid else {
            warn!("Replay {}: no frame rate known", self.block_id);
            return;
        };
        let n_decoded = decoded.frames.len();
        let frames = map_to_grid(seg, &grid, decoded.frames);
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
        lock(&self.caps).get_or_insert(decoded.caps);
        if let (Some(lo), Some(hi)) = (frames.first().map(|f| f.0), frames.last().map(|f| f.0)) {
            let mut store = lock(&self.decoded);
            store.segments.insert(seg.start_ns, (lo, hi));
            store.frames.extend(frames);
        }
        self.stats.decodes.fetch_add(1, Ordering::Relaxed);
        self.stats.last_decode_ms.store(ms, Ordering::Relaxed);
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
                    for s in &wanted {
                        if inflight.len() >= self.workers {
                            break;
                        }
                        let key = s.start_ns;
                        if decoded.segments.contains_key(&key)
                            || inflight.contains(&key)
                            || self.store.bytes(key).is_none()
                        {
                            continue;
                        }
                        inflight.insert(key);
                        if tx.send(Arc::clone(s)).is_err() {
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
            } else if self.timeline.grid().is_none() {
                // The index gave no frame rate: decode the first segment to
                // learn it from the stream.
                if let Some(first) = self.store.segments_in(i64::MIN / 2, i64::MAX / 2).first() {
                    let key = first.start_ns;
                    let mut inflight = lock(&self.inflight);
                    if self.store.bytes(key).is_some() && inflight.insert(key) {
                        let _ = tx.send(Arc::clone(first));
                    }
                }
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
        let (appsrc, clock, base) = loop {
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
                    if has_caps && self.timeline.grid().is_some() {
                        break (appsrc, clock, base);
                    }
                }
            }
            drop(appsrc);
            std::thread::sleep(Duration::from_millis(10));
        };
        let Some(grid) = self.timeline.grid() else {
            return;
        };

        let mut caps = lock(&self.caps).clone().expect("caps checked above");
        caps.make_mut().set(
            "framerate",
            gst::Fraction::new(grid.fps_n as i32, grid.fps_d as i32),
        );
        let video_info = gst_video::VideoInfo::from_caps(&caps).ok();
        appsrc.set_caps(Some(&caps));
        info!("Replay {}: output running, caps {}", self.block_id, caps);
        let weak = appsrc.downgrade();
        drop(appsrc);
        let tai_caps = gst::Caps::builder("timestamp/x-tai").build();

        let fd = grid.frame_ns();
        let now_rt = || clock.time().nseconds() as i64 - base.nseconds() as i64;
        let mut k = (now_rt() as f64 / fd).ceil() as i64;
        let mut last: Option<(i64, gst::Buffer)> = None;

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

            let frame = self.timeline.frame_at(rt);
            let hit = match frame.and_then(|f| self.frame_at(f).map(|b| (f, b))) {
                Some((f, b)) => {
                    if self.read_strip && last.as_ref().is_none_or(|(lf, _)| *lf != f) {
                        if let Some(info) = &video_info {
                            let n = read_bitstrip(&b, info).map_or(-1, i64::from);
                            self.stats.strip_frame.store(n, Ordering::Relaxed);
                            // The test material numbers frames from the
                            // group origin.
                            if n != f - self.timeline.origin() {
                                self.stats.strip_mismatches.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    last = Some((f, b));
                    self.stats.shown_frame.store(f, Ordering::Relaxed);
                    Some(f)
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
                    let tai = grid.tai_of_frame(*f);
                    if tai >= 0 {
                        gst::ReferenceTimestampMeta::add(
                            o,
                            &tai_caps,
                            gst::ClockTime::from_nseconds(tai as u64),
                            gst::ClockTime::from_nseconds(fd as u64),
                        );
                    }
                }
                let pushed = appsrc.push_buffer(out);
                if let Some(f) = hit {
                    self.timeline.note_pushed(f);
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
