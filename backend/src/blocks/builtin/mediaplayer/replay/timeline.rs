//! Shared replay timeline.
//!
//! One timeline per (flow, replay group) owns the cursor and the speed. **The
//! group shares time, not frame numbers**: the cursor `t` is an instant on the
//! group's time axis (TAI ns, each angle moved by its `offset_ms`), a function
//! of the flow's running time, `T(rt) = T0 + speed * (rt - rt0)`, so every
//! player that asks for the same running time gets the same instant and the
//! angles cannot drift apart. A command re-anchors the line at "now".
//!
//! Each player has its own [`FrameGrid`] (its source's rate and phase) and
//! shows, at group time `T`, **its latest frame that starts at or before `T`**
//! (sample and hold: the picture that camera had at `T`). Each player outputs
//! at its own output rate (`output_framerate`, by default the source's
//! rate), so a 100p angle played at 50 % on a 50p output shows every source
//! frame, and a 25p angle on a 50p output shows each frame twice.
//!
//! The group has two grids: the **source** grid, the finest source grid in
//! the group (highest rate; on a tie the smallest phase, so the choice does
//! not depend on which angle reported first), and the **output** grid, the
//! output's rate on the source grid's phase. Timecodes ([`ReplayFrame`]) are
//! on the output grid; `jog` steps on the group's `jog_grid`. The API has no
//! absolute frame numbers. Times are only wall-clock times when every source
//! says its index is real TAI ([`TimeDomain`]).
//!
//! **Where running time comes from** ([`TimeSource`]): live, the flow clock
//! (`Clock`); offline (export), a frame counter that the output advances one
//! frame at a time, as fast as downstream takes the frames (`Counter`). The
//! cursor is the same function of running time in both, so the same commands
//! at the same running times give the same frames.
//!
//! Sub-modules: `stats` (one player's knowledge and counters), `edge` (the
//! live edge between its steps), `state` (the API's view: state,
//! diagnostics, discovery, and the events the group sends by itself),
//! `switch` (the checks and events of a source switch).

mod command;
mod edge;
mod state;
mod stats;
mod switch;

pub use command::{parse_command, take_client_ref, Command, CommandError, Request};
pub use edge::EdgeTracker;
pub use stats::{
    DecodeRecord, IndexView, PlayerConfig, PlayerStats, SourceIdent, StartMode, READY_TICKS,
};
#[cfg(test)]
pub(crate) use switch::tests as switch_tests;
pub use switch::{OpenedSource, SourceControl, Switch};

use super::grid::FrameGrid;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Instant;
use strom_types::replay::{
    ReplayCause, ReplayEvent, ReplayEventKind, ReplayJogGrid, ReplayLiveMarginSource, ReplayMark,
    ReplayReady, ReplayTarget,
};
use strom_types::FlowId;
use tracing::warn;

/// Unknown value for the i64 atomics.
pub const UNKNOWN: i64 = i64::MIN;

/// Longest wait for every angle's extent before the origin is fixed without
/// the angles that have none.
pub const ORIGIN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The computed live margin: the live edge's step plus this, clamped.
pub const LIVE_MARGIN_OVER_STEP_MS: i64 = 400;
pub const MIN_LIVE_MARGIN_MS: i64 = 800;
pub const MAX_LIVE_MARGIN_MS: i64 = 3000;

/// Step of the live edge assumed before it has been seen to grow (1 s
/// segments).
const DEFAULT_EDGE_STEP_NS: i64 = 1_000_000_000;

/// Slack over one edge step before a group that follows live counts as
/// having lost the edge (index polls and uploads jitter).
const LIVE_SLACK_NS: i64 = 250_000_000;

const MS: i64 = 1_000_000;

/// Where the group's running time ("now") comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSource {
    /// The flow clock: the clock source's running time (live).
    Clock,
    /// A frame counter: the running time of the frame the output is about to
    /// push, `k * frame period`, set by the player that pushes it (offline).
    /// Fetching and decoding follow it at whatever pace the output goes.
    Counter,
}

/// `counter` when the group runs on the clock.
const NO_COUNTER: i64 = i64::MIN;

/// Running time of output frame `k` at `fps_n/fps_d`, in whole ns: computed
/// exactly (rounded down), so the same `k` is the same running time in
/// every mode and on every run.
pub fn tick_rt(k: i64, fps_n: u32, fps_d: u32) -> i64 {
    (k as i128 * 1_000_000_000 * fps_d as i128 / fps_n.max(1) as i128) as i64
}

