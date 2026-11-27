//! The group's clip queue (`cue_next`, `clear_queue`): clips that play one
//! after the other, each from its in point at its own speed. The queue only
//! runs clips; which clips, in which order, is the client's (a playlist in
//! a backend that fills the queue one clip ahead or more).
//!
//! The swap is the timeline's: when the clip the group holds (its cue) is
//! played forward to its out point and a clip is queued, the line is redrawn
//! from the running time at which the cursor reached the out point, at the
//! next clip's in point and speed, with its cue. That running time is
//! computed from the line, not taken from whoever looks first, so every
//! player swaps on the same tick. A tick from before the swap that comes in
//! after it still gets the old clip's frame (see [`Queue::before`]).
//!
//! The players prepare the next clip while the current one plays (see
//! `player::lane`): a second lane fetched and decoded at the next clip's in
//! point, shown from the tick on which the queue started that clip.

use super::super::grid::FrameGrid;
use super::switch::OpenedSource;
use super::{
    lock, ramp_view, travel, Anchor, Applied, Bounds, Command, CommandError, EndAction, Region,
    ReplayTimeline, MAX_SPEED,
};
use std::collections::VecDeque;
use std::sync::Arc;
use strom_types::replay::{ReplayCause, ReplayEventKind, MAX_QUEUE_LEN};

/// A clip in the queue, its times resolved on the group's axis.
#[derive(Clone)]
pub struct QueueItem {
    /// Numbers the clips queued in a run, from 1: which clip a player
    /// prepared a lane for.
    pub seq: u64,
    /// `[start, end)` on the group's axis.
    pub start: i64,
    pub end: i64,
    pub speed: f64,
    pub end_action: EndAction,
    pub client_ref: Option<String>,
    /// The angle that shows another source for this clip, and that source.
    pub source: Option<(String, OpenedSource)>,
}

impl std::fmt::Debug for QueueItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueItem")
            .field("seq", &self.seq)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("speed", &self.speed)
            .field("client_ref", &self.client_ref)
            .field("block", &self.source.as_ref().map(|(b, _)| b))
            .finish()
    }
}

impl QueueItem {
    /// The URI angle `block` shows for this clip, if it switches.
    pub fn source_for(&self, block: &str) -> Option<&OpenedSource> {
        self.source
            .as_ref()
            .filter(|(b, _)| b == block)
            .map(|(_, s)| s)
    }
}

/// The queue and the clip it started last.
#[derive(Default)]
pub(super) struct Queue {
    items: VecDeque<Arc<QueueItem>>,
    /// The clip the queue started that the group still holds (as its cue);
    /// `None` once a command sets another cue or drops it.
    current: Option<Arc<QueueItem>>,
    /// The number of the last clip queued.
    last_seq: u64,
    /// The line before the last swap, and the clip it played: the motion of
    /// a running time before the swap, for a tick that asks after it.
    /// Dropped by any command.
    before: Option<(Anchor, u64)>,
}

impl Queue {
    /// The clip playing from the queue, 0 when none.
    fn current_seq(&self) -> u64 {
        self.current.as_ref().map_or(0, |i| i.seq)
    }

    /// A command changed the motion: the line before the last swap no
    /// longer leads to it. `keeps_clip`: the cue the queue set is still
    /// held.
    pub(super) fn commanded(&mut self, keeps_clip: bool) {
        self.before = None;
        if !keeps_clip {
            self.current = None;
        }
    }

    /// Empty the queue (and forget the clip playing): a new run, `live`,
    /// `end`, `uncue`.
    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.before = None;
    }
}

