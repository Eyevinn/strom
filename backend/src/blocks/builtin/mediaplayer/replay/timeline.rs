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

use super::grid::FrameGrid;
use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use strom_types::FlowId;
use tracing::warn;

/// Unknown value for the i64 atomics below.
pub const UNKNOWN: i64 = i64::MIN;

/// Default distance behind the live edge for the `live` command.
pub const DEFAULT_LIVE_MARGIN_MS: u64 = 1500;

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
        }
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
    /// Go to a frame (relative to the group origin), keeping the speed.
    Seek {
        frame: i64,
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
    /// the cursor is on afterwards, and the speed.
    pub fn apply(&self, cmd: Command) -> (i64, f64) {
        let now = self.now_rt();
        let Some((grid, b)) = self.bounds() else {
            return (0, self.speed());
        };
        let origin = self.origin();
        let fd = grid.frame_ns();
        let centre = |k: i64| k as f64 + 0.5;
        let mut a = lock(&self.anchor);
        let cur = Self::eval(&a, now, &grid, &b);
        let cur_frame = cur.floor() as i64;
        let (f, speed) = match cmd {
            Command::Jog { frames } => (centre(cur_frame + frames), 0.0),
            Command::Seek { frame } => (centre(origin + frame), a.speed),
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
        (frame - origin, speed)
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
        let players: Vec<serde_json::Value> = lock(&self.players)
            .iter()
            .filter_map(|w| w.upgrade())
            .map(|p| {
                serde_json::json!({
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
            v["behind_live_ms"] = (((b.live_end - f) as f64 * grid.frame_ns()) / 1e6)
                .round()
                .into();
        } else {
            v["frame"] = 0.into();
            v["total_frames"] = 0.into();
            v["frame_ns"] = 20e6.into();
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: i64 = 1_800_000_000_000_000_000;
    const S: i64 = 1_000_000_000;

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
        assert_eq!(tl.apply(Command::Seek { frame: 123 }), (123, 0.0));
        assert_eq!(tl.apply(Command::Jog { frames: -3 }), (120, 0.0));
        let (tai, _) = tl.cursor_tai(tl.now_rt()).unwrap();
        // The cursor rests at the centre of the frame.
        assert_eq!(tai, BASE + 120 * 20_000_000 + 10_000_000);
        // Clamped to the range.
        assert_eq!(tl.apply(Command::Seek { frame: -50 }).0, 0);
        assert_eq!(tl.apply(Command::End).0, 499);
    }

    #[test]
    fn live_goes_behind_the_common_edge() {
        let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE, BASE + 8 * S)]);
        let (frame, speed) = tl.apply(Command::Live {
            margin_ms: Some(1000),
        });
        assert_eq!(speed, 1.0);
        assert_eq!(frame, 8 * 50 - 50);
        // The index grows: the edge follows.
        p[1].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
        p[0].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
        assert_eq!(tl.bounds().unwrap().1.live_end - tl.origin(), 12 * 50);
        let (frame, _) = tl.apply(Command::Live { margin_ms: None });
        assert_eq!(frame, 12 * 50 - 75);
    }

    #[test]
    fn command_parses_live() {
        let c: Command = serde_json::from_str(r#"{"cmd":"live"}"#).unwrap();
        assert!(matches!(c, Command::Live { margin_ms: None }));
        let c: Command = serde_json::from_str(r#"{"cmd":"seek","frame":-5}"#).unwrap();
        assert!(matches!(c, Command::Seek { frame: -5 }));
    }
}
