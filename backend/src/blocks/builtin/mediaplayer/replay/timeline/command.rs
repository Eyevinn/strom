//! Commands: parsing, resolving their targets, and applying them to the
//! group's motion (the table of what each command does to cursor, speed,
//! cue, live and ramp).

use super::super::grid::FrameGrid;
use super::{
    eval, frame_centre, frame_in, lock, ms_to_ns, nearest_frame_start, ramp_view, Anchor, Applied,
    Bounds, Clip, EndAction, Ramp, Region, ReplayTimeline, TimeDomain, MAX_RAMP_MS, MAX_SPEED, MS,
};
use std::sync::atomic::Ordering;
use std::time::Instant;
use strom_types::replay::{
    ReplayCause, ReplayErrorCode, ReplayEventKind, ReplayMark, ReplayMarkAngle, ReplayTarget,
    MAX_CLIENT_REF_LEN, MAX_LIVE_MARGIN_SET_MS, MAX_MARK_LABEL_LEN,
};
use strom_types::tai::{parse_utc, utc_to_tai_ns};

pub use strom_types::replay::ReplayCommand as Command;

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

/// Why a command was refused. Nothing changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    /// The command is malformed or a value is out of range (HTTP 400).
    Invalid(String),
    /// The command is valid but does not apply to the group's state now,
    /// e.g. no media yet (HTTP 409).
    Conflict(String),
    /// The range the command names is wholly outside the media (HTTP 409).
    OutOfRange(String),
}

impl CommandError {
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(m) | Self::Conflict(m) | Self::OutOfRange(m) => m,
        }
    }

    pub fn code(&self) -> ReplayErrorCode {
        match self {
            Self::Invalid(_) => ReplayErrorCode::Invalid,
            Self::Conflict(_) => ReplayErrorCode::Conflict,
            Self::OutOfRange(_) => ReplayErrorCode::OutOfRange,
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

fn out_of_range(m: impl Into<String>) -> CommandError {
    CommandError::OutOfRange(m.into())
}

/// A command as parsed from the wire, with its `client_ref`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub command: Command,
    pub client_ref: Option<String>,
}

/// The `client_ref` of a command object, if any, taken out of it.
pub fn take_client_ref(value: &mut serde_json::Value) -> Result<Option<String>, CommandError> {
    let Some(o) = value.as_object_mut() else {
        return Ok(None);
    };
    match o.remove("client_ref") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.chars().count() <= MAX_CLIENT_REF_LEN => {
            Ok(Some(s))
        }
        Some(serde_json::Value::String(_)) => Err(invalid(format!(
            "client_ref: longer than {} characters",
            MAX_CLIENT_REF_LEN
        ))),
        Some(_) => Err(invalid("client_ref: must be a string")),
    }
}

/// Parse a command from JSON. A `seq` field (the WebSocket's correlation id)
/// is ignored and `client_ref` taken out; any other unknown field is an
/// error, so a typo is not silently dropped.
pub fn parse_command(mut value: serde_json::Value) -> Result<Request, CommandError> {
    let Some(o) = value.as_object_mut() else {
        return Err(invalid("a command is a JSON object with a \"cmd\" field"));
    };
    o.remove("seq");
    let client_ref = take_client_ref(&mut value)?;
    let command = serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
    Ok(Request {
        command,
        client_ref,
    })
}

impl ReplayTimeline {
    /// The instant a command names. Exactly one field; `rel_ms` counts from
    /// `cursor`; `utc` names the start of the frame of `output` (the grid
    /// the API's timecodes are on) nearest it. Not clamped to the media.
    pub(super) fn resolve(
        &self,
        at: &ReplayTarget,
        b: &Bounds,
        cursor: i64,
        domain: TimeDomain,
        output: Option<FrameGrid>,
        what: &str,
    ) -> Result<i64, CommandError> {
        let given = [
            at.t.is_some(),
            at.utc.is_some(),
            at.from_live_ms.is_some(),
            at.rel_ms.is_some(),
        ];
        if given.iter().filter(|g| **g).count() != 1 {
            return Err(invalid(format!(
                "{}: give exactly one of t, utc, from_live_ms, rel_ms",
                what
            )));
        }
        if let Some(t) = &at.t {
            return t.trim().parse::<i64>().map_err(|_| {
                invalid(format!(
                    "{}: bad t '{}', want integer nanoseconds as a string",
                    what, t
                ))
            });
        }
        if let Some(u) = &at.utc {
            let t = parse_utc(u).map(utc_to_tai_ns).ok_or_else(|| {
                invalid(format!(
                    "{}: bad utc '{}', want e.g. 2026-11-24T13:21:56.080Z",
                    what, u
                ))
            })?;
            if domain != TimeDomain::Tai {
                return Err(conflict(format!(
                    "{}: utc needs the time domain tai, the group's is {}",
                    what,
                    domain.as_str()
                )));
            }
            return Ok(output.map_or(t, |g| nearest_frame_start(&g, t)));
        }
        if let Some(ms) = at.from_live_ms {
            return Ok((b.live_end - 1).saturating_sub(ms_to_ns(ms)));
        }
        let ms = at.rel_ms.unwrap_or(0);
        Ok(cursor.saturating_add(ms.saturating_mul(MS)))
    }