/// The first running time in `(a.rt, rt]` at which the line of `a` has
/// reached `end`, if it has by `rt`. Bisection on the line's distance,
/// exact to the ns for any speed or ramp.
fn crossing(a: &Anchor, rt: i64, from: i64, end: i64) -> Option<i64> {
    let pos = |t: i64| from as i128 + travel(a.speed, a.ramp, t - a.rt).0.round() as i128;
    if rt <= a.rt || pos(rt) < end as i128 || pos(a.rt) >= end as i128 {
        return None;
    }
    let (mut lo, mut hi) = (a.rt, rt);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if pos(mid) >= end as i128 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

impl ReplayTimeline {
    /// Start the queued clips whose turn has come by `rt`: while the cue's
    /// line reaches the cue's out by `rt` and a clip is queued, redraw the
    /// line at that clip from the running time the out was reached. Sends
    /// `ended` (the clip that ended, its ref) and `item_started` (the new
    /// clip, its ref) for each.
    pub(super) fn run_queue(&self, a: &mut Anchor, rt: i64, b: &Bounds, growing: bool) {
        loop {
            let Some(r) = a.region else {
                return;
            };
            if rt < a.rt {
                return;
            }
            let mut q = lock(&self.queue);
            let Some(next) = q.items.front().cloned() else {
                return;
            };
            // Where the cue's play ends, as `eval` has it: an out past media
            // that no longer grows ends where the media does.
            let grows = r.end > b.end && growing;
            let end = if grows { r.end } else { r.end.min(b.end) };
            let from = a.tai.unwrap_or(b.start);
            let Some(at) = crossing(a, rt, from, end.max(r.start.max(b.start) + 1)) else {
                return;
            };
            q.items.pop_front();
            let previous = q.current_seq();
            q.before = Some((*a, previous));
            q.current = Some(Arc::clone(&next));
            let waiting = q.items.len();
            drop(q);
            let start = next.start.clamp(b.start, b.end - 1);
            *a = Anchor {
                rt: at,
                tai: Some(start),
                speed: next.speed,
                ramp: None,
                region: Some(Region {
                    start: next.start.max(b.start),
                    end: next.end,
                    end_action: next.end_action,
                }),
                live: false,
                catch_up: false,
            };
            let ended_ref = std::mem::replace(&mut *lock(&self.cue_ref), next.client_ref.clone());
            *lock(&self.anchor_ref) = next.client_ref.clone();
            let view = self.view();
            self.emit(
                ReplayCause::CueEnd,
                ended_ref,
                ReplayEventKind::Ended {
                    at: view.instant(end - 1),
                },
            );
            self.emit(
                ReplayCause::CueEnd,
                next.client_ref.clone(),
                ReplayEventKind::ItemStarted {
                    at: view.instant(start),
                    waiting,
                },
            );
        }
    }

    /// For a running time before the line of `a` starts: the line before
    /// the last swap, if `a` is the swap's and `rt` is on that line.
    pub(super) fn line_before(&self, a: &Anchor, rt: i64) -> Option<Anchor> {
        let q = lock(&self.queue);
        let (prev, _) = q.before.as_ref()?;
        (rt < a.rt && rt >= prev.rt).then_some(*prev)
    }

    /// The cursor at running time `rt`, and the number of the queued clip
    /// that plays then (0: none). A player shows the lane it prepared for a
    /// clip from the first tick that clip plays on.
    pub fn cursor_item(&self, rt: i64) -> Option<(i64, u64)> {
        let b = self.bounds()?;
        let mut a = lock(&self.anchor);
        let (t, _) = self.advance(&mut a, rt, &b);
        let q = lock(&self.queue);
        let seq = match &q.before {
            Some((prev, seq)) if rt < a.rt && rt >= prev.rt => *seq,
            _ => q.current_seq(),
        };
        Some((t, seq))
    }

    /// The cursor now, and the queued clip playing.
    pub fn cursor_item_now(&self) -> Option<(i64, u64)> {
        self.at_now(|now| self.cursor_item(now))
    }

    /// The clip playing from the queue (0: none) and the next clip queued.
    pub fn queue_view(&self) -> (u64, Option<Arc<QueueItem>>) {
        let q = lock(&self.queue);
        (q.current_seq(), q.items.front().cloned())
    }

    /// Clips waiting in the queue.
    pub fn queued(&self) -> usize {
        lock(&self.queue).items.len()
    }

    /// Apply `cue_next` or `clear_queue` at running time `now`. The motion
    /// does not change. A `cue_next` with a source is checked as a switch
    /// is (`prepare_switch`) and its source opened here; the angle switches
    /// when the clip starts.
    pub(super) fn apply_queue(
        &self,
        cmd: Command,
        client_ref: Option<String>,
        now: i64,
        b: &Bounds,
        output: Option<FrameGrid>,
    ) -> Result<Applied, CommandError> {
        let item = match cmd {
            Command::CueNext {
                in_point,
                out,
                speed,
                end_action,
                source,
                block,
                replace,
            } => {
                let speed = speed.unwrap_or(1.0);
                if !speed.is_finite() || speed <= 0.0 || speed > MAX_SPEED {
                    return Err(CommandError::Invalid(format!(
                        "cue_next: speed {} is outside 0 .. {} (forward, not 0)",
                        speed, MAX_SPEED
                    )));
                }
                let domain = self.time_domain();
                let cur = {
                    let mut a = lock(&self.anchor);
                    self.advance(&mut a, now, b).0
                };
                let start = self.resolve(&in_point, b, cur, domain, output, "cue_next in")?;
                let end = self.resolve(&out, b, cur, domain, output, "cue_next out")?;
                if end <= start {
                    return Err(CommandError::Invalid(
                        "cue_next: out must be after in".into(),
                    ));
                }
                if start >= b.end || end <= b.start {
                    return Err(CommandError::OutOfRange(
                        "cue_next: the range is outside the media".into(),
                    ));
                }
                let source = self
                    .prepare_switch(source.as_deref(), block.as_deref())?
                    .map(|s| s.into_parts());
                Some((
                    QueueItem {
                        seq: 0,
                        start: start.max(b.start),
                        end,
                        speed,
                        end_action: end_action.unwrap_or_default(),
                        client_ref: client_ref.clone(),
                        source,
                    },
                    replace.unwrap_or(false),
                ))
            }
            Command::ClearQueue {} => None,
            _ => unreachable!("only queue commands"),
        };
        let mut a = lock(&self.anchor);
        let (tai, speed) = self.advance(&mut a, now, b);
        if item.is_some() && now > a.rt && a.region.is_some_and(|r| r.end_action == EndAction::Loop)
        {
            // A looping clip's line has passed its out once per lap: redraw
            // it here, the same motion, so the clip queued starts at the end
            // of this lap and not at one gone by.
            *a = Anchor {
                rt: now,
                tai: Some(tai),
                speed,
                ramp: a.ramp_left(now),
                ..*a
            };
            lock(&self.queue).commanded(true);
        }
        {
            let mut q = lock(&self.queue);
            match item {
                Some((mut item, replace)) => {
                    if replace {
                        q.items.clear();
                    }
                    if q.items.len() >= MAX_QUEUE_LEN {
                        return Err(CommandError::Conflict(format!(
                            "cue_next: the queue holds {} clips, its most",
                            MAX_QUEUE_LEN
                        )));
                    }
                    q.last_seq += 1;
                    item.seq = q.last_seq;
                    q.items.push_back(Arc::new(item));
                }
                None => q.items.clear(),
            }
        }
        Ok(Applied {
            tai,
            speed,
            region: a.region,
            live: a.live,
            ramp: ramp_view(&a, now),
            mark: None,
            client_ref,
        })
    }
}
