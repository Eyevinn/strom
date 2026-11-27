//! The API's view of a replay group: the state, the diagnostics, discovery,
//! command results, and the events for changes of readiness and range.

use super::super::grid::FrameGrid;
use super::{lock, mono_ns, Applied, Bounds, PlayerStats, Region, ReplayTimeline, TimeDomain, MS};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use strom_types::replay::{
    ReplayAngleDiagnostics, ReplayAngleInfo, ReplayAngleState, ReplayCause, ReplayCommandResult,
    ReplayCue, ReplayDecodeRecord, ReplayDiagnostics, ReplayEventKind, ReplayExtent, ReplayFrame,
    ReplayGrid, ReplayGridRate, ReplayGroupInfo, ReplayInstant, ReplayLiveMargin, ReplayMotion,
    ReplayOnAir, ReplayPosition, ReplayRampState, ReplayRange, ReplayReady, ReplaySpan,
    ReplayState, ReplayTimeDomain,
};
use strom_types::tai::{format_utc_ms, tai_to_utc_ns};
use strom_types::FlowId;

const NS: i64 = 1_000_000_000;

/// How times are written for the API: `utc` only on real TAI, frames on the
/// output grid.
pub(crate) struct View {
    pub domain: TimeDomain,
    pub output: Option<FrameGrid>,
}

impl View {
    pub fn instant(&self, t: i64) -> ReplayInstant {
        ReplayInstant {
            t: t.to_string(),
            utc: self.utc(t),
        }
    }

    fn utc(&self, t: i64) -> Option<String> {
        (self.domain == TimeDomain::Tai).then(|| format_utc_ms(tai_to_utc_ns(t)))
    }

    /// A frame of `grid` that starts at `start` (its own time: a source
    /// frame can sit off its grid after a phase change), with its index
    /// among the frames of `grid` that start in the same second. TAI and UTC
    /// seconds differ by whole seconds, so the axis' second is the UTC one.
    pub fn frame_at(&self, grid: &FrameGrid, start: i64) -> ReplayFrame {
        let second = start.div_euclid(NS) * NS;
        let before = ((start - second) as i128 * grid.fps_n as i128)
            .div_euclid(grid.fps_d as i128 * NS as i128) as i64;
        ReplayFrame {
            t: start.to_string(),
            utc: self.utc(start),
            frame_in_second: before,
        }
    }

    /// The frame of `grid` that holds `t`.
    pub fn frame_on(&self, grid: &FrameGrid, t: i64) -> ReplayFrame {
        let k = grid.floor_frame(t);
        let start = grid.tai_of_frame(k);
        let second = start.div_euclid(NS) * NS;
        ReplayFrame {
            t: start.to_string(),
            utc: self.utc(start),
            frame_in_second: k - grid.ceil_frame(second),
        }
    }

    /// The output-grid frame that holds `t`, once the grid is known.
    pub fn frame(&self, t: i64) -> Option<ReplayFrame> {
        self.output.as_ref().map(|g| self.frame_on(g, t))
    }

    pub fn position(&self, t: i64) -> ReplayPosition {
        ReplayPosition {
            at: self.instant(t),
            frame: self.frame(t),
        }
    }

    pub fn span(&self, start: i64, end: i64) -> ReplaySpan {
        ReplaySpan {
            start: self.instant(start),
            end: self.instant(end),
            duration_ms: (end - start) / MS,
        }
    }

    fn cue(&self, r: Region) -> ReplayCue {
        ReplayCue {
            in_point: self.instant(r.start),
            out: self.instant(r.end),
            end_action: r.end_action,
        }
    }
}

pub(crate) fn api_domain(d: TimeDomain) -> ReplayTimeDomain {
    match d {
        TimeDomain::Unknown => ReplayTimeDomain::Unknown,
        TimeDomain::Tai => ReplayTimeDomain::Tai,
        TimeDomain::Unanchored => ReplayTimeDomain::Unanchored,
    }
}

fn rank(r: ReplayReady) -> u8 {
    match r {
        ReplayReady::Indexing => 0,
        ReplayReady::Warming => 1,
        ReplayReady::Ready => 2,
    }
}