    /// Apply a command at "now". A source switch (`cue {source}`,
    /// `source`) is checked and its source opened first, and started once
    /// the motion applied: a refused command changes nothing.
    pub fn apply(&self, req: Request) -> Result<Applied, CommandError> {
        let switch = match &req.command {
            Command::Cue { source, block, .. } => {
                self.prepare_switch(source.as_deref(), block.as_deref())?
            }
            Command::Source { source, block } => {
                self.prepare_switch(Some(source), block.as_deref())?
            }
            _ => None,
        };
        let client_ref = req.client_ref.clone();
        // "Now" is read and used under one hold of `clock_source`: a change
        // of time base in between would put the anchor on the old base.
        let applied = self.at_now(|now| self.apply_req_at(req, now))?;
        if let Some(s) = switch {
            s.begin(client_ref);
        }
        Ok(applied)
    }

    /// Apply a command without a `client_ref` at running time `now`.
    pub fn apply_at(&self, cmd: Command, now: i64) -> Result<Applied, CommandError> {
        self.apply_req_at(
            Request {
                command: cmd,
                client_ref: None,
            },
            now,
        )
    }

    /// Apply a command at running time `now`. Fails, changing nothing, on a
    /// target that does not parse or a group that has no media yet.
    pub fn apply_req_at(&self, req: Request, now: i64) -> Result<Applied, CommandError> {
        let Request {
            command: cmd,
            client_ref,
        } = req;
        let b = self
            .bounds()
            .ok_or_else(|| conflict("the group has no media yet"))?;
        let reference = self.reference();
        let domain = self.time_domain();
        if let Command::Mark { label, at } = cmd {
            if label
                .as_ref()
                .is_some_and(|l| l.chars().count() > MAX_MARK_LABEL_LEN)
            {
                return Err(invalid(format!(
                    "mark: label longer than {} characters",
                    MAX_MARK_LABEL_LEN
                )));
            }
            // Before the anchor is locked: a mark reads the players.
            let at = {
                let a = lock(&self.anchor);
                let cur = eval(&a, now, &b, self.live_params(&b)).tai;
                let output = self.output_grid();
                at.map(|at| self.resolve(&at, &b, cur, domain, output, "mark at"))
                    .transpose()?
            };
            if at.is_some_and(|t| t < b.start || t >= b.end) {
                return Err(out_of_range("mark: at is outside the media"));
            }
            let mark = self.mark(label, at)?;
            let mut a = lock(&self.anchor);
            let (cur, cur_speed) = self.advance(&mut a, now, &b);
            return Ok(Applied {
                tai: cur,
                speed: cur_speed,
                region: a.region,
                live: a.live,
                ramp: ramp_view(&a, now),
                mark: Some(mark),
                client_ref,
            });
        }
        if let Command::CueNext { .. } | Command::ClearQueue {} = cmd {
            return self.apply_queue(cmd, client_ref, now, &b, self.output_grid());
        }
        if let Command::Source { .. } = cmd {
            // The switch is the player's (see `apply`): the motion stays.
            let mut a = lock(&self.anchor);
            let (cur, cur_speed) = self.advance(&mut a, now, &b);
            return Ok(Applied {
                tai: cur,
                speed: cur_speed,
                region: a.region,
                live: a.live,
                ramp: ramp_view(&a, now),
                mark: None,
                client_ref,
            });
        }
        let mut a = lock(&self.anchor);
        let (cur, cur_speed) = self.advance(&mut a, now, &b);
        let mut ramp = None;
        // A cue held, at the moment the command arrives.
        let held = a.region;
        let live = match cmd {
            Command::Live { .. } => true,
            // A group with a cue is not live: uncue leaves live as it is
            // (and it plays on, and may catch up).
            Command::Uncue {} => held.is_none() && a.live,
            Command::FollowLive { on } => on && a.live,
            _ => false,
        };
        // What the cue becomes: kept (if it still holds the cursor), set, or
        // dropped.
        enum Cue {
            Keep,
            Set(Region),
            Drop,
        }
        let output = self.output_grid();
        let resolve =
            |at: &ReplayTarget, what: &str| self.resolve(at, &b, cur, domain, output, what);
        let (tai, speed, cue) = match &cmd {
            Command::Jog { frames } => {
                let g = self
                    .jog_grid_of(self.jog_grid())
                    .ok_or_else(|| conflict("the group's frame rate is not known yet"))?;
                (
                    frame_centre(&g, frame_in(&g, &b, g.floor_frame(cur), *frames)),
                    0.0,
                    Cue::Keep,
                )
            }
            Command::Seek { to, speed } => {
                let to = resolve(to, "seek to")?;
                let speed = match speed {
                    Some(s) => check_speed(*s)?,
                    None => cur_speed,
                };
                (to, speed, Cue::Keep)
            }
            Command::Speed { speed } => (cur, check_speed(*speed)?, Cue::Keep),
            Command::Ramp { to, duration_ms } => {
                let to = check_speed(*to)?;
                if *duration_ms > MAX_RAMP_MS {
                    return Err(invalid(format!(
                        "ramp: duration_ms {} is over {}",
                        duration_ms, MAX_RAMP_MS
                    )));
                }
                if *duration_ms > 0 {
                    ramp = Some(Ramp {
                        to,
                        duration_ns: *duration_ms as i64 * MS,
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
                match until {
                    Some(u) => {
                        let until = resolve(u, "play until")?;
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
                            if until >= cur {
                                return Err(conflict("play: until is not behind the cursor"));
                            }
                            let end = held.map_or(cur + 1, |r| r.end.max(cur + 1));
                            Region {
                                start: until,
                                end,
                                end_action: EndAction::Freeze,
                            }
                        };
                        if r.start >= b.end || r.end <= b.start {
                            return Err(out_of_range("play: until is outside the media"));
                        }
                        let end_action = end_action
                            .or(held.map(|h| h.end_action))
                            .unwrap_or_default();
                        (
                            cur,
                            speed,
                            Cue::Set(Region {
                                start: r.start.max(b.start),
                                end_action,
                                ..r
                            }),
                        )
                    }
                    None => match held {
                        Some(r) => {
                            let r = Region {
                                end_action: end_action.unwrap_or(r.end_action),
                                ..r
                            };
                            // At the end the play runs to: start again from
                            // the other end. An out past media that still
                            // grows is where play runs to (it holds at the
                            // edge until the media gets there).
                            let half = reference.map_or(1, |g| (g.frame_ns() / 2.0) as i64).max(1);
                            let growing = r.end > b.end && self.growing(&b);
                            let r_end = if growing { r.end } else { r.end.min(b.end) };
                            let from = if forward && cur >= r_end - half {
                                r.start
                            } else if !forward && cur < r.start + half {
                                r.end.min(b.end) - 1
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
                if let Some(ms) = margin_ms {
                    if *ms > MAX_LIVE_MARGIN_SET_MS {
                        return Err(invalid(format!(
                            "live: margin_ms {} is over {}",
                            ms, MAX_LIVE_MARGIN_SET_MS
                        )));
                    }
                    self.margin_set_ns.store(ms_to_ns(*ms), Ordering::Relaxed);
                }
                (self.live_target(&b), 1.0, Cue::Drop)
            }
            Command::Cue {
                in_point,
                out,
                end_action,
                ..
            } => {
                let start = resolve(in_point, "cue in")?;
                let end = resolve(out, "cue out")?;
                if end <= start {
                    return Err(invalid("cue: out must be after in"));
                }
                if start >= b.end || end <= b.start {
                    return Err(out_of_range("cue: the range is outside the media"));
                }
                let r = Region {
                    start: start.max(b.start),
                    end,
                    end_action: end_action.unwrap_or_default(),
                };
                (r.start, 0.0, Cue::Set(r))
            }
            Command::Uncue {} => {
                // The ramp goes on.
                ramp = a.ramp_left(now);
                (cur, cur_speed, Cue::Drop)
            }
            Command::FollowLive { on } => {
                *lock(&self.follow_live_set) = Some(*on);
                ramp = a.ramp_left(now);
                (cur, cur_speed, Cue::Keep)
            }
            Command::Mark { .. }
            | Command::Source { .. }
            | Command::CueNext { .. }
            | Command::ClearQueue {} => unreachable!("handled above"),
        };
        // Clamp to the media, so later moves start from what is shown, then
        // keep the cue only if it holds the cursor.
        let tai = tai.clamp(b.start, b.end - 1);
        let sets_cue = matches!(cue, Cue::Set(_));
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
        // The cue's ref: a new cue takes the command's; a play of the held
        // cue with a ref takes that; any other command leaves it.
        let cue_ref = match &cmd {
            Command::Cue { .. } | Command::Play { until: Some(_), .. } => Some(client_ref.clone()),
            Command::Play { .. } if sets_cue && client_ref.is_some() => Some(client_ref.clone()),
            _ => None,
        };
        {
            let mut r = lock(&self.cue_ref);
            if region.is_none() {
                *r = None;
            } else if let Some(new) = cue_ref {
                *r = new;
            }
        }
        {
            // `live` and `end` leave clip play, and `uncue` drops the clip a
            // queue follows: the queue goes. Otherwise the clip the queue
            // started stays while its cue is held.
            let mut q = lock(&self.queue);
            match &cmd {
                Command::Live { .. } | Command::End {} | Command::Uncue {} => q.clear(),
                Command::Cue { .. } | Command::Play { until: Some(_), .. } => q.commanded(false),
                _ => q.commanded(region.is_some()),
            }
        }
        // `follow_live` does not change the motion: the line's ref stays.
        if !matches!(cmd, Command::FollowLive { .. }) {
            *lock(&self.anchor_ref) = client_ref.clone();
        }
        self.commanded.store(true, Ordering::Relaxed);
        *lock(&self.last_cmd) = (speed == 0.0 && ramp.is_none()).then(|| (Instant::now(), tai));
        if matches!(cmd, Command::Live { .. }) {
            self.emit(
                ReplayCause::Live,
                client_ref.clone(),
                ReplayEventKind::Live {
                    at: self.view().instant(tai),
                },
            );
        }
        Ok(Applied {
            tai,
            speed,
            region,
            live,
            ramp: ramp_view(&a, now),
            mark: None,
            client_ref,
        })
    }

    /// Start `clip` at running time `rt`: cue `[in, out)`, freezing at the
    /// end, and play it from `in` at its speed. The same two commands an
    /// operator gives, applied at an exact running time.
    pub fn start_clip(&self, clip: &Clip, rt: i64) -> Result<Applied, CommandError> {
        if clip.speed.is_nan() || clip.speed <= 0.0 {
            return Err(invalid(format!(
                "clip: speed {} is not forward play",
                clip.speed
            )));
        }
        self.apply_at(
            Command::Cue {
                in_point: clip.in_point.clone(),
                out: clip.out.clone(),
                end_action: Some(EndAction::Freeze),
                source: None,
                block: None,
            },
            rt,
        )?;
        self.apply_at(
            Command::Play {
                speed: Some(clip.speed),
                until: None,
                end_action: None,
            },
            rt,
        )
    }

    /// The picture going out now (the group time of the last pushed tick,
    /// and each angle's last pushed frame), or at `at` (each angle's frame
    /// that holds it).
    fn mark(&self, label: Option<String>, at: Option<i64>) -> Result<ReplayMark, CommandError> {
        let t = match at {
            Some(t) => t,
            None => self
                .pushed_tai()
                .ok_or_else(|| conflict("mark: no picture has gone out yet"))?,
        };
        let view = self.view();
        let angles = self
            .players()
            .iter()
            .filter_map(|p| {
                let g = p.group_grid()?;
                let shown = match at {
                    Some(t) => {
                        let (s, e) = p.extent()?;
                        (s <= t && t < e).then(|| g.tai_of_frame(g.floor_frame(t)))?
                    }
                    None => p.shown_group_tai()?,
                };
                Some(ReplayMarkAngle {
                    block: p.block_id.clone(),
                    shown: view.frame_at(&g, shown),
                })
            })
            .collect();
        Ok(ReplayMark {
            label,
            at: view.instant(t),
            frame: view.frame(t),
            angles,
        })
    }
}