/// A clip played once: from `in_point` to `out` (exclusive) at a constant
/// forward `speed`, then the end. The bounded program of an export.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub in_point: ReplayTarget,
    pub out: ReplayTarget,
    pub speed: f64,
}

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

/// `"<s>:<ns>"`, the TAMS timestamp form (for logs).
pub fn tai_string(tai_ns: i64) -> String {
    format!(
        "{}:{:09}",
        tai_ns.div_euclid(1_000_000_000),
        tai_ns.rem_euclid(1_000_000_000)
    )
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
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
    i64::try_from(ms).unwrap_or(i64::MAX).saturating_mul(MS)
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

/// Where the cursor line starts: an instant, or the group's start, and how
/// it moves from there.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Anchor {
    rt: i64,
    /// Cursor at `rt`; `None` = at the start of the group's range.
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
    /// The cursor was behind the live point at `rt`: forward play at any
    /// speed that reaches it catches up and goes live (with `follow_live`).
    /// Play at or ahead of the live point catches up when it runs at 1x or
    /// faster (see [`eval`]).
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

impl Anchor {
    /// The ramp still running at `rt`, re-anchored there.
    fn ramp_left(&self, rt: i64) -> Option<Ramp> {
        let r = self.ramp?;
        let left = r.duration_ns - (rt - self.rt).max(0);
        (left > 0).then_some(Ramp {
            to: r.to,
            duration_ns: left,
        })
    }
}

/// An end of bounded play that changes the motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// Freeze at the end of a cue; `media_end`: the end of media that no
    /// longer grows, before the cue's.
    Freeze { media_end: bool },
    /// `end_action: live` at the end of a cue.
    Live { media_end: bool },
    /// Unbounded forward play reached the live point.
    CaughtUp,
    /// Following live, the live point moved ahead of the cursor by more than
    /// the edge's own steps explain.
    EdgeMoved,
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

/// What [`eval`] needs to know about the group besides its bounds.
#[derive(Debug, Clone, Copy)]
struct Live {
    /// Where the group plays from when live: the live edge minus the margin.
    from: i64,
    /// The same on the edge's smooth estimate (see [`EdgeTracker`]): where a
    /// group that goes live from ahead of `from` is put back to.
    smooth_from: i64,
    /// Following live, a cursor more than this behind `from` has lost the
    /// edge.
    slack: i64,
    /// The media still grows (see [`EdgeTracker::estimate`]).
    growing: bool,
    /// The group goes live by itself.
    follow: bool,
}

/// The cursor of `a` at `rt`, clamped to the media.
fn eval(a: &Anchor, rt: i64, b: &Bounds, l: Live) -> Eval {
    let from = a.tai.unwrap_or(b.start) as i128;
    let (moved, mut speed) = travel(a.speed, a.ramp, rt - a.rt);
    let mut pos = from + moved.round() as i128;
    let mut hit = None;
    if let Some(r) = a.region {
        // An out (or in) past the media ends play where the media does: on
        // media that has stopped growing, play would otherwise hold at its
        // end for good, with no `Ended` and no end action. While the media
        // grows, an out past today's end is where it will be: play holds at
        // the edge until the media reaches it.
        let start = r.start.max(b.start) as i128;
        let media_end = !l.growing && r.end > b.end;
        let r_end = if l.growing { r.end } else { r.end.min(b.end) };
        let end = (r_end as i128).max(start + 1);
        if pos >= end || pos < start {
            match r.end_action {
                EndAction::Loop => pos = start + (pos - start).rem_euclid(end - start),
                EndAction::Freeze | EndAction::Live => {
                    let at_end = pos >= end;
                    pos = if at_end { end - 1 } else { start };
                    speed = 0.0;
                    let media_end = media_end && at_end;
                    hit = Some(if r.end_action == EndAction::Live {
                        Hit::Live { media_end }
                    } else {
                        Hit::Freeze { media_end }
                    });
                }
            }
        }
    } else if !a.live && speed > 0.0 {
        // Play that started behind the live point catches up when it gets
        // there. Play at or ahead of it catches up at once if it runs at 1x
        // or faster (a ramp: and ramps towards that), since it would run into
        // the edge; slower play falls behind the edge and stays slow motion.
        let settles = a.ramp.map_or(speed, |r| r.to);
        let reaches = a.catch_up || (speed >= 1.0 && settles >= 1.0);
        if l.follow && reaches && pos >= l.from as i128 {
            hit = Some(Hit::CaughtUp);
        } else {
            // Forward play stops on the last frame every angle has (or where
            // it started, past it) until media grows.
            pos = pos.min((b.live_end as i128 - 1).max(from));
        }
    }
    if a.live && a.region.is_none() && pos < l.from as i128 - l.slack as i128 {
        // The line was drawn from an edge that lagged (an angle's index had
        // not caught up yet, e.g. right after a start): it would stay that far
        // behind for good.
        hit = Some(Hit::EdgeMoved);
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

/// The running time `el` paused at, as GStreamer keeps it while paused.
fn start_time_of(el: &gst::Element) -> Option<i64> {
    el.start_time().map(|t| t.nseconds() as i64)
}

/// Host monotonic time, ns since the first call.
fn mono_ns() -> i64 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_nanos() as i64
}

/// The group's playable range on its axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Start of the earliest angle.
    pub start: i64,
    /// End of the latest angle.
    pub end: i64,
    /// End of the angle that ends first: the live edge, where all have media.
    pub live_end: i64,
}

/// Bounds from each player's extent. Angles are aligned on the group's axis:
/// the range is the union of what the angles cover, the live edge where all
/// have media.
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

/// What a command did: the motion afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub tai: i64,
    pub speed: f64,
    pub region: Option<Region>,
    /// Following the live edge.
    pub live: bool,
    /// The ramp running after the command: from, to, duration and time left
    /// (ns).
    pub ramp: Option<(f64, f64, i64, i64)>,
    /// The mark, for `mark`.
    pub mark: Option<ReplayMark>,
    pub client_ref: Option<String>,
}