/// The least ready angle; indexing without one.
pub(crate) fn group_ready(players: &[Arc<PlayerStats>]) -> ReplayReady {
    players
        .iter()
        .map(|p| p.ready())
        .min_by_key(|r| rank(*r))
        .unwrap_or(ReplayReady::Indexing)
}

fn rate(g: &FrameGrid) -> ReplayGridRate {
    ReplayGridRate {
        fps: format!("{}/{}", g.fps_n, g.fps_d),
        frame_ns: g.frame_ns(),
    }
}

/// UTC ns now, from the host's wall clock.
fn wall_utc_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

/// What the events last said.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    ready: Option<ReplayReady>,
    range: Option<RangeSig>,
}

/// What a `range_changed` is about: the start, the angles that have media
/// (which ones, not only how many), the holes, and an end or a live edge that
/// went back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RangeSig {
    start: i64,
    end: i64,
    live_end: i64,
    angles: u64,
    gaps: u64,
}

/// Which angles have media: their block ids, in any order.
fn angles_signature(players: &[Arc<PlayerStats>]) -> u64 {
    use std::hash::{Hash, Hasher};
    players
        .iter()
        .filter(|p| p.extent().is_some())
        .fold(0u64, |acc, p| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            p.block_id.hash(&mut h);
            // Order-free; the count tells one angle from the same twice.
            acc.wrapping_add(h.finish()).wrapping_add(1)
        })
}

impl ReplayTimeline {
    pub(crate) fn view(&self) -> View {
        View {
            domain: self.time_domain(),
            output: self.output_grid(),
        }
    }

    /// Send `ready` and `range_changed` when they change, and arm
    /// `start_mode: live` when the group becomes ready.
    pub(super) fn watch_changes(&self, players: &[Arc<PlayerStats>], b: Option<&Bounds>) {
        let ready = group_ready(players);
        let sig = b.map(|b| RangeSig {
            start: b.start,
            end: b.end,
            live_end: b.live_end,
            angles: angles_signature(players),
            gaps: players.iter().fold(0u64, |h, p| {
                h.wrapping_mul(31).wrapping_add(p.gaps_signature())
            }),
        });
        let mut w = lock(&self.watch);
        if w.ready != Some(ready) {
            // The first look only records: the state says where it starts.
            if w.ready.replace(ready).is_some() {
                self.emit(ReplayCause::Media, None, ReplayEventKind::Ready { ready });
            }
            if ready == ReplayReady::Ready
                && self.start_mode() == super::StartMode::Live
                && !self.commanded.load(Ordering::Relaxed)
            {
                self.start_live.store(true, Ordering::Relaxed);
            }
        }
        let (Some(b), Some(sig)) = (b, sig) else {
            return;
        };
        let Some(old) = w.range.replace(sig) else {
            return;
        };
        // The edge growing (the end moving on) is in the state, not an event.
        let changed = old.start != sig.start
            || old.angles != sig.angles
            || old.gaps != sig.gaps
            || sig.end < old.end
            || sig.live_end < old.live_end;
        if changed {
            let cause = if sig.start > old.start && old.angles == sig.angles {
                ReplayCause::Retention
            } else {
                ReplayCause::Media
            };
            let range = self.range_api(b, &self.view());
            self.emit(cause, None, ReplayEventKind::RangeChanged { range });
        }
    }

    fn range_api(&self, b: &Bounds, v: &View) -> ReplayRange {
        let (est, growing) = lock(&self.edge).estimate(mono_ns(), b.live_end);
        ReplayRange {
            start: v.instant(b.start),
            live_edge: v.instant(b.live_end),
            end: v.instant(b.end),
            live_edge_estimate: v.instant(est),
            growing,
        }
    }

    /// What a command did, for the API.
    pub fn result(&self, a: &Applied) -> ReplayCommandResult {
        let v = self.view();
        ReplayCommandResult {
            motion: ReplayMotion {
                cursor: v.position(a.tai),
                speed: a.speed,
                live: a.live,
                cue: a.region.map(|r| v.cue(r)),
                ramp: a.ramp.map(|(from, to, d, left)| ReplayRampState {
                    from,
                    to,
                    duration_ms: (d / MS) as u64,
                    remaining_ms: (left / MS) as u64,
                }),
            },
            client_ref: a.client_ref.clone(),
            mark: a.mark.clone(),
        }
    }

