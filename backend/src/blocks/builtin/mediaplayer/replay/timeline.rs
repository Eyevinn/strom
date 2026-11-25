//! Shared replay timeline.
//!
//! One timeline per (flow, replay group) owns the cursor and the speed. **The
//! group shares time, not frame numbers**: the cursor is a TAI instant, a
//! function of the flow's running time, `T(rt) = T0 + speed * (rt - rt0)`, so
//! every player that asks for the same running time gets the same instant and
//! the angles cannot drift apart. A command re-anchors the line at "now".
//!
//! Each player has its own [`FrameGrid`] (its source's rate and phase) and
//! shows, at group time `T`, **its latest frame that starts at or before `T`**
//! (sample and hold: the picture that camera had at `T`). Each player outputs
//! at its own output rate (`replay_output_framerate`, by default the source's
//! rate), so a 100p angle played at 50 % on a 50p output shows every source
//! frame, and a 25p angle on a 50p output shows each frame twice.
//!
//! Frame numbers in commands and telemetry count frames of the group's
//! **reference grid**: the finest source grid in the group (highest rate; on a
//! tie the smallest phase, so the choice does not depend on which angle
//! reported first). A jog of one reference frame therefore never skips a frame
//! of any angle. Frame 0 is the reference frame that holds the group's origin,
//! the start of the earliest angle when the group first saw its sources.
//! Commands can also address an instant as UTC or TAI, which is exact; and
//! telemetry gives the UTC and TAI of the frames it names (see [`FrameTime`]).
//! Times are only wall-clock times when every source says its index is real
//! TAI ([`TimeDomain`]).

use super::grid::FrameGrid;
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use strom_types::tai::{
    format_calendar_ms, format_utc_ms, parse_utc, tai_to_utc_ns, utc_to_tai_ns,
};
use strom_types::tams::parse_timestamp_signed;
use strom_types::FlowId;

/// Unknown value for the i64 atomics below.
pub const UNKNOWN: i64 = i64::MIN;

/// Default distance behind the live edge for the `live` command.
pub const DEFAULT_LIVE_MARGIN_MS: u64 = 1500;

/// Fastest shuttle, either direction.
pub const MAX_SPEED: f64 = 8.0;

/// What the TAI of a source's index means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeDomain {
    /// Not known yet (the index has not been read).
    Unknown = 0,
    /// Real TAI: converts to UTC, the time on the wall when it was captured.
    Tai = 1,
    /// TAI on an arbitrary base (test material): ordered and synced between
    /// angles, but the UTC it converts to is not a real time.
    Unanchored = 2,
}

impl TimeDomain {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Tai,
            2 => Self::Unanchored,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Tai => "tai",
            Self::Unanchored => "unanchored",
        }
    }

    /// A group is real TAI only when every angle is.
    pub fn combine(domains: impl IntoIterator<Item = Self>) -> Self {
        let mut out = Self::Unknown;
        for d in domains {
            match d {
                Self::Unanchored => return Self::Unanchored,
                Self::Unknown => return Self::Unknown,
                Self::Tai => out = Self::Tai,
            }
        }
        out
    }
}

/// `"<s>:<ns>"`, the TAMS timestamp form.
pub fn tai_string(tai_ns: i64) -> String {
    format!(
        "{}:{}",
        tai_ns.div_euclid(1_000_000_000),
        tai_ns.rem_euclid(1_000_000_000)
    )
}

/// The time of a frame on a grid, for telemetry and acks: where frame `k`
/// starts, as TAI and as UTC (TAI - [`TAI_MINUS_UTC_NS`]).
///
/// [`TAI_MINUS_UTC_NS`]: strom_types::tai::TAI_MINUS_UTC_NS
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameTime {
    /// Frame relative to the origin frame on the same grid.
    pub frame: i64,
    pub tai_ns: i64,
    pub utc_ns: i64,
    /// Index of the frame among the frames that start in its UTC second.
    pub frame_in_second: i64,
}

impl FrameTime {
    pub fn new(grid: &FrameGrid, abs_frame: i64, origin: i64) -> Self {
        let tai_ns = grid.tai_of_frame(abs_frame);
        let utc_ns = tai_to_utc_ns(tai_ns);
        let second_tai = utc_to_tai_ns(utc_ns.div_euclid(1_000_000_000) * 1_000_000_000);
        Self {
            frame: abs_frame - origin,
            tai_ns,
            utc_ns,
            frame_in_second: abs_frame - grid.ceil_frame(second_tai),
        }
    }

