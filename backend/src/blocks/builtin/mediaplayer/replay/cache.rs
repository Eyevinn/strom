//! Index window and coded-bytes cache of one replay player.
//!
//! A fetcher thread with its own small async runtime keeps, around the
//! group cursor:
//! - the index: segments listed from the [`SourceIndex`] in fixed TAI chunks,
//!   plus, when the source can grow, a live poll at segment cadence of the
//!   newest segments back to the edge the index knows (a newest-first page,
//!   no time filter), and now and then a bounded listing of a hole just
//!   behind the edge, for segments registered late;
//! - the coded bytes of the segments within `[T - behind, T + ahead]`, fetched
//!   nearest first in the direction of travel, a few at a time.
//!
//! The decode side only reads this store, so the output tick never waits on
//! the network: a segment whose bytes have not arrived is simply not decoded
//! yet, and the tick repeats its last frame.

use super::backoff::{worth_warning, Backoff};
use super::decode::check_supported;
use super::source::{Segment, SourceIndex, SourceInfo};
use super::timeline::{IndexView, PlayerStats, ReplayTimeline, TimeDomain};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

const S: i64 = 1_000_000_000;
/// Index listing granularity on the TAI timeline.
const CHUNK_NS: i64 = 20 * S;
/// Listed chunks kept either side of the cursor's chunk.
const KEEP_CHUNKS: i64 = 15;
/// Concurrent object fetches per player.
const MAX_INFLIGHT: usize = 4;
/// Wait before listing a failed chunk again: doubling up to the max.
const LIST_RETRY: Duration = Duration::from_millis(250);
const LIST_RETRY_MAX: Duration = Duration::from_secs(5);
/// Wait before fetching a failed segment again: doubling up to the max.
const FETCH_RETRY: Duration = Duration::from_millis(500);
const FETCH_RETRY_MAX: Duration = Duration::from_secs(30);
/// A failed fetch re-lists its chunk (URLs may have expired), but not more
/// often than this.
const RELIST_MIN: Duration = Duration::from_secs(5);
/// Default budget for one player's coded bytes. `STROM_REPLAY_CODED_MB`
/// overrides it. The window ahead can be up to two minutes (`read_ahead_s`),
/// over 1 GB at 50 Mbit/s with the window behind.
const DEFAULT_CODED_BUDGET_MB: u64 = 512;
/// Segments kept fetched whatever the budget: the most urgent ones.
const MIN_CODED_SEGMENTS: usize = 2;
/// Wait before polling the live edge again after a failed poll: the segment
/// cadence doubling per consecutive failure, up to this.
const LIVE_POLL_RETRY_MAX: Duration = Duration::from_secs(5);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A fetched segment: its end (TAI ns) and its bytes.
type Fetched = (i64, Arc<Vec<u8>>);

/// Segments known around the cursor and the bytes fetched for them. Keyed by
/// segment start (TAI ns), which stays stable as a live index grows.
#[derive(Default)]
pub struct SegmentStore {
    index: Mutex<BTreeMap<i64, Arc<Segment>>>,
    /// Fetched bytes by segment start, with the segment's end (eviction keeps
    /// a segment that overlaps the window, however long it is).
    bytes: Mutex<HashMap<i64, Fetched>>,
    info: Mutex<Option<SourceInfo>>,
    hold: Mutex<Option<Hold>>,
}

/// The cursor is in a hole of the recording: the picture to hold is the last
/// frame of `seg`, the segment that ends where the hole starts. `hole` is
/// `[start, end)`, its end the media after it, or as far as the index has
/// listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub seg: Arc<Segment>,
    pub hole: (i64, i64),
}

impl Hold {
    pub fn holds(&self, t: i64) -> bool {
        self.hole.0 <= t && t < self.hole.1
    }
}

impl SegmentStore {
    /// The hole the cursor is in, if any, and what to show there.
    pub fn hold(&self) -> Option<Hold> {
        lock(&self.hold).clone()
    }

    pub fn info(&self) -> Option<SourceInfo> {
        lock(&self.info).clone()
    }

    /// Segments overlapping `[start, end)`.
    pub fn segments_in(&self, start: i64, end: i64) -> Vec<Arc<Segment>> {
        let index = lock(&self.index);
        let mut out = Vec::new();
        // The segment containing `start` begins before it.
        if let Some((_, s)) = index.range(..start).next_back() {
            if s.end_ns > start {
                out.push(Arc::clone(s));
            }
        }
        out.extend(index.range(start..end).map(|(_, s)| Arc::clone(s)));
        out
    }

    pub fn bytes(&self, start: i64) -> Option<Arc<Vec<u8>>> {
        lock(&self.bytes).get(&start).map(|(_, b)| Arc::clone(b))
    }

    fn insert(&self, segs: impl IntoIterator<Item = Segment>) {
        let mut index = lock(&self.index);
        for s in segs {
            index.insert(s.start_ns, Arc::new(s));
        }
    }

    /// Insert the segments the index does not have yet (by start), keeping
    /// those it has: a live poll lists the same few again.
    fn insert_new(&self, segs: impl IntoIterator<Item = Segment>) {
        let mut index = lock(&self.index);
        for s in segs {
            index.entry(s.start_ns).or_insert_with(|| Arc::new(s));
        }
    }
}

/// A hole shorter than this between two segments is rounding, not a gap,
/// when the frame rate is not known (else see [`seam_tolerance`]).
const MIN_GAP_NS: i64 = 1_000_000;
/// How often a player recomputes what it publishes about its index.
const INDEX_VIEW_EVERY: Duration = Duration::from_millis(500);
/// A hole younger than this behind the live edge may still fill with a
/// segment registered late: it is not reported as a gap yet.
const LIVE_BACKFILL_NS: i64 = 10 * S;
/// How far behind the live edge a hole is listed again for segments
/// registered late (an uploader that parked them during an outage). A hole
/// older than this fills when its chunk is listed again (after the cursor has
/// been away and come back).
const LATE_BACKFILL_NS: i64 = 60 * S;
/// Wait between listings of the same hole behind the live edge: doubling
/// from the first to the max. The newest-first page of each poll covers the
/// last few segments anyway, so this is for older holes only.
const HOLE_RETRY: Duration = Duration::from_secs(1);
const HOLE_RETRY_MAX: Duration = Duration::from_secs(10);
/// Coverage intervals kept before the far ones are forgotten.
const MAX_COVERAGE_INTERVALS: usize = 2048;
/// What a pruning keeps around the cursor and the live edge.
const COVERAGE_KEEP_NS: i64 = 900 * S;
/// Segments per page of a live poll, newest first: some seconds of
/// segments, so a segment registered a little late (after a later one) is in
/// the next poll's page without a listing of its own.
const LIVE_POLL_PAGE: usize = 8;
/// Pages one live poll reads at most. When the source has more new segments
/// (the index fell behind), the stretch between is a hole: listed by time if
/// it is within [`LATE_BACKFILL_NS`] of the edge, else by chunk when the
/// cursor gets there.
const LIVE_POLL_PAGES: usize = 4;
/// How far from the cursor's window an open hole is followed: chunks are
/// listed outwards from it until media closes it, up to this far.
const RESOLVE_REACH_NS: i64 = 600 * S;

/// The longest hole between two segments that is a seam, not a gap: just
/// under one frame period (timestamp rounding, writer jitter), so that a
/// single missing frame still counts as a gap.
pub fn seam_tolerance(frame_ns: Option<f64>) -> i64 {
    frame_ns.map_or(MIN_GAP_NS, |f| (f as i64 - MIN_GAP_NS).max(MIN_GAP_NS))
}

/// Insert `[s, e)` into sorted, disjoint intervals, merging those that
/// overlap or are at most `tol` apart.
fn insert_interval(v: &mut Vec<(i64, i64)>, s: i64, e: i64, tol: i64) {
    if e <= s {
        return;
    }
    let (mut ns, mut ne) = (s, e);
    let mut out = Vec::with_capacity(v.len() + 1);
    let mut placed = false;
    for &(a, b) in v.iter() {
        if b.saturating_add(tol) < ns {
            out.push((a, b));
        } else if a > ne.saturating_add(tol) {
            if !placed {
                out.push((ns, ne));
                placed = true;
            }
            out.push((a, b));
        } else {
            ns = ns.min(a);
            ne = ne.max(b);
        }
    }
    if !placed {
        out.push((ns, ne));
    }
    *v = out;
}

