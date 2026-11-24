//! Shared replay timeline.
//!
//! One timeline per (flow, replay group) owns the cursor and the speed. The
//! cursor is a position on the group's [`FrameGrid`], which is anchored on
//! TAI, so frame `k` is the same instant on every angle however their
//! recordings start, end or are segmented. The cursor is a function of the
//! flow's running time, `f(rt) = f0 + speed * (rt - rt0) / frame_period`, so
//! every player that asks for the same output tick gets the same frame and the
//! angles cannot drift apart. A command re-anchors the line at "now".
//!
//! Frame numbers in commands and telemetry are relative to the group's origin:
//! the first frame of the earliest angle when the group first saw its sources.
//! Commands can also address an instant as UTC or TAI, and telemetry gives the
//! UTC and TAI of the frames it names (see [`FrameTime`]). Times are only
//! wall-clock times when every source says its index is real TAI
//! ([`TimeDomain`]).

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
use tracing::warn;

/// Unknown value for the i64 atomics below.
pub const UNKNOWN: i64 = i64::MIN;

/// Default distance behind the live edge for the `live` command.
pub const DEFAULT_LIVE_MARGIN_MS: u64 = 1500;

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

/// The time of a frame on the group's grid, for telemetry and acks: where
/// frame `k` starts, as TAI and as UTC (TAI - [`TAI_MINUS_UTC_NS`]).
///
/// [`TAI_MINUS_UTC_NS`]: strom_types::tai::TAI_MINUS_UTC_NS
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameTime {
    /// Frame relative to the group origin.
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

    /// JSON for the control socket. `tai_ns` is a string: it does not fit in
    /// a JavaScript number.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame,
            "utc": format_utc_ms(self.utc_ns),
            "tai": format!(
                "{}:{}",
                self.tai_ns.div_euclid(1_000_000_000),
                self.tai_ns.rem_euclid(1_000_000_000)
            ),
            "tai_ns": self.tai_ns.to_string(),
            "tai_clock": format_calendar_ms(self.tai_ns),
            "frame_in_second": self.frame_in_second,
        })
    }
}

/// UTC ns now, from the host's wall clock.
fn wall_utc_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

/// The frame most players pushed last (ties: the earliest). Players report
/// at their own ticks, so they can be a frame apart in one sample.
fn consensus_frame(frames: &[i64]) -> Option<i64> {
    let mut sorted: Vec<i64> = frames.iter().copied().filter(|f| *f != UNKNOWN).collect();
    sorted.sort_unstable();
    let mut best: Option<(i64, usize)> = None;
    for run in sorted.chunk_by(|a, b| a == b) {
        if best.is_none_or(|(_, n)| run.len() > n) {
            best = Some((run[0], run.len()));
        }
    }
    best.map(|(f, _)| f)
}

/// State and telemetry of one player in the group.
pub struct PlayerStats {
    pub block_id: String,
    /// TAI extent of the player's source, `[start, end)`.
    pub start_tai: AtomicI64,
    pub end_tai: AtomicI64,
    /// Absolute frame last pushed.
    pub shown_frame: AtomicI64,
    /// Frame number read from the pushed picture's bitstrip (diagnostics).
    pub strip_frame: AtomicI64,
    /// Pushed pictures whose bitstrip differs from their timeline frame.
    pub strip_mismatches: AtomicU64,
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
            shown_frame: AtomicI64::new(UNKNOWN),
            strip_frame: AtomicI64::new(-1),
            strip_mismatches: AtomicU64::new(0),
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
}

#[derive(Clone, Copy)]
struct Anchor {
    rt: i64,
    /// Cursor in absolute frames on the grid; -inf = "at the start".
    f: f64,
    speed: f64,
}