    /// JSON for the control API. `tai_ns` is a string: it does not fit in a
    /// JavaScript number.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame,
            "utc": format_utc_ms(self.utc_ns),
            "tai": tai_string(self.tai_ns),
            "tai_ns": self.tai_ns.to_string(),
            "tai_clock": format_calendar_ms(self.tai_ns),
            "frame_in_second": self.frame_in_second,
        })
    }
}

/// An instant (not a frame) as JSON: UTC, TAI, and the TAI ns as a string.
pub fn instant_json(tai_ns: i64) -> serde_json::Value {
    serde_json::json!({
        "utc": format_utc_ms(tai_to_utc_ns(tai_ns)),
        "tai": tai_string(tai_ns),
        "tai_ns": tai_ns.to_string(),
    })
}

/// UTC ns now, from the host's wall clock.
fn wall_utc_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn fps_string(g: &FrameGrid) -> String {
    format!("{}/{}", g.fps_n, g.fps_d)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// State and telemetry of one player in the group.
pub struct PlayerStats {
    pub block_id: String,
    /// TAI extent of the player's source, `[start, end)`.
    pub start_tai: AtomicI64,
    pub end_tai: AtomicI64,
    /// The player's own frame grid (its source's rate, anchored on its media).
    grid: Mutex<Option<FrameGrid>>,
    /// Output rate as `(n, d)`, once the output runs.
    output_rate: Mutex<Option<(u32, u32)>>,
    /// Absolute frame (own grid) last pushed.
    pub shown_frame: AtomicI64,
    /// Absolute frame (own grid) the last tick asked for.
    pub wanted_frame: AtomicI64,
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
    /// [`TimeDomain`] of the source, as u8.
    time_domain: AtomicU8,
}

impl PlayerStats {
    pub fn new(block_id: &str) -> Self {
        Self {
            block_id: block_id.to_string(),
            start_tai: AtomicI64::new(UNKNOWN),
            end_tai: AtomicI64::new(UNKNOWN),
            grid: Mutex::new(None),
            output_rate: Mutex::new(None),
            shown_frame: AtomicI64::new(UNKNOWN),
            wanted_frame: AtomicI64::new(UNKNOWN),
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
            time_domain: AtomicU8::new(TimeDomain::Unknown as u8),
        }
    }

    pub fn set_time_domain(&self, d: TimeDomain) {
        self.time_domain.store(d as u8, Ordering::Relaxed);
    }

    pub fn time_domain(&self) -> TimeDomain {
        TimeDomain::from_u8(self.time_domain.load(Ordering::Relaxed))
    }

    pub fn extent(&self) -> Option<(i64, i64)> {
        let s = self.start_tai.load(Ordering::Relaxed);
        let e = self.end_tai.load(Ordering::Relaxed);
        (s != UNKNOWN && e != UNKNOWN && e > s).then_some((s, e))
    }

    /// The player's frame grid, once its frame rate is known.
    pub fn grid(&self) -> Option<FrameGrid> {
        *lock(&self.grid)
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

    pub fn set_output_rate(&self, fps_n: u32, fps_d: u32) {
        *lock(&self.output_rate) = Some((fps_n, fps_d));
    }

    pub fn output_rate(&self) -> Option<(u32, u32)> {
        *lock(&self.output_rate)
    }
}

/// The group's finest grid: highest rate, then smallest phase. Deterministic
/// whatever order the players reported in.
pub fn reference_grid(grids: impl IntoIterator<Item = FrameGrid>) -> Option<FrameGrid> {
    grids.into_iter().min_by(|a, b| {
        // a/b > c/d  <=>  a*d > c*b; the higher rate sorts first.
        let ra = a.fps_n as u64 * b.fps_d as u64;
        let rb = b.fps_n as u64 * a.fps_d as u64;
        rb.cmp(&ra).then(a.phase_ns.cmp(&b.phase_ns))
    })
}

/// The middle of frame `k`: where the cursor rests after a frame-addressed
/// command, so rounding never lands it on a neighbour.
pub fn frame_centre(grid: &FrameGrid, k: i64) -> i64 {
    let a = grid.tai_of_frame(k);
    let b = grid.tai_of_frame(k + 1);
    a + (b - a) / 2
}

/// Where the cursor line starts: a TAI instant, or the group's start.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Anchor {
    rt: i64,
    /// Cursor TAI at `rt`; `None` = at the start of the group's range.
    tai: Option<i64>,
    speed: f64,
}

const START_ANCHOR: Anchor = Anchor {
    rt: 0,
    tai: None,
    speed: 0.0,
};

/// Operator commands.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Pause and move by a number of reference-grid frames.
    Jog {
        frames: i64,
    },
    /// Go to a reference frame (relative to the group origin), or to an
    /// instant given as UTC (RFC 3339, e.g. `2026-11-24T13:21:56.080Z`) or TAI
    /// (TAMS `"<s>:<ns>"`), keeping the speed. Exactly one of the three.
    Seek {
        #[serde(default)]
        frame: Option<i64>,
        #[serde(default)]
        utc: Option<String>,
        #[serde(default)]
        tai: Option<String>,
    },
    /// Move by milliseconds, keeping the speed.
    SeekRel {
        ms: i64,
    },
    /// Play at a speed (negative = reverse, 0 = pause).
    Speed {
        speed: f64,
    },
    Play,
    Pause,
    /// Go to the live edge (the last instant every angle has) and pause.
    End,
    /// Play at 1x from the live edge minus a margin (default 1.5 s).
    Live {
        #[serde(default)]
        margin_ms: Option<u64>,
    },
}

