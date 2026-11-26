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
//! at its own output rate (`output_framerate`, by default the source's
//! rate), so a 100p angle played at 50 % on a 50p output shows every source
//! frame, and a 25p angle on a 50p output shows each frame twice.
//!
//! Frame numbers in commands and telemetry count frames of the group's
//! **reference grid**: the finest source grid in the group (highest rate; on a
//! tie the smallest phase, so the choice does not depend on which angle
//! reported first). A jog of one reference frame therefore never skips a frame
//! of any angle. Frame 0 is the reference frame that holds the group's origin,
//! the start of the earliest angle once every angle has reported its extent.
//! Commands can also address an instant as UTC or TAI, which is exact; and
//! telemetry gives the UTC and TAI of the frames it names (see [`FrameTime`]).
//! Times are only wall-clock times when every source says its index is real
//! TAI ([`TimeDomain`]).

use super::grid::FrameGrid;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use strom_types::replay::{
    ReplayCommandResult, ReplayCue, ReplayEvent, ReplayFrameTime, ReplayInstant, ReplayLiveReason,
    ReplayMark, ReplayMarkAngle, ReplayTimeRef,
};
use strom_types::tai::{
    format_calendar_ms, format_utc_ms, parse_utc, tai_to_utc_ns, utc_to_tai_ns,
};
use strom_types::tams::parse_timestamp_signed;
use strom_types::FlowId;
use tracing::warn;

/// Unknown value for the i64 atomics below.
pub const UNKNOWN: i64 = i64::MIN;

/// Longest wait for every angle's extent before frame 0 is fixed without
/// the angles that have none.
pub const ORIGIN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

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
    /// The time of a source frame that starts at `tai`, its own time (which
    /// can sit off `grid` when the source changed its frame phase). Numbered
    /// by the nearest frame of `grid`.
    pub fn of_frame_at(grid: &FrameGrid, tai: i64, origin: i64) -> Self {
        let utc_ns = tai_to_utc_ns(tai);
        let second_tai = utc_to_tai_ns(utc_ns.div_euclid(1_000_000_000) * 1_000_000_000);
        // Frames of this run that start in the second before this one.
        let before = ((tai - second_tai) as i128 * grid.fps_n as i128)
            .div_euclid(grid.fps_d as i128 * 1_000_000_000) as i64;
        Self {
            frame: grid.nearest_frame(tai) - origin,
            tai_ns: tai,
            utc_ns,
            frame_in_second: before,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self.to_api()).unwrap_or_default()
    }

    pub fn to_api(&self) -> ReplayFrameTime {
        ReplayFrameTime {
            frame: self.frame,
            utc: format_utc_ms(self.utc_ns),
            tai: tai_string(self.tai_ns),
            tai_ns: self.tai_ns.to_string(),
            tai_clock: format_calendar_ms(self.tai_ns),
            frame_in_second: self.frame_in_second,
        }
    }
}

/// An instant (not a frame): UTC, TAI, and the TAI ns as a string.
pub fn instant(tai_ns: i64) -> ReplayInstant {
    ReplayInstant {
        utc: format_utc_ms(tai_to_utc_ns(tai_ns)),
        tai: tai_string(tai_ns),
        tai_ns: tai_ns.to_string(),
    }
}

/// [`instant`] as JSON.
pub fn instant_json(tai_ns: i64) -> serde_json::Value {
    serde_json::to_value(instant(tai_ns)).unwrap_or_default()
}