    fn grid_api(&self) -> ReplayGrid {
        ReplayGrid {
            output: self.output_grid().as_ref().map(rate),
            source: self.reference().as_ref().map(rate),
        }
    }

    fn angle_state(&self, p: &PlayerStats, v: &View) -> ReplayAngleState {
        let grid = p.group_grid();
        let view = p.group_index_view();
        ReplayAngleState {
            block: p.block_id.clone(),
            ready: p.ready(),
            fps: grid.map(|g| format!("{}/{}", g.fps_n, g.fps_d)),
            frame_rate_missing: p.frame_rate_missing.load(Ordering::Relaxed),
            offset_ms: p.offset_ns() / MS,
            extent: p.extent().map(|(s, e)| ReplayExtent {
                start: v.instant(s),
                end: v.instant(e),
            }),
            shown: grid
                .zip(p.shown_group_tai())
                .map(|(g, t)| v.frame_at(&g, t)),
            in_sync: p.in_sync.load(Ordering::Relaxed),
            in_gap: p.group_gap().map(|(s, e)| v.span(s, e)),
            gaps: view.gaps.iter().map(|&(s, e)| v.span(s, e)).collect(),
        }
    }

    /// The group state.
    pub fn state(&self) -> ReplayState {
        let now = self.now_rt();
        let bounds = self.bounds();
        let players = self.players();
        let v = self.view();
        let (margin_ns, margin_source) = self.live_margin();
        let mut s = ReplayState {
            time_domain: api_domain(v.domain),
            ready: group_ready(&players),
            grid: self.grid_api(),
            jog_grid: self.jog_grid(),
            follow_live: self.follow_live(),
            live_margin: ReplayLiveMargin {
                ms: (margin_ns / MS).max(0) as u64,
                source: margin_source,
            },
            motion: None,
            on_air: None,
            range: None,
            angles: players.iter().map(|p| self.angle_state(p, &v)).collect(),
        };
        let Some(b) = bounds else {
            return s;
        };
        let (cursor, speed) = self.advance(&mut lock(&self.anchor), now, &b);
        let a = *lock(&self.anchor);
        s.motion = Some(ReplayMotion {
            cursor: v.position(cursor),
            speed,
            live: a.live,
            cue: a.region.map(|r| v.cue(r)),
            ramp: super::ramp_view(&a, now).map(|(from, to, d, left)| ReplayRampState {
                from,
                to,
                duration_ms: (d / MS) as u64,
                remaining_ms: (left / MS) as u64,
            }),
        });
        let range = self.range_api(&b, &v);
        let est: i64 = range.live_edge_estimate.t.parse().unwrap_or(b.live_end);
        s.range = Some(range);
        if let Some(t) = self.pushed_tai() {
            let ms = |d: i64| (d as f64 / 1e6).round() as i64;
            s.on_air = Some(ReplayOnAir {
                at: v.instant(t),
                frame: v.frame(t),
                behind_live_ms: ms(b.live_end - t),
                behind_live_estimate_ms: ms(est - t),
                // How old the picture is: wall clock minus capture time.
                age_ms: (v.domain == TimeDomain::Tai)
                    .then(|| (wall_utc_ns() - tai_to_utc_ns(t)) / MS),
            });
        }
        s
    }