/// Lock order: `clock_source`, then `anchor`, then the others. `players`,
/// `first_extent_at`, `created`, `last_cmd`, `last_pushed`, `edge`,
/// `anchor_ref`, `cue_ref`, `follow_live_set`, `watch` and `event_seq` are
/// leaves:
/// nothing else is taken while one of them is held, except that `watch`
/// takes `edge` and `event_seq`.
pub struct ReplayTimeline {
    anchor: Mutex<Anchor>,
    /// The `client_ref` of the command that drew the anchor's line: the
    /// events the line runs into that are not about the cue (`caught_up`,
    /// `edge_moved`) carry it.
    anchor_ref: Mutex<Option<String>>,
    /// The `client_ref` of the cue the anchor holds (of the `cue` or
    /// `play {until}` that set it, or a later `play` with a ref that plays
    /// it): the cue's ends (`cue_end`, `media_end`) carry it. Goes with the
    /// cue.
    cue_ref: Mutex<Option<String>>,
    /// The group's start once every angle has reported its extent (see
    /// [`ReplayTimeline::bounds`]): numbers the bitstrip of test material.
    origin_tai: AtomicI64,
    /// When the group first had an extent, for the origin's fallback.
    first_extent_at: Mutex<Option<Instant>>,
    clock_source: Mutex<Option<ClockSource>>,
    /// Running time from the frame counter ([`TimeSource::Counter`]), or
    /// [`NO_COUNTER`] on the clock.
    counter: AtomicI64,
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
    /// The margin a `live {margin_ms}` set for the run, or [`UNKNOWN`].
    margin_set_ns: AtomicI64,
    /// `follow_live` as a command set it for the run.
    follow_live_set: Mutex<Option<bool>>,
    /// A command has been applied this run (`start_mode` only acts before).
    commanded: AtomicBool,
    /// The group became ready with `start_mode: live`: the next look at the
    /// cursor takes it live.
    start_live: AtomicBool,
    /// The flow played again after a pause while the group followed live:
    /// the next look at the cursor takes it to the live point.
    resume_live: AtomicBool,
    /// What the events last said (readiness, range).
    watch: Mutex<state::Watch>,
    /// Number of the next event.
    event_seq: Mutex<u64>,
    /// What the group does by itself (a bounded play ends, it goes live).
    events: tokio::sync::broadcast::Sender<ReplayEvent>,
}