/// A TAI range `[start, end)` as JSON, with its length.
fn range_json(start: i64, end: i64) -> serde_json::Value {
    serde_json::json!({
        "start": instant_json(start),
        "end": instant_json(end),
        "duration_ms": (end - start) / 1_000_000,
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

/// What a player's index knows (see `cache::Coverage`): the TAI ranges it
/// has listed, and the holes in the source it has listed from end to end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexView {
    pub listed: Vec<(i64, i64)>,
    pub gaps: Vec<(i64, i64)>,
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
    /// Absolute frame (own grid, nearest) last pushed.
    pub shown_frame: AtomicI64,
    /// The own TAI of the frame last pushed (see `decode::map_to_grid`): it
    /// can sit off the player's grid when the source changed its phase.
    pub shown_tai: AtomicI64,
    /// The last tick found the frame that holds its time.
    pub in_sync: std::sync::atomic::AtomicBool,
    /// The hole of the recording the last tick's time was in, `[start, end)`.
    gap_now: Mutex<Option<(i64, i64)>>,
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
    /// No frame rate is known: none in the index, the stream's caps or its
    /// timestamps. Nothing can be placed on the timeline until one is.
    pub frame_rate_missing: std::sync::atomic::AtomicBool,
    /// [`TimeDomain`] of the source, as u8.
    time_domain: AtomicU8,
    index_view: Mutex<IndexView>,
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
            shown_tai: AtomicI64::new(UNKNOWN),
            in_sync: std::sync::atomic::AtomicBool::new(false),
            gap_now: Mutex::new(None),
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
            frame_rate_missing: std::sync::atomic::AtomicBool::new(false),
            time_domain: AtomicU8::new(TimeDomain::Unknown as u8),
            index_view: Mutex::new(IndexView::default()),
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

    pub fn set_index_view(&self, v: IndexView) {
        *lock(&self.index_view) = v;
    }

    pub fn index_view(&self) -> IndexView {
        lock(&self.index_view).clone()
    }

    pub fn set_gap(&self, gap: Option<(i64, i64)>) {
        *lock(&self.gap_now) = gap;
    }

    pub fn gap(&self) -> Option<(i64, i64)> {
        *lock(&self.gap_now)
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

/// The start of the frame of `grid` whose start is nearest `t` (within half
/// a frame). A UTC from a client names a frame this way: the API gives frame
/// times in UTC with milliseconds, a frame starts anywhere inside its
/// millisecond (e.g. 1.164 ms past it), and taking the truncated time as
/// exact would land in the frame before.
pub fn nearest_frame_start(grid: &FrameGrid, t: i64) -> i64 {
    grid.tai_of_frame(grid.nearest_frame(t))
}

/// Frame `from + by` on `grid`, kept to one frame either side of the group's
/// range (the cursor is clamped to the range after). Client values can be
/// anything: this never overflows.
fn frame_in(grid: &FrameGrid, b: &Bounds, from: i64, by: i64) -> i64 {
    from.saturating_add(by).clamp(
        grid.floor_frame(b.start) - 1,
        grid.floor_frame(b.end - 1) + 1,
    )
}

/// Milliseconds from a client as ns, saturating.
fn ms_to_ns(ms: u64) -> i64 {
    i64::try_from(ms)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000_000)
}

/// A speed from a client, checked against the range.
fn check_speed(speed: f64) -> Result<f64, CommandError> {
    if !speed.is_finite() || speed.abs() > MAX_SPEED {
        return Err(invalid(format!(
            "speed: {} is outside -{} .. {}",
            speed, MAX_SPEED, MAX_SPEED
        )));
    }
    Ok(speed)
}

pub use strom_types::replay::ReplayEndAction as EndAction;

/// The bound of play the server holds (a cue): `[start, end)`, and what
/// happens when play reaches either end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub start: i64,
    pub end: i64,
    pub end_action: EndAction,
}

impl Region {
    fn contains(&self, tai: i64) -> bool {
        self.start <= tai && tai < self.end
    }

    fn to_api(self) -> ReplayCue {
        ReplayCue {
            in_point: instant(self.start),
            out: instant(self.end),
            end_action: self.end_action,
        }
    }
}

/// Longest speed ramp a client may ask for.
pub const MAX_RAMP_MS: u64 = 60_000;

/// A linear change of speed from the anchor's speed to `to` over
/// `duration_ns` of running time, then on at `to`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ramp {
    to: f64,
    duration_ns: i64,
}

/// Distance moved (ns of media) and the speed, `dt` after an anchor at
/// `speed`, ramping or not.
fn travel(speed: f64, ramp: Option<Ramp>, dt: i64) -> (f64, f64) {
    let t = dt as f64;
    match ramp {
        Some(r) if dt > 0 => {
            let d = r.duration_ns as f64;
            if t < d {
                let v = speed + (r.to - speed) * t / d;
                (speed * t + (r.to - speed) * t * t / (2.0 * d), v)
            } else {
                ((speed + r.to) / 2.0 * d + r.to * (t - d), r.to)
            }
        }
        _ => (speed * t, speed),
    }
}

/// Where the cursor line starts: a TAI instant, or the group's start, and
/// how it moves from there.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Anchor {
    rt: i64,
    /// Cursor TAI at `rt`; `None` = at the start of the group's range.
    tai: Option<i64>,
    /// Speed at `rt`.
    speed: f64,
    /// A speed ramp that starts at `rt`.
    ramp: Option<Ramp>,
    /// The bound of play, if a cue is held. Invariant: holds the cursor at
    /// `rt` (commands that leave it drop it).
    region: Option<Region>,
    /// Following the live edge: playing at 1x behind it (after `live`, or
    /// after catching up with it).
    live: bool,
    /// The cursor was behind the live point at `rt`: forward play that
    /// reaches it catches up and goes live.
    catch_up: bool,
}

const START_ANCHOR: Anchor = Anchor {
    rt: 0,
    tai: None,
    speed: 0.0,
    ramp: None,
    region: None,
    live: false,
    catch_up: false,
};

/// An end of bounded play that changes the motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Freeze,
    Live,
    /// Unbounded forward play reached the live point.
    CaughtUp,
}

/// Where the cursor is at a running time, and what (if anything) it ran
/// into: the pure part of the motion, the same for every caller.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Eval {
    tai: i64,
    speed: f64,
    /// Play reached an end of the region whose action changes the anchor.
    hit: Option<Hit>,
}

