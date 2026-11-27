//! What one player of a replay group knows and reports: its source's extent,
//! grid and index, what it pushed, its counters, and its configuration.
//!
//! Times stored here are the source's own (TAMS) times. The accessors that
//! say "group" move them by the player's `offset_ms` onto the group's axis.

use super::super::grid::FrameGrid;
use super::switch::SourceControl;
use super::{lock, TimeDomain, UNKNOWN};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Weak};
use strom_types::replay::{ReplayJogGrid, ReplayReady};

/// Consecutive in-sync ticks before an angle counts as ready (decoding keeps
/// up with play).
pub const READY_TICKS: u64 = 10;

/// How a group starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartMode {
    /// Paused at the start of the media.
    #[default]
    Paused,
    /// Live as soon as the group is ready.
    Live,
}

/// Which source a player shows, for discovery and the state. Without
/// credentials.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceIdent {
    /// The source URI.
    pub source: Option<String>,
    /// The source's own id (TAMS flow id, directory name).
    pub source_id: Option<String>,
    /// The store the source is in.
    pub store: Option<String>,
}

/// A player's replay properties and identity, as built. The source can
/// change during the run ([`PlayerStats::source_ident`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerConfig {
    /// The block's name in the flow.
    pub name: Option<String>,
    /// The source URI without credentials.
    pub source: Option<String>,
    /// The source's own id (TAMS flow id, directory name).
    pub source_id: Option<String>,
    /// The store the source is in, without credentials.
    pub store: Option<String>,
    pub follow_live: bool,
    pub jog_grid: ReplayJogGrid,
    pub start_mode: StartMode,
    /// `None`: computed from the live edge's steps.
    pub live_margin_ms: Option<u64>,
    /// Trim: the source's media sits this much later on the group's axis.
    pub offset_ns: i64,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            name: None,
            source: None,
            source_id: None,
            store: None,
            follow_live: true,
            jog_grid: ReplayJogGrid::Source,
            start_mode: StartMode::Paused,
            live_margin_ms: None,
            offset_ns: 0,
        }
    }
}

/// What a player's index knows (see `cache::Coverage`): the TAI ranges it
/// has listed, and the holes in the source it has listed from end to end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexView {
    pub listed: Vec<(i64, i64)>,
    pub gaps: Vec<(i64, i64)>,
}

/// Decodes kept per player in [`PlayerStats`]' log.
const DECODE_LOG: usize = 16;

/// One segment decode, for diagnostics.
#[derive(Debug, Clone)]
pub struct DecodeRecord {
    /// The segment's TAI range, `[start, end)`.
    pub start_ns: i64,
    pub end_ns: i64,
    /// Wall time of the decode, ms.
    pub ms: u64,
    /// Frames placed on the player's grid.
    pub frames: usize,
    /// Why the segment gave no usable frames.
    pub error: Option<String>,
}