/// What a player's index has learned about its source, kept for the
/// player's life: the stretches it has listed, and the stretches media
/// covers. A few intervals, not the segments, so it survives the eviction of
/// the index around the cursor; a hole is known once it has been listed from
/// the media before it to the media after it, however many listings that
/// took.
#[derive(Debug, Default)]
pub struct Coverage {
    /// Listed stretches, merged where they touch.
    listed: Vec<(i64, i64)>,
    /// Media, merged across seams of up to the tolerance.
    media: Vec<(i64, i64)>,
}

/// The hole a point is in: where the media before it ends and the media
/// after it starts, each `None` while the index has not listed that far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoleAt {
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// The listed stretch around the point.
    pub listed: (i64, i64),
}

impl Coverage {
    pub fn add_listed(&mut self, s: i64, e: i64) {
        insert_interval(&mut self.listed, s, e, 0);
    }

    pub fn add_media(&mut self, s: i64, e: i64, tol: i64) {
        insert_interval(&mut self.media, s, e, tol);
    }

    /// Intervals held, listed and media.
    pub fn intervals(&self) -> usize {
        self.listed.len() + self.media.len()
    }

    /// Forget all outside `keep`: listed and media are both cut to it, so a
    /// hole is never reported where media was forgotten but its listing
    /// kept.
    pub fn retain_within(&mut self, keep: &[(i64, i64)]) {
        let mut k = Vec::new();
        for &(s, e) in keep {
            insert_interval(&mut k, s, e, 0);
        }
        let cut = |v: &[(i64, i64)]| -> Vec<(i64, i64)> {
            v.iter()
                .flat_map(|&(s, e)| {
                    k.iter().filter_map(move |&(ks, ke)| {
                        let (a, b) = (s.max(ks), e.min(ke));
                        (b > a).then_some((a, b))
                    })
                })
                .collect()
        };
        self.listed = cut(&self.listed);
        self.media = cut(&self.media);
    }

    /// Holes between two pieces of media that overlap `[lo, hi)`, whole
    /// (not clipped), oldest first.
    pub fn gaps_in(&self, lo: i64, hi: i64) -> Vec<(i64, i64)> {
        self.media
            .windows(2)
            .map(|w| (w[0].1, w[1].0))
            .filter(|&(a, b)| b > lo && a < hi)
            .collect()
    }

    /// The listed stretch that holds `[a, b]`.
    fn listed_around(&self, a: i64, b: i64) -> Option<(i64, i64)> {
        self.listed.iter().copied().find(|&(s, e)| s <= a && b <= e)
    }

    /// Holes in the source: stretches of at least `tol` between two pieces
    /// of media that the index has listed from end to end, within `extent`.
    pub fn view(&self, extent: (i64, i64), tol: i64) -> IndexView {
        let clip = |(s, e): (i64, i64)| {
            let (s, e) = (s.max(extent.0), e.min(extent.1));
            (e > s).then_some((s, e))
        };
        let gaps = self
            .media
            .windows(2)
            .map(|w| (w[0].1, w[1].0))
            .filter(|&(a, b)| b - a >= tol && self.listed_around(a, b).is_some())
            .filter_map(clip)
            .collect();
        IndexView {
            listed: self.listed.iter().copied().filter_map(clip).collect(),
            gaps,
        }
    }

    /// The hole `t` is in, if the index has listed `t` and no media covers
    /// it. Outside `extent` (before the first or after the last segment) is
    /// not a hole.
    pub fn hole_at(&self, t: i64, extent: (i64, i64)) -> Option<HoleAt> {
        if t < extent.0 || t >= extent.1 || self.media.iter().any(|&(s, e)| s <= t && t < e) {
            return None;
        }
        let listed = self.listed_around(t, t)?;
        let before = self.media.iter().rev().find(|m| m.1 <= t).map(|m| m.1);
        let after = self.media.iter().find(|m| m.0 > t).map(|m| m.0);
        Some(HoleAt {
            start: before.filter(|&a| listed.0 <= a),
            end: after.filter(|&b| b <= listed.1),
            listed,
        })
    }

    /// A point to list next, to close a hole whose end the index has not
    /// reached: an edge of a listed stretch, inside `extent`, that no media
    /// crosses, within [`RESOLVE_REACH_NS`] of `[lo, hi]`. The nearest to
    /// `cursor` first.
    pub fn to_resolve(&self, cursor: i64, lo: i64, hi: i64, extent: (i64, i64)) -> Option<i64> {
        // Media at an edge (touching it counts: a listing does not return
        // the segment that ends where it starts) means the stretch beyond
        // it is not a hole the index is inside of.
        let at_start = |p: i64| self.media.iter().any(|&(s, e)| s <= p && p < e);
        let at_end = |p: i64| self.media.iter().any(|&(s, e)| s < p && p <= e);
        let near = |p: i64| p >= lo - RESOLVE_REACH_NS && p <= hi + RESOLVE_REACH_NS;
        let mut edges = Vec::new();
        for &(ls, le) in &self.listed {
            // Open on the left: list what is before `ls`.
            if ls > extent.0 && near(ls) && !at_start(ls) {
                edges.push(ls - 1);
            }
            if le < extent.1 && near(le) && !at_end(le) {
                edges.push(le);
            }
        }
        edges.into_iter().min_by_key(|p| (p - cursor).abs())
    }
}

/// Segments ordered by urgency: from the cursor outwards, with distance
/// against the direction of travel counting double.
pub fn by_urgency(segs: &[Arc<Segment>], cursor: i64, forward: bool) -> Vec<Arc<Segment>> {
    let mut v: Vec<Arc<Segment>> = segs.to_vec();
    v.sort_by_key(|s| {
        let contains = s.start_ns <= cursor && cursor < s.end_ns;
        let ahead = if forward {
            s.start_ns >= cursor
        } else {
            s.end_ns <= cursor
        };
        let dist = if contains {
            0
        } else if s.start_ns > cursor {
            s.start_ns - cursor
        } else {
            cursor - s.end_ns
        };
        // Ahead weighs half as much as behind.
        (if contains || ahead { dist } else { dist * 2 }, s.start_ns)
    });
    v
}

/// The most urgent of `order` (ordered by urgency) whose bytes fit in
/// `budget`, counting `size_of` each. The first `min_keep` always fit. Stops
/// at the first that does not, so what is kept is never less urgent than
/// what is left out.
fn within_budget(
    order: &[Arc<Segment>],
    size_of: impl Fn(&Segment) -> u64,
    budget: u64,
    min_keep: usize,
) -> Vec<Arc<Segment>> {
    let mut total = 0u64;
    let mut out = Vec::new();
    for (i, s) in order.iter().enumerate() {
        total = total.saturating_add(size_of(s));
        if i >= min_keep && total > budget {
            break;
        }
        out.push(Arc::clone(s));
    }
    out
}

/// Which index chunks have been listed. A chunk at the live edge gets new
/// segments from the live poll; it is listed again (dirty) only when a fetch
/// failed (expired URLs).
#[derive(Default)]
struct ChunkState {
    listed: HashSet<i64>,
    dirty: HashSet<i64>,
    /// When each chunk was last listed successfully.
    listed_at: HashMap<i64, Instant>,
}

impl ChunkState {
    /// List `c` again after a failed fetch, unless it was listed recently.
    fn relist_after_fetch_error(&mut self, c: i64, now: Instant) {
        if self
            .listed_at
            .get(&c)
            .is_none_or(|t| now.duration_since(*t) >= RELIST_MIN)
        {
            self.dirty.insert(c);
        }
    }
}