/// The cursor of `a` at `rt`, clamped to the media. `live_from` is where the
/// group plays from when live (the live edge minus the margin).
fn eval(a: &Anchor, rt: i64, b: &Bounds, live_from: i64) -> Eval {
    let from = a.tai.unwrap_or(b.start) as i128;
    let (moved, mut speed) = travel(a.speed, a.ramp, rt - a.rt);
    let mut pos = from + moved.round() as i128;
    let mut hit = None;
    if let Some(r) = a.region {
        let (start, end) = (r.start as i128, r.end as i128);
        if pos >= end || pos < start {
            match r.end_action {
                EndAction::Loop => pos = start + (pos - start).rem_euclid(end - start),
                EndAction::Freeze | EndAction::Live => {
                    pos = if pos >= end { end - 1 } else { start };
                    speed = 0.0;
                    hit = Some(if r.end_action == EndAction::Live {
                        Hit::Live
                    } else {
                        Hit::Freeze
                    });
                }
            }
        }
    } else if a.catch_up && !a.live && speed > 0.0 && pos >= live_from as i128 {
        hit = Some(Hit::CaughtUp);
    }
    if a.live {
        // Following live never runs past the common edge (where an angle
        // would have no picture): it holds there until the next segment.
        pos = pos.min(b.live_end as i128 - 1);
    }
    Eval {
        tai: pos.clamp(b.start as i128, (b.end - 1) as i128) as i64,
        speed,
        hit,
    }
}

pub use strom_types::replay::ReplayCommand as Command;

/// Why a command was refused. Nothing changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The command is malformed or a value is out of range (HTTP 400).
    Invalid(String),
    /// The command is valid but does not apply to the group's state now,
    /// e.g. no media yet (HTTP 409).
    Conflict(String),
}

impl CommandError {
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(m) | Self::Conflict(m) => m,
        }
    }

    /// `"invalid"` or `"conflict"`, for the WebSocket error.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::Conflict(_) => "conflict",
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

fn invalid(m: impl Into<String>) -> CommandError {
    CommandError::Invalid(m.into())
}

fn conflict(m: impl Into<String>) -> CommandError {
    CommandError::Conflict(m.into())
}

/// Parse a command from JSON. A `seq` field (the WebSocket's correlation id)
/// is ignored; any other unknown field is an error, so a typo is not
/// silently dropped.
pub fn parse_command(mut value: serde_json::Value) -> Result<Command, CommandError> {
    if let Some(o) = value.as_object_mut() {
        o.remove("seq");
    } else {
        return Err(invalid("a command is a JSON object with a \"cmd\" field"));
    }
    serde_json::from_value(value).map_err(|e| invalid(e.to_string()))
}

/// Steps of the live edge kept for the estimate.
const EDGE_STEPS: usize = 16;
/// Edge steps closer together than this (host time) count as one.
const EDGE_COALESCE_NS: i64 = 250_000_000;

/// The live edge between its steps. The index grows one segment at a time,
/// so the known edge jumps (once a second for 1 s segments) while capture
/// goes on all the time. Each step tells when (host monotonic time) media up
/// to which instant was known; the media grows at 1x, so the edge now is at
/// least `m_now + max(e_k - m_k)` over recent steps (the step seen with the
/// least delay sets the line). Kept within one step of the known edge, and
/// only while the edge grows.
#[derive(Debug, Default)]
pub struct EdgeTracker {
    steps: std::collections::VecDeque<(i64, i64)>,
}

impl EdgeTracker {
    /// The known edge `edge` at host time `mono` (ns, monotonic).
    pub fn observe(&mut self, mono: i64, edge: i64) {
        match self.steps.back() {
            Some(&(_, last)) if edge == last => return,
            // Went back: a new run or another source. Start over.
            Some(&(_, last)) if edge < last => self.steps.clear(),
            _ => {}
        }
        // Angles step one after the other: the common edge moves a few ms
        // when the first does, the rest when the last does. Steps that close
        // in time are one step, seen when it completed.
        if let Some(last) = self.steps.back_mut() {
            if mono - last.0 < EDGE_COALESCE_NS {
                *last = (mono, edge);
                return;
            }
        }
        self.steps.push_back((mono, edge));
        while self.steps.len() > EDGE_STEPS {
            self.steps.pop_front();
        }
    }

    /// Median step of the edge, once it has stepped twice.
    fn step(&self) -> Option<i64> {
        let mut d: Vec<i64> = self
            .steps
            .iter()
            .zip(self.steps.iter().skip(1))
            .map(|(a, b)| b.1 - a.1)
            .collect();
        if d.len() < 2 {
            return None;
        }
        d.sort_unstable();
        Some(d[d.len() / 2])
    }