    fn angle_diagnostics(&self, p: &PlayerStats, v: &View) -> ReplayAngleDiagnostics {
        let decoder = lock(&p.decoder).clone();
        let decodes = p.decodes.load(Ordering::Relaxed);
        let o = p.offset_ns();
        let wanted = p.wanted_tai.load(Ordering::Relaxed);
        ReplayAngleDiagnostics {
            block: p.block_id.clone(),
            output_fps: p.output_rate().map(|(n, d)| format!("{}/{}", n, d)),
            wanted: (wanted != super::UNKNOWN).then(|| v.instant(wanted)),
            indexed: p
                .group_index_view()
                .listed
                .iter()
                .map(|&(s, e)| v.span(s, e))
                .collect(),
            strip_frame: p.strip_frame.load(Ordering::Relaxed),
            strip_mismatches: p.strip_mismatches.load(Ordering::Relaxed),
            code_utc_us: p.code_utc_us.load(Ordering::Relaxed),
            time_checked: p.time_checked.load(Ordering::Relaxed),
            time_errors: p.time_errors.load(Ordering::Relaxed),
            repeated_ticks: p.repeated_ticks.load(Ordering::Relaxed),
            skipped_frames: p.skipped_frames.load(Ordering::Relaxed),
            misses: p.misses.load(Ordering::Relaxed),
            late_ticks: p.late_ticks.load(Ordering::Relaxed),
            index_segments: p.index_segments.load(Ordering::Relaxed),
            fetched_segments: p.fetched_segments.load(Ordering::Relaxed),
            fetches: p.fetches.load(Ordering::Relaxed),
            fetch_errors: p.fetch_errors.load(Ordering::Relaxed),
            last_fetch_ms: p.last_fetch_ms.load(Ordering::Relaxed),
            cached_segments: p.cached_segments.load(Ordering::Relaxed),
            decodes,
            last_decode_ms: p.last_decode_ms.load(Ordering::Relaxed),
            decode_failures: p.decode_failures.load(Ordering::Relaxed),
            decode_ms_avg: (decodes > 0)
                .then(|| p.decode_ms_total.load(Ordering::Relaxed) as f64 / decodes as f64),
            decode_ms_max: p.decode_ms_max.load(Ordering::Relaxed),
            decoder: decoder.as_ref().map(|d| d.0.clone()),
            decode_workers: decoder.as_ref().map(|d| d.1),
            recent_decodes: lock(&p.decode_log)
                .iter()
                .map(|d| ReplayDecodeRecord {
                    segment: v.span(d.start_ns + o, d.end_ns + o),
                    ms: d.ms,
                    frames: d.frames,
                    ok: d.error.is_none(),
                    error: d.error.clone(),
                })
                .collect(),
        }
    }

    /// Engine diagnostics.
    pub fn diagnostics(&self) -> ReplayDiagnostics {
        let v = self.view();
        let b = self.bounds();
        let (step, seen) = {
            let edge = lock(&self.edge);
            (
                edge.step().map(|s| s / MS),
                edge.last_seen().map(|m| (mono_ns() - m) / MS),
            )
        };
        ReplayDiagnostics {
            now_utc: format_utc_ms(wall_utc_ns()),
            cmd_to_push_ms: self.cmd_to_push_us.load(Ordering::Relaxed) as f64 / 1000.0,
            live_edge_step_ms: step,
            live_edge_seen_ms_ago: seen,
            live_edge_age_ms: b
                .filter(|_| v.domain == TimeDomain::Tai)
                .map(|b| (wall_utc_ns() - tai_to_utc_ns(b.live_end)) / MS),
            angles: self
                .players()
                .iter()
                .map(|p| self.angle_diagnostics(p, &v))
                .collect(),
        }
    }

    /// The group for discovery.
    pub fn info(&self, flow_id: FlowId, group: &str) -> ReplayGroupInfo {
        let players = self.players();
        let angles: Vec<ReplayAngleInfo> = players
            .iter()
            .map(|p| ReplayAngleInfo {
                block: p.block_id.clone(),
                name: p.config.name.clone(),
                source_id: p.config.source_id.clone(),
                source: p.config.source.clone(),
            })
            .collect();
        let stores: Vec<Option<&String>> =
            players.iter().map(|p| p.config.store.as_ref()).collect();
        let source = match stores.first() {
            Some(Some(first)) if stores.iter().all(|s| *s == Some(*first)) => {
                Some((*first).clone())
            }
            _ => None,
        };
        ReplayGroupInfo {
            flow_id: flow_id.to_string(),
            group: group.to_string(),
            time_domain: api_domain(self.time_domain()),
            grid: self.grid_api(),
            ready: group_ready(&players),
            angles,
            source,
        }
    }
}