/// Operator commands.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Pause and move by a number of frames.
    Jog {
        frames: i64,
    },
    /// Go to a frame (relative to the group origin), or to the frame that
    /// holds an instant given as UTC (RFC 3339, e.g.
    /// `2026-11-24T13:21:56.080Z`) or TAI (TAMS `"<s>:<ns>"`), keeping the
    /// speed. Exactly one of the three.
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
    /// Go to the last frame and pause.
    End,
    /// Play at 1x from the live edge minus a margin (default 1.5 s).
    Live {
        #[serde(default)]
        margin_ms: Option<u64>,
    },
}

/// The group's playable range in absolute frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// First frame of the earliest angle.
    pub start: i64,
    /// One past the last frame of the latest angle.
    pub end: i64,
    /// One past the last frame every angle has: the live edge.
    pub live_end: i64,
}

/// Bounds from each player's TAI extent. Angles are aligned on TAI: the range
/// is the union of what the angles cover, the live edge where all have media.
pub fn group_bounds(grid: &FrameGrid, extents: &[(i64, i64)]) -> Option<Bounds> {
    let mut it = extents
        .iter()
        .map(|(s, e)| (grid.nearest_frame(*s), grid.nearest_frame(*e)));
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

pub struct ReplayTimeline {
    anchor: Mutex<Anchor>,
    grid: Mutex<Option<FrameGrid>>,
    /// Absolute frame shown as 0 in commands and telemetry.
    origin: AtomicI64,
    clock_source: Mutex<Option<gst::glib::WeakRef<gst::Element>>>,
    created: Instant,
    players: Mutex<Vec<Weak<PlayerStats>>>,
    /// Last paused-position command: when it was applied and its frame, so a
    /// player can report how long until it pushed that frame.
    last_cmd: Mutex<Option<(Instant, i64)>>,
    pub cmd_to_push_us: AtomicU64,
}

/// Timelines keyed by (flow, replay group).
type TimelineMap = HashMap<(FlowId, String), Arc<ReplayTimeline>>;

static TIMELINES: LazyLock<Mutex<TimelineMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn get_or_create(flow_id: FlowId, group: &str) -> Arc<ReplayTimeline> {
    let mut map = TIMELINES.lock().unwrap_or_else(|p| p.into_inner());
    map.entry((flow_id, group.to_string()))
        .or_insert_with(|| Arc::new(ReplayTimeline::new()))
        .clone()
}

pub fn get(flow_id: FlowId, group: &str) -> Option<Arc<ReplayTimeline>> {
    TIMELINES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(flow_id, group.to_string()))
        .cloned()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ReplayTimeline {
    fn new() -> Self {
        Self {
            anchor: Mutex::new(Anchor {
                rt: 0,
                f: f64::NEG_INFINITY,
                speed: 0.0,
            }),
            grid: Mutex::new(None),
            origin: AtomicI64::new(UNKNOWN),
            clock_source: Mutex::new(None),
            created: Instant::now(),
            players: Mutex::new(Vec::new()),
            last_cmd: Mutex::new(None),
            cmd_to_push_us: AtomicU64::new(0),
        }
    }

    /// Called by each player at build. The first player of a new run resets
    /// the grid, so a rebuilt flow can change source or frame rate.
    pub fn register_player(&self, stats: &Arc<PlayerStats>) {
        let mut players = lock(&self.players);
        players.retain(|w| w.strong_count() > 0);
        if players.is_empty() {
            *lock(&self.grid) = None;
            self.origin.store(UNKNOWN, Ordering::Relaxed);
            *lock(&self.anchor) = Anchor {
                rt: 0,
                f: f64::NEG_INFINITY,
                speed: 0.0,
            };
        }
        players.push(Arc::downgrade(stats));
    }

    /// Set the group's grid from the first source that knows its frame rate,
    /// anchored at `anchor_tai_ns`. Returns the group's grid, which may differ
    /// from the asked rate if another angle came first.
    pub fn ensure_grid(&self, fps_n: u32, fps_d: u32, anchor_tai_ns: i64) -> Option<FrameGrid> {
        let mut g = lock(&self.grid);
        if let Some(grid) = *g {
            if (grid.fps_n as u64) * (fps_d as u64) != (fps_n as u64) * (grid.fps_d as u64) {
                warn!(
                    "Replay group: source at {}/{} fps in a {}/{} fps group",
                    fps_n, fps_d, grid.fps_n, grid.fps_d
                );
            }
            return Some(grid);
        }
        *g = FrameGrid::new(fps_n, fps_d, anchor_tai_ns);
        *g
    }

    pub fn grid(&self) -> Option<FrameGrid> {
        *lock(&self.grid)
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

    fn extents(&self) -> Vec<(i64, i64)> {
        lock(&self.players)
            .iter()
            .filter_map(|w| w.upgrade())
            .filter_map(|p| p.extent())
            .collect()
    }

    /// The group's grid and range, once some angle has reported its extent.
    pub fn bounds(&self) -> Option<(FrameGrid, Bounds)> {
        let grid = self.grid()?;
        let b = group_bounds(&grid, &self.extents())?;
        let _ =
            self.origin
                .compare_exchange(UNKNOWN, b.start, Ordering::Relaxed, Ordering::Relaxed);
        Some((grid, b))
    }

    pub fn origin(&self) -> i64 {
        self.origin.load(Ordering::Relaxed)
    }

    fn eval(a: &Anchor, rt: i64, grid: &FrameGrid, b: &Bounds) -> f64 {
        let f = a.f + a.speed * (rt - a.rt) as f64 / grid.frame_ns();
        // Frame centres, so float error never floors to a neighbour.
        f.clamp(
            b.start as f64 + 0.5,
            (b.end as f64 - 0.5).max(b.start as f64 + 0.5),
        )
    }

    /// The absolute frame under the cursor at a running time.
    pub fn frame_at(&self, rt: i64) -> Option<i64> {
        let (grid, b) = self.bounds()?;
        let a = *lock(&self.anchor);
        Some(Self::eval(&a, rt, &grid, &b).floor() as i64)
    }

    /// The cursor's TAI at a running time, and the speed.
    pub fn cursor_tai(&self, rt: i64) -> Option<(i64, f64)> {
        let (grid, b) = self.bounds()?;
        let a = *lock(&self.anchor);
        let f = Self::eval(&a, rt, &grid, &b);
        let k = f.floor() as i64;
        let tai = grid.tai_of_frame(k) + ((f - k as f64) * grid.frame_ns()) as i64;
        Some((tai, a.speed))
    }

    pub fn speed(&self) -> f64 {
        lock(&self.anchor).speed
    }

    /// Apply a command at "now". Returns the frame (relative to the origin)
    /// the cursor is on afterwards, and the speed. Fails, changing nothing,
    /// on a seek target that does not parse.
    pub fn apply(&self, cmd: Command) -> Result<(i64, f64), String> {
        let now = self.now_rt();
        let Some((grid, b)) = self.bounds() else {
            return Ok((0, self.speed()));
        };
        let origin = self.origin();
        let seek_to = match &cmd {
            Command::Seek { frame, utc, tai } => {
                Some(seek_target(&grid, origin, *frame, utc, tai)?)
            }
            _ => None,
        };
        let fd = grid.frame_ns();
        let centre = |k: i64| k as f64 + 0.5;
        let mut a = lock(&self.anchor);
        let cur = Self::eval(&a, now, &grid, &b);
        let cur_frame = cur.floor() as i64;
        let (f, speed) = match cmd {
            Command::Jog { frames } => (centre(cur_frame + frames), 0.0),
            Command::Seek { .. } => (centre(seek_to.unwrap_or(cur_frame)), a.speed),
            Command::SeekRel { ms } => (cur + ms as f64 * 1e6 / fd, a.speed),
            Command::Speed { speed } => {
                if speed == 0.0 {
                    (centre(cur_frame), 0.0)
                } else {
                    (cur, speed.clamp(-8.0, 8.0))
                }
            }
            Command::Play => (cur, 1.0),
            Command::Pause => (centre(cur_frame), 0.0),
            Command::End => (b.end as f64 - 0.5, 0.0),
            Command::Live { margin_ms } => {
                let margin = margin_ms.unwrap_or(DEFAULT_LIVE_MARGIN_MS) as f64 * 1e6 / fd;
                (b.live_end as f64 - margin, 1.0)
            }
        };
        let anchored = Anchor { rt: now, f, speed };
        // Store the clamped position so later moves start from what is shown.
        let f = Self::eval(&anchored, now, &grid, &b);
        *a = Anchor { f, ..anchored };
        let frame = f.floor() as i64;
        *lock(&self.last_cmd) = (speed == 0.0).then(|| (Instant::now(), frame));
        Ok((frame - origin, speed))
    }

    /// UTC/TAI of a frame relative to the origin, once the grid is known.
    pub fn frame_time(&self, rel_frame: i64) -> Option<FrameTime> {
        let (grid, _) = self.bounds()?;
        let origin = self.origin();
        Some(FrameTime::new(&grid, origin + rel_frame, origin))
    }

    /// The group's time domain: real TAI only when every player says so.
    pub fn time_domain(&self) -> TimeDomain {
        TimeDomain::combine(
            lock(&self.players)
                .iter()
                .filter_map(|w| w.upgrade())
                .map(|p| p.time_domain()),
        )
    }

    /// A player pushed absolute `frame`: if it is the target of the last
    /// command, record how long that took (first player wins).
    pub fn note_pushed(&self, frame: i64) {
        let mut last = lock(&self.last_cmd);
        if let Some((at, target)) = *last {
            if target == frame {
                self.cmd_to_push_us
                    .store(at.elapsed().as_micros() as u64, Ordering::Relaxed);
                *last = None;
            }
        }
    }

    pub fn telemetry(&self) -> serde_json::Value {
        let now = self.now_rt();
        let bounds = self.bounds();
        let origin = self.origin();
        let rel = |k: i64| if k == UNKNOWN { -1 } else { k - origin };
        let grid = bounds.map(|(g, _)| g);
        let time_of = |k: i64| match grid {
            Some(g) if k != UNKNOWN && origin != UNKNOWN => FrameTime::new(&g, k, origin).to_json(),
            _ => serde_json::Value::Null,
        };
        let players = lock(&self.players)
            .iter()
            .filter_map(|w| w.upgrade())
            .collect::<Vec<_>>();
        let shown: Vec<i64> = players
            .iter()
            .map(|p| p.shown_frame.load(Ordering::Relaxed))
            .collect();
        let domain = TimeDomain::combine(players.iter().map(|p| p.time_domain()));
        let players: Vec<serde_json::Value> = players
            .iter()
            .zip(&shown)
            .map(|(p, &shown)| {
                serde_json::json!({
                    "shown": time_of(shown),
                    "block": p.block_id,
                    "shown_frame": rel(p.shown_frame.load(Ordering::Relaxed)),
                    "strip_frame": p.strip_frame.load(Ordering::Relaxed),
                    "strip_mismatches": p.strip_mismatches.load(Ordering::Relaxed),
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
            })
            .collect();
        let mut v = serde_json::json!({
            "type": "state",
            "speed": self.speed(),
            "cmd_to_push_ms": self.cmd_to_push_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "players": players,
            "time_domain": domain.as_str(),
            "now_utc": format_utc_ms(wall_utc_ns()),
        });
        if let Some((grid, b)) = bounds {
            let a = *lock(&self.anchor);
            let f = Self::eval(&a, now, &grid, &b).floor() as i64;
            let tai = grid.tai_of_frame(f);
            v["frame"] = (f - origin).into();
            v["start_frame"] = (b.start - origin).into();
            v["total_frames"] = (b.end - origin).into();
            v["live_frame"] = (b.live_end - origin).into();
            v["frame_ns"] = grid.frame_ns().into();
            v["tai"] = format!(
                "{}:{}",
                tai.div_euclid(1_000_000_000),
                tai.rem_euclid(1_000_000_000)
            )
            .into();
            let behind_ms = |k: i64| (((b.live_end - k) as f64 * grid.frame_ns()) / 1e6).round();
            v["behind_live_ms"] = behind_ms(f).into();
            v["cursor"] = time_of(f);
            v["start"] = time_of(b.start);
            v["end"] = time_of(b.end);
            // The live edge is where the last common frame ends.
            v["live_edge"] = time_of(b.live_end);
            // What the players last pushed: the picture going out now.
            if let Some(k) = consensus_frame(&shown) {
                let mut pushed = time_of(k);
                pushed["behind_live_ms"] = behind_ms(k).into();
                if domain == TimeDomain::Tai {
                    // How old the picture is: wall clock minus capture time.
                    let utc = tai_to_utc_ns(grid.tai_of_frame(k));
                    pushed["age_ms"] = ((wall_utc_ns() - utc) / 1_000_000).into();
                }
                v["pushed"] = pushed;
            }
        } else {
            v["frame"] = 0.into();
            v["total_frames"] = 0.into();
            v["frame_ns"] = 20e6.into();
        }
        v
    }
}

/// The absolute frame a seek addresses.
fn seek_target(
    grid: &FrameGrid,
    origin: i64,
    frame: Option<i64>,
    utc: &Option<String>,
    tai: &Option<String>,
) -> Result<i64, String> {
    match (frame, utc, tai) {
        (Some(f), None, None) => Ok(origin + f),
        (None, Some(u), None) => parse_utc(u)
            .map(|ns| grid.floor_frame(utc_to_tai_ns(ns)))
            .ok_or_else(|| format!("seek: bad utc '{}', want e.g. 2026-11-24T13:21:56.080Z", u)),
        (None, None, Some(t)) => parse_timestamp_signed(t)
            .map(|ns| grid.floor_frame(ns))
            .ok_or_else(|| format!("seek: bad tai '{}', want \"<seconds>:<nanoseconds>\"", t)),
        _ => Err("seek: give exactly one of frame, utc, tai".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: i64 = 1_800_000_000_000_000_000;
    const S: i64 = 1_000_000_000;

    fn seek(frame: i64) -> Command {
        Command::Seek {
            frame: Some(frame),
            utc: None,
            tai: None,
        }
    }

    #[test]
    fn angles_align_on_tai_not_on_segments() {
        let grid = FrameGrid::new(50, 1, BASE).unwrap();
        let k0 = grid.floor_frame(BASE);
        // cam1 covers 0..300 s, cam2 starts 2 s later and ends 1 s earlier.
        let b = group_bounds(
            &grid,
            &[(BASE, BASE + 300 * S), (BASE + 2 * S, BASE + 299 * S)],
        )
        .unwrap();
        assert_eq!(b.start, k0);
        assert_eq!(b.end, k0 + 300 * 50);
        assert_eq!(b.live_end, k0 + 299 * 50);
        assert!(group_bounds(&grid, &[]).is_none());
    }

    fn timeline_with(extents: &[(i64, i64)]) -> (ReplayTimeline, Vec<Arc<PlayerStats>>) {
        let tl = ReplayTimeline::new();
        let mut keep = Vec::new();
        for (i, (s, e)) in extents.iter().enumerate() {
            let p = Arc::new(PlayerStats::new(&format!("cam{}", i)));
            p.start_tai.store(*s, Ordering::Relaxed);
            p.end_tai.store(*e, Ordering::Relaxed);
            tl.register_player(&p);
            keep.push(p);
        }
        tl.ensure_grid(50, 1, BASE).unwrap();
        (tl, keep)
    }

    #[test]
    fn commands_use_origin_relative_frames() {
        let (tl, _p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE + S, BASE + 10 * S)]);
        let k0 = tl.grid().unwrap().floor_frame(BASE);
        let rt = tl.now_rt();
        // Starts at the first frame.
        assert_eq!(tl.frame_at(rt), Some(k0));
        assert_eq!(tl.origin(), k0);
        assert_eq!(tl.apply(seek(123)).unwrap(), (123, 0.0));
        assert_eq!(tl.apply(Command::Jog { frames: -3 }).unwrap(), (120, 0.0));
        let (tai, _) = tl.cursor_tai(tl.now_rt()).unwrap();
        // The cursor rests at the centre of the frame.
        assert_eq!(tai, BASE + 120 * 20_000_000 + 10_000_000);
        // Clamped to the range.
        assert_eq!(tl.apply(seek(-50)).unwrap().0, 0);
        assert_eq!(tl.apply(Command::End).unwrap().0, 499);
    }

    #[test]
    fn live_goes_behind_the_common_edge() {
        let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE, BASE + 8 * S)]);
        let (frame, speed) = tl
            .apply(Command::Live {
                margin_ms: Some(1000),
            })
            .unwrap();
        assert_eq!(speed, 1.0);
        assert_eq!(frame, 8 * 50 - 50);
        // The index grows: the edge follows.
        p[1].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
        p[0].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
        assert_eq!(tl.bounds().unwrap().1.live_end - tl.origin(), 12 * 50);
        let (frame, _) = tl.apply(Command::Live { margin_ms: None }).unwrap();
        assert_eq!(frame, 12 * 50 - 75);
    }

    #[test]
    fn command_parses_live() {
        let c: Command = serde_json::from_str(r#"{"cmd":"live"}"#).unwrap();
        assert!(matches!(c, Command::Live { margin_ms: None }));
        let c: Command = serde_json::from_str(r#"{"cmd":"seek","frame":-5}"#).unwrap();
        assert!(matches!(
            c,
            Command::Seek {
                frame: Some(-5),
                utc: None,
                tai: None
            }
        ));
    }

    /// TAI start of the spike's live ingest (off the 50 fps second grid).
    const INGEST: i64 = 1_795_526_553_080_598_792;

    fn one_player(base: i64, domain: TimeDomain) -> (ReplayTimeline, Arc<PlayerStats>) {
        let tl = ReplayTimeline::new();
        let p = Arc::new(PlayerStats::new("cam1"));
        p.start_tai.store(base, Ordering::Relaxed);
        p.end_tai.store(base + 60 * S, Ordering::Relaxed);
        p.set_time_domain(domain);
        tl.register_player(&p);
        tl.ensure_grid(50, 1, base).unwrap();
        (tl, p)
    }

    fn seek_utc(utc: &str) -> Command {
        Command::Seek {
            frame: None,
            utc: Some(utc.to_string()),
            tai: None,
        }
    }

    #[test]
    fn frame_time_is_utc_of_the_frame_start() {
        let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
        let t = tl.frame_time(0).unwrap();
        assert_eq!(t.tai_ns, INGEST);
        assert_eq!(t.utc_ns, INGEST - 37 * S);
        assert_eq!(format_utc_ms(t.utc_ns), "2026-11-24T13:21:56.080Z");
        // Frames start at .000598792, .020598792, ... in each second.
        assert_eq!(t.frame_in_second, 4);
        let j = t.to_json();
        assert_eq!(j["tai"], "1795526553:80598792");
        assert_eq!(j["tai_ns"], "1795526553080598792");
        assert_eq!(j["tai_clock"], "2026-11-24T13:22:33.080");
        assert_eq!(tl.frame_time(45).unwrap().frame_in_second, 49);
        assert_eq!(tl.frame_time(46).unwrap().frame_in_second, 0);
    }

    #[test]
    fn seek_by_utc_or_tai() {
        let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
        // 13:22:00.000 UTC lies in the frame that starts at 13:21:59.980598792.
        assert_eq!(
            tl.apply(seek_utc("2026-11-24T13:22:00.000Z")).unwrap(),
            (195, 0.0)
        );
        assert_eq!(
            format_utc_ms(tl.frame_time(195).unwrap().utc_ns),
            "2026-11-24T13:21:59.980Z"
        );
        assert_eq!(tl.apply(seek(0)).unwrap().0, 0);
        assert_eq!(
            tl.apply(seek_utc("2026-11-24T14:22:00.000+01:00"))
                .unwrap()
                .0,
            195
        );
        let by_tai = Command::Seek {
            frame: None,
            utc: None,
            tai: Some("1795526557:0".to_string()),
        };
        assert_eq!(tl.apply(by_tai).unwrap().0, 195);
        // Bad input changes nothing.
        assert!(tl.apply(seek_utc("13:22:00")).is_err());
        let both = Command::Seek {
            frame: Some(1),
            utc: Some("2026-11-24T13:22:00Z".to_string()),
            tai: None,
        };
        assert!(tl.apply(both).is_err());
        assert_eq!(tl.frame_at(tl.now_rt()), Some(tl.origin() + 195));
        let c: Command =
            serde_json::from_str(r#"{"cmd":"seek","utc":"2026-11-24T13:22:00Z"}"#).unwrap();
        assert!(matches!(
            c,
            Command::Seek {
                frame: None,
                utc: Some(_),
                tai: None
            }
        ));
    }

    #[test]
    fn telemetry_gives_utc_of_the_pushed_frame() {
        let (tl, p) = one_player(INGEST, TimeDomain::Tai);
        tl.apply(seek(195)).unwrap();
        // Nothing pushed yet: no "pushed" time.
        assert!(tl.telemetry().get("pushed").is_none());
        // The player pushed a different frame from the cursor: "pushed" is
        // what went out, not what was asked for.
        p.shown_frame.store(tl.origin() + 190, Ordering::Relaxed);
        let v = tl.telemetry();
        assert_eq!(v["time_domain"], "tai");
        assert_eq!(v["cursor"]["frame"], 195);
        assert_eq!(v["cursor"]["utc"], "2026-11-24T13:21:59.980Z");
        assert_eq!(v["pushed"]["frame"], 190);
        assert_eq!(v["pushed"]["utc"], "2026-11-24T13:21:59.880Z");
        assert_eq!(v["pushed"]["frame_in_second"], 44);
        assert_eq!(v["pushed"]["behind_live_ms"], (3000 - 190) as f64 * 20.0);
        // Captured in the past, so the picture's age is positive.
        assert!(v["pushed"]["age_ms"].as_i64().unwrap() > 0);
        assert_eq!(v["players"][0]["shown"]["utc"], "2026-11-24T13:21:59.880Z");
        assert_eq!(v["start"]["utc"], "2026-11-24T13:21:56.080Z");
        assert_eq!(v["live_edge"]["utc"], "2026-11-24T13:22:56.080Z");
        assert_eq!(v["live_edge"]["frame"], 3000);
        assert!(v["now_utc"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn unanchored_source_still_shows_its_times() {
        let (tl, p) = one_player(BASE, TimeDomain::Unanchored);
        p.shown_frame
            .store(tl.bounds().unwrap().1.start, Ordering::Relaxed);
        let v = tl.telemetry();
        assert_eq!(v["time_domain"], "unanchored");
        assert_eq!(v["pushed"]["utc"], format_utc_ms(BASE - 37 * S));
        // No wall clock to compare with.
        assert!(v["pushed"].get("age_ms").is_none());
    }

    #[test]
    fn group_domain_needs_every_angle() {
        use TimeDomain::*;
        assert_eq!(TimeDomain::combine([Tai, Tai]), Tai);
        assert_eq!(TimeDomain::combine([Tai, Unknown]), Unknown);
        assert_eq!(TimeDomain::combine([Tai, Unanchored, Unknown]), Unanchored);
        assert_eq!(TimeDomain::combine([]), Unknown);
    }

    #[test]
    fn pushed_frame_is_the_majority() {
        assert_eq!(consensus_frame(&[7, 8, 8, 7, 8]), Some(8));
        assert_eq!(consensus_frame(&[8, 7]), Some(7));
        assert_eq!(consensus_frame(&[UNKNOWN, 5]), Some(5));
        assert_eq!(consensus_frame(&[UNKNOWN]), None);
    }
}