    /// The estimated edge at `mono`, and whether the edge is growing. Not
    /// growing (fewer than three steps, or none for three step lengths): the
    /// known edge.
    pub fn estimate(&self, mono: i64, edge: i64) -> (i64, bool) {
        let (Some(step), Some(&(last_m, _))) = (self.step(), self.steps.back()) else {
            return (edge, false);
        };
        if mono - last_m > 3 * step {
            return (edge, false);
        }
        let offset = self.steps.iter().map(|&(m, e)| e - m).max().unwrap_or(0);
        ((mono + offset).clamp(edge, edge + step), true)
    }
}

/// Host monotonic time, ns since the first call.
fn mono_ns() -> i64 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_nanos() as i64
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
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub tai: i64,
    pub speed: f64,
    pub region: Option<Region>,
    /// Following the live edge.
    pub live: bool,
    /// The mark, for `mark`.
    pub mark: Option<ReplayMark>,
}

pub struct ReplayTimeline {
    anchor: Mutex<Anchor>,
    /// TAI that frame 0 holds: the group's start once every angle has
    /// reported its extent (see [`ReplayTimeline::bounds`]).
    origin_tai: AtomicI64,
    /// When the group first had an extent, for the origin's fallback.
    first_extent_at: Mutex<Option<Instant>>,
    clock_source: Mutex<Option<gst::glib::WeakRef<gst::Element>>>,
    /// Start of the stand-in running time before the flow plays; reset with
    /// the group, so it is on the same footing as the new run's running time.
    created: Mutex<Instant>,
    players: Mutex<Vec<Weak<PlayerStats>>>,
    /// Last paused-position command: when it was applied and its cursor, so a
    /// player can report how long until it pushed that instant.
    last_cmd: Mutex<Option<(Instant, i64)>>,
    pub cmd_to_push_us: AtomicU64,
    /// Running time and cursor of the latest tick any player pushed.
    last_pushed: Mutex<Option<(i64, i64)>>,
    /// The live edge's steps, for a smooth estimate.
    edge: Mutex<EdgeTracker>,
    /// Distance behind the live edge the group plays at when it goes live:
    /// the last `live` command's margin.
    live_margin_ns: AtomicI64,
    /// What the group does by itself (a bounded play ends, it goes live).
    events: tokio::sync::broadcast::Sender<ReplayEvent>,
}

/// Timelines keyed by (flow, group).
type TimelineMap = HashMap<(FlowId, String), Arc<ReplayTimeline>>;

