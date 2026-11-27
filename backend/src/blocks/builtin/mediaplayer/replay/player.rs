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
//! - Offline (a [`Clip`] to export), the group runs on a frame counter
//!   instead ([`super::timeline::TimeSource::Counter`]): the tick is output
//!   frame `k`, running time `k * period`. It waits for the frame to be
//!   decoded instead of repeating the last one, pushes with backpressure
//!   (a blocking, non-live appsrc), and ends the stream at the clip's out
//!   point. Same timeline, same frames, as fast as downstream goes.
//! - A source switch (`lane`) puts a second index, fetcher and frame store
//!   beside the first, behind the same decoders and output: no rebuild. The
//!   tick holds the last picture until the new source has the one for the
//!   cursor, then shows the new source.

use super::super::bridge::NetworkBuffer;
use super::backoff::{worth_warning, Backoff};
#[cfg(test)]
use super::cache::Hold;
use super::cache::{by_urgency, Fetcher, SegmentStore};
use super::decode::{
    caps_frame_rate, choose_memory, decode_timeout, map_to_grid, pick_decoder, FrameMemory,
    SegmentDecoder,
};
use super::source::{Segment, SourceIndex};
use super::timeline::{
    tick_rt, Clip, DecodeRecord, OpenedSource, PlayerConfig, PlayerStats, ReplayTimeline,
    SourceControl, SourceIdent,
};
use crate::gst::frame_rate::estimate_frame_rate;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

mod lane;

pub use lane::SWITCH_TIMEOUT;
use lane::{Lane, Lanes, Settle};

const S: i64 = 1_000_000_000;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Default budget for one player's decoded frames (1080p NV12 is about
/// 3 MB a frame), in the memory they are decoded into: GPU memory when the
/// frames are in CUDA or GL memory. `STROM_REPLAY_DECODED_MB` overrides it.
const DEFAULT_DECODED_BUDGET_MB: u64 = 768;
/// Decode workers per player, each with its own long-lived decoder.
const DECODE_WORKERS: usize = 2;
/// Segments kept decoded whatever the budget: the one under the cursor and
/// the next.
const MIN_DECODED_SEGMENTS: usize = 2;
/// Upper bound of the decoded window ahead of the cursor, seconds.
pub const MAX_DECODE_AHEAD_S: u64 = 30;
/// Offline: longest wait for the media, and then for one frame, before the
/// export fails (a fetch or decode that never succeeds).
const OFFLINE_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// One decoded segment: the TAI of its first and last frame, and the bytes
/// they hold.
#[derive(Debug, Clone, Copy)]
struct DecodedSegment {
    first: i64,
    last: i64,
    bytes: u64,
}

/// Decoded frames by their own TAI (see `map_to_grid`), and which frames
/// each decoded segment contributed (for eviction).
#[derive(Default)]
struct FrameStore {
    frames: BTreeMap<i64, gst::Buffer>,
    segments: HashMap<i64, DecodedSegment>,
    /// Decoded bytes per second of media, from the latest decode: the size
    /// estimate of a segment not decoded yet.
    bytes_per_s: f64,
}

impl FrameStore {
    /// Drop the segments not in `keep` and their frames, returned so they
    /// can be freed outside the lock. A frame in the span of a segment that
    /// stays may be that segment's too (overlapping segments): it stays, or
    /// that segment would count as decoded with frames missing.
    fn evict_except(&mut self, keep: &HashSet<i64>) -> Vec<gst::Buffer> {
        let evict: Vec<DecodedSegment> = self
            .segments
            .iter()
            .filter(|(k, _)| !keep.contains(k))
            .map(|(_, d)| *d)
            .collect();
        self.segments.retain(|k, _| keep.contains(k));
        let kept: Vec<(i64, i64)> = self.segments.values().map(|d| (d.first, d.last)).collect();
        let mut doomed = Vec::new();
        for d in evict {
            let frames: Vec<i64> = self
                .frames
                .range(d.first..=d.last)
                .map(|(f, _)| *f)
                .filter(|f| !kept.iter().any(|&(a, b)| a <= *f && *f <= b))
                .collect();
            doomed.extend(frames.iter().filter_map(|f| self.frames.remove(f)));
        }
        doomed
    }

    /// Decoded size of a segment, or an estimate.
    fn size_of(&self, seg: &Segment) -> u64 {
        match self.segments.get(&seg.start_ns) {
            Some(d) => d.bytes,
            None => (self.bytes_per_s * seg.duration_ns() as f64 / 1e9) as u64,
        }
    }
}

/// The frame that holds `t`: the latest that starts at or before it, if the
/// next frame would not have started yet (`period_ns`, rounded up). By its
/// own start, not by a frame number on the player's grid: a source that
/// changed its frame phase (a restart) is shown at its real times.
fn frame_holding<B: Clone>(frames: &BTreeMap<i64, B>, t: i64, period_ns: i64) -> Option<(i64, B)> {
    let (&ft, b) = frames.range(..=t).next_back()?;
    (t - ft < period_ns).then(|| (ft, b.clone()))
}

/// A decoded frame and its own TAI.
type TaiFrame = (i64, gst::Buffer);

