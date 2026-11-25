//! Index window and coded-bytes cache of one replay player.
//!
//! A fetcher thread with its own small async runtime keeps, around the
//! group cursor:
//! - the index: segments listed from the [`SourceIndex`] in fixed TAI chunks,
//!   plus the live edge polled at segment cadence when the source can grow;
//! - the coded bytes of the segments within `[T - behind, T + ahead]`, fetched
//!   nearest first in the direction of travel, a few at a time.
//!
//! The decode side only reads this store, so the output tick never waits on
//! the network: a segment whose bytes have not arrived is simply not decoded
//! yet, and the tick repeats its last frame.

use super::backoff::{worth_warning, Backoff};
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
}

impl SegmentStore {
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
}

/// A hole shorter than this between two segments is rounding, not a gap.
const MIN_GAP_NS: i64 = 1_000_000;
/// How often a player recomputes what it publishes about its index.
const INDEX_VIEW_EVERY: Duration = Duration::from_millis(500);

/// What the index knows: the ranges of listed chunks within the extent, and
/// the stretches inside them that no segment covers (gaps). Outside the
/// listed ranges the index has not looked, so nothing is said there; a gap
/// that runs on past a listed range is cut at its edge.
pub fn index_view(
    segs: &BTreeMap<i64, Arc<Segment>>,
    listed: &HashSet<i64>,
    extent: (i64, i64),
) -> IndexView {
    let mut chunks: Vec<i64> = listed.iter().copied().collect();
    chunks.sort_unstable();
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for c in chunks {
        let (s, e) = (
            (c * CHUNK_NS).max(extent.0),
            ((c + 1) * CHUNK_NS).min(extent.1),
        );
        if e <= s {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.1 >= s => last.1 = last.1.max(e),
            _ => ranges.push((s, e)),
        }
    }
    let mut gaps = Vec::new();
    for &(ls, le) in &ranges {
        // Walk the segments that overlap [ls, le); `covered` is how far
        // media reaches so far.
        let mut covered = ls;
        let first = segs.range(..ls).next_back().map(|(_, s)| s);
        for seg in first.into_iter().chain(segs.range(ls..le).map(|(_, s)| s)) {
            if seg.start_ns - covered > MIN_GAP_NS {
                gaps.push((covered, seg.start_ns));
            }
            covered = covered.max(seg.end_ns);
        }
        if le - covered > MIN_GAP_NS {
            gaps.push((covered, le));
        }
    }
    IndexView {
        listed: ranges,
        gaps,
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

/// Which index chunks have been listed. A chunk at the live edge gets new
/// segments from the live poll; it is listed again (dirty) only when the poll
/// skipped segments or a fetch failed (expired URLs).
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
            match r {
                Ok((info, Some(first), Some(last))) => {
                    info!(
                        "Replay {}: {} frame rate {:?}, container {:?}",
                        self.block_id,
                        self.source.describe(),
                        info.frame_rate,
                        info.container
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
        let mut next_view = Instant::now();

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
                match self.source.last().await {
                    Ok(Some(l)) if l.end_ns > end => {
                        if l.start_ns > end {
                            // Missed segments in between: list them again.
                            for c in chunk_of(end)..=chunk_of(l.start_ns) {
                                chunks.dirty.insert(c);
                            }
                        }
                        seg_ns = l.duration_ns().clamp(S / 5, 5 * S);
                        end = l.end_ns;
                        self.store.insert([l]);
                        self.publish_extent(start, end);
                    }
                    Ok(_) => {}
                    Err(e) => debug!("Replay {}: live poll failed: {:#}", self.block_id, e),
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
            for c in chunk_of(lo.max(start))..=chunk_of(hi.min(end - 1)) {
                if chunks.listed.contains(&c) && !chunks.dirty.contains(&c) {
                    continue;
                }
                if !list_failed.ready(&c, Instant::now()) {
                    continue;
                }
                match self.source.list(c * CHUNK_NS, (c + 1) * CHUNK_NS).await {
                    Ok(segs) => {
                        self.store.insert(segs);
                        chunks.listed.insert(c);
                        chunks.dirty.remove(&c);
                        chunks.listed_at.insert(c, Instant::now());
                        list_failed.succeed(&c);
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
                        break;
                    }
                }
            }

            // Fetch bytes, most urgent first.
            let window = self.store.segments_in(lo, hi);
            for seg in by_urgency(&window, cursor, forward) {
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
            lock(&self.store.bytes)
                .retain(|start, (end, _)| *end > lo - margin && *start < hi + margin);
            let cc = chunk_of(cursor);
            let keep = |c: i64| (c - cc).abs() <= KEEP_CHUNKS;
            chunks.listed.retain(|c| keep(*c));
            chunks.listed_at.retain(|c, _| keep(*c));
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
                index.retain(|k, _| keep(chunk_of(*k)) || *k == start);
                self.stats
                    .index_segments
                    .store(index.len() as u64, Ordering::Relaxed);
                if Instant::now() >= next_view {
                    next_view = Instant::now() + INDEX_VIEW_EVERY;
                    self.stats
                        .set_index_view(index_view(&index, &chunks.listed, (start, end)));
                }
            }
            self.stats
                .fetched_segments
                .store(lock(&self.store.bytes).len() as u64, Ordering::Relaxed);
        }
        jobs.abort_all();
    }

    fn publish_extent(&self, start: i64, end: i64) {
        self.stats.start_tai.store(start, Ordering::Relaxed);
        self.stats.end_tai.store(end, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::source::Location;
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
        lists: AtomicU64,
        fetches: Mutex<HashMap<i64, u32>>,
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
                fetches: Mutex::new(HashMap::new()),
            }
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
            false
        }
        async fn info(&self) -> anyhow::Result<SourceInfo> {
            Ok(SourceInfo {
                frame_rate: Some((25, 1)),
                container: None,
                absolute_time: true,
            })
        }
        async fn list(&self, start: i64, end: i64) -> anyhow::Result<Vec<Segment>> {
            self.lists.fetch_add(1, Ordering::Relaxed);
            if self.list_fails {
                anyhow::bail!("gateway down");
            }
            Ok(self
                .segs
                .iter()
                .filter(|s| s.end_ns > start && s.start_ns < end)
                .cloned()
                .collect())
        }
        async fn first(&self) -> anyhow::Result<Option<Segment>> {
            Ok(self.segs.first().cloned())
        }
        async fn last(&self) -> anyhow::Result<Option<Segment>> {
            Ok(self.segs.last().cloned())
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

    fn seg_at(start: i64, end: i64) -> Segment {
        Segment {
            start_ns: start,
            end_ns: end,
            ..(*seg(0)).clone()
        }
    }

    #[test]
    fn index_view_reports_holes_only_where_it_listed() {
        // Segments of 1 s at 0..10 s, a hole 4..6 s, and one at 45..46 s.
        // Chunk 1 (20..40 s) was never listed.
        let mut index = BTreeMap::new();
        for i in (0..4).chain(6..10) {
            index.insert(i * S, Arc::new(seg_at(i * S, (i + 1) * S)));
        }
        index.insert(45 * S, Arc::new(seg_at(45 * S, 46 * S)));
        let listed: HashSet<i64> = [0, 2].into_iter().collect();
        let v = index_view(&index, &listed, (0, 46 * S));
        assert_eq!(v.listed, vec![(0, 20 * S), (40 * S, 46 * S)]);
        // Holes are cut at the listed ranges: nothing is said about 20..40 s.
        assert_eq!(
            v.gaps,
            vec![(4 * S, 6 * S), (10 * S, 20 * S), (40 * S, 45 * S)]
        );
        // With every chunk listed, 10..45 s is one gap, even though the
        // chunk in the middle holds no segment at all.
        let listed: HashSet<i64> = [0, 1, 2].into_iter().collect();
        let v = index_view(&index, &listed, (0, 46 * S));
        assert_eq!(v.listed, vec![(0, 46 * S)]);
        assert_eq!(v.gaps, vec![(4 * S, 6 * S), (10 * S, 45 * S)]);
        // A segment that starts before a listed range covers its start, and
        // sub-millisecond seams are not gaps.
        let mut index = BTreeMap::new();
        index.insert(-5 * S, Arc::new(seg_at(-5 * S, S)));
        index.insert(S + 500, Arc::new(seg_at(S + 500, 2 * S)));
        let v = index_view(&index, &[0].into_iter().collect(), (-5 * S, 2 * S));
        assert!(v.gaps.is_empty(), "{:?}", v.gaps);
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

    #[test]
    fn chunks_are_floor_divided() {
        assert_eq!(chunk_of(0), 0);
        assert_eq!(chunk_of(CHUNK_NS - 1), 0);
        assert_eq!(chunk_of(-1), -1);
    }
}