/// The element whose clock and base time define the group's "now", and the
/// base time the group's running times are on.
struct ClockSource {
    element: gst::glib::WeakRef<gst::Element>,
    /// Base time (ns) the anchor's running times are on, once known.
    base: Option<i64>,
    /// While the flow is paused: the running time it paused at, which is
    /// "now" until it plays again.
    paused: Option<i64>,
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

/// Every group with a player alive, optionally of one flow, in a stable
/// order.
pub fn running(flow: Option<FlowId>) -> Vec<(FlowId, String, Arc<ReplayTimeline>)> {
    let mut out: Vec<_> = {
        let mut map = lock(&TIMELINES);
        prune(&mut map);
        map.iter()
            .filter(|((f, _), _)| flow.is_none_or(|w| w == *f))
            .map(|((f, g), tl)| (*f, g.clone(), Arc::clone(tl)))
            .collect()
    };
    out.retain(|(_, _, tl)| !tl.players().is_empty());
    out.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    out
}

impl ReplayTimeline {
    fn new() -> Self {
        Self {
            anchor: Mutex::new(START_ANCHOR),
            anchor_ref: Mutex::new(None),
            cue_ref: Mutex::new(None),
            origin_tai: AtomicI64::new(UNKNOWN),
            first_extent_at: Mutex::new(None),
            clock_source: Mutex::new(None),
            counter: AtomicI64::new(NO_COUNTER),
            created: Mutex::new(Instant::now()),
            players: Mutex::new(Vec::new()),
            last_cmd: Mutex::new(None),
            cmd_to_push_us: AtomicU64::new(0),
            last_pushed: Mutex::new(None),
            edge: Mutex::new(EdgeTracker::default()),
            margin_set_ns: AtomicI64::new(UNKNOWN),
            follow_live_set: Mutex::new(None),
            commanded: AtomicBool::new(false),
            start_live: AtomicBool::new(false),
            resume_live: AtomicBool::new(false),
            watch: Mutex::new(state::Watch::default()),
            event_seq: Mutex::new(0),
            events: tokio::sync::broadcast::channel(64).0,
        }
    }

    /// Events from now on.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ReplayEvent> {
        self.events.subscribe()
    }

    /// Send an event, numbered one after the previous.
    fn emit(&self, cause: ReplayCause, client_ref: Option<String>, kind: ReplayEventKind) {
        let mut seq = lock(&self.event_seq);
        *seq += 1;
        // No subscriber is fine.
        let _ = self.events.send(ReplayEvent {
            event_seq: *seq,
            cause,
            client_ref,
            kind,
        });
    }