/// The last decoded frame of `seg`, if it is decoded.
fn held_in(store: &FrameStore, seg: &Segment) -> Option<TaiFrame> {
    let last = store.segments.get(&seg.start_ns)?.last;
    store.frames.get(&last).map(|b| (last, b.clone()))
}

/// The most urgent of `wanted` (ordered by urgency) whose decoded frames fit
/// in `budget` bytes, counting `size_of` each. The first `min_keep` always
/// fit. Stops at the first that does not, so what is kept is never less
/// urgent than what is left out.
fn within_budget(
    wanted: &[Arc<Segment>],
    size_of: impl Fn(&Segment) -> u64,
    budget: u64,
    min_keep: usize,
) -> Vec<Arc<Segment>> {
    let mut total = 0u64;
    let mut out = Vec::new();
    for (i, s) in wanted.iter().enumerate() {
        total = total.saturating_add(size_of(s));
        if i >= min_keep && total > budget {
            break;
        }
        out.push(Arc::clone(s));
    }
    out
}

/// How a player opens another source for a switch: from a URI as the
/// `source` property takes it, with the block's auth and media path.
pub type OpenSource = dyn Fn(&str) -> Result<OpenedSource, String> + Send + Sync;

/// What a player outputs: at which rate, and paced how.
#[derive(Clone, Default)]
pub struct Output {
    /// Output frame rate; `None` = the source's own rate.
    pub rate: Option<(u32, u32)>,
    /// Offline: the clip to play once, on the frame counter. `None` = live,
    /// on the flow clock.
    pub clip: Option<Clip>,
    /// The player's replay properties and identity.
    pub config: PlayerConfig,
    /// The `source` property as given, and how to open another source; a
    /// player without it cannot switch source.
    pub switching: Option<(String, Arc<OpenSource>)>,
}

pub struct ReplayPlayer {
    pub block_id: String,
    timeline: Arc<ReplayTimeline>,
    pub stats: Arc<PlayerStats>,
    /// Lock order: `lanes` before any lock of a lane.
    lanes: Mutex<Lanes>,
    switching: Option<Arc<OpenSource>>,
    /// Seconds of coded bytes a fetcher reads ahead.
    prefetch_s: u64,
    appsrc: gst::glib::WeakRef<gst_app::AppSrc>,
    caps: Mutex<Option<gst::Caps>>,
    decoder: &'static str,
    /// The memory frames are decoded into, chosen once for all workers.
    memory: OnceLock<FrameMemory>,
    workers: usize,
    /// Decoded window around the cursor, in its direction of travel.
    decode_behind_ns: i64,
    decode_ahead_ns: i64,
    /// Most bytes of decoded frames to hold.
    decoded_budget: u64,
    /// Output frame rate; `None` = the source's own rate.
    output_rate: Option<(u32, u32)>,
    /// Offline: the clip to play once, on the frame counter. `None` = live,
    /// on the flow clock.
    clip: Option<Clip>,
    /// Read the bitstrip and burned-in time of pushed frames
    /// (STROM_REPLAY_BITSTRIP=1).
    read_strip: bool,
    started: AtomicBool,
    stop: Arc<AtomicBool>,
    /// The last queued clip a lane was prepared for (see
    /// `prepare_queued`): each clip is prepared once.
    prepared: AtomicU64,
}