/// The group's playable range, TAI ns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Start of the earliest angle.
    pub start: i64,
    /// End of the latest angle.
    pub end: i64,
    /// End of the angle that ends first: the live edge, where all have media.
    pub live_end: i64,
}

/// Bounds from each player's TAI extent. Angles are aligned on TAI: the range
/// is the union of what the angles cover, the live edge where all have media.
pub fn group_bounds(extents: &[(i64, i64)]) -> Option<Bounds> {
    let mut it = extents.iter().copied();
    let first = it.next()?;
    let (start, end, live_end) = it.fold((first.0, first.1, first.1), |(s, e, l), (ps, pe)| {
        (s.min(ps), e.max(pe), l.min(pe))
    });
    (end > start).then_some(Bounds {
        start,
        end,
        live_end: live_end.max(start + 1),
    })
}

/// What a command did: where the cursor is afterwards, and the speed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Applied {
    pub tai: i64,
    pub speed: f64,
}

pub struct ReplayTimeline {
    anchor: Mutex<Anchor>,
    /// TAI that frame 0 holds: the group's start when first known.
    origin_tai: AtomicI64,
    clock_source: Mutex<Option<gst::glib::WeakRef<gst::Element>>>,
    created: Instant,
    players: Mutex<Vec<Weak<PlayerStats>>>,
    /// Last paused-position command: when it was applied and its cursor, so a
    /// player can report how long until it pushed that instant.
    last_cmd: Mutex<Option<(Instant, i64)>>,
    pub cmd_to_push_us: AtomicU64,
    /// Running time and cursor of the latest tick any player pushed.
    last_pushed: Mutex<Option<(i64, i64)>>,
}

/// Timelines keyed by (flow, replay group).
type TimelineMap = HashMap<(FlowId, String), Arc<ReplayTimeline>>;

static TIMELINES: LazyLock<Mutex<TimelineMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_or_create(flow_id: FlowId, group: &str) -> Arc<ReplayTimeline> {
    let mut map = lock(&TIMELINES);
    map.entry((flow_id, group.to_string()))
        .or_insert_with(|| Arc::new(ReplayTimeline::new()))
        .clone()
}

pub fn get(flow_id: FlowId, group: &str) -> Option<Arc<ReplayTimeline>> {
    lock(&TIMELINES).get(&(flow_id, group.to_string())).cloned()
}

impl ReplayTimeline {
    fn new() -> Self {
        Self {
            anchor: Mutex::new(START_ANCHOR),
            origin_tai: AtomicI64::new(UNKNOWN),
            clock_source: Mutex::new(None),
            created: Instant::now(),
            players: Mutex::new(Vec::new()),
            last_cmd: Mutex::new(None),
            cmd_to_push_us: AtomicU64::new(0),
            last_pushed: Mutex::new(None),
        }
    }