static TIMELINES: LazyLock<Mutex<TimelineMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drop the timelines nothing else holds: their players (which hold an `Arc`
/// each) are gone, and no client is connected. Without this a stopped or
/// deleted flow's groups stayed in the map for the life of the process.
fn prune(map: &mut TimelineMap) {
    map.retain(|_, tl| Arc::strong_count(tl) > 1);
}

pub fn get_or_create(flow_id: FlowId, group: &str) -> Arc<ReplayTimeline> {
    let mut map = lock(&TIMELINES);
    prune(&mut map);
    map.entry((flow_id, group.to_string()))
        .or_insert_with(|| Arc::new(ReplayTimeline::new()))
        .clone()
}

/// The group's timeline, while a player of the group is alive (or a client
/// still holds it).
pub fn get(flow_id: FlowId, group: &str) -> Option<Arc<ReplayTimeline>> {
    let mut map = lock(&TIMELINES);
    prune(&mut map);
    map.get(&(flow_id, group.to_string())).cloned()
}

impl ReplayTimeline {
    fn new() -> Self {
        Self {
            anchor: Mutex::new(START_ANCHOR),
            origin_tai: AtomicI64::new(UNKNOWN),
            first_extent_at: Mutex::new(None),
            clock_source: Mutex::new(None),
            created: Mutex::new(Instant::now()),
            players: Mutex::new(Vec::new()),
            last_cmd: Mutex::new(None),
            cmd_to_push_us: AtomicU64::new(0),
            last_pushed: Mutex::new(None),
            edge: Mutex::new(EdgeTracker::default()),
            live_margin_ns: AtomicI64::new(DEFAULT_LIVE_MARGIN_MS as i64 * 1_000_000),
            events: tokio::sync::broadcast::channel(64).0,
        }
    }

    /// Events from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ReplayEvent> {
        self.events.subscribe()
    }

    fn emit(&self, e: ReplayEvent) {
        // No subscriber is fine.
        let _ = self.events.send(e);
    }

    /// Called by each player at build. The first player of a new run (no
    /// player registered, the previous run's having unregistered) resets the
    /// group, so a rebuilt flow can change sources, rates and range.
    pub fn register_player(&self, stats: &Arc<PlayerStats>) {
        let mut players = lock(&self.players);
        players.retain(|w| w.strong_count() > 0);
        if players.is_empty() {
            self.origin_tai.store(UNKNOWN, Ordering::Relaxed);
            *lock(&self.first_extent_at) = None;
            *lock(&self.anchor) = START_ANCHOR;
            *lock(&self.last_pushed) = None;
            *lock(&self.last_cmd) = None;
            *lock(&self.clock_source) = None;
            *lock(&self.created) = Instant::now();
        }
        players.push(Arc::downgrade(stats));
    }

    /// Called when a player shuts down (its flow stops). Its stats may live
    /// on a little in threads that are winding down; they no longer count
    /// for the group's range, grids or reset.
    pub fn unregister_player(&self, stats: &Arc<PlayerStats>) {
        let target = Arc::downgrade(stats);
        lock(&self.players).retain(|w| w.strong_count() > 0 && !w.ptr_eq(&target));
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
        lock(&self.created).elapsed().as_nanos() as i64
    }

    /// The group's range, once some angle has reported its extent.
    ///
    /// Also fixes the origin (frame 0) at the group's start, once every
    /// registered angle has reported its extent: fixed at the first extent,
    /// frame 0 depended on which angle's index answered first (frame numbers
    /// moved by hundreds between runs). An angle whose source never answers
    /// does not hold frame numbers back for ever: after [`ORIGIN_WAIT`] the
    /// origin is fixed from the angles that have.
    pub fn bounds(&self) -> Option<Bounds> {
        self.bounds_at(Instant::now())
    }

    fn bounds_at(&self, now: Instant) -> Option<Bounds> {
        let players = self.players();
        let extents: Vec<(i64, i64)> = players.iter().filter_map(|p| p.extent()).collect();
        let b = group_bounds(&extents)?;
        lock(&self.edge).observe(mono_ns(), b.live_end);
        if self.origin_tai.load(Ordering::Relaxed) == UNKNOWN {
            let first = *lock(&self.first_extent_at).get_or_insert(now);
            let all = extents.len() == players.len();
            if all || now.duration_since(first) >= ORIGIN_WAIT {
                if !all {
                    warn!(
                        "Replay: {} of {} angles have no range after {:?}; frame 0 is \
                         fixed without them",
                        players.len() - extents.len(),
                        players.len(),
                        ORIGIN_WAIT
                    );
                }
                let _ = self.origin_tai.compare_exchange(
                    UNKNOWN,
                    b.start,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        }
        Some(b)
    }

    /// TAI of the group's frame 0 (start of the earliest angle once every
    /// angle had reported its extent), or [`UNKNOWN`].
    pub fn origin_tai(&self) -> i64 {
        self.origin_tai.load(Ordering::Relaxed)
    }

    /// The absolute frame on `grid` that holds the group origin.
    pub fn origin_on(&self, grid: &FrameGrid) -> Option<i64> {
        let o = self.origin_tai();
        (o != UNKNOWN).then(|| grid.floor_frame(o))
    }

    /// The cursor at `rt`. When the motion ran into something that changes
    /// it (the end of a bounded play), the change is made here, at `rt`, and
    /// an event sent: whoever looks first (a player's tick, the state) moves
    /// the group on. The new line starts where the old one ended, so callers
    /// at nearby running times still agree.
    fn advance(&self, a: &mut Anchor, rt: i64, b: &Bounds) -> (i64, f64) {
        let live_from = self.live_target(b);
        let e = eval(a, rt, b, live_from);
        let Some(hit) = e.hit else {
            return (e.tai, e.speed);
        };
        if rt < a.rt {
            // A tick from before the last command: report, do not commit.
            return (e.tai, e.speed);
        }
        match hit {
            Hit::Freeze => {
                *a = Anchor {
                    rt,
                    tai: Some(e.tai),
                    speed: 0.0,
                    ramp: None,
                    region: a.region,
                    live: false,
                    catch_up: false,
                };
                self.emit(ReplayEvent::Ended { at: instant(e.tai) });
                (e.tai, 0.0)
            }
            Hit::Live => {
                let to = live_from.clamp(b.start, b.end - 1);
                *a = Anchor {
                    rt,
                    tai: Some(to),
                    speed: 1.0,
                    ramp: None,
                    region: None,
                    live: true,
                    catch_up: false,
                };
                self.emit(ReplayEvent::Live {
                    reason: ReplayLiveReason::EndAction,
                    at: instant(to),
                });
                (to, 1.0)
            }
            Hit::CaughtUp => {
                // Go on from where play is, at 1x: no jump back.
                *a = Anchor {
                    rt,
                    tai: Some(e.tai),
                    speed: 1.0,
                    ramp: None,
                    region: None,
                    live: true,
                    catch_up: false,
                };
                self.emit(ReplayEvent::Live {
                    reason: ReplayLiveReason::CaughtUp,
                    at: instant(e.tai),
                });
                (e.tai, 1.0)
            }
        }
    }

    /// Where `live` plays from: the live edge minus the margin.
    fn live_target(&self, b: &Bounds) -> i64 {
        b.live_end
            .saturating_sub(self.live_margin_ns.load(Ordering::Relaxed))
    }

    /// The cursor's TAI at a running time.
    pub fn tai_at(&self, rt: i64) -> Option<i64> {
        self.cursor_tai(rt).map(|(t, _)| t)
    }

    /// The frame of `grid` a player shows at a running time, and the cursor:
    /// the player's latest frame that starts at or before the cursor.
    #[cfg(test)]
    pub fn frame_on(&self, grid: &FrameGrid, rt: i64) -> Option<(i64, i64)> {
        let t = self.tai_at(rt)?;
        Some((grid.floor_frame(t), t))
    }

    /// The cursor's TAI at a running time, and the speed.
    pub fn cursor_tai(&self, rt: i64) -> Option<(i64, f64)> {
        let b = self.bounds()?;
        Some(self.advance(&mut lock(&self.anchor), rt, &b))
    }

    /// The speed now.
    pub fn speed(&self) -> f64 {
        self.cursor_tai(self.now_rt())
            .map_or(lock(&self.anchor).speed, |(_, s)| s)
    }

    /// The cue the group holds, if any.
    pub fn cue(&self) -> Option<Region> {
        lock(&self.anchor).region
    }

    /// The instant a command names. Exactly one field of `at`; frames need
    /// the reference grid.
    fn resolve(
        &self,
        at: &ReplayTimeRef,
        b: &Bounds,
        reference: Option<FrameGrid>,
        what: &str,
    ) -> Result<i64, CommandError> {
        let given = [
            at.frame.is_some(),
            at.utc.is_some(),
            at.tai.is_some(),
            at.from_live_ms.is_some(),
        ];
        if given.iter().filter(|g| **g).count() != 1 {
            return Err(invalid(format!(
                "{}: give exactly one of frame, utc, tai, from_live_ms",
                what
            )));
        }
        if let Some(f) = at.frame {
            let g = reference.ok_or_else(|| conflict("the group's frame rate is not known yet"))?;
            let origin = self
                .origin_on(&g)
                .ok_or_else(|| conflict("the group's frame 0 is not fixed yet"))?;
            return Ok(frame_centre(&g, frame_in(&g, b, origin, f)));
        }
        if let Some(u) = &at.utc {
            let t = parse_utc(u).map(utc_to_tai_ns).ok_or_else(|| {
                invalid(format!(
                    "{}: bad utc '{}', want e.g. 2026-11-24T13:21:56.080Z",
                    what, u
                ))
            })?;
            return Ok(reference.map_or(t, |g| nearest_frame_start(&g, t)));
        }
        if let Some(t) = &at.tai {
            return parse_timestamp_signed(t).ok_or_else(|| {
                invalid(format!(
                    "{}: bad tai '{}', want \"<seconds>:<nanoseconds>\"",
                    what, t
                ))
            });
        }
        let ms = at.from_live_ms.unwrap_or(0);
        Ok(b.live_end.saturating_sub(ms_to_ns(ms)))
    }

    /// Apply a command at "now".
    pub fn apply(&self, cmd: Command) -> Result<Applied, CommandError> {
        self.apply_at(cmd, self.now_rt())
    }

    /// Apply a command at running time `now`. Fails, changing nothing, on a
    /// target that does not parse or a group that has no media yet.
    pub fn apply_at(&self, cmd: Command, now: i64) -> Result<Applied, CommandError> {
        let b = self
            .bounds()
            .ok_or_else(|| conflict("the group has no media yet"))?;
        let reference = self.reference();
        let need_ref =
            || reference.ok_or_else(|| conflict("the group's frame rate is not known yet"));
        let mut a = lock(&self.anchor);
        let (cur, cur_speed) = self.advance(&mut a, now, &b);
        if let Command::Mark { label } = cmd {
            let mark = self.mark(label)?;
            return Ok(Applied {
                tai: cur,
                speed: cur_speed,
                region: a.region,
                live: a.live,
                mark: Some(mark),
            });
        }
        let mut ramp = None;
        let live = match cmd {
            Command::Live { .. } => true,
            Command::Uncue {} => a.live,
            _ => false,
        };
        // What the cue becomes: kept (if it still holds the cursor), set, or
        // dropped.
        enum Cue {
            Keep,
            Set(Region),
            Drop,
        }
        let (tai, speed, cue) = match cmd {
            Command::Jog { frames } => {
                let g = need_ref()?;
                (
                    frame_centre(&g, frame_in(&g, &b, g.floor_frame(cur), frames)),
                    0.0,
                    Cue::Keep,
                )
            }
            Command::Seek {
                frame,
                utc,
                tai,
                from_live_ms,
                speed,
            } => {
                let at = ReplayTimeRef {
                    frame,
                    utc,
                    tai,
                    from_live_ms,
                };
                let to = self.resolve(&at, &b, reference, "seek")?;
                let speed = match speed {
                    Some(s) => check_speed(s)?,
                    None => cur_speed,
                };
                (to, speed, Cue::Keep)
            }
            Command::SeekRel { ms } => (
                cur.saturating_add(ms.saturating_mul(1_000_000)),
                cur_speed,
                Cue::Keep,
            ),
            Command::Speed { speed } => (cur, check_speed(speed)?, Cue::Keep),
            Command::Ramp { to, duration_ms } => {
                let to = check_speed(to)?;
                if duration_ms > MAX_RAMP_MS {
                    return Err(invalid(format!(
                        "ramp: duration_ms {} is over {}",
                        duration_ms, MAX_RAMP_MS
                    )));
                }
                if duration_ms > 0 {
                    ramp = Some(Ramp {
                        to,
                        duration_ns: duration_ms as i64 * 1_000_000,
                    });
                    (cur, cur_speed, Cue::Keep)
                } else {
                    (cur, to, Cue::Keep)
                }
            }
            Command::Play {
                speed,
                until,
                end_action,
            } => {
                let speed = check_speed(speed.unwrap_or(1.0))?;
                if speed == 0.0 {
                    return Err(invalid("play: speed 0 is a pause"));
                }
                let forward = speed > 0.0;
                let held = a.region;
                match until {
                    Some(u) => {
                        let until = self.resolve(&u, &b, reference, "play until")?;
                        let r = if forward {
                            if until <= cur {
                                return Err(conflict("play: until is not ahead of the cursor"));
                            }
                            let start = held.map_or(cur, |r| r.start.min(cur));
                            Region {
                                start,
                                end: until,
                                end_action: EndAction::Freeze,
                            }
                        } else {
                            if until > cur {
                                return Err(conflict("play: until is not behind the cursor"));
                            }
                            let end = held.map_or(cur + 1, |r| r.end.max(cur + 1));
                            Region {
                                start: until,
                                end,
                                end_action: EndAction::Freeze,
                            }
                        };
                        let end_action = end_action
                            .or(held.map(|h| h.end_action))
                            .unwrap_or_default();
                        (cur, speed, Cue::Set(Region { end_action, ..r }))
                    }
                    None => match held {
                        Some(r) => {
                            let r = Region {
                                end_action: end_action.unwrap_or(r.end_action),
                                ..r
                            };
                            // At the end the play runs to: start again from
                            // the other end.
                            let half = reference.map_or(1, |g| (g.frame_ns() / 2.0) as i64).max(1);
                            let from = if forward && cur >= r.end - half {
                                r.start
                            } else if !forward && cur < r.start + half {
                                r.end - 1
                            } else {
                                cur
                            };
                            (from, speed, Cue::Set(r))
                        }
                        None if end_action.is_some() => {
                            return Err(invalid("play: end_action needs until or a cue"))
                        }
                        None => (cur, speed, Cue::Keep),
                    },
                }
            }
            Command::Pause {} => (cur, 0.0, Cue::Keep),
            Command::End {} => (b.live_end - 1, 0.0, Cue::Drop),
            Command::Live { margin_ms } => {
                let margin = ms_to_ns(margin_ms.unwrap_or(DEFAULT_LIVE_MARGIN_MS));
                self.live_margin_ns.store(margin, Ordering::Relaxed);
                (self.live_target(&b), 1.0, Cue::Drop)
            }
            Command::Cue {
                in_point,
                out,
                end_action,
            } => {
                let start = self.resolve(&in_point, &b, reference, "cue in")?;
                let end = self.resolve(&out, &b, reference, "cue out")?;
                if end <= start {
                    return Err(invalid("cue: out must be after in"));
                }
                if start >= b.end || end <= b.start {
                    return Err(conflict("cue: the range is outside the media"));
                }
                let r = Region {
                    start: start.max(b.start),
                    end,
                    end_action: end_action.unwrap_or_default(),
                };
                (r.start, 0.0, Cue::Set(r))
            }
            Command::Uncue {} => (cur, cur_speed, Cue::Drop),
            Command::Mark { .. } => unreachable!("handled above"),
        };
        // Clamp to the media, so later moves start from what is shown, then
        // keep the cue only if it holds the cursor.
        let tai = tai.clamp(b.start, b.end - 1);
        let region = match cue {
            Cue::Keep => a.region,
            Cue::Set(r) => Some(r),
            Cue::Drop => None,
        }
        .filter(|r| r.contains(tai));
        *a = Anchor {
            rt: now,
            tai: Some(tai),
            speed,
            ramp,
            region,
            live,
            catch_up: tai < self.live_target(&b),
        };
        *lock(&self.last_cmd) = (speed == 0.0 && ramp.is_none()).then(|| (Instant::now(), tai));
        Ok(Applied {
            tai,
            speed,
            region,
            live,
            mark: None,
        })
    }

    /// The reference frame that holds `tai` (relative to the origin) and its
    /// time, once the reference grid is known.
    pub fn reference_time(&self, tai: i64) -> Option<FrameTime> {
        let g = self.reference()?;
        let origin = self.origin_on(&g)?;
        Some(FrameTime::new(&g, g.floor_frame(tai), origin))
    }

    /// What a command did, for the API.
    pub fn result(&self, a: &Applied) -> ReplayCommandResult {
        let time = self.reference_time(a.tai);
        ReplayCommandResult {
            speed: a.speed,
            cursor_tai: instant(a.tai),
            frame: time.as_ref().map(|t| t.frame),
            time: time.map(|t| t.to_api()),
            cue: a.region.map(Region::to_api),
            live: a.live,
            mark: a.mark.clone(),
        }
    }

    /// The picture going out now: the group time of the last pushed tick, and
    /// each angle's last pushed frame.
    fn mark(&self, label: Option<String>) -> Result<ReplayMark, CommandError> {
        let t = self
            .pushed_tai()
            .ok_or_else(|| conflict("mark: no picture has gone out yet"))?;
        let time = self
            .reference_time(t)
            .ok_or_else(|| conflict("the group's frame rate is not known yet"))?;
        let angles = self
            .players()
            .iter()
            .filter_map(|p| {
                let g = p.grid()?;
                let o = self.origin_on(&g)?;
                let shown = p.shown_tai.load(Ordering::Relaxed);
                (shown != UNKNOWN).then(|| ReplayMarkAngle {
                    block: p.block_id.clone(),
                    shown: FrameTime::of_frame_at(&g, shown, o).to_api(),
                })
            })
            .collect();
        Ok(ReplayMark {
            label,
            cursor_tai: instant(t),
            time: time.to_api(),
            angles,
        })
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
        let view = p.index_view();
        let shown_tai = p.shown_tai.load(Ordering::Relaxed);
        let shown_time = match (grid, origin) {
            (Some(g), Some(o)) if shown_tai != UNKNOWN => {
                FrameTime::of_frame_at(&g, shown_tai, o).to_json()
            }
            _ => serde_json::Value::Null,
        };
        serde_json::json!({
            "block": p.block_id,
            "fps": grid.map(|g| fps_string(&g)),
            "frame_rate_missing": p.frame_rate_missing.load(Ordering::Relaxed),
            "frame_ns": grid.map(|g| g.frame_ns()),
            "output_fps": p.output_rate().map(|(n, d)| format!("{}/{}", n, d)),
            "extent": p.extent().map(|(s, e)| serde_json::json!({
                "start": instant_json(s),
                "end": instant_json(e),
            })),
            "indexed": view.listed.iter().map(|&(s, e)| range_json(s, e)).collect::<Vec<_>>(),
            "gaps": view.gaps.iter().map(|&(s, e)| range_json(s, e)).collect::<Vec<_>>(),
            "shown": shown_time,
            "shown_frame": rel(shown),
            "wanted_frame": rel(wanted),
            "in_sync": p.in_sync.load(Ordering::Relaxed),
            "in_gap": p.gap().map(|(s, e)| range_json(s, e)),
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
        let no_frame_rate: Vec<&str> = players
            .iter()
            .filter(|p| p.frame_rate_missing.load(Ordering::Relaxed))
            .map(|p| p.block_id.as_str())
            .collect();
        let mut v = serde_json::json!({
            "type": "state",
            "speed": self.speed(),
            "cmd_to_push_ms": self.cmd_to_push_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "players": players_json,
            "time_domain": domain.as_str(),
            "now_utc": format_utc_ms(wall_utc_ns()),
            "no_frame_rate": no_frame_rate,
        });
        let origin = reference.and_then(|g| self.origin_on(&g));
        let (Some(b), Some(g), Some(origin)) = (bounds, reference, origin) else {
            v["frame"] = 0.into();
            v["total_frames"] = 0.into();
            // No made-up rate: the frame period is unknown until a player has one.
            v["frame_ns"] = serde_json::Value::Null;
            return v;
        };
        let time_of = |tai: i64| FrameTime::new(&g, g.floor_frame(tai), origin).to_json();
        let (cursor, speed) = self.advance(&mut lock(&self.anchor), now, &b);
        v["speed"] = speed.into();
        let a = *lock(&self.anchor);
        v["live"] = a.live.into();
        if let Some(r) = a.ramp {
            let left = r.duration_ns - (now - a.rt);
            if left > 0 {
                v["ramp"] = serde_json::json!({
                    "from": a.speed,
                    "to": r.to,
                    "duration_ms": r.duration_ns / 1_000_000,
                    "remaining_ms": left / 1_000_000,
                });
            }
        }
        v["cue"] = serde_json::to_value(self.cue().map(Region::to_api)).unwrap_or_default();
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
        // Between the index's steps: where the edge is now, estimated.
        let (est, growing) = lock(&self.edge).estimate(mono_ns(), b.live_end);
        v["live_edge_estimate"] = instant_json(est);
        v["live_growing"] = growing.into();
        v["behind_live_estimate_ms"] = ((est - cursor) as f64 / 1e6).round().into();
        // What the players last pushed: the picture going out now.
        if let Some(t) = self.pushed_tai() {
            let mut pushed = time_of(t);
            pushed["cursor_tai"] = tai_string(t).into();
            pushed["behind_live_ms"] = behind_ms(t).into();
            pushed["behind_live_estimate_ms"] = ((est - t) as f64 / 1e6).round().into();
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