impl ReplayPlayer {
    pub fn new(
        block_id: &str,
        source: Arc<dyn SourceIndex>,
        timeline: Arc<ReplayTimeline>,
        appsrc: &gst_app::AppSrc,
        cache_ahead_s: u64,
        prefetch_s: u64,
        output: Output,
    ) -> Arc<Self> {
        let Output {
            rate: output_rate,
            clip,
            config,
            switching,
        } = output;
        let ident = SourceIdent {
            source: config.source.clone(),
            source_id: config.source_id.clone(),
            store: config.store.clone(),
        };
        let stats = Arc::new(PlayerStats::with_config(block_id, config));
        if let Some((n, d)) = output_rate {
            stats.set_output_rate(n, d);
        }
        timeline.register_player(&stats);
        if clip.is_some() {
            timeline.use_counter();
        }
        let decoder = pick_decoder();
        stats.set_decoder(decoder, DECODE_WORKERS);
        info!("Replay player {}: decoder {}", block_id, decoder);
        let prefetch_s = prefetch_s
            .max(cache_ahead_s.min(MAX_DECODE_AHEAD_S) + 2)
            .min(NetworkBuffer::MAX_READ_AHEAD_S);
        let fetcher = Fetcher::new(
            block_id,
            source,
            Arc::clone(&timeline),
            Arc::clone(&stats),
            prefetch_s,
        );
        let (uri, switching) = match switching {
            // An export plays one clip from one source.
            Some((uri, open)) if clip.is_none() => (Some(uri), Some(open)),
            _ => (None, None),
        };
        let player = Arc::new(Self {
            block_id: block_id.to_string(),
            lanes: Mutex::new(Lanes {
                current: Lane::new(fetcher, uri, ident, None, 0, false),
                pending: None,
            }),
            switching,
            prefetch_s,
            timeline,
            stats,
            appsrc: appsrc.downgrade(),
            caps: Mutex::new(None),
            decoder,
            memory: OnceLock::new(),
            workers: DECODE_WORKERS,
            decode_behind_ns: 2 * S,
            decode_ahead_ns: cache_ahead_s.clamp(1, MAX_DECODE_AHEAD_S) as i64 * S,
            decoded_budget: strom_types::env::var_opt("STROM_REPLAY_DECODED_MB")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|mb| *mb > 0)
                .unwrap_or(DEFAULT_DECODED_BUDGET_MB)
                .saturating_mul(1 << 20),
            output_rate,
            clip,
            read_strip: strom_types::env::var_opt("STROM_REPLAY_BITSTRIP")
                .is_some_and(|v| v == "1" || v == "true"),
            started: AtomicBool::new(false),
            stop: Arc::new(AtomicBool::new(false)),
            prepared: AtomicU64::new(0),
        });
        if player.switching.is_some() {
            let control: Weak<dyn SourceControl> = Arc::downgrade(&player) as Weak<ReplayPlayer>;
            player.stats.set_control(control);
        }
        player
    }

    /// Start the threads, once, when the flow starts.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        self.current().spawn();
        let (tx, rx) = mpsc::channel::<DecodeJob>();
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
            .spawn(move || match me.clip.clone() {
                Some(clip) => me.offline_loop(clip),
                None => me.tick_loop(),
            })
            .ok();
    }

    fn alive(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Stop the player's threads and every lane's fetcher.
    fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let lanes = lock(&self.lanes);
        lanes.current.halt();
        if let Some(p) = &lanes.pending {
            p.halt();
        }
    }

    /// The lane shown.
    fn current(&self) -> Arc<Lane> {
        Arc::clone(&lock(&self.lanes).current)
    }

    /// The lane shown, and the one being switched to.
    fn lanes(&self) -> (Arc<Lane>, Option<Arc<Lane>>) {
        let l = lock(&self.lanes);
        (Arc::clone(&l.current), l.pending.clone())
    }

    fn is_current(&self, lane: &Arc<Lane>) -> bool {
        Arc::ptr_eq(&lock(&self.lanes).current, lane)
    }

    /// Stop the player for good (its flow stopped or was torn down): every
    /// thread winds down on its own (a decode within its timeout, the fetcher
    /// at once, the others within one loop), and the player leaves its
    /// group, so the group's next run starts from a clean state.
    pub fn shutdown(&self) {
        if !self.stop.load(Ordering::SeqCst) {
            debug!("Replay {}: shutting down", self.block_id);
        }
        self.halt();
        self.timeline.unregister_player(&self.stats);
    }

    // --- decoding -----------------------------------------------------------

    fn decode_worker(self: Arc<Self>, rx: Arc<Mutex<mpsc::Receiver<DecodeJob>>>) {
        // One long-lived decoder per worker, kept across segments.
        let anchor = self.appsrc.upgrade().map(|a| a.upcast::<gst::Element>());
        let mut decoder =
            SegmentDecoder::with_memory(self.decoder, self.frame_memory(), anchor.as_ref());
        drop(anchor);
        while self.alive() {
            let job = lock(&rx).recv_timeout(Duration::from_millis(100));
            let (lane, seg) = match job {
                Ok(j) => j,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let started = Instant::now();
            match self.decode_one(&lane, &seg, &mut decoder) {
                Ok(()) => lock(&lane.failed).succeed(&seg.start_ns),
                Err(e) => {
                    lane.stats().note_decode(DecodeRecord {
                        start_ns: seg.start_ns,
                        end_ns: seg.end_ns,
                        ms: started.elapsed().as_millis() as u64,
                        frames: 0,
                        error: Some(e.clone()),
                    });
                    let attempts = lock(&lane.failed).fail(seg.start_ns, Instant::now());
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
            lock(&lane.inflight).remove(&seg.start_ns);
        }
    }

    /// The memory to decode into: chosen the first time from the decoder and
    /// what the flow takes from the output (see `choose_memory`).
    fn frame_memory(&self) -> FrameMemory {
        *self.memory.get_or_init(|| {
            let pad = self.appsrc.upgrade().and_then(|a| a.static_pad("src"));
            let memory = choose_memory(self.decoder, pad.as_ref());
            info!(
                "Replay {}: frames in {} memory ({})",
                self.block_id,
                memory.name(),
                self.decoder
            );
            // The diagnostics show where the frames are kept.
            self.stats.set_decoder(
                &format!("{} ({})", self.decoder, memory.name()),
                self.workers,
            );
            memory
        })
    }

    /// Decode one segment into the frame store. An error means the segment
    /// gave no usable frames; it is retried later with backoff.
    fn decode_one(
        &self,
        lane: &Arc<Lane>,
        seg: &Segment,
        decoder: &mut SegmentDecoder,
    ) -> Result<(), String> {
        // Evicted meanwhile: nothing to do, it is sent again when refetched.
        let Some(bytes) = lane.store().bytes(seg.start_ns) else {
            return Ok(());
        };
        let stats = lane.stats();
        let container = lane.store().info().and_then(|i| i.container);
        let started = Instant::now();
        let result = decoder.decode(
            bytes,
            container.as_deref(),
            decode_timeout(seg.duration_ns()),
        );
        let ms = started.elapsed().as_millis() as u64;
        let decoded = result?;
        // No frame rate from the index: take it from the stream, its caps or
        // else its timestamps (an SPS without timing, e.g. VideoToolbox).
        let grid = stats.grid().or_else(|| {
            let (n, d) = caps_frame_rate(&decoded.caps).or_else(|| {
                let est = estimate_frame_rate(decoded.frames.iter().map(|(pts, _)| *pts));
                if let Some((n, d)) = est {
                    info!(
                        "Replay {}: no frame rate in the index or the stream's caps, \
                         {}/{} estimated from the timestamps of segment {}",
                        self.block_id, n, d, seg.object_id
                    );
                }
                est
            })?;
            stats.init_grid(n, d, seg.first_sample_tai())
        });
        let Some(grid) = grid else {
            if !stats.frame_rate_missing.swap(true, Ordering::Relaxed) {
                warn!(
                    "Replay {}: the source has no frame rate: none in the index, the \
                     stream's caps or the timestamps of segment {} ({} frames); nothing \
                     can be shown",
                    self.block_id,
                    seg.object_id,
                    decoded.frames.len()
                );
            }
            return Err("no frame rate: none in the index or the stream".into());
        };
        stats.frame_rate_missing.store(false, Ordering::Relaxed);
        let n_decoded = decoded.frames.len();
        let frames = map_to_grid(seg, &grid, decoded.frames, decoded.first_pts);
        if frames.is_empty() {
            return Err(format!(
                "none of {} decoded frames fall inside the segment's timerange",
                n_decoded
            ));
        }
        let n_frames = frames.len();
        let expected = ((seg.end_ns - seg.start_ns) as f64 / grid.frame_ns()).round() as usize;
        if frames.len() != expected {
            let first = frames[0].0;
            let offset = |t: i64| ((t - first) as f64 / grid.frame_ns()).round() as i64;
            let have: HashSet<i64> = frames.iter().map(|f| offset(f.0)).collect();
            let missing: Vec<i64> = (0..expected as i64).filter(|k| !have.contains(k)).collect();
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
                    let e = format!(
                        "decoded as {}, the output is {}; not shown",
                        decoded.caps, out
                    );
                    // A source switched to that cannot go out: the switch
                    // gives up, instead of holding the last picture until
                    // it times out.
                    drop(caps);
                    if !self.is_current(lane) {
                        *lock(&lane.error) = Some(e.clone());
                    }
                    return Err(e);
                }
                Some(_) => {}
                None => *caps = Some(decoded.caps),
            }
        }
        if let (Some(lo), Some(hi)) = (frames.first().map(|f| f.0), frames.last().map(|f| f.0)) {
            let bytes: u64 = frames.iter().map(|(_, b)| b.size() as u64).sum();
            let mut store = lock(&lane.decoded);
            if seg.duration_ns() > 0 {
                store.bytes_per_s = bytes as f64 * 1e9 / seg.duration_ns() as f64;
            }
            store.segments.insert(
                seg.start_ns,
                DecodedSegment {
                    first: lo,
                    last: hi,
                    bytes,
                },
            );
            store.frames.extend(frames);
            lane.frame_ready.notify_all();
        }
        stats.note_decode(DecodeRecord {
            start_ns: seg.start_ns,
            end_ns: seg.end_ns,
            ms,
            frames: n_frames,
            error: None,
        });
        Ok(())
    }

    fn scheduler(self: Arc<Self>, tx: mpsc::Sender<DecodeJob>) {
        while self.alive() {
            self.prepare_queued();
            let (current, pending) = self.lanes();
            // The source switched to first: it is what the picture waits
            // for. Two sources share the budget.
            let lanes: Vec<Arc<Lane>> = pending.into_iter().chain([current]).collect();
            let budget = self.decoded_budget / lanes.len() as u64;
            if let Some((cursor, speed)) = self.timeline.cursor_now() {
                for lane in &lanes {
                    let busy: usize = lanes.iter().map(|l| lock(&l.inflight).len()).sum();
                    // A queued clip's lane: at its in point, forward.
                    let (at, v) = lane.fetcher.aim().map_or((cursor, speed), |t| (t, 1.0));
                    if !self.schedule(lane, &tx, at, v, budget, busy) {
                        return;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Keep `lane`'s segments around the group cursor `group_cursor`
    /// decoded, within `budget` bytes, and evict the rest. `busy` decodes
    /// are in flight over all lanes. `false` when the workers are gone.
    fn schedule(
        &self,
        lane: &Arc<Lane>,
        tx: &mpsc::Sender<DecodeJob>,
        group_cursor: i64,
        speed: f64,
        budget: u64,
        busy: usize,
    ) -> bool {
        let stats = lane.stats();
        // The group's cursor, in this source's own time.
        let cursor = group_cursor.saturating_sub(stats.offset_ns());
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
        let store = lane.store();
        let mut near = by_urgency(&store.segments_in(lo, hi), cursor, forward);
        // In a hole, the segment before it holds the picture: most
        // urgent, however far behind it ends.
        if let Some(h) = store.hold().filter(|h| h.holds(cursor)) {
            near.retain(|s| s.start_ns != h.seg.start_ns);
            near.insert(0, h.seg);
        }
        // The most urgent segments whose frames fit the memory budget.
        let wanted = {
            let decoded = lock(&lane.decoded);
            within_budget(&near, |s| decoded.size_of(s), budget, MIN_DECODED_SEGMENTS)
        };
        {
            let decoded = lock(&lane.decoded);
            let mut inflight = lock(&lane.inflight);
            let mut failed = lock(&lane.failed);
            let keys: HashSet<i64> = near.iter().map(|s| s.start_ns).collect();
            failed.retain(|k| keys.contains(k));
            // The workers are shared: the other lanes' decodes count.
            let others = busy.saturating_sub(inflight.len());
            let jobs = pick_decodes(
                &wanted,
                |k| decoded.segments.contains_key(&k) || store.bytes(k).is_none(),
                &mut inflight,
                &failed,
                self.workers.saturating_sub(others),
                Instant::now(),
            );
            for s in jobs {
                if tx.send((Arc::clone(lane), s)).is_err() {
                    return false;
                }
            }
        }
        let keep: HashSet<i64> = wanted.iter().map(|s| s.start_ns).collect();
        // Frees the evicted frames after the lock is released, so a
        // large eviction does not hold up the output tick.
        let doomed = {
            let mut decoded = lock(&lane.decoded);
            let doomed = decoded.evict_except(&keep);
            stats
                .cached_segments
                .store(decoded.segments.len() as u64, Ordering::Relaxed);
            doomed
        };
        drop(doomed);
        true
    }

    fn frame_holding(&self, t: i64, period_ns: i64) -> Option<(i64, gst::Buffer)> {
        frame_holding(&lock(&self.current().decoded).frames, t, period_ns)
    }

    /// In a hole of the recording at `t`: the hole and the last frame before
    /// it (once its segment is decoded), as when play runs into the hole.
    fn held_frame(&self, t: i64) -> Option<((i64, i64), Option<TaiFrame>)> {
        let lane = self.current();
        let h = lane.store().hold().filter(|h| h.holds(t))?;
        let decoded = lock(&lane.decoded);
        let frame = held_in(&decoded, &h.seg);
        Some((h.hole, frame))
    }

    // --- output -------------------------------------------------------------

    fn tick_loop(self: Arc<Self>) {
        // Wait for the flow to play and for the first frames to exist.
        let (appsrc, clock, mut base, mut grid) = loop {
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
                    // A source switched to before the first picture.
                    let c = self.timeline.cursor_item_now();
                    self.settle_switch(c.map(|c| c.0), c.map_or(0, |c| c.1));
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
        let now_rt = |base: gst::ClockTime| clock.time().nseconds() as i64 - base.nseconds() as i64;
        // Ticks from running time 0 on: a PTS is never negative.
        let mut k = ((now_rt(base) as f64 / fd).ceil() as i64).max(0);
        // Shown TAI and buffer of the last picture.
        let mut last: Option<(i64, gst::Buffer)> = None;
        let mut last_hit: Option<i64> = None;
        let mut period_ns = grid.frame_ns().ceil() as i64;

        while self.alive() {
            let Some(appsrc) = weak.upgrade() else {
                break;
            };
            match appsrc.current_state() {
                gst::State::Null => break,
                gst::State::Playing => {}
                // Paused (or on its way down): a live source pushes nothing.
                // Pushed now, the pictures would queue, with running times
                // the flow goes back on when it plays again.
                _ => {
                    drop(appsrc);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }
            // The base time changes when the flow goes PAUSED -> PLAYING:
            // read it each tick, as the timeline's "now" does.
            if let Some(b) = appsrc.base_time() {
                if b != base {
                    // Running time went back by the time spent paused: so do
                    // the ticks, or the next one would wait that long. The
                    // group moves onto the new base before this player asks
                    // it for a time on it.
                    base = b;
                    self.timeline.observe_base(b.nseconds() as i64);
                    k = ((now_rt(base) as f64 / fd).ceil() as i64).max(0);
                }
            }
            drop(appsrc);
            let rt = tick_rt(k, on, od_);
            let id =
                clock.new_single_shot_id(base + gst::ClockTime::from_nseconds(rt.max(0) as u64));
            let _ = id.wait();

            let Some(appsrc) = weak.upgrade() else {
                break;
            };
            match appsrc.current_state() {
                gst::State::Null => break,
                gst::State::Playing => {}
                // Paused while waiting: the top of the loop waits it out.
                _ => continue,
            }
            // Woke up late: skip to the current tick instead of bursting.
            let now = now_rt(base);
            if now > rt + 2 * fd as i64 {
                self.stats.late_ticks.fetch_add(1, Ordering::Relaxed);
                k = ((now as f64 / fd).ceil() as i64).max(0);
                continue;
            }

            // The group's time at this tick, and this angle's frame for it.
            // And the queued clip that plays then (0: none).
            let (want_group, item) = self
                .timeline
                .cursor_item(rt)
                .map_or((None, 0), |(t, i)| (Some(t), i));
            if let Some(t) = want_group {
                self.stats.wanted_tai.store(t, Ordering::Relaxed);
            }
            // Switching source: the last picture holds until the new source
            // has the one for this time; from then on it is the source shown,
            // with its own grid and trim.
            let switching = match self.settle_switch(want_group, item) {
                Settle::Waiting => true,
                Settle::Committed => {
                    if let Some(g) = self.stats.grid() {
                        grid = g;
                        period_ns = grid.frame_ns().ceil() as i64;
                    }
                    last_hit = None;
                    false
                }
                Settle::None => false,
            };
            // The group's time at this tick, in this source's own time.
            let offset = self.stats.offset_ns();
            let want = want_group.map(|t| t.saturating_sub(offset));
            let mut hit = want
                .filter(|_| !switching)
                .and_then(|t| self.frame_holding(t, period_ns).map(|(ft, b)| (ft, t, b)));
            // No frame holds the time: in a hole of the recording, the last
            // frame before it (also right after a seek into the hole).
            let mut gap = None;
            if let (None, Some(t), false) = (&hit, want, switching) {
                if let Some((hole, frame)) = self.held_frame(t) {
                    gap = Some(hole);
                    hit = frame.map(|(ft, b)| (ft, t, b));
                }
            }
            self.stats.set_gap(gap);
            self.stats.note_tick(hit.is_some(), want);
            let hit = match hit {
                Some((ft, t, b)) => {
                    let new_picture = last.as_ref().is_none_or(|(lt, _)| *lt != ft);
                    // A held frame is from before the hole, not of the time.
                    if self.read_strip && new_picture && gap.is_none() {
                        if let Some(info) = &video_info {
                            self.check_picture(&b, info, &grid, ft, t);
                        }
                    }
                    let f = grid.nearest_frame(ft);
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
                    last = Some((ft, b));
                    self.stats.shown_frame.store(f, Ordering::Relaxed);
                    self.stats.shown_tai.store(ft, Ordering::Relaxed);
                    Some(t)
                }
                None => {
                    if !switching {
                        self.stats.misses.fetch_add(1, Ordering::Relaxed);
                    }
                    None
                }
            };
            if let Some((shown_tai, b)) = &last {
                let out = out_buffer(b, rt, fd as i64, *shown_tai, &grid, &tai_caps);
                let pushed = appsrc.push_buffer(out);
                if let Some(t) = hit {
                    self.timeline.note_pushed(rt, t.saturating_add(offset));
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

    /// The offline output: the clip on the frame counter. Tick `k` is the
    /// output frame at running time `k * period`; it waits for its frame to
    /// be decoded (no repeat on a miss), pushes it with backpressure, and
    /// ends the stream when play reaches the out point. Nothing waits on the
    /// clock, so it runs as fast as fetching, decoding and downstream allow.
    fn offline_loop(self: Arc<Self>, clip: Clip) {
        let started = Instant::now();
        let result = self.run_offline(&clip);
        // Held only for the end: a strong ref across the run would keep the
        // element alive after its flow is torn down.
        let Some(appsrc) = self.appsrc.upgrade() else {
            self.halt();
            return;
        };
        match result {
            Ok(frames) => {
                let wall = started.elapsed().as_secs_f64();
                let media = self
                    .stats
                    .output_rate()
                    .map_or(0.0, |(n, d)| frames as f64 * d as f64 / n as f64);
                info!(
                    "Replay {}: clip done, {} frames ({:.2} s) in {:.2} s, {:.1}x real time",
                    self.block_id,
                    frames,
                    media,
                    wall,
                    if wall > 0.0 { media / wall } else { 0.0 }
                );
                let _ = appsrc.end_of_stream();
            }
            Err(None) => debug!("Replay {}: offline output stopped", self.block_id),
            Err(Some(e)) => {
                warn!("Replay {}: export failed: {}", self.block_id, e);
                appsrc.post_error_message(gst::error_msg!(
                    gst::StreamError::Failed,
                    ["Replay {}: {}", self.block_id, e]
                ));
            }
        }
        self.halt();
    }

    /// The body of [`Self::offline_loop`]: the number of frames pushed, or
    /// why it stopped (`None`: the flow stopped).
    fn run_offline(&self, clip: &Clip) -> Result<u64, Option<String>> {
        // Media, frame rate and caps: the first decode.
        let deadline = Instant::now() + OFFLINE_FRAME_TIMEOUT;
        let appsrc = || self.appsrc.upgrade().ok_or(None);
        let grid = loop {
            if !self.alive() {
                return Err(None);
            }
            let appsrc = appsrc()?;
            // Pushed before the flow is on its way to play, a buffer would
            // be refused (flushing).
            if let (true, true, Some(grid), Some(_)) = (
                appsrc.current_state() >= gst::State::Paused,
                lock(&self.caps).is_some(),
                self.stats.grid(),
                self.timeline.bounds(),
            ) {
                break grid;
            }
            if Instant::now() > deadline {
                return Err(Some(format!(
                    "no media from the source after {:?}",
                    OFFLINE_FRAME_TIMEOUT
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let (on, od) = self.output_rate.unwrap_or((grid.fps_n, grid.fps_d));
        self.stats.set_output_rate(on, od);
        let mut caps = lock(&self.caps).clone().ok_or(None)?;
        caps.make_mut()
            .set("framerate", gst::Fraction::new(on as i32, od as i32));
        let video_info = gst_video::VideoInfo::from_caps(&caps).ok();
        appsrc()?.set_caps(Some(&caps));
        self.timeline.set_counter(0);
        let applied = self
            .timeline
            .start_clip(clip, 0)
            .map_err(|e| Some(format!("clip: {}", e)))?;
        info!(
            "Replay {}: offline clip from {} at {}x, output {}/{} fps, caps {}",
            self.block_id, applied.tai, applied.speed, on, od, caps
        );
        let tai_caps = gst::Caps::builder("timestamp/x-tai").build();
        let period_ns = grid.frame_ns().ceil() as i64;
        let mut last_frame: Option<i64> = None;
        let mut k: i64 = 0;
        loop {
            let rt = tick_rt(k, on, od);
            self.timeline.set_counter(rt);
            let Some(t_group) = self.timeline.playing_at(rt) else {
                return Ok(k as u64);
            };
            self.stats.wanted_tai.store(t_group, Ordering::Relaxed);
            let t = t_group.saturating_sub(self.stats.offset_ns());
            let (ft, b, gap) = self.wait_frame(t, period_ns)?;
            self.stats.set_gap(gap);
            if self.read_strip && gap.is_none() && last_frame != Some(ft) {
                if let Some(info) = &video_info {
                    self.check_picture(&b, info, &grid, ft, t);
                }
            }
            last_frame = Some(ft);
            self.stats
                .shown_frame
                .store(grid.nearest_frame(ft), Ordering::Relaxed);
            self.stats.shown_tai.store(ft, Ordering::Relaxed);
            let duration = tick_rt(k + 1, on, od) - rt;
            // Blocks while downstream is full: the backpressure that paces
            // the export.
            appsrc()?
                .push_buffer(out_buffer(&b, rt, duration, ft, &grid, &tai_caps))
                .map_err(|e| {
                    debug!("Replay {}: push stopped: {:?}", self.block_id, e);
                    None
                })?;
            self.stats.note_tick(true, Some(t));
            self.timeline.note_pushed(rt, t_group);
            k += 1;
        }
    }

    /// The frame that holds `t`, waiting until it is decoded: the latest
    /// frame at or before `t`, or in a hole of the recording the last frame
    /// before the hole (and the hole). An error after
    /// [`OFFLINE_FRAME_TIMEOUT`]; `None` when the player stops.
    #[allow(clippy::type_complexity)]
    fn wait_frame(
        &self,
        t: i64,
        period_ns: i64,
    ) -> Result<(i64, gst::Buffer, Option<(i64, i64)>), Option<String>> {
        let deadline = Instant::now() + OFFLINE_FRAME_TIMEOUT;
        // An export does not switch source: one lane.
        let lane = self.current();
        let mut store = lock(&lane.decoded);
        loop {
            if let Some((ft, b)) = frame_holding(&store.frames, t, period_ns) {
                return Ok((ft, b, None));
            }
            if let Some(h) = lane.store().hold().filter(|h| h.holds(t)) {
                if let Some((ft, b)) = held_in(&store, &h.seg) {
                    return Ok((ft, b, Some(h.hole)));
                }
            }
            if !self.alive() {
                return Err(None);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Some(format!(
                    "the frame at TAI {} was not decoded within {:?}",
                    super::timeline::tai_string(t),
                    OFFLINE_FRAME_TIMEOUT
                )));
            }
            store = lane
                .frame_ready
                .wait_timeout(store, (deadline - now).min(Duration::from_millis(50)))
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Diagnostics on a new pushed picture of the test material: the bitstrip
    /// (frame number from the group origin, same-rate material) and the
    /// burned-in capture time, which must hold the group time `t` of the tick.
    /// `shown_tai` is the frame's own TAI.
    fn check_picture(
        &self,
        b: &gst::Buffer,
        info: &gst_video::VideoInfo,
        grid: &super::grid::FrameGrid,
        shown_tai: i64,
        t: i64,
    ) {
        let n = read_bitstrip(b, info).map_or(-1, i64::from);
        self.stats.strip_frame.store(n, Ordering::Relaxed);
        if let Some(origin) = self.timeline.origin_on(grid) {
            if n != grid.nearest_frame(shown_tai) - origin {
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

/// The output buffer for a tick at running time `rt`: the decoded frame,
/// restamped, carrying the capture time (TAI) of the source frame shown.
fn out_buffer(
    b: &gst::Buffer,
    rt: i64,
    duration: i64,
    shown_tai: i64,
    grid: &super::grid::FrameGrid,
    tai_caps: &gst::Caps,
) -> gst::Buffer {
    let mut out = b.copy();
    if let Some(o) = out.get_mut() {
        o.set_pts(gst::ClockTime::from_nseconds(rt as u64));
        o.set_dts(gst::ClockTime::NONE);
        o.set_duration(gst::ClockTime::from_nseconds(duration as u64));
        if shown_tai >= 0 {
            gst::ReferenceTimestampMeta::add(
                o,
                tai_caps,
                gst::ClockTime::from_nseconds(shown_tai as u64),
                gst::ClockTime::from_nseconds(grid.frame_ns() as u64),
            );
        }
    }
    out
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

/// A segment for a decode worker, and the lane it is from.
type DecodeJob = (Arc<Lane>, Arc<Segment>);

/// Whether frames in caps `b` can go out under caps `a`: same memory,
/// format, size and interlacing (frame rate and colorimetry aside).
fn same_layout(a: &gst::Caps, b: &gst::Caps) -> bool {
    let memory = |c: &gst::Caps| {
        c.features(0)
            .map_or_else(|| "memory:SystemMemory".to_string(), |f| f.to_string())
    };
    if memory(a) != memory(b) {
        return false;
    }
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
        self.shutdown();
    }
}

/// Owner of a player for its flow's lifetime: shuts the player down when
/// dropped. The player's own threads hold it too, so its `Drop` alone would
/// not run while they live.
pub struct PlayerGuard(pub Arc<ReplayPlayer>);

impl Drop for PlayerGuard {
    fn drop(&mut self) {
        self.0.shutdown();
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
        // Frames in CUDA memory cannot go out under system-memory caps.
        let mut cuda = caps(1280, 50);
        cuda.make_mut()
            .set_features(0, Some(gst::CapsFeatures::new(["memory:CUDAMemory"])));
        assert!(!same_layout(&caps(1280, 50), &cuda));
        assert!(same_layout(&cuda, &cuda.clone()));
    }

    #[test]
    fn decoded_window_keeps_to_its_budget() {
        let wanted: Vec<_> = (0..6).map(seg).collect();
        let mb = 1u64 << 20;
        // 155 MB a segment (1 s of 1080p50 NV12), 512 MB budget: three fit.
        let kept = within_budget(&wanted, |_| 155 * mb, 512 * mb, 2);
        assert_eq!(kept.len(), 3);
        // The segment under the cursor and the next are always kept.
        let kept = within_budget(&wanted, |_| 1000 * mb, 512 * mb, 2);
        assert_eq!(kept.len(), 2);
        // Sizes not known yet (nothing decoded) do not hold anything back.
        let kept = within_budget(&wanted, |_| 0, 512 * mb, 2);
        assert_eq!(kept.len(), 6);
    }

    #[test]
    fn frame_store_estimates_undecoded_segments() {
        let mut store = FrameStore {
            bytes_per_s: 100.0,
            ..Default::default()
        };
        store.segments.insert(
            0,
            DecodedSegment {
                first: 0,
                last: 49,
                bytes: 70,
            },
        );
        assert_eq!(store.size_of(&seg(0)), 70);
        assert_eq!(store.size_of(&seg(1)), 100);
    }

    #[test]
    fn evicting_a_segment_keeps_the_frames_of_one_that_overlaps_it() {
        gst::init().unwrap();
        let mut store = FrameStore::default();
        // Segment 0 holds frames 0..=9, segment 5 (overlapping) 5..=14.
        for f in 0..15 {
            store.frames.insert(f, gst::Buffer::new());
        }
        let span = |first, last| DecodedSegment {
            first,
            last,
            bytes: 0,
        };
        store.segments.insert(0, span(0, 9));
        store.segments.insert(5, span(5, 14));
        let doomed = store.evict_except(&HashSet::from([5]));
        assert_eq!(doomed.len(), 5);
        assert_eq!(
            store.frames.keys().copied().collect::<Vec<_>>(),
            (5..15).collect::<Vec<_>>()
        );
        assert!(store.segments.contains_key(&5) && !store.segments.contains_key(&0));
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
    fn a_frame_is_found_by_its_own_start_after_a_phase_change() {
        // The first run's frames start on the 20 ms grid; after a restart
        // (and a gap) the source's frames start 8.8 ms later in the period.
        let p = 20_000_000;
        let shift = 8_800_000;
        let mut frames = BTreeMap::new();
        for i in 0..10 {
            frames.insert(i * p, i);
        }
        let run2 = 100 * p + shift;
        for i in 0..10 {
            frames.insert(run2 + i * p, 100 + i);
        }
        // Inside the second run, the frame shown is the one captured last
        // at or before the group time.
        assert_eq!(
            frame_holding(&frames, run2 + p + 5_000_000, p),
            Some((run2 + p, 101))
        );
        // Just before a frame of the second run starts: the one before it,
        // not the frame the first run's grid would name.
        assert_eq!(
            frame_holding(&frames, run2 + 2 * p - 1, p),
            Some((run2 + p, 101))
        );
        assert_eq!(frame_holding(&frames, run2 - 1, p), None);
        assert_eq!(frame_holding(&frames, 5 * p + 1, p), Some((5 * p, 5)));
        // A frame not decoded is a miss, not the frame before it.
        frames.remove(&(3 * p));
        assert_eq!(frame_holding(&frames, 3 * p + 1, p), None);
    }

    #[test]
    fn a_hole_shows_the_last_frame_before_it() {
        gst::init().unwrap();
        let p = 20_000_000;
        let mut store = FrameStore::default();
        // Segment 0..1 s decoded; the cursor was at 50 s and showed a frame
        // there; segment 1 s.. is a hole.
        for i in 0..50 {
            store.frames.insert(i * p, gst::Buffer::new());
        }
        store.segments.insert(
            0,
            DecodedSegment {
                first: 0,
                last: 49 * p,
                bytes: 0,
            },
        );
        store.frames.insert(50 * S, gst::Buffer::new());
        let h = Hold {
            seg: seg(0),
            hole: (S, 93 * S),
        };
        // A seek to 20 s: no frame holds it, and the frame before 20 s in the
        // store would be far off; the hold is the last frame of segment 0.
        assert!(frame_holding(&store.frames, 20 * S, p).is_none());
        assert!(h.holds(20 * S));
        assert_eq!(held_in(&store, &h.seg).map(|(t, _)| t), Some(49 * p));
        assert!(!h.holds(93 * S));
        // Not decoded yet: nothing to hold.
        store.segments.clear();
        assert!(held_in(&store, &h.seg).is_none());
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