/// State and telemetry of one player in the group.
pub struct PlayerStats {
    pub block_id: String,
    pub config: PlayerConfig,
    /// TAI extent of the player's source, `[start, end)` (source time).
    pub start_tai: AtomicI64,
    pub end_tai: AtomicI64,
    /// The player's own frame grid (its source's rate, anchored on its media).
    grid: Mutex<Option<FrameGrid>>,
    /// Output rate as `(n, d)`: configured, or once the output runs.
    output_rate: Mutex<Option<(u32, u32)>>,
    /// Absolute frame (own grid, nearest) last pushed.
    pub shown_frame: AtomicI64,
    /// The own TAI of the frame last pushed (see `decode::map_to_grid`): it
    /// can sit off the player's grid when the source changed its phase.
    pub shown_tai: AtomicI64,
    /// The last tick found the frame that holds its time.
    pub in_sync: AtomicBool,
    /// Consecutive in-sync ticks, and whether decoding has kept up once.
    sync_run: AtomicU64,
    warm: AtomicBool,
    /// The hole of the recording the last tick's time was in, `[start, end)`.
    gap_now: Mutex<Option<(i64, i64)>>,
    /// Group time the last tick asked for.
    pub wanted_tai: AtomicI64,
    /// Frame number read from the pushed picture's bitstrip (diagnostics).
    pub strip_frame: AtomicI64,
    /// Pushed pictures whose bitstrip differs from their timeline frame.
    pub strip_mismatches: AtomicU64,
    /// Burned-in UTC (µs) read from the last new pushed picture (diagnostics).
    pub code_utc_us: AtomicI64,
    /// New pushed pictures whose burned-in time was read, and those whose time
    /// does not hold the group time of their tick (a wrong source frame).
    pub time_checked: AtomicU64,
    pub time_errors: AtomicU64,
    /// Ticks that pushed the same source frame as the tick before, and source
    /// frames jumped over between consecutive ticks (speed vs output rate).
    pub repeated_ticks: AtomicU64,
    pub skipped_frames: AtomicU64,
    pub misses: AtomicU64,
    pub late_ticks: AtomicU64,
    pub index_segments: AtomicU64,
    pub fetched_segments: AtomicU64,
    pub fetches: AtomicU64,
    pub fetch_errors: AtomicU64,
    pub last_fetch_ms: AtomicU64,
    pub cached_segments: AtomicU64,
    pub decodes: AtomicU64,
    pub last_decode_ms: AtomicU64,
    /// Decodes that gave no usable frames, and the time spent in successful
    /// decodes (sum and longest), ms.
    pub decode_failures: AtomicU64,
    pub(super) decode_ms_total: AtomicU64,
    pub(super) decode_ms_max: AtomicU64,
    /// The decoder element and the number of decode workers.
    pub(super) decoder: Mutex<Option<(String, usize)>>,
    /// The latest decodes, oldest first (diagnostics).
    pub(super) decode_log: Mutex<std::collections::VecDeque<DecodeRecord>>,
    /// No frame rate is known: none in the index, the stream's caps or its
    /// timestamps. Nothing can be placed on the timeline until one is.
    pub frame_rate_missing: AtomicBool,
    /// [`TimeDomain`] of the source, as u8.
    time_domain: AtomicU8,
    index_view: Mutex<IndexView>,
    /// The source shown now (the build's, or the last one switched to).
    ident: Mutex<SourceIdent>,
    /// The trim of the source shown now, ns.
    offset_ns: AtomicI64,
    /// The source being switched to, without credentials.
    pending_source: Mutex<Option<String>>,
    /// The player, for a source switch; `None` for a player that cannot
    /// switch (an export, a test).
    control: Mutex<Option<Weak<dyn SourceControl>>>,
}

impl PlayerStats {
    pub fn new(block_id: &str) -> Self {
        Self::with_config(block_id, PlayerConfig::default())
    }

    pub fn with_config(block_id: &str, config: PlayerConfig) -> Self {
        Self {
            block_id: block_id.to_string(),
            ident: Mutex::new(SourceIdent {
                source: config.source.clone(),
                source_id: config.source_id.clone(),
                store: config.store.clone(),
            }),
            offset_ns: AtomicI64::new(config.offset_ns),
            pending_source: Mutex::new(None),
            control: Mutex::new(None),
            config,
            start_tai: AtomicI64::new(UNKNOWN),
            end_tai: AtomicI64::new(UNKNOWN),
            grid: Mutex::new(None),
            output_rate: Mutex::new(None),
            shown_frame: AtomicI64::new(UNKNOWN),
            shown_tai: AtomicI64::new(UNKNOWN),
            in_sync: AtomicBool::new(false),
            sync_run: AtomicU64::new(0),
            warm: AtomicBool::new(false),
            gap_now: Mutex::new(None),
            wanted_tai: AtomicI64::new(UNKNOWN),
            strip_frame: AtomicI64::new(-1),
            strip_mismatches: AtomicU64::new(0),
            code_utc_us: AtomicI64::new(-1),
            time_checked: AtomicU64::new(0),
            time_errors: AtomicU64::new(0),
            repeated_ticks: AtomicU64::new(0),
            skipped_frames: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            late_ticks: AtomicU64::new(0),
            index_segments: AtomicU64::new(0),
            fetched_segments: AtomicU64::new(0),
            fetches: AtomicU64::new(0),
            fetch_errors: AtomicU64::new(0),
            last_fetch_ms: AtomicU64::new(0),
            cached_segments: AtomicU64::new(0),
            decodes: AtomicU64::new(0),
            last_decode_ms: AtomicU64::new(0),
            decode_failures: AtomicU64::new(0),
            decode_ms_total: AtomicU64::new(0),
            decode_ms_max: AtomicU64::new(0),
            decoder: Mutex::new(None),
            decode_log: Mutex::new(std::collections::VecDeque::new()),
            frame_rate_missing: AtomicBool::new(false),
            time_domain: AtomicU8::new(TimeDomain::Unknown as u8),
            index_view: Mutex::new(IndexView::default()),
        }
    }