/// Wait after the `failures`-th consecutive failed live poll.
fn live_poll_retry(seg_ns: i64, failures: u32) -> Duration {
    Duration::from_nanos(seg_ns.max(0) as u64)
        .saturating_mul(1 << failures.min(16))
        .min(LIVE_POLL_RETRY_MAX)
}

fn chunk_of(tai: i64) -> i64 {
    tai.div_euclid(CHUNK_NS)
}

pub struct Fetcher {
    pub store: Arc<SegmentStore>,
    source: Arc<dyn SourceIndex>,
    timeline: Arc<ReplayTimeline>,
    stats: Arc<PlayerStats>,
    block_id: String,
    ahead_ns: i64,
    behind_ns: i64,
    /// Most coded bytes to hold.
    coded_budget: u64,
}

impl Fetcher {
    pub fn new(
        block_id: &str,
        source: Arc<dyn SourceIndex>,
        timeline: Arc<ReplayTimeline>,
        stats: Arc<PlayerStats>,
        prefetch_s: u64,
    ) -> Self {
        let ahead_ns = prefetch_s.max(2) as i64 * S;
        Self {
            store: Arc::new(SegmentStore::default()),
            source,
            timeline,
            stats,
            block_id: block_id.to_string(),
            ahead_ns,
            behind_ns: (ahead_ns / 2).max(2 * S),
            coded_budget: strom_types::env::var_opt("STROM_REPLAY_CODED_MB")
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|mb| *mb > 0)
                .unwrap_or(DEFAULT_CODED_BUDGET_MB)
                .saturating_mul(1 << 20),
        }
    }

    /// Run on a dedicated thread until `stop` is set.
    pub fn spawn(self: Arc<Self>, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
        let name = format!("replay-fetch-{}", self.block_id);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("Replay {}: no fetch runtime: {}", self.block_id, e);
                    return;
                }
            };
            // Leave at once on stop, also from a request that is waiting on
            // the network (dropping the future cancels it).
            let watch = Arc::clone(&stop);
            rt.block_on(async move {
                tokio::select! {
                    _ = self.run(stop) => {}
                    _ = async {
                        while !watch.load(Ordering::Relaxed) {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                    } => {}
                }
            });
        });
        match spawned {
            Ok(h) => Some(h),
            Err(e) => {
                warn!("Replay: cannot start fetch thread: {}", e);
                None
            }
        }
    }

    async fn init(&self, stop: &AtomicBool) -> Option<(Segment, Segment)> {
        let mut backoff = Duration::from_millis(250);
        while !stop.load(Ordering::Relaxed) {
            let r = async {
                let info = self.source.info().await?;
                let first = self.source.first().await?;
                let last = self.source.last().await?;
                anyhow::Ok((info, first, last))
            }
            .await;
            // A source replay cannot decode is one clear error, not a decode
            // failure per segment and a picture that never comes.
            if let Ok((info, _, _)) = &r {
                if let Err(e) = check_supported(info.container.as_deref(), info.codec.as_deref()) {
                    warn!(
                        "Replay {}: {} cannot be played: {}; nothing is fetched or shown",
                        self.block_id,
                        self.source.describe(),
                        e
                    );
                    return None;
                }
            }
            match r {
                Ok((info, Some(first), Some(last))) => {
                    info!(
                        "Replay {}: {} frame rate {:?}, container {:?}, codec {:?}",
                        self.block_id,
                        self.source.describe(),
                        info.frame_rate,
                        info.container,
                        info.codec
                    );
                    if let Some((n, d)) = info.frame_rate {
                        self.stats.init_grid(n, d, first.start_ns);
                    }
                    self.stats.set_time_domain(if info.absolute_time {
                        TimeDomain::Tai
                    } else {
                        TimeDomain::Unanchored
                    });
                    *lock(&self.store.info) = Some(info);
                    return Some((first, last));
                }
                Ok(_) => debug!("Replay {}: source has no segments yet", self.block_id),
                Err(e) => warn!(
                    "Replay {}: index of {} unavailable: {:#}",
                    self.block_id,
                    self.source.describe(),
                    e
                ),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
        None
    }

    async fn run(self: Arc<Self>, stop: Arc<AtomicBool>) {
        let Some((first, last)) = self.init(&stop).await else {
            return;
        };
        let mut start = first.start_ns;
        let mut end = last.end_ns;
        let mut seg_ns = last.duration_ns().clamp(S / 5, 5 * S);
        self.publish_extent(start, end);
        let mut coverage = Coverage::default();
        for seg in [&first, &last] {
            coverage.add_listed(seg.start_ns, seg.end_ns);
            coverage.add_media(seg.start_ns, seg.end_ns, self.tolerance());
        }
        self.store.insert([first, last]);

        let mut chunks = ChunkState::default();
        let mut inflight: HashSet<i64> = HashSet::new();
        let mut list_failed: Backoff<i64> = Backoff::new(LIST_RETRY, LIST_RETRY_MAX);
        let mut failed: Backoff<i64> = Backoff::new(FETCH_RETRY, FETCH_RETRY_MAX);
        let mut jobs: JoinSet<(anyhow::Result<Vec<u8>>, Duration)> = JoinSet::new();
        // The segment of each fetch task, also when the task panics.
        let mut tasks: HashMap<tokio::task::Id, Arc<Segment>> = HashMap::new();
        let mut next_poll = Instant::now() + Duration::from_nanos(seg_ns as u64);
        let mut next_first = Instant::now() + Duration::from_secs(30);
        let mut holes: Backoff<i64> = Backoff::new(HOLE_RETRY, HOLE_RETRY_MAX);
        let mut poll_failures: u32 = 0;
        let mut next_view = Instant::now();
        // Coded bytes per second of media, from the latest fetch: the size
        // estimate of a segment not fetched yet.
        let mut bytes_per_s = 0f64;

        while !stop.load(Ordering::Relaxed) {
            // Where the group is, or this angle's start before it knows.
            let rt = self.timeline.now_rt();
            let (cursor, speed) = self.timeline.cursor_tai(rt).unwrap_or((start, 0.0));
            let forward = speed >= 0.0;
            let lo = cursor
                - if forward {
                    self.behind_ns
                } else {
                    self.ahead_ns
                };
            let hi = cursor
                + if forward {
                    self.ahead_ns
                } else {
                    self.behind_ns
                };

            // Live edge, at segment cadence.
            if self.source.may_grow() && Instant::now() >= next_poll {
                next_poll = Instant::now() + Duration::from_nanos(seg_ns as u64);
                match self.poll_live(end, &mut coverage, &mut holes).await {
                    Ok(r) => {
                        poll_failures = 0;
                        if let Some((new_end, new_seg_ns)) = r {
                            seg_ns = new_seg_ns;
                            if new_end > end {
                                end = new_end;
                                self.publish_extent(start, end);
                            }
                        }
                    }
                    Err(e) => {
                        poll_failures = poll_failures.saturating_add(1);
                        if worth_warning(poll_failures) {
                            warn!(
                                "Replay {}: live poll failed (attempt {}): {:#}",
                                self.block_id, poll_failures, e
                            );
                        } else {
                            debug!(
                                "Replay {}: live poll failed (attempt {}): {:#}",
                                self.block_id, poll_failures, e
                            );
                        }
                        next_poll = Instant::now() + live_poll_retry(seg_ns, poll_failures);
                    }
                }
                if Instant::now() >= next_first {
                    next_first = Instant::now() + Duration::from_secs(30);
                    if let Ok(Some(f)) = self.source.first().await {
                        start = f.start_ns;
                        self.publish_extent(start, end);
                    }
                }
            }

            // Index chunks covering the window.
            let mut listing_ok = true;
            for c in chunk_of(lo.max(start))..=chunk_of(hi.min(end - 1)) {
                if chunks.listed.contains(&c) && !chunks.dirty.contains(&c) {
                    continue;
                }
                if !list_failed.ready(&c, Instant::now()) {
                    continue;
                }
                if !self
                    .list_chunk(c, end, &mut chunks, &mut list_failed, &mut coverage)
                    .await
                {
                    listing_ok = false;
                    break;
                }
            }
            // A hole near the window whose other end the index has not
            // reached: list one more chunk towards it.
            if listing_ok {
                if let Some(p) = coverage.to_resolve(cursor, lo, hi, (start, end)) {
                    let c = chunk_of(p);
                    if list_failed.ready(&c, Instant::now()) {
                        self.list_chunk(c, end, &mut chunks, &mut list_failed, &mut coverage)
                            .await;
                    }
                }
            }

            // In a hole: the segment before it holds the picture.
            let hold = coverage.hole_at(cursor, (start, end)).and_then(|h| {
                let a = h.start?;
                let tol = self.tolerance();
                let seg = lock(&self.store.index)
                    .range(..a)
                    .next_back()
                    .map(|(_, s)| Arc::clone(s))
                    .filter(|s| s.end_ns + tol >= a)?;
                Some(Hold {
                    seg,
                    hole: (a, h.end.unwrap_or(h.listed.1)),
                })
            });
            let hold_key = hold.as_ref().map(|h| h.seg.start_ns);
            *lock(&self.store.hold) = hold.clone();

            // Fetch bytes, most urgent first, as far as the budget goes.
            let window = self.store.segments_in(lo, hi);
            let mut order: Vec<Arc<Segment>> = hold.map(|h| h.seg).into_iter().collect();
            order.extend(
                by_urgency(&window, cursor, forward)
                    .into_iter()
                    .filter(|s| Some(s.start_ns) != hold_key),
            );
            let coded_size = |s: &Segment| -> u64 {
                lock(&self.store.bytes).get(&s.start_ns).map_or(
                    (bytes_per_s * s.duration_ns() as f64 / 1e9) as u64,
                    |(_, b)| b.len() as u64,
                )
            };
            let allowed = within_budget(&order, coded_size, self.coded_budget, MIN_CODED_SEGMENTS);
            for seg in allowed {
                if inflight.len() >= MAX_INFLIGHT {
                    break;
                }
                let key = seg.start_ns;
                if inflight.contains(&key)
                    || self.store.bytes(key).is_some()
                    || !failed.ready(&key, Instant::now())
                {
                    continue;
                }
                inflight.insert(key);
                let source = Arc::clone(&self.source);
                let task_seg = Arc::clone(&seg);
                let handle = jobs.spawn(async move {
                    let t0 = Instant::now();
                    let r = source.fetch(&task_seg).await;
                    (r, t0.elapsed())
                });
                tasks.insert(handle.id(), seg);
            }

            // Collect finished fetches, or wait a little.
            let wait = tokio::time::sleep(Duration::from_millis(10));
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    done = jobs.join_next_with_id(), if !jobs.is_empty() => {
                        // A panicked task frees its slot and counts as a failure.
                        let (id, r, took) = match done {
                            Some(Ok((id, (r, took)))) => (id, r, took),
                            Some(Err(e)) => {
                                (e.id(), Err(anyhow::anyhow!("fetch task: {}", e)), Duration::ZERO)
                            }
                            None => break,
                        };
                        let Some(seg) = tasks.remove(&id) else { continue };
                        inflight.remove(&seg.start_ns);
                        self.stats.fetches.fetch_add(1, Ordering::Relaxed);
                        match r {
                            Ok(bytes) => {
                                self.stats
                                    .last_fetch_ms
                                    .store(took.as_millis() as u64, Ordering::Relaxed);
                                failed.succeed(&seg.start_ns);
                                if seg.duration_ns() > 0 {
                                    bytes_per_s =
                                        bytes.len() as f64 * 1e9 / seg.duration_ns() as f64;
                                }
                                lock(&self.store.bytes)
                                    .insert(seg.start_ns, (seg.end_ns, Arc::new(bytes)));
                            }
                            Err(e) => {
                                self.stats.fetch_errors.fetch_add(1, Ordering::Relaxed);
                                let attempts = failed.fail(seg.start_ns, Instant::now());
                                if worth_warning(attempts) {
                                    warn!(
                                        "Replay {}: fetch of {} failed (attempt {}): {:#}",
                                        self.block_id, seg.object_id, attempts, e
                                    );
                                } else {
                                    debug!(
                                        "Replay {}: fetch of {} failed (attempt {}): {:#}",
                                        self.block_id, seg.object_id, attempts, e
                                    );
                                }
                                // URLs may have expired: list the chunk again.
                                chunks.relist_after_fetch_error(
                                    chunk_of(seg.start_ns),
                                    Instant::now(),
                                );
                            }
                        }
                    }
                    _ = &mut wait => break,
                }
            }

            // Evict what is far from the cursor: segments that do not overlap
            // the window plus a margin. By overlap, not by start: a long
            // segment holding the cursor started long before the window.
            let margin = seg_ns;
            {
                let mut bytes = lock(&self.store.bytes);
                bytes.retain(|start, (end, _)| {
                    (*end > lo - margin && *start < hi + margin) || Some(*start) == hold_key
                });
                // Over the byte budget: keep the most urgent that fit, the
                // same ones the fetch above stops at.
                let total: u64 = bytes.values().map(|(_, b)| b.len() as u64).sum();
                if total > self.coded_budget {
                    let size =
                        |s: &Segment| bytes.get(&s.start_ns).map_or(0, |(_, b)| b.len() as u64);
                    let fit: HashSet<i64> =
                        within_budget(&order, size, self.coded_budget, MIN_CODED_SEGMENTS)
                            .iter()
                            .map(|s| s.start_ns)
                            .collect();
                    bytes.retain(|start, _| fit.contains(start) || Some(*start) == hold_key);
                }
            }
            let cc = chunk_of(cursor);
            let keep = |c: i64| (c - cc).abs() <= KEEP_CHUNKS;
            chunks.listed.retain(|c| keep(*c));
            chunks.dirty.retain(|c| keep(*c));
            chunks.listed_at.retain(|c, _| keep(*c));
            if coverage.intervals() > MAX_COVERAGE_INTERVALS {
                coverage.retain_within(&[
                    (cursor - COVERAGE_KEEP_NS, cursor + COVERAGE_KEEP_NS),
                    (end - COVERAGE_KEEP_NS, end),
                ]);
            }
            list_failed.retain(|c| keep(*c));
            let near: HashSet<i64> = self
                .store
                .segments_in(lo - margin, hi + margin)
                .iter()
                .map(|s| s.start_ns)
                .collect();
            failed.retain(|k| near.contains(k));
            {
                let mut index = lock(&self.store.index);
                index.retain(|k, _| keep(chunk_of(*k)) || *k == start || Some(*k) == hold_key);
                self.stats
                    .index_segments
                    .store(index.len() as u64, Ordering::Relaxed);
                if Instant::now() >= next_view {
                    next_view = Instant::now() + INDEX_VIEW_EVERY;
                    let mut view = coverage.view((start, end), self.tolerance());
                    if self.source.may_grow() {
                        // Holes near the live edge may still fill with
                        // segments registered late: not gaps yet.
                        let settled = end - LIVE_BACKFILL_NS;
                        view.gaps.retain(|g| g.1 <= settled);
                    }
                    self.stats.set_index_view(view);
                }
            }
            self.stats
                .fetched_segments
                .store(lock(&self.store.bytes).len() as u64, Ordering::Relaxed);
        }
        jobs.abort_all();
    }

    /// Poll the live edge: read the newest segments back to the edge `end`
    /// the index knows, a newest-first page at a time, and merge them into
    /// the index. A hole older than the page and within
    /// [`LATE_BACKFILL_NS`] of the edge (a segment registered late, or more
    /// new segments than the pages held) is listed by time, with backoff per
    /// hole. Returns the new end and segment length, `None` when the source
    /// has nothing.
    async fn poll_live(
        &self,
        end: i64,
        coverage: &mut Coverage,
        holes: &mut Backoff<i64>,
    ) -> anyhow::Result<Option<(i64, i64)>> {
        let newest = self
            .source
            .newest(end, LIVE_POLL_PAGE, LIVE_POLL_PAGES)
            .await?;
        let (Some(first), Some(last)) = (newest.segs.first(), newest.segs.last()) else {
            return Ok(None);
        };
        let tol = self.tolerance();
        // The page knows the source from its oldest segment to its newest
        // (and from `end` on when it reached it), not the stretch before.
        let page_from = if newest.more {
            first.start_ns
        } else {
            first.start_ns.min(end)
        };
        let new_end = last.end_ns.max(end);
        let seg_ns = last.duration_ns();
        coverage.add_listed(page_from, new_end);
        for s in &newest.segs {
            coverage.add_media(s.start_ns, s.end_ns, tol);
        }
        self.store.insert_new(newest.segs);
        if newest.more {
            debug!(
                "Replay {}: live poll fell behind, the stretch before the edge is a hole",
                self.block_id
            );
        }

        // Holes before the page, near the edge: list the newest one whose
        // backoff allows, by time and bounded to it. One listing per poll at
        // most, and none while there is no hole.
        let floor = new_end - LATE_BACKFILL_NS;
        let gaps = coverage.gaps_in(floor, page_from);
        holes.retain(|a| gaps.iter().any(|g| g.0 == *a));
        let now = Instant::now();
        if let Some(&(key, b)) = gaps.iter().rev().find(|g| holes.ready(&g.0, now)) {
            let (a, b) = (key.max(floor), b.min(page_from));
            match self.source.list(a, b).await {
                Ok(segs) => {
                    coverage.add_listed(a, b);
                    for s in &segs {
                        coverage.add_media(s.start_ns, s.end_ns, tol);
                    }
                    self.store.insert_new(segs);
                }
                Err(e) => debug!(
                    "Replay {}: listing a hole behind the live edge failed: {:#}",
                    self.block_id, e
                ),
            }
            if coverage.gaps_in(a, b).iter().any(|g| g.0 == key) {
                holes.fail(key, Instant::now());
            }
        }
        Ok(Some((new_end, seg_ns.clamp(S / 5, 5 * S))))
    }

    /// The longest seam that is not a gap, for this source's frame rate.
    fn tolerance(&self) -> i64 {
        seam_tolerance(self.stats.grid().map(|g| g.frame_ns()))
    }

    /// List index chunk `c` (the source's end is `end`): its segments go
    /// into the index and the coverage. False if the listing failed.
    async fn list_chunk(
        &self,
        c: i64,
        end: i64,
        chunks: &mut ChunkState,
        list_failed: &mut Backoff<i64>,
        coverage: &mut Coverage,
    ) -> bool {
        match self.source.list(c * CHUNK_NS, (c + 1) * CHUNK_NS).await {
            Ok(segs) => {
                // The listing knows the source up to its end then, not the
                // rest of the chunk: media written there later is not a hole.
                let known_end = segs.iter().map(|s| s.end_ns).fold(end, i64::max);
                coverage.add_listed(c * CHUNK_NS, ((c + 1) * CHUNK_NS).min(known_end));
                let tol = self.tolerance();
                for seg in &segs {
                    coverage.add_media(seg.start_ns, seg.end_ns, tol);
                }
                self.store.insert(segs);
                chunks.listed.insert(c);
                chunks.dirty.remove(&c);
                chunks.listed_at.insert(c, Instant::now());
                list_failed.succeed(&c);
                true
            }
            Err(e) => {
                let attempts = list_failed.fail(c, Instant::now());
                if worth_warning(attempts) {
                    warn!(
                        "Replay {}: listing failed (attempt {}): {:#}",
                        self.block_id, attempts, e
                    );
                } else {
                    debug!(
                        "Replay {}: listing failed (attempt {}): {:#}",
                        self.block_id, attempts, e
                    );
                }
                false
            }
        }
    }

    fn publish_extent(&self, start: i64, end: i64) {
        self.stats.start_tai.store(start, Ordering::Relaxed);
        self.stats.end_tai.store(end, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::source::{read_newest, Location};
    use std::sync::atomic::AtomicU64;

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
    fn coded_budget_keeps_the_most_urgent() {
        let segs: Vec<_> = (0..6).map(seg).collect();
        let kept = |budget| within_budget(&segs, |_| 10, budget, MIN_CODED_SEGMENTS).len();
        assert_eq!(kept(35), 3);
        assert_eq!(kept(5), MIN_CODED_SEGMENTS);
        assert_eq!(kept(1000), 6);
    }

    #[test]
    fn coverage_pruning_cuts_listed_and_media_alike() {
        let mut c = Coverage::default();
        c.add_listed(0, 100 * S);
        for i in 0..10 {
            // Media with a 1 s hole after each 9 s.
            c.add_media(i * 10 * S, (i * 10 + 9) * S, 0);
        }
        assert_eq!(c.intervals(), 11);
        c.retain_within(&[(42 * S, 61 * S), (55 * S, 70 * S)]);
        assert_eq!(c.listed, vec![(42 * S, 70 * S)]);
        assert_eq!(
            c.media,
            vec![(42 * S, 49 * S), (50 * S, 59 * S), (60 * S, 69 * S)]
        );
        // The holes within are still holes, nothing outside is one.
        let gaps = c.view((0, 100 * S), 1).gaps;
        assert_eq!(gaps, vec![(49 * S, 50 * S), (59 * S, 60 * S)]);
    }

    #[test]
    fn live_poll_retry_grows_to_its_cap() {
        assert_eq!(live_poll_retry(S, 1), Duration::from_secs(2));
        assert_eq!(live_poll_retry(S, 2), Duration::from_secs(4));
        assert_eq!(live_poll_retry(S, 40), LIVE_POLL_RETRY_MAX);
    }

    #[test]
    fn urgency_prefers_direction_of_travel() {
        let segs: Vec<_> = (0..8).map(seg).collect();
        let cursor = 4 * S + S / 2;
        let fw: Vec<i64> = by_urgency(&segs, cursor, true)
            .iter()
            .map(|s| s.start_ns / S)
            .collect();
        assert_eq!(&fw[..4], &[4, 5, 3, 6]);
        let bw: Vec<i64> = by_urgency(&segs, cursor, false)
            .iter()
            .map(|s| s.start_ns / S)
            .collect();
        assert_eq!(&bw[..4], &[4, 3, 5, 2]);
    }

    #[test]
    fn store_finds_segment_containing_window_start() {
        let store = SegmentStore::default();
        store.insert((0..5).map(|i| (*seg(i)).clone()));
        let hit: Vec<i64> = store
            .segments_in(2 * S + 1, 3 * S + 1)
            .iter()
            .map(|s| s.start_ns / S)
            .collect();
        assert_eq!(hit, vec![2, 3]);
        assert!(store.segments_in(10 * S, 11 * S).is_empty());
    }

    /// A source of `n` segments of `len_s` seconds from TAI 100 s, whose
    /// listings and fetches can be made to fail. Counts the calls.
    struct Mock {
        segs: Vec<Segment>,
        list_fails: bool,
        fetch_fails: bool,
        fetch_panics: bool,
        fetch_hangs: bool,
        /// Listings by time (`list`): the expensive query on a TAMS gateway.
        lists: AtomicU64,
        /// Pages of newest-first listings (`newest`): the cheap one.
        pages: AtomicU64,
        fetches: Mutex<HashMap<i64, u32>>,
        /// A growing source: segments registered while the player runs,
        /// in registration order (not necessarily time order).
        grows: bool,
        live: Mutex<Vec<Segment>>,
    }

    impl Mock {
        fn new(n: i64, len_s: i64, list_fails: bool, fetch_fails: bool) -> Arc<Self> {
            Arc::new(Self::build(n, len_s, list_fails, fetch_fails))
        }

        fn build(n: i64, len_s: i64, list_fails: bool, fetch_fails: bool) -> Self {
            let segs = (0..n)
                .map(|i| Segment {
                    start_ns: (100 + i * len_s) * S,
                    end_ns: (100 + (i + 1) * len_s) * S,
                    ts_offset_ns: 0,
                    object_start_ns: None,
                    object_id: i.to_string(),
                    location: Location::Urls(vec![]),
                })
                .collect();
            Self {
                segs,
                list_fails,
                fetch_fails,
                fetch_panics: false,
                fetch_hangs: false,
                lists: AtomicU64::new(0),
                pages: AtomicU64::new(0),
                fetches: Mutex::new(HashMap::new()),
                grows: false,
                live: Mutex::new(Vec::new()),
            }
        }

        /// All segments, in time order.
        fn all(&self) -> Vec<Segment> {
            let mut v = self.segs.clone();
            v.extend(lock(&self.live).iter().cloned());
            v.sort_by_key(|s| s.start_ns);
            v
        }

        /// Register segment `i` (counted like `build`'s, `len_ms` long).
        fn register(&self, i: i64, len_ms: i64) {
            let ms = 1_000_000;
            lock(&self.live).push(Segment {
                start_ns: 100 * S + i * len_ms * ms,
                end_ns: 100 * S + (i + 1) * len_ms * ms,
                ts_offset_ns: 0,
                object_start_ns: None,
                object_id: i.to_string(),
                location: Location::Urls(vec![]),
            });
        }

        fn fetches_of(&self, start_s: i64) -> u32 {
            lock(&self.fetches)
                .get(&(start_s * S))
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait::async_trait]
    impl SourceIndex for Mock {
        fn describe(&self) -> String {
            "mock".into()
        }
        fn may_grow(&self) -> bool {
            self.grows
        }
        async fn info(&self) -> anyhow::Result<SourceInfo> {
            Ok(SourceInfo {
                frame_rate: Some((25, 1)),
                container: None,
                codec: None,
                absolute_time: true,
            })
        }
        async fn list(&self, start: i64, end: i64) -> anyhow::Result<Vec<Segment>> {
            self.lists.fetch_add(1, Ordering::Relaxed);
            if self.list_fails {
                anyhow::bail!("gateway down");
            }
            Ok(self
                .all()
                .into_iter()
                .filter(|s| s.end_ns > start && s.start_ns < end)
                .collect())
        }
        /// Newest first with paging, as a TAMS store with
        /// `reverse_order=true`: the cursor is the offset from the newest.
        async fn newest(
            &self,
            after: i64,
            page: usize,
            max_pages: usize,
        ) -> anyhow::Result<crate::blocks::builtin::mediaplayer::replay::source::Since> {
            let mut all = self.all();
            all.reverse();
            read_newest(after, page, max_pages, |c: Option<usize>| {
                self.pages.fetch_add(1, Ordering::Relaxed);
                let off = c.unwrap_or(0);
                let got: Vec<Segment> = all.iter().skip(off).take(page).cloned().collect();
                let n = got.len();
                let next = (off + n < all.len()).then_some(off + n);
                async move { Ok((got, n, next)) }
            })
            .await
        }
        async fn first(&self) -> anyhow::Result<Option<Segment>> {
            Ok(self.all().first().cloned())
        }
        async fn last(&self) -> anyhow::Result<Option<Segment>> {
            Ok(self.all().last().cloned())
        }
        async fn fetch(&self, seg: &Segment) -> anyhow::Result<Vec<u8>> {
            *lock(&self.fetches).entry(seg.start_ns).or_insert(0) += 1;
            if self.fetch_panics {
                panic!("fetch task panics");
            }
            if self.fetch_hangs {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            if self.fetch_fails {
                anyhow::bail!("404");
            }
            Ok(vec![0; 16])
        }
    }

    /// Run a fetcher on `mock` (prefetch 2 s) for `run`, after seeking the
    /// group to `seek_s` seconds TAI once the source is known.
    fn run_fetcher(mock: Arc<Mock>, seek_s: Option<i64>, run: Duration) {
        let timeline = super::super::timeline::get_or_create(uuid::Uuid::new_v4(), "g");
        let stats = Arc::new(PlayerStats::new("p"));
        timeline.register_player(&stats);
        let fetcher = Arc::new(Fetcher::new(
            "p",
            mock,
            Arc::clone(&timeline),
            Arc::clone(&stats),
            2,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        Arc::clone(&fetcher).spawn(Arc::clone(&stop));
        let t0 = Instant::now();
        while stats.extent().is_none() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(t) = seek_s {
            timeline
                .apply(super::super::timeline::Command::Seek {
                    frame: None,
                    utc: None,
                    tai: Some(format!("{}:0", t)),
                    from_live_ms: None,
                    speed: None,
                })
                .unwrap();
        }
        std::thread::sleep(run);
        stop.store(true, Ordering::Relaxed);
    }

    #[test]
    fn gaps_are_holes_in_the_source_not_in_the_listing() {
        let tol = seam_tolerance(Some(20e6));
        let ms = 1_000_000;
        // 1 s segments 0..30 s, a 93 s hole, then 123..140 s. Segments
        // meet with seams of up to 18 ms (writer jitter): not gaps.
        let mut segs: Vec<(i64, i64)> = (0..30).map(|i| (i * S, (i + 1) * S)).collect();
        segs[5].1 -= 18 * ms;
        segs.extend((123..140).map(|i| (i * S, (i + 1) * S)));
        let list = |cov: &mut Coverage, c: i64| {
            let (cs, ce) = (c * CHUNK_NS, (c + 1) * CHUNK_NS);
            cov.add_listed(cs, ce.min(140 * S));
            for &(s, e) in segs.iter().filter(|(s, e)| *e > cs && *s < ce) {
                cov.add_media(s, e, tol);
            }
        };
        let extent = (0, 140 * S);
        let mut cov = Coverage::default();
        // Listed around the start and the end only: the hole is not known
        // from end to end, so nothing is said about it (no clipped pieces,
        // no false gaps at the listing's edges).
        list(&mut cov, 0);
        list(&mut cov, 6);
        let v = cov.view(extent, tol);
        assert_eq!(v.listed, vec![(0, 20 * S), (120 * S, 140 * S)]);
        assert!(v.gaps.is_empty(), "{:?}", v.gaps);
        // The edges inside the hole are open: listing goes on towards them,
        // nearest the cursor first.
        assert_eq!(
            cov.to_resolve(10 * S, 9 * S, 12 * S, extent),
            Some(120 * S - 1)
        );
        list(&mut cov, 1);
        // The cursor in the hole: its start is known, its end not yet.
        let h = cov.hole_at(35 * S, extent).unwrap();
        assert_eq!((h.start, h.end), (Some(30 * S), None));
        for c in 2..6 {
            list(&mut cov, c);
        }
        // Listed from end to end in six listings: one gap of 93 s.
        let v = cov.view(extent, tol);
        assert_eq!(v.listed, vec![extent]);
        assert_eq!(v.gaps, vec![(30 * S, 123 * S)]);
        assert_eq!(cov.to_resolve(10 * S, 9 * S, 12 * S, extent), None);
        let h = cov.hole_at(35 * S, extent).unwrap();
        assert_eq!((h.start, h.end), (Some(30 * S), Some(123 * S)));
        assert_eq!(cov.hole_at(10 * S, extent), None);
        assert_eq!(cov.hole_at(5 * S + 990 * ms, extent), None, "a seam");
        assert_eq!(cov.hole_at(141 * S, extent), None, "past the end");
        // A single missing frame is a gap.
        let mut cov = Coverage::default();
        cov.add_listed(0, 2 * S);
        cov.add_media(0, S, tol);
        cov.add_media(S + 20 * ms, 2 * S, tol);
        assert_eq!(cov.view((0, 2 * S), tol).gaps, vec![(S, S + 20 * ms)]);
    }

    #[test]
    fn fetcher_reports_a_long_hole_whole() {
        // 1 s segments 100..110 s and 203..220 s TAI: a 93 s hole across
        // five index chunks, the cursor at the start (2 s window).
        let mut mock = Mock::build(120, 1, false, false);
        mock.segs
            .retain(|s| s.start_ns < 110 * S || s.start_ns >= 203 * S);
        let timeline = super::super::timeline::get_or_create(uuid::Uuid::new_v4(), "g");
        let stats = Arc::new(PlayerStats::new("p"));
        timeline.register_player(&stats);
        let fetcher = Arc::new(Fetcher::new(
            "p",
            Arc::new(mock),
            timeline,
            Arc::clone(&stats),
            2,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        Arc::clone(&fetcher).spawn(Arc::clone(&stop));
        let t0 = Instant::now();
        while stats.index_view().gaps.is_empty() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(stats.index_view().gaps, vec![(110 * S, 203 * S)]);
    }

    #[test]
    fn a_seek_into_a_hole_fetches_the_segment_before_it() {
        // 1 s segments 100..110 s and 203..220 s; seek to 150 s, 40 s into
        // the hole: far behind the fetch window.
        let mut mock = Mock::build(120, 1, false, false);
        mock.segs
            .retain(|s| s.start_ns < 110 * S || s.start_ns >= 203 * S);
        let mock = Arc::new(mock);
        let timeline = super::super::timeline::get_or_create(uuid::Uuid::new_v4(), "g");
        let stats = Arc::new(PlayerStats::new("p"));
        timeline.register_player(&stats);
        let fetcher = Arc::new(Fetcher::new(
            "p",
            mock.clone(),
            Arc::clone(&timeline),
            Arc::clone(&stats),
            2,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        Arc::clone(&fetcher).spawn(Arc::clone(&stop));
        let t0 = Instant::now();
        while stats.extent().is_none() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        timeline
            .apply(super::super::timeline::Command::Seek {
                frame: None,
                utc: None,
                tai: Some("150:0".into()),
                from_live_ms: None,
                speed: None,
            })
            .unwrap();
        let t0 = Instant::now();
        // The hole's end comes as the listing reaches it.
        let closed = || fetcher.store.hold().is_some_and(|h| h.hole.1 == 203 * S);
        while (!closed() || mock.fetches_of(109) == 0) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        let h = fetcher.store.hold().expect("in a hole");
        assert_eq!(h.seg.start_ns, 109 * S);
        assert_eq!(h.hole, (110 * S, 203 * S));
        assert!(fetcher.store.bytes(109 * S).is_some());
    }

    #[test]
    fn a_listed_edge_on_a_segment_boundary_is_not_followed() {
        // Segments that start or end on a chunk edge: nothing to resolve.
        let mut cov = Coverage::default();
        cov.add_listed(20 * S, 40 * S);
        cov.add_media(20 * S, 40 * S, MIN_GAP_NS);
        assert_eq!(cov.to_resolve(30 * S, 29 * S, 32 * S, (0, 100 * S)), None);
    }

    #[test]
    fn failing_listing_backs_off() {
        let mock = Mock::new(10, 1, true, false);
        run_fetcher(Arc::clone(&mock), None, Duration::from_millis(1500));
        // 250 + 500 + 1000 ms: a handful, not one per 10 ms loop.
        let lists = mock.lists.load(Ordering::Relaxed);
        assert!((1..=5).contains(&lists), "{} listings", lists);
    }

    #[test]
    fn failing_fetch_backs_off_and_relists_rarely() {
        let mock = Mock::new(10, 1, false, true);
        run_fetcher(Arc::clone(&mock), None, Duration::from_millis(2000));
        // 500 + 1000 ms: three attempts at most in 2 s (a fixed 500 ms retry
        // makes four or five).
        let n = mock.fetches_of(100);
        assert!((1..=3).contains(&n), "{} fetches", n);
        // The chunk is listed once, and at most once more for the failures.
        let lists = mock.lists.load(Ordering::Relaxed);
        assert!(lists <= 2, "{} listings", lists);
    }

    #[test]
    fn long_segment_under_the_cursor_is_fetched_once() {
        // 10 s segments, cursor 8 s into the first: the segment started more
        // than behind + margin before the cursor, but holds it.
        let mock = Mock::new(3, 10, false, false);
        run_fetcher(Arc::clone(&mock), Some(108), Duration::from_millis(1000));
        assert_eq!(mock.fetches_of(100), 1);
    }

    #[test]
    fn panicked_fetch_is_retried() {
        let mock = Arc::new(Mock {
            fetch_panics: true,
            ..Mock::build(10, 1, false, false)
        });
        run_fetcher(Arc::clone(&mock), None, Duration::from_millis(1200));
        // The slot is freed and the segment retried after its backoff.
        assert!(mock.fetches_of(100) >= 2);
    }

    #[test]
    fn stop_ends_the_fetcher_during_a_slow_request() {
        let mock = Arc::new(Mock {
            fetch_hangs: true,
            ..Mock::build(10, 1, false, false)
        });
        let timeline = super::super::timeline::get_or_create(uuid::Uuid::new_v4(), "g");
        let stats = Arc::new(PlayerStats::new("p"));
        timeline.register_player(&stats);
        let fetcher = Arc::new(Fetcher::new("p", mock.clone(), timeline, stats, 2));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = Arc::clone(&fetcher).spawn(Arc::clone(&stop)).unwrap();
        let t0 = Instant::now();
        while mock.fetches_of(100) == 0 && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        let t1 = Instant::now();
        while !handle.is_finished() && t1.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(handle.is_finished());
        assert!(t1.elapsed() < Duration::from_secs(1));
    }

    /// Index of the first live segment in the growing mock: 30 s after the
    /// first, in another index chunk than the cursor (which stays at the
    /// start), so only the live poll indexes the edge.
    const LIVE0: i64 = 150;

    /// A growing mock with segment 0 and the live segments `[LIVE0,
    /// LIVE0 + n)` of `len_ms` from TAI 100 s, and a fetcher on it.
    fn live_fetcher(n: i64, len_ms: i64) -> (Arc<Mock>, Arc<Fetcher>, Arc<AtomicBool>) {
        live_fetcher_from(0, n, len_ms)
    }

    /// As [`live_fetcher`], with the segments `[1, before)` too: from
    /// `LIVE0` on there is no hole in the source.
    fn live_fetcher_from(
        before: i64,
        n: i64,
        len_ms: i64,
    ) -> (Arc<Mock>, Arc<Fetcher>, Arc<AtomicBool>) {
        let mock = Arc::new(Mock {
            grows: true,
            ..Mock::build(0, 1, false, false)
        });
        mock.register(0, len_ms);
        for i in (1..before).chain(LIVE0..LIVE0 + n) {
            mock.register(i, len_ms);
        }
        let timeline = super::super::timeline::get_or_create(uuid::Uuid::new_v4(), "g");
        let stats = Arc::new(PlayerStats::new("p"));
        timeline.register_player(&stats);
        let fetcher = Arc::new(Fetcher::new("p", mock.clone(), timeline, stats, 2));
        let stop = Arc::new(AtomicBool::new(false));
        Arc::clone(&fetcher).spawn(Arc::clone(&stop));
        (mock, fetcher, stop)
    }

    /// Wait until the fetcher's index has the live segments `[LIVE0,
    /// LIVE0 + n)` of `len_ms`, or 5 s. The ones it has, counted from LIVE0.
    fn wait_indexed(f: &Fetcher, n: i64, len_ms: i64) -> Vec<i64> {
        let ms = 1_000_000;
        let at = |i: i64| 100 * S + i * len_ms * ms;
        let t0 = Instant::now();
        loop {
            let have: Vec<i64> = f
                .store
                .segments_in(at(LIVE0), at(LIVE0 + n))
                .iter()
                .map(|s| (s.start_ns - at(LIVE0)) / (len_ms * ms))
                .collect();
            if have.len() as i64 == n || t0.elapsed() > Duration::from_secs(5) {
                return have;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn live_poll_takes_every_segment_since_the_edge() {
        // 200 ms segments (the poll runs every 200 ms). Three are registered
        // at once between two polls: polling only the latest misses two.
        let (mock, f, stop) = live_fetcher(1, 200);
        assert_eq!(wait_indexed(&f, 1, 200).len(), 1);
        std::thread::sleep(Duration::from_millis(300));
        for i in 1..4 {
            mock.register(LIVE0 + i, 200);
        }
        let have = wait_indexed(&f, 4, 200);
        // And once more after a pause of a few polls.
        std::thread::sleep(Duration::from_millis(500));
        for i in 4..7 {
            mock.register(LIVE0 + i, 200);
        }
        let have2 = wait_indexed(&f, 7, 200);
        stop.store(true, Ordering::Relaxed);
        assert_eq!(have, (0..4).collect::<Vec<_>>());
        assert_eq!(have2, (0..7).collect::<Vec<_>>());
    }

    #[test]
    fn live_poll_fills_a_segment_registered_late() {
        // Segment 3 is registered after 4: the poll sees 4 first and a hole
        // where 3 goes, and takes 3 on a later poll. Not a gap meanwhile.
        let (mock, f, stop) = live_fetcher(3, 200);
        assert_eq!(wait_indexed(&f, 3, 200).len(), 3);
        mock.register(LIVE0 + 4, 200);
        let t0 = Instant::now();
        while f.stats.extent().map(|e| e.1) != Some(100 * S + (LIVE0 + 5) * 200_000_000)
            && t0.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        // A few polls with the hole open.
        std::thread::sleep(Duration::from_millis(700));
        let ms = 1_000_000;
        let a = 100 * S + LIVE0 * 200 * ms;
        let have: Vec<i64> = f
            .store
            .segments_in(a, a + 1000 * ms)
            .iter()
            .map(|s| (s.start_ns - a) / (200 * ms))
            .collect();
        assert_eq!(have, vec![0, 1, 2, 4]);
        assert_eq!(
            f.stats.index_view().gaps,
            vec![],
            "a hole at the live edge is not a gap yet"
        );
        mock.register(LIVE0 + 3, 200);
        let have = wait_indexed(&f, 5, 200);
        stop.store(true, Ordering::Relaxed);
        assert_eq!(have, (0..5).collect::<Vec<_>>());
    }

    #[test]
    fn live_poll_far_behind_jumps_to_the_edge() {
        // More segments than one poll takes: the index gets the edge, not a
        // stale end that catches up a poll at a time.
        let (mock, f, stop) = live_fetcher(1, 200);
        assert_eq!(wait_indexed(&f, 1, 200).len(), 1);
        // 400 segments of 200 ms: 80 s, more than the pages of one poll
        // and more than the hole listing reaches back.
        let n = LIVE0 + 1 + 400;
        for i in LIVE0 + 1..n {
            mock.register(i, 200);
        }
        let edge = 100 * S + n * 200_000_000;
        let t0 = Instant::now();
        while f.stats.extent().map(|e| e.1) != Some(edge) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        assert_eq!(f.stats.extent().map(|e| e.1), Some(edge));
    }

    #[test]
    fn gaps_in_are_whole_holes_between_media() {
        let mut cov = Coverage::default();
        cov.add_media(0, 10 * S, MIN_GAP_NS);
        assert_eq!(cov.gaps_in(0, 20 * S), vec![]);
        cov.add_media(11 * S, 12 * S, MIN_GAP_NS);
        cov.add_media(15 * S, 16 * S, MIN_GAP_NS);
        assert_eq!(
            cov.gaps_in(5 * S, 20 * S),
            vec![(10 * S, 11 * S), (12 * S, 15 * S)]
        );
        // Not clipped, and only those that overlap.
        assert_eq!(cov.gaps_in(13 * S, 14 * S), vec![(12 * S, 15 * S)]);
        assert_eq!(cov.gaps_in(16 * S, 30 * S), vec![]);
    }

    /// Wait until `f` has the live segments `[LIVE0, LIVE0 + n)` with
    /// `len_ms`, and the listings by time stay put for a while (the fetcher
    /// has listed what it found at start).
    fn settle(mock: &Mock, f: &Fetcher, n: i64, len_ms: i64) {
        assert_eq!(wait_indexed(f, n, len_ms).len() as i64, n);
        let t0 = Instant::now();
        loop {
            let l = mock.lists.load(Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(600));
            if mock.lists.load(Ordering::Relaxed) == l || t0.elapsed() > Duration::from_secs(5) {
                return;
            }
        }
    }

    #[test]
    fn live_poll_lists_by_time_only_for_holes() {
        // A source without holes, growing a segment per poll: each poll is
        // one newest-first page, and nothing is listed by time.
        let (mock, f, stop) = live_fetcher_from(LIVE0, 1, 200);
        settle(&mock, &f, 1, 200);
        let (lists, pages) = (
            mock.lists.load(Ordering::Relaxed),
            mock.pages.load(Ordering::Relaxed),
        );
        for i in 1..16 {
            mock.register(LIVE0 + i, 200);
            std::thread::sleep(Duration::from_millis(200));
        }
        let have = wait_indexed(&f, 16, 200);
        let lists = mock.lists.load(Ordering::Relaxed) - lists;
        let pages = mock.pages.load(Ordering::Relaxed) - pages;
        stop.store(true, Ordering::Relaxed);
        assert_eq!(have, (0..16).collect::<Vec<_>>());
        assert_eq!(lists, 0, "no listing by time without a hole");
        assert!(pages >= 10, "a page per poll, got {}", pages);
    }

    #[test]
    fn live_poll_pages_back_to_the_edge() {
        // More new segments than one page between two polls: the poll
        // follows the store's paging back to the edge it knows, and lists
        // nothing by time.
        let (mock, f, stop) = live_fetcher_from(LIVE0, 1, 200);
        settle(&mock, &f, 1, 200);
        let (lists, pages) = (
            mock.lists.load(Ordering::Relaxed),
            mock.pages.load(Ordering::Relaxed),
        );
        let n = 1 + 3 * LIVE_POLL_PAGE as i64;
        for i in 1..n {
            mock.register(LIVE0 + i, 200);
        }
        let have = wait_indexed(&f, n, 200);
        let lists = mock.lists.load(Ordering::Relaxed) - lists;
        let pages = mock.pages.load(Ordering::Relaxed) - pages;
        stop.store(true, Ordering::Relaxed);
        assert_eq!(have, (0..n).collect::<Vec<_>>());
        assert_eq!(lists, 0, "paging, not a listing by time");
        // 24 new segments: three pages of eight, the third reaching the edge.
        assert!(pages >= 3, "followed the pages, got {}", pages);
    }

    #[test]
    fn live_poll_fills_a_late_segment_older_than_the_page() {
        // Segment 3 is registered after 4..=16, so it is older than one
        // page of newest segments: a listing by time of just that hole fills
        // it, a few times at most, and it is not a gap meanwhile.
        let (mock, f, stop) = live_fetcher_from(LIVE0, 3, 200);
        settle(&mock, &f, 3, 200);
        let lists = mock.lists.load(Ordering::Relaxed);
        for i in 4..17 {
            mock.register(LIVE0 + i, 200);
        }
        let edge = 100 * S + (LIVE0 + 17) * 200_000_000;
        let t0 = Instant::now();
        while f.stats.extent().map(|e| e.1) != Some(edge) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        // Polls with the hole open: listed by time with backoff.
        std::thread::sleep(Duration::from_millis(1500));
        let a = 100 * S + LIVE0 * 200_000_000;
        assert!(f
            .store
            .segments_in(a + 600_000_000, a + 800_000_000)
            .is_empty());
        assert_eq!(
            f.stats.index_view().gaps,
            vec![],
            "a hole near the live edge is not a gap yet"
        );
        let open = mock.lists.load(Ordering::Relaxed) - lists;
        assert!(
            (1..=3).contains(&open),
            "the hole is listed with backoff, not per poll: {}",
            open
        );
        mock.register(LIVE0 + 3, 200);
        let have = wait_indexed(&f, 17, 200);
        stop.store(true, Ordering::Relaxed);
        assert_eq!(have, (0..17).collect::<Vec<_>>());
    }

    #[test]
    fn chunks_are_floor_divided() {
        assert_eq!(chunk_of(0), 0);
        assert_eq!(chunk_of(CHUNK_NS - 1), 0);
        assert_eq!(chunk_of(-1), -1);
    }
}