    /// Called by each player at build. The first player of a new run resets
    /// the group, so a rebuilt flow can change sources.
    pub fn register_player(&self, stats: &Arc<PlayerStats>) {
        let mut players = lock(&self.players);
        players.retain(|w| w.strong_count() > 0);
        if players.is_empty() {
            self.origin_tai.store(UNKNOWN, Ordering::Relaxed);
            *lock(&self.anchor) = START_ANCHOR;
            *lock(&self.last_pushed) = None;
        }
        players.push(Arc::downgrade(stats));
    }

    fn players(&self) -> Vec<Arc<PlayerStats>> {
        lock(&self.players)
            .iter()
            .filter_map(|w| w.upgrade())
            .collect()
    }

    /// The group's reference grid (see the module docs), once a player knows
    /// its frame rate.
    pub fn reference(&self) -> Option<FrameGrid> {
        reference_grid(self.players().iter().filter_map(|p| p.grid()))
    }

    /// An element in the flow whose clock and base time define "now".
    pub fn set_clock_source(&self, element: &gst::Element) {
        let mut src = lock(&self.clock_source);
        let alive = src
            .as_ref()
            .and_then(|w| w.upgrade())
            .is_some_and(|e| e.current_state() == gst::State::Playing);
        if !alive {
            *src = Some(element.downgrade());
        }
    }

    /// The flow's running time now, or a monotonic stand-in before it plays.
    pub fn now_rt(&self) -> i64 {
        let src = lock(&self.clock_source);
        if let Some(el) = src.as_ref().and_then(|w| w.upgrade()) {
            if let (Some(clock), Some(base)) = (el.clock(), el.base_time()) {
                return clock.time().nseconds() as i64 - base.nseconds() as i64;
            }
        }
        self.created.elapsed().as_nanos() as i64
    }