    /// Called by each player at build. The first player of a new run (no
    /// player registered, the previous run's having unregistered) resets the
    /// group, so a rebuilt flow can change sources, rates and range.
    pub fn register_player(&self, stats: &Arc<PlayerStats>) {
        // `clock_source` and `anchor` are held across the check, the reset and
        // the registration: a command (which takes both) runs wholly before
        // the new run (and sees no player of it) or wholly after the reset.
        // Taken in the lock order; `players` is a leaf, so it is released
        // before the reset takes the other leaves.
        let mut src = lock(&self.clock_source);
        let mut anchor = lock(&self.anchor);
        let first_of_run = {
            let mut players = lock(&self.players);
            players.retain(|w| w.strong_count() > 0);
            players.is_empty()
        };
        if first_of_run {
            self.origin_tai.store(UNKNOWN, Ordering::Relaxed);
            *lock(&self.first_extent_at) = None;
            *anchor = START_ANCHOR;
            *lock(&self.anchor_ref) = None;
            *lock(&self.cue_ref) = None;
            *lock(&self.last_pushed) = None;
            *lock(&self.last_cmd) = None;
            *src = None;
            self.counter.store(NO_COUNTER, Ordering::Relaxed);
            *lock(&self.created) = Instant::now();
            *lock(&self.edge) = EdgeTracker::default();
            self.margin_set_ns.store(UNKNOWN, Ordering::Relaxed);
            *lock(&self.follow_live_set) = None;
            self.commanded.store(false, Ordering::Relaxed);
            self.start_live.store(false, Ordering::Relaxed);
            self.resume_live.store(false, Ordering::Relaxed);
            *lock(&self.watch) = state::Watch::default();
            self.cmd_to_push_us.store(0, Ordering::Relaxed);
        }
        lock(&self.players).push(Arc::downgrade(stats));
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

    /// The group's source grid (see the module docs) on its axis, once a
    /// player knows its frame rate.
    pub fn reference(&self) -> Option<FrameGrid> {
        reference_grid(self.players().iter().filter_map(|p| p.group_grid()))
    }

    /// The group's output grid: the highest output rate of its players, on
    /// the source grid's phase.
    pub fn output_grid(&self) -> Option<FrameGrid> {
        let reference = self.reference()?;
        let (n, d) = self
            .players()
            .iter()
            .filter_map(|p| p.effective_output_rate())
            .filter_map(|(n, d)| FrameGrid::new(n, d, 0))
            .reduce(|a, b| {
                if reference_grid([a, b]) == Some(a) {
                    a
                } else {
                    b
                }
            })
            .map_or((reference.fps_n, reference.fps_d), |g| (g.fps_n, g.fps_d));
        FrameGrid::new(n, d, reference.phase_ns)
    }

    /// The grid `jog` steps on.
    fn jog_grid_of(&self, which: ReplayJogGrid) -> Option<FrameGrid> {
        match which {
            ReplayJogGrid::Source => self.reference(),
            ReplayJogGrid::Output => self.output_grid(),
        }
    }

    /// `source` if any player says so, else `output` (with no player, the
    /// default).
    pub fn jog_grid(&self) -> ReplayJogGrid {
        let players = self.players();
        if !players.is_empty()
            && players
                .iter()
                .all(|p| p.config.jog_grid == ReplayJogGrid::Output)
        {
            ReplayJogGrid::Output
        } else {
            ReplayJogGrid::Source
        }
    }

    /// Whether the group goes live by itself: as the last `follow_live`
    /// command set it, else `false` if any player's property says so.
    pub fn follow_live(&self) -> bool {
        if let Some(on) = *lock(&self.follow_live_set) {
            return on;
        }
        self.players().iter().all(|p| p.config.follow_live)
    }

    /// The first player's start mode.
    fn start_mode(&self) -> StartMode {
        self.players()
            .first()
            .map_or(StartMode::Paused, |p| p.config.start_mode)
    }

    /// The live margin (ns) and where it comes from: a `live {margin_ms}` of
    /// this run, else the largest `live_margin_ms` property, else the live
    /// edge's step + 400 ms, clamped to 800 .. 3000 ms.
    pub fn live_margin(&self) -> (i64, ReplayLiveMarginSource) {
        let set = self.margin_set_ns.load(Ordering::Relaxed);
        if set != UNKNOWN {
            return (set, ReplayLiveMarginSource::Set);
        }
        if let Some(ms) = self
            .players()
            .iter()
            .filter_map(|p| p.config.live_margin_ms)
            .max()
        {
            return (ms_to_ns(ms), ReplayLiveMarginSource::Set);
        }
        let step = lock(&self.edge).step().unwrap_or(DEFAULT_EDGE_STEP_NS);
        let ms =
            (step / MS + LIVE_MARGIN_OVER_STEP_MS).clamp(MIN_LIVE_MARGIN_MS, MAX_LIVE_MARGIN_MS);
        (ms * MS, ReplayLiveMarginSource::Computed)
    }

    /// Run the group on a frame counter from running time 0 on, instead of
    /// the flow clock (see [`TimeSource`]). Called at build by an offline
    /// player, after it registered.
    pub fn use_counter(&self) {
        let _src = lock(&self.clock_source);
        let _ = self
            .counter
            .compare_exchange(NO_COUNTER, 0, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// The group's time source.
    pub fn time_source(&self) -> TimeSource {
        if self.counter.load(Ordering::Relaxed) == NO_COUNTER {
            TimeSource::Clock
        } else {
            TimeSource::Counter
        }
    }

    /// Counter time: the running time of the next frame out. No effect on a
    /// group that runs on the clock.
    pub fn set_counter(&self, rt: i64) {
        let _src = lock(&self.clock_source);
        let _ = self
            .counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                (c != NO_COUNTER).then_some(rt)
            });
    }

    /// An element in the flow whose clock and base time define "now".
    ///
    /// "Now" can jump when the source changes, most of all from the
    /// stand-in (time since the group was reset) to the flow's running time,
    /// which starts near 0 when the flow plays. The anchor is moved by the
    /// same amount, so a command given before the flow played keeps its
    /// place on the new time base.
    pub fn set_clock_source(&self, element: &gst::Element) {
        let mut src = lock(&self.clock_source);
        if self.counter.load(Ordering::Relaxed) != NO_COUNTER {
            // On the counter: the clock does not define "now".
            return;
        }
        let alive = src
            .as_ref()
            .and_then(|s| s.element.upgrade())
            .is_some_and(|e| e.current_state() == gst::State::Playing);
        if !alive {
            let before = self.rt_of(&mut src);
            *src = Some(ClockSource {
                element: element.downgrade(),
                base: None,
                paused: None,
            });
            let after = self.rt_of(&mut src);
            // Still holding `clock_source`: no command reads "now" between.
            self.shift_rt(after - before);
        }
    }

    /// The flow's base time may have changed, as a player saw it: it changes
    /// when the flow goes PAUSED -> PLAYING. A player that sees the new base
    /// before the timeline does calls this before it uses the new base.
    pub fn observe_base(&self) {
        let mut src = lock(&self.clock_source);
        let _ = self.rt_of(&mut src);
    }

    /// The flow plays (again) on base time `base` (ns), "now" being `now` on
    /// it, under the `clock_source` lock.
    ///
    /// A pause of the flow is a pause of the group: GStreamer's running time
    /// resumes where it paused (the base moves on by the time spent paused),
    /// and the cursor goes on from where it stopped, as the group's running
    /// times are left on that running time. "Now" was frozen while paused;
    /// if it was frozen at another running time than play resumes at (the
    /// element has no start time), the group's running times move by the
    /// difference. A group that followed live goes to the live point instead.
    fn resume(&self, s: &mut ClockSource, el: &gst::Element, base: i64, now: i64) {
        let rebased = s.base.replace(base).is_some_and(|old| old != base);
        let frozen = s.paused.take();
        if let Some(f) = frozen {
            let at = if rebased {
                start_time_of(el).unwrap_or(now)
            } else {
                now
            };
            self.shift_rt(at - f);
        }
        if (rebased || frozen.is_some()) && lock(&self.anchor).live {
            self.resume_live.store(true, Ordering::Relaxed);
        }
    }

    /// Run `f` with "now", holding `clock_source` so the time base cannot
    /// change between reading "now" and using it. `f` must not take
    /// `clock_source`.
    fn at_now<R>(&self, f: impl FnOnce(i64) -> R) -> R {
        let mut src = lock(&self.clock_source);
        let now = self.rt_of(&mut src);
        f(now)
    }

    /// Move every running time the group keeps by `delta`, for a new time
    /// base.
    fn shift_rt(&self, delta: i64) {
        if delta == 0 {
            return;
        }
        lock(&self.anchor).rt += delta;
        if let Some((rt, _)) = lock(&self.last_pushed).as_mut() {
            *rt += delta;
        }
    }

    /// The flow's running time now, or a monotonic stand-in before it plays;
    /// on the counter, the counter.
    pub fn now_rt(&self) -> i64 {
        self.rt_of(&mut lock(&self.clock_source))
    }

    /// "Now" from the clock source: frozen while the flow is paused, and
    /// after a pause on the new base time (see [`Self::resume`]).
    fn rt_of(&self, src: &mut Option<ClockSource>) -> i64 {
        let counter = self.counter.load(Ordering::Relaxed);
        if counter != NO_COUNTER {
            return counter;
        }
        if let Some(s) = src.as_mut() {
            if let Some(el) = s.element.upgrade() {
                if let (Some(clock), Some(base)) = (el.clock(), el.base_time()) {
                    let base = base.nseconds() as i64;
                    let now = clock.time().nseconds() as i64 - base;
                    if el.current_state() != gst::State::Playing
                        || el.pending_state() == gst::State::Paused
                    {
                        return *s
                            .paused
                            .get_or_insert_with(|| start_time_of(&el).unwrap_or(now));
                    }
                    self.resume(s, &el, base, now);
                    return now;
                }
            }
        }
        lock(&self.created).elapsed().as_nanos() as i64
    }

    /// The group's range, once some angle has reported its extent. Also
    /// sends the events for changes of readiness and range.
    ///
    /// Also fixes the origin at the group's start, once every registered
    /// angle has reported its extent, or after [`ORIGIN_WAIT`] without the
    /// angles that have not.
    pub fn bounds(&self) -> Option<Bounds> {
        self.bounds_at(Instant::now())
    }

    fn bounds_at(&self, now: Instant) -> Option<Bounds> {
        let players = self.players();
        let extents: Vec<(i64, i64)> = players.iter().filter_map(|p| p.extent()).collect();
        let b = group_bounds(&extents);
        if let Some(b) = &b {
            lock(&self.edge).observe(mono_ns(), b.live_end);
        }
        self.watch_changes(&players, b.as_ref());
        let b = b?;
        if self.origin_tai.load(Ordering::Relaxed) == UNKNOWN {
            let first = *lock(&self.first_extent_at).get_or_insert(now);
            let all = extents.len() == players.len();
            if all || now.duration_since(first) >= ORIGIN_WAIT {
                if !all {
                    warn!(
                        "Replay: {} of {} angles have no range after {:?}; the origin is \
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

    /// The group's start once every angle had reported its extent, or
    /// [`UNKNOWN`].
    pub fn origin_tai(&self) -> i64 {
        self.origin_tai.load(Ordering::Relaxed)
    }

    /// The absolute frame on `grid` that holds the group origin (the frame
    /// numbers of the test material's bitstrip count from it).
    pub fn origin_on(&self, grid: &FrameGrid) -> Option<i64> {
        let o = self.origin_tai();
        (o != UNKNOWN).then(|| grid.floor_frame(o))
    }

    /// What [`eval`] needs besides the anchor and bounds.
    fn live_params(&self, b: &Bounds) -> Live {
        let (est, growing) = lock(&self.edge).estimate(mono_ns(), b.live_end);
        let margin = self.live_margin().0;
        Live {
            from: b.live_end.saturating_sub(margin),
            smooth_from: est.saturating_sub(margin),
            slack: self.live_slack(),
            growing,
            follow: self.follow_live(),
        }
    }

    /// The cursor at `rt`. When the motion ran into something that changes
    /// it (the end of a bounded play, the live point), the change is made
    /// here, at `rt`, and an event sent: whoever looks first (a player's
    /// tick, the state) moves the group on. The new line starts where the
    /// old one ended, so callers at nearby running times still agree.
    fn advance(&self, a: &mut Anchor, rt: i64, b: &Bounds) -> (i64, f64) {
        let l = self.live_params(b);
        if rt >= a.rt && self.resume_live.swap(false, Ordering::Relaxed) && a.live {
            // The flow played again after a pause while following live.
            let to = l.from.clamp(b.start, b.end - 1);
            *a = Anchor {
                rt,
                tai: Some(to),
                speed: 1.0,
                ramp: None,
                region: None,
                live: true,
                catch_up: false,
            };
            let client_ref = lock(&self.anchor_ref).take();
            self.emit(
                ReplayCause::EdgeMoved,
                client_ref,
                ReplayEventKind::Live {
                    at: self.view().instant(to),
                },
            );
            return (to, 1.0);
        }
        if rt >= a.rt
            && self.start_live.swap(false, Ordering::Relaxed)
            && !self.commanded.load(Ordering::Relaxed)
        {
            // `start_mode: live`, and the group just became ready.
            let to = l.from.clamp(b.start, b.end - 1);
            *a = Anchor {
                rt,
                tai: Some(to),
                speed: 1.0,
                ramp: None,
                region: None,
                live: true,
                catch_up: false,
            };
            self.emit(
                ReplayCause::StartMode,
                None,
                ReplayEventKind::Live {
                    at: self.view().instant(to),
                },
            );
            return (to, 1.0);
        }
        let growing = a.region.is_some_and(|r| r.end > b.end) && l.growing;
        let e = eval(a, rt, b, Live { growing, ..l });
        let Some(hit) = e.hit else {
            return (e.tai, e.speed);
        };
        if rt < a.rt {
            // A tick from before the last command: report, do not commit.
            return (e.tai, e.speed);
        }
        let client_ref = match hit {
            // The cue stays: so does its ref, for the next play of it.
            Hit::Freeze { .. } => lock(&self.cue_ref).clone(),
            // The cue is dropped.
            Hit::Live { .. } => lock(&self.cue_ref).take(),
            Hit::CaughtUp | Hit::EdgeMoved => lock(&self.anchor_ref).take(),
        };
        let (to, speed, live, region, cause, kind_live) = match hit {
            Hit::Freeze { media_end } => (
                e.tai,
                0.0,
                false,
                a.region,
                if media_end {
                    ReplayCause::MediaEnd
                } else {
                    ReplayCause::CueEnd
                },
                false,
            ),
            Hit::Live { media_end } => (
                l.from.clamp(b.start, b.end - 1),
                1.0,
                true,
                None,
                if media_end {
                    ReplayCause::MediaEnd
                } else {
                    ReplayCause::CueEnd
                },
                true,
            ),
            // Jump forward to the live point, as a new `live` would.
            Hit::EdgeMoved => (
                l.from.clamp(b.start, b.end - 1),
                1.0,
                true,
                None,
                ReplayCause::EdgeMoved,
                true,
            ),
            // Go on from where play is, at 1x: no jump back. Unless play is
            // well ahead of the live point (it started there, e.g. `end`
            // then `play`): live from there would hold at the edge on every
            // step, so it goes back to the live point on the smooth edge.
            Hit::CaughtUp => {
                let to = if l.growing && e.tai > l.smooth_from.saturating_add(LIVE_SLACK_NS) {
                    l.smooth_from.max(l.from)
                } else {
                    e.tai
                };
                (
                    to.clamp(b.start, (b.live_end - 1).max(b.start)),
                    1.0,
                    true,
                    None,
                    ReplayCause::CaughtUp,
                    true,
                )
            }
        };
        *a = Anchor {
            rt,
            tai: Some(to),
            speed,
            ramp: None,
            region,
            live,
            catch_up: false,
        };
        let at = self.view().instant(to);
        let kind = if kind_live {
            ReplayEventKind::Live { at }
        } else {
            ReplayEventKind::Ended { at }
        };
        self.emit(cause, client_ref, kind);
        (to, speed)
    }

    /// How far behind the live point a group that follows live may fall
    /// before it jumps forward: one step of the known edge (it grows a
    /// segment at a time, so a cursor on a good line is up to one step
    /// behind), plus [`LIVE_SLACK_NS`].
    fn live_slack(&self) -> i64 {
        lock(&self.edge).step().unwrap_or(DEFAULT_EDGE_STEP_NS) + LIVE_SLACK_NS
    }

    /// Whether the group's media still grows: its live edge has stepped
    /// recently.
    fn growing(&self, b: &Bounds) -> bool {
        lock(&self.edge).estimate(mono_ns(), b.live_end).1
    }

    /// Where `live` plays from: the live edge minus the margin.
    fn live_target(&self, b: &Bounds) -> i64 {
        b.live_end.saturating_sub(self.live_margin().0)
    }

    /// The cursor at a running time.
    pub fn tai_at(&self, rt: i64) -> Option<i64> {
        self.cursor_tai(rt).map(|(t, _)| t)
    }

    /// The cursor at a running time, and the speed.
    pub fn cursor_tai(&self, rt: i64) -> Option<(i64, f64)> {
        let b = self.bounds()?;
        Some(self.advance(&mut lock(&self.anchor), rt, &b))
    }

    /// The cursor now, and the speed.
    pub fn cursor_now(&self) -> Option<(i64, f64)> {
        self.at_now(|now| self.cursor_tai(now))
    }

    /// The cue the group holds, if any.
    pub fn cue(&self) -> Option<Region> {
        lock(&self.anchor).region
    }

    /// The cursor at `rt` while play goes on; `None` once it has stopped
    /// (a clip reached its out point and froze) or there is no media.
    pub fn playing_at(&self, rt: i64) -> Option<i64> {
        let (t, speed) = self.cursor_tai(rt)?;
        (speed != 0.0).then_some(t)
    }

    /// The group's time domain: real TAI only when every player says so.
    pub fn time_domain(&self) -> TimeDomain {
        TimeDomain::combine(self.players().iter().map(|p| p.time_domain()))
    }

    /// The group's readiness: the least ready angle; indexing without one.
    pub fn ready(&self) -> ReplayReady {
        state::group_ready(&self.players())
    }

    /// A player pushed the picture for the cursor at `tai` (group time), on
    /// the tick at running time `rt`: remember the group's latest pushed
    /// time, and if it is the target of the last paused command, how long
    /// that took (first player wins).
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
}

/// The ramp of `a` still running at `now`: from, to, duration and time left
/// (ns). `from` is the speed the ramp started from.
fn ramp_view(a: &Anchor, now: i64) -> Option<(f64, f64, i64, i64)> {
    let r = a.ramp?;
    let left = r.duration_ns - (now - a.rt).max(0);
    (left > 0).then_some((a.speed, r.to, r.duration_ns, left))
}

#[cfg(test)]
mod tests;