    /// The trim, ns: source time + offset = group time.
    pub fn offset_ns(&self) -> i64 {
        self.offset_ns.load(Ordering::Relaxed)
    }

    /// The source shown now.
    pub fn source_ident(&self) -> SourceIdent {
        lock(&self.ident).clone()
    }

    pub fn pending_source(&self) -> Option<String> {
        lock(&self.pending_source).clone()
    }

    pub fn set_pending_source(&self, source: Option<String>) {
        *lock(&self.pending_source) = source;
    }

    /// Who switches this player's source.
    pub fn set_control(&self, control: Weak<dyn SourceControl>) {
        *lock(&self.control) = Some(control);
    }

    pub fn control(&self) -> Option<Arc<dyn SourceControl>> {
        lock(&self.control).as_ref().and_then(|w| w.upgrade())
    }

    /// Take over what `from` (the stats a new source was read into) knows
    /// about its source: identity, trim, extent, grid, time domain and
    /// index. The counters stay: they are the player's.
    pub fn adopt_source(&self, from: &PlayerStats, ident: SourceIdent) {
        *lock(&self.ident) = ident;
        self.offset_ns.store(from.offset_ns(), Ordering::Relaxed);
        self.start_tai
            .store(from.start_tai.load(Ordering::Relaxed), Ordering::Relaxed);
        self.end_tai
            .store(from.end_tai.load(Ordering::Relaxed), Ordering::Relaxed);
        *lock(&self.grid) = from.grid();
        self.time_domain
            .store(from.time_domain.load(Ordering::Relaxed), Ordering::Relaxed);
        *lock(&self.index_view) = from.index_view();
        self.frame_rate_missing.store(
            from.frame_rate_missing.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        *lock(&self.gap_now) = None;
        *lock(&self.pending_source) = None;
    }

    pub fn set_time_domain(&self, d: TimeDomain) {
        self.time_domain.store(d as u8, Ordering::Relaxed);
    }

    pub fn time_domain(&self) -> TimeDomain {
        TimeDomain::from_u8(self.time_domain.load(Ordering::Relaxed))
    }

    /// The source's extent, source time.
    pub fn source_extent(&self) -> Option<(i64, i64)> {
        let s = self.start_tai.load(Ordering::Relaxed);
        let e = self.end_tai.load(Ordering::Relaxed);
        (s != UNKNOWN && e != UNKNOWN && e > s).then_some((s, e))
    }

    /// The extent on the group's axis.
    pub fn extent(&self) -> Option<(i64, i64)> {
        let o = self.offset_ns();
        self.source_extent()
            .map(|(s, e)| (s.saturating_add(o), e.saturating_add(o)))
    }

    /// The player's frame grid in source time, once its frame rate is known.
    pub fn grid(&self) -> Option<FrameGrid> {
        *lock(&self.grid)
    }

    /// The player's frame grid on the group's axis.
    pub fn group_grid(&self) -> Option<FrameGrid> {
        let g = self.grid()?;
        if self.offset_ns() == 0 {
            return Some(g);
        }
        FrameGrid::new(
            g.fps_n,
            g.fps_d,
            g.phase_ns.saturating_add(self.offset_ns()),
        )
    }

    /// Set the player's grid from its source's frame rate, anchored at an
    /// instant of its media, unless already set. Returns the grid in use.
    pub fn init_grid(&self, fps_n: u32, fps_d: u32, anchor_tai_ns: i64) -> Option<FrameGrid> {
        let mut g = lock(&self.grid);
        if g.is_none() {
            *g = FrameGrid::new(fps_n, fps_d, anchor_tai_ns);
        }
        *g
    }

    /// The decoder element and how many workers decode for the player.
    pub fn set_decoder(&self, name: &str, workers: usize) {
        *lock(&self.decoder) = Some((name.to_string(), workers));
    }

    /// One segment decoded (or failed to), for the counters and the log.
    pub fn note_decode(&self, rec: DecodeRecord) {
        if rec.error.is_some() {
            self.decode_failures.fetch_add(1, Ordering::Relaxed);
        } else {
            self.decodes.fetch_add(1, Ordering::Relaxed);
            self.last_decode_ms.store(rec.ms, Ordering::Relaxed);
            self.decode_ms_total.fetch_add(rec.ms, Ordering::Relaxed);
            self.decode_ms_max.fetch_max(rec.ms, Ordering::Relaxed);
        }
        let mut log = lock(&self.decode_log);
        if log.len() >= DECODE_LOG {
            log.pop_front();
        }
        log.push_back(rec);
    }

    /// One output tick: whether it found the frame that holds its time
    /// `wanted` (source time). [`READY_TICKS`] in a row make the angle ready
    /// for the run; a tick at a time the source has no media for (another
    /// angle starts earlier) has nothing to decode and counts as in time.
    pub fn note_tick(&self, in_sync: bool, wanted: Option<i64>) {
        self.in_sync.store(in_sync, Ordering::Relaxed);
        let no_media = match (wanted, self.source_extent()) {
            (Some(t), Some((s, e))) => t < s || t >= e,
            _ => false,
        };
        if in_sync || no_media {
            if self.sync_run.fetch_add(1, Ordering::Relaxed) + 1 >= READY_TICKS {
                self.warm.store(true, Ordering::Relaxed);
            }
        } else {
            self.sync_run.store(0, Ordering::Relaxed);
        }
    }

    /// Indexing until the extent is known, warming until decoding has kept
    /// up, then ready for the run.
    pub fn ready(&self) -> ReplayReady {
        if self.source_extent().is_none() {
            ReplayReady::Indexing
        } else if self.warm.load(Ordering::Relaxed) {
            ReplayReady::Ready
        } else {
            ReplayReady::Warming
        }
    }

    pub fn set_index_view(&self, v: IndexView) {
        *lock(&self.index_view) = v;
    }

    /// The index view, source time.
    pub fn index_view(&self) -> IndexView {
        lock(&self.index_view).clone()
    }

    /// The index view on the group's axis.
    pub fn group_index_view(&self) -> IndexView {
        let o = self.offset_ns();
        let v = self.index_view();
        let shift = |r: Vec<(i64, i64)>| {
            r.into_iter()
                .map(|(s, e)| (s.saturating_add(o), e.saturating_add(o)))
                .collect()
        };
        IndexView {
            listed: shift(v.listed),
            gaps: shift(v.gaps),
        }
    }

    /// A cheap summary of the holes, to notice when they change.
    pub(super) fn gaps_signature(&self) -> u64 {
        let v = lock(&self.index_view);
        v.gaps.iter().fold(v.gaps.len() as u64, |h, &(s, e)| {
            h.wrapping_mul(1_000_003)
                .wrapping_add(s as u64)
                .wrapping_mul(1_000_003)
                .wrapping_add(e as u64)
        })
    }

    pub fn set_gap(&self, gap: Option<(i64, i64)>) {
        *lock(&self.gap_now) = gap;
    }

    /// The hole the last tick was in, on the group's axis.
    pub fn group_gap(&self) -> Option<(i64, i64)> {
        let o = self.offset_ns();
        lock(&self.gap_now).map(|(s, e)| (s.saturating_add(o), e.saturating_add(o)))
    }

    pub fn set_output_rate(&self, fps_n: u32, fps_d: u32) {
        *lock(&self.output_rate) = Some((fps_n, fps_d));
    }

    pub fn output_rate(&self) -> Option<(u32, u32)> {
        *lock(&self.output_rate)
    }

    /// The rate the output runs (or will run) at: configured, else the
    /// source's own.
    pub fn effective_output_rate(&self) -> Option<(u32, u32)> {
        self.output_rate()
            .or_else(|| self.grid().map(|g| (g.fps_n, g.fps_d)))
    }

    /// The source frame last pushed, its own time on the group's axis.
    pub fn shown_group_tai(&self) -> Option<i64> {
        let t = self.shown_tai.load(Ordering::Relaxed);
        (t != UNKNOWN).then(|| t.saturating_add(self.offset_ns()))
    }
}