    /// The group's range, once some angle has reported its extent.
    pub fn bounds(&self) -> Option<Bounds> {
        let extents: Vec<(i64, i64)> = self.players().iter().filter_map(|p| p.extent()).collect();
        let b = group_bounds(&extents)?;
        let _ = self.origin_tai.compare_exchange(
            UNKNOWN,
            b.start,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        Some(b)
    }

    /// TAI of the group's frame 0 (start of the earliest angle when the group
    /// first saw its sources), or [`UNKNOWN`].
    pub fn origin_tai(&self) -> i64 {
        self.origin_tai.load(Ordering::Relaxed)
    }

    /// The absolute frame on `grid` that holds the group origin.
    pub fn origin_on(&self, grid: &FrameGrid) -> Option<i64> {
        let o = self.origin_tai();
        (o != UNKNOWN).then(|| grid.floor_frame(o))
    }

    fn eval(a: &Anchor, rt: i64, b: &Bounds) -> i64 {
        let from = a.tai.unwrap_or(b.start) as i128;
        let moved = (a.speed * (rt - a.rt) as f64).round() as i128;
        (from + moved).clamp(b.start as i128, (b.end - 1) as i128) as i64
    }

    /// The cursor's TAI at a running time.
    pub fn tai_at(&self, rt: i64) -> Option<i64> {
        let b = self.bounds()?;
        Some(Self::eval(&lock(&self.anchor), rt, &b))
    }

    /// The frame of `grid` a player shows at a running time, and the cursor:
    /// the player's latest frame that starts at or before the cursor.
    pub fn frame_on(&self, grid: &FrameGrid, rt: i64) -> Option<(i64, i64)> {
        let t = self.tai_at(rt)?;
        Some((grid.floor_frame(t), t))
    }

    /// The cursor's TAI at a running time, and the speed.
    pub fn cursor_tai(&self, rt: i64) -> Option<(i64, f64)> {
        let b = self.bounds()?;
        let a = *lock(&self.anchor);
        Some((Self::eval(&a, rt, &b), a.speed))
    }

    pub fn speed(&self) -> f64 {
        lock(&self.anchor).speed
    }

    /// Apply a command at "now".
    pub fn apply(&self, cmd: Command) -> Result<Applied, String> {
        self.apply_at(cmd, self.now_rt())
    }

    /// Apply a command at running time `now`. Fails, changing nothing, on a
    /// target that does not parse or a group that has no media yet.
    pub fn apply_at(&self, cmd: Command, now: i64) -> Result<Applied, String> {
        let b = self
            .bounds()
            .ok_or_else(|| "the group has no media yet".to_string())?;
        let reference = self.reference();
        let need_ref =
            || reference.ok_or_else(|| "the group's frame rate is not known yet".to_string());
        let mut a = lock(&self.anchor);
        let cur = Self::eval(&a, now, &b);
        let (tai, speed) = match cmd {
            Command::Jog { frames } => {
                let g = need_ref()?;
                (frame_centre(&g, g.floor_frame(cur) + frames), 0.0)
            }
            Command::Seek { frame, utc, tai } => {
                let to = match (frame, utc, tai) {
                    (Some(f), None, None) => {
                        let g = need_ref()?;
                        frame_centre(&g, g.floor_frame(self.origin_tai()) + f)
                    }
                    (None, Some(u), None) => parse_utc(&u).map(utc_to_tai_ns).ok_or_else(|| {
                        format!("seek: bad utc '{}', want e.g. 2026-11-24T13:21:56.080Z", u)
                    })?,
                    (None, None, Some(t)) => parse_timestamp_signed(&t).ok_or_else(|| {
                        format!("seek: bad tai '{}', want \"<seconds>:<nanoseconds>\"", t)
                    })?,
                    _ => return Err("seek: give exactly one of frame, utc, tai".to_string()),
                };
                (to, a.speed)
            }
            Command::SeekRel { ms } => (cur.saturating_add(ms * 1_000_000), a.speed),
            Command::Speed { speed } => {
                if !speed.is_finite() || speed.abs() > MAX_SPEED {
                    return Err(format!(
                        "speed: {} is outside -{} .. {}",
                        speed, MAX_SPEED, MAX_SPEED
                    ));
                }
                (cur, speed)
            }
            Command::Play => (cur, 1.0),
            Command::Pause => (cur, 0.0),
            Command::End => (b.live_end - 1, 0.0),
            Command::Live { margin_ms } => {
                let margin = margin_ms.unwrap_or(DEFAULT_LIVE_MARGIN_MS) as i64 * 1_000_000;
                (b.live_end - margin, 1.0)
            }
        };
        let anchored = Anchor {
            rt: now,
            tai: Some(tai),
            speed,
        };
        // Store the clamped position so later moves start from what is shown.
        let tai = Self::eval(&anchored, now, &b);
        *a = Anchor {
            tai: Some(tai),
            ..anchored
        };
        *lock(&self.last_cmd) = (speed == 0.0).then(|| (Instant::now(), tai));
        Ok(Applied { tai, speed })
    }

    /// The reference frame that holds `tai` (relative to the origin) and its
    /// time, once the reference grid is known.
    pub fn reference_time(&self, tai: i64) -> Option<FrameTime> {
        let g = self.reference()?;
        let origin = self.origin_on(&g)?;
        Some(FrameTime::new(&g, g.floor_frame(tai), origin))
    }

    /// The group's time domain: real TAI only when every player says so.
    pub fn time_domain(&self) -> TimeDomain {
        TimeDomain::combine(self.players().iter().map(|p| p.time_domain()))
    }

    /// A player pushed the picture for the cursor at `tai`, on the tick at
    /// running time `rt`: remember the group's latest pushed time, and if it is
    /// the target of the last paused command, how long that took (first player
    /// wins).
    pub fn note_pushed(&self, rt: i64, tai: i64) {
        {
            let mut p = lock(&self.last_pushed);
            if p.is_none_or(|(prt, _)| rt >= prt) {
                *p = Some((rt, tai));
            }
        }
        let mut last = lock(&self.last_cmd);
        if let Some((at, target)) = *last {
            if target == tai {
                self.cmd_to_push_us
                    .store(at.elapsed().as_micros() as u64, Ordering::Relaxed);
                *last = None;
            }
        }
    }

    /// The group time of the latest pushed tick.
    pub fn pushed_tai(&self) -> Option<i64> {
        lock(&self.last_pushed).map(|(_, t)| t)
    }

    fn player_json(&self, p: &PlayerStats) -> serde_json::Value {
        let grid = p.grid();
        let origin = grid.and_then(|g| self.origin_on(&g));
        let rel = |k: i64| match origin {
            Some(o) if k != UNKNOWN => k - o,
            _ => -1,
        };
        let shown = p.shown_frame.load(Ordering::Relaxed);
        let wanted = p.wanted_frame.load(Ordering::Relaxed);
        let shown_time = match (grid, origin) {
            (Some(g), Some(o)) if shown != UNKNOWN => FrameTime::new(&g, shown, o).to_json(),
            _ => serde_json::Value::Null,
        };
        serde_json::json!({
            "block": p.block_id,
            "fps": grid.map(|g| fps_string(&g)),
            "frame_ns": grid.map(|g| g.frame_ns()),
            "output_fps": p.output_rate().map(|(n, d)| format!("{}/{}", n, d)),
            "extent": p.extent().map(|(s, e)| serde_json::json!({
                "start": instant_json(s),
                "end": instant_json(e),
            })),
            "shown": shown_time,
            "shown_frame": rel(shown),
            "wanted_frame": rel(wanted),
            "in_sync": shown != UNKNOWN && shown == wanted,
            "strip_frame": p.strip_frame.load(Ordering::Relaxed),
            "strip_mismatches": p.strip_mismatches.load(Ordering::Relaxed),
            "code_utc_us": p.code_utc_us.load(Ordering::Relaxed),
            "time_checked": p.time_checked.load(Ordering::Relaxed),
            "time_errors": p.time_errors.load(Ordering::Relaxed),
            "repeated_ticks": p.repeated_ticks.load(Ordering::Relaxed),
            "skipped_frames": p.skipped_frames.load(Ordering::Relaxed),
            "misses": p.misses.load(Ordering::Relaxed),
            "late_ticks": p.late_ticks.load(Ordering::Relaxed),
            "index_segments": p.index_segments.load(Ordering::Relaxed),
            "fetched_segments": p.fetched_segments.load(Ordering::Relaxed),
            "fetches": p.fetches.load(Ordering::Relaxed),
            "fetch_errors": p.fetch_errors.load(Ordering::Relaxed),
            "last_fetch_ms": p.last_fetch_ms.load(Ordering::Relaxed),
            "cached_segments": p.cached_segments.load(Ordering::Relaxed),
            "decodes": p.decodes.load(Ordering::Relaxed),
            "last_decode_ms": p.last_decode_ms.load(Ordering::Relaxed),
        })
    }

    pub fn telemetry(&self) -> serde_json::Value {
        let now = self.now_rt();
        let bounds = self.bounds();
        let reference = self.reference();
        let players = self.players();
        let domain = TimeDomain::combine(players.iter().map(|p| p.time_domain()));
        let players_json: Vec<serde_json::Value> =
            players.iter().map(|p| self.player_json(p)).collect();
        let mut v = serde_json::json!({
            "type": "state",
            "speed": self.speed(),
            "cmd_to_push_ms": self.cmd_to_push_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "players": players_json,
            "time_domain": domain.as_str(),
            "now_utc": format_utc_ms(wall_utc_ns()),
        });
        let (Some(b), Some(g)) = (bounds, reference) else {
            v["frame"] = 0.into();
            v["total_frames"] = 0.into();
            v["frame_ns"] = 20e6.into();
            return v;
        };
        let origin = g.floor_frame(self.origin_tai());
        let time_of = |tai: i64| FrameTime::new(&g, g.floor_frame(tai), origin).to_json();
        let cursor = Self::eval(&lock(&self.anchor), now, &b);
        let behind_ms = |t: i64| ((b.live_end - t) as f64 / 1e6).round();
        v["reference_fps"] = fps_string(&g).into();
        v["frame_ns"] = g.frame_ns().into();
        v["frame"] = (g.floor_frame(cursor) - origin).into();
        v["start_frame"] = (g.floor_frame(b.start) - origin).into();
        v["total_frames"] = (g.floor_frame(b.end - 1) + 1 - origin).into();
        v["live_frame"] = (g.floor_frame(b.live_end - 1) + 1 - origin).into();
        v["tai"] = tai_string(cursor).into();
        v["cursor_tai"] = instant_json(cursor);
        v["behind_live_ms"] = behind_ms(cursor).into();
        v["cursor"] = time_of(cursor);
        v["start"] = time_of(b.start);
        v["end"] = time_of(b.end);
        // The live edge is where the last common frame ends.
        v["live_edge"] = time_of(b.live_end);
        // What the players last pushed: the picture going out now.
        if let Some(t) = self.pushed_tai() {
            let mut pushed = time_of(t);
            pushed["cursor_tai"] = tai_string(t).into();
            pushed["behind_live_ms"] = behind_ms(t).into();
            if domain == TimeDomain::Tai {
                // How old the picture is: wall clock minus capture time.
                pushed["age_ms"] = ((wall_utc_ns() - tai_to_utc_ns(t)) / 1_000_000).into();
            }
            v["pushed"] = pushed;
        }
        v
    }
}

#[cfg(test)]
mod tests;
