use super::*;
use strom_types::replay::{ReplayLiveMarginSource, ReplayTimeDomain};
use strom_types::replay::{MAX_CLIENT_REF_LEN, MAX_LIVE_MARGIN_SET_MS, MAX_MARK_LABEL_LEN};
use strom_types::tai::{format_utc_ms, parse_utc, tai_to_utc_ns, utc_to_tai_ns};

const BASE: i64 = 1_800_000_000_000_000_000;
const S: i64 = 1_000_000_000;

fn seek_at(to: ReplayTarget) -> Command {
    Command::Seek { to, speed: None }
}

fn seek_tai(tai: i64) -> Command {
    seek_at(ReplayTarget::t(tai))
}

fn at_tai(t: i64) -> ReplayTarget {
    ReplayTarget::t(t)
}

/// The source-grid frame that holds `t`, counted from the group's origin
/// (the frame numbers the tests below reason in; the API has none).
fn frame_no(tl: &ReplayTimeline, t: i64) -> i64 {
    let g = tl.reference().unwrap();
    g.floor_frame(t) - tl.origin_on(&g).unwrap()
}

/// A seek to the middle of source-grid frame `n` from the origin.
fn seek(tl: &ReplayTimeline, n: i64) -> Command {
    tl.bounds();
    let g = tl.reference().unwrap();
    seek_tai(frame_centre(&g, tl.origin_on(&g).unwrap() + n))
}

/// The speed now.
fn speed_of(tl: &ReplayTimeline) -> f64 {
    tl.cursor_now().map_or(0.0, |c| c.1)
}

/// The frame of `grid` a player shows at a running time, and the cursor.
fn frame_on(tl: &ReplayTimeline, grid: &FrameGrid, rt: i64) -> Option<(i64, i64)> {
    let t = tl.tai_at(rt)?;
    Some((grid.floor_frame(t), t))
}

/// The next event's cause and kind.
fn next(events: &mut tokio::sync::broadcast::Receiver<ReplayEvent>) -> Option<ReplayEvent> {
    events.try_recv().ok()
}

/// A player with an extent and, if given, a frame rate anchored at its start.
/// Its live margin is set to 1500 ms, so the tests' numbers do not depend on
/// the computed margin (tested on its own).
fn player(name: &str, start: i64, end: i64, fps: Option<(u32, u32)>) -> Arc<PlayerStats> {
    player_with(
        name,
        start,
        end,
        fps,
        PlayerConfig {
            live_margin_ms: Some(1500),
            ..Default::default()
        },
    )
}

fn player_with(
    name: &str,
    start: i64,
    end: i64,
    fps: Option<(u32, u32)>,
    config: PlayerConfig,
) -> Arc<PlayerStats> {
    let p = Arc::new(PlayerStats::with_config(name, config));
    p.start_tai.store(start, Ordering::Relaxed);
    p.end_tai.store(end, Ordering::Relaxed);
    if let Some((n, d)) = fps {
        p.init_grid(n, d, start);
    }
    p
}

fn timeline_of(players: &[Arc<PlayerStats>]) -> ReplayTimeline {
    let tl = ReplayTimeline::new();
    for p in players {
        tl.register_player(p);
    }
    tl
}

fn timeline_with(extents: &[(i64, i64)]) -> (ReplayTimeline, Vec<Arc<PlayerStats>>) {
    let keep: Vec<_> = extents
        .iter()
        .enumerate()
        .map(|(i, (s, e))| player(&format!("cam{}", i), *s, *e, Some((50, 1))))
        .collect();
    (timeline_of(&keep), keep)
}

#[test]
fn angles_align_on_tai_not_on_segments() {
    // cam1 covers 0..300 s, cam2 starts 2 s later and ends 1 s earlier.
    let b = group_bounds(&[(BASE, BASE + 300 * S), (BASE + 2 * S, BASE + 299 * S)]).unwrap();
    assert_eq!(b.start, BASE);
    assert_eq!(b.end, BASE + 300 * S);
    assert_eq!(b.live_end, BASE + 299 * S);
    assert!(group_bounds(&[]).is_none());
}

#[test]
fn commands_use_origin_relative_frames() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE + S, BASE + 10 * S)]);
    let g = tl.reference().unwrap();
    let rt = 0;
    // Starts at the first frame.
    assert_eq!(tl.tai_at(rt), Some(BASE));
    assert_eq!(tl.origin_tai(), BASE);
    let a = tl.apply_at(seek(&tl, 123), rt).unwrap();
    assert_eq!(frame_no(&tl, a.tai), 123);
    assert_eq!(a.speed, 0.0);
    let a = tl.apply_at(Command::Jog { frames: -3 }, rt).unwrap();
    assert_eq!(frame_no(&tl, a.tai), 120);
    // The cursor rests at the centre of the frame.
    assert_eq!(a.tai, BASE + 120 * 20 * MS + 10 * MS);
    assert_eq!(g.floor_frame(a.tai) - g.floor_frame(BASE), 120);
    // Clamped to the range.
    let a = tl.apply_at(seek(&tl, -50), rt).unwrap();
    assert_eq!(a.tai, BASE);
    // `end` goes to the live edge (the last instant every angle has).
    let a = tl.apply_at(Command::End {}, rt).unwrap();
    assert_eq!(frame_no(&tl, a.tai), 499);
}

#[test]
fn end_is_the_common_edge_not_the_longest_angle() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE, BASE + 8 * S)]);
    let a = tl.apply_at(Command::End {}, 0).unwrap();
    assert_eq!(a.tai, BASE + 8 * S - 1);
}

#[test]
fn live_goes_behind_the_common_edge() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE, BASE + 8 * S)]);
    let a = tl
        .apply_at(
            Command::Live {
                margin_ms: Some(1000),
            },
            0,
        )
        .unwrap();
    assert_eq!(a.speed, 1.0);
    assert_eq!(a.tai, BASE + 7 * S);
    // The index grows: the edge follows.
    p[1].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
    p[0].end_tai.store(BASE + 12 * S, Ordering::Relaxed);
    assert_eq!(tl.bounds().unwrap().live_end, BASE + 12 * S);
    // The margin a `live` set holds for the rest of the run.
    let a = tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert_eq!(a.tai, BASE + 11 * S);
}

#[test]
fn command_parses_live() {
    let c: Command = serde_json::from_str(r#"{"cmd":"live"}"#).unwrap();
    assert!(matches!(c, Command::Live { margin_ms: None }));
    let c: Command = serde_json::from_str(r#"{"cmd":"seek","to":{"t":"-5"}}"#).unwrap();
    assert_eq!(c, seek_tai(-5));
}

/// TAI start of the spike's live ingest (off the 50 fps second grid).
const INGEST: i64 = 1_795_526_553_080_598_792;

fn one_player(base: i64, domain: TimeDomain) -> (ReplayTimeline, Arc<PlayerStats>) {
    let p = player("cam1", base, base + 60 * S, Some((50, 1)));
    p.set_time_domain(domain);
    (timeline_of(std::slice::from_ref(&p)), p)
}

fn seek_utc(utc: &str) -> Command {
    seek_at(ReplayTarget {
        utc: Some(utc.to_string()),
        ..Default::default()
    })
}

#[test]
fn frame_time_is_utc_of_the_frame_start() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    let v = tl.view();
    let f = v.frame(INGEST).unwrap();
    assert_eq!(f.t, INGEST.to_string());
    assert_eq!(f.utc.as_deref(), Some("2026-11-24T13:21:56.080Z"));
    // Frames start at .000598792, .020598792, ... in each second.
    assert_eq!(f.frame_in_second, 4);
    let at = |k: i64| v.frame(INGEST + k * 20 * MS).unwrap().frame_in_second;
    assert_eq!(at(45), 49);
    assert_eq!(at(46), 0);
    // An instant inside a frame: the frame's start.
    assert_eq!(v.frame(INGEST + 7 * MS).unwrap().t, INGEST.to_string());
    assert_eq!(v.instant(INGEST).t, "1795526553080598792");
}

#[test]
fn seek_by_utc_names_the_nearest_frame_start() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    // Frames start at .000598792, .020598792, ... in each second. The frame
    // that starts at 13:22:00.000598792 is shown in UTC as
    // 13:22:00.000Z; seeking that text lands on it, not on the frame before.
    let a = tl
        .apply_at(seek_utc("2026-11-24T13:22:00.000Z"), 0)
        .unwrap();
    let start = utc_to_tai_ns(parse_utc("2026-11-24T13:22:00Z").unwrap()) + 598_792;
    assert_eq!(a.tai, start);
    assert_eq!(frame_no(&tl, a.tai), 196);
    // Every frame time the API gives in UTC seeks back to its own frame.
    let v = tl.view();
    for k in [0, 1, 195, 196, 1000] {
        let f = v.frame(INGEST + k * 20 * MS).unwrap();
        let a = tl.apply_at(seek_utc(f.utc.as_deref().unwrap()), 0).unwrap();
        assert_eq!(frame_no(&tl, a.tai), k);
    }
    // Up to half a frame either side of a start names that frame.
    let a = tl
        .apply_at(seek_utc("2026-11-24T13:22:00.010Z"), 0)
        .unwrap();
    assert_eq!(a.tai, start);
    let a = tl
        .apply_at(seek_utc("2026-11-24T13:22:00.011Z"), 0)
        .unwrap();
    assert_eq!(a.tai, start + 20 * MS);
    let a2 = tl
        .apply_at(seek_utc("2026-11-24T14:22:00.000+01:00"), 0)
        .unwrap();
    assert_eq!(a2.tai, start);
    // `t` is exact.
    let a3 = tl.apply_at(seek_tai(1_795_526_557 * S), 0).unwrap();
    assert_eq!(a3.tai, 1_795_526_557 * S);
    // Bad input changes nothing.
    assert!(tl.apply_at(seek_utc("13:22:00"), 0).is_err());
    let both = seek_at(ReplayTarget {
        t: Some("1".to_string()),
        utc: Some("2026-11-24T13:22:00Z".to_string()),
        ..Default::default()
    });
    assert!(matches!(
        tl.apply_at(both, 0),
        Err(CommandError::Invalid(_))
    ));
    let bad_t = seek_at(ReplayTarget {
        t: Some("1795526557:0".to_string()),
        ..Default::default()
    });
    assert!(matches!(
        tl.apply_at(bad_t, 0),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        tl.apply_at(seek_at(ReplayTarget::default()), 0),
        Err(CommandError::Invalid(_))
    ));
    assert_eq!(tl.tai_at(0), Some(1_795_526_557 * S));
}

#[test]
fn speed_is_validated() {
    let (tl, _p) = one_player(BASE, TimeDomain::Unanchored);
    assert!(tl.apply_at(Command::Speed { speed: 9.0 }, 0).is_err());
    assert!(tl.apply_at(Command::Speed { speed: f64::NAN }, 0).is_err());
    assert_eq!(speed_of(&tl), 0.0);
    assert!(tl.apply_at(Command::Speed { speed: -8.0 }, 0).is_ok());
}

#[test]
fn state_gives_utc_of_the_picture_on_air() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    let g = p.grid().unwrap();
    let a = tl.apply_at(seek(&tl, 195), tl.now_rt()).unwrap();
    // Nothing pushed yet: nothing on air.
    assert!(tl.state().on_air.is_none());
    // The player pushed a different instant from the cursor: on air is what
    // went out, not what was asked for.
    let pushed = a.tai - 5 * 20 * MS;
    p.shown_tai
        .store(g.tai_of_frame(g.floor_frame(pushed)), Ordering::Relaxed);
    tl.note_pushed(1, pushed);
    let s = tl.state();
    assert_eq!(s.time_domain, ReplayTimeDomain::Tai);
    let m = s.motion.unwrap();
    assert_eq!(m.cursor.at.t, a.tai.to_string());
    assert_eq!(
        m.cursor.frame.unwrap().utc.as_deref(),
        Some("2026-11-24T13:21:59.980Z")
    );
    let on_air = s.on_air.unwrap();
    assert_eq!(on_air.at.t, pushed.to_string());
    let f = on_air.frame.unwrap();
    assert_eq!(f.utc.as_deref(), Some("2026-11-24T13:21:59.880Z"));
    assert_eq!(f.frame_in_second, 44);
    assert_eq!(on_air.behind_live_ms, (3000 - 190) * 20 - 10);
    // Captured in the past, so the picture's age is positive.
    assert!(on_air.age_ms.unwrap() > 0);
    let shown = s.angles[0].shown.as_ref().unwrap();
    assert_eq!(shown.utc.as_deref(), Some("2026-11-24T13:21:59.880Z"));
    assert_eq!(s.angles[0].fps.as_deref(), Some("50/1"));
    let r = s.range.unwrap();
    assert_eq!(r.start.utc.as_deref(), Some("2026-11-24T13:21:56.080Z"));
    assert_eq!(r.live_edge.utc.as_deref(), Some("2026-11-24T13:22:56.080Z"));
    assert_eq!(s.grid.output.unwrap().fps, "50/1");
    assert_eq!(s.grid.source.unwrap().frame_ns, 20e6);
    assert!(tl.diagnostics().now_utc.ends_with('Z'));
}

#[test]
fn unanchored_times_have_no_utc_and_refuse_utc_targets() {
    let (tl, _p) = one_player(BASE, TimeDomain::Unanchored);
    tl.note_pushed(0, BASE);
    let s = tl.state();
    assert_eq!(s.time_domain, ReplayTimeDomain::Unanchored);
    let on_air = s.on_air.unwrap();
    assert_eq!(on_air.at.t, BASE.to_string());
    assert_eq!(on_air.at.utc, None);
    assert_eq!(on_air.frame.unwrap().utc, None);
    // No wall clock to compare with.
    assert!(on_air.age_ms.is_none());
    assert!(s.range.unwrap().start.utc.is_none());
    // A UTC target needs real TAI: 409, nothing changes.
    let before = tl.tai_at(0);
    let e = tl
        .apply_at(seek_utc(&format_utc_ms(tai_to_utc_ns(BASE + S))), 0)
        .unwrap_err();
    assert!(matches!(e, CommandError::Conflict(_)), "{:?}", e);
    assert_eq!(tl.tai_at(0), before);
}

#[test]
fn group_domain_needs_every_angle() {
    use TimeDomain::*;
    assert_eq!(TimeDomain::combine([Tai, Tai]), Tai);
    assert_eq!(TimeDomain::combine([Tai, Unknown]), Unknown);
    assert_eq!(TimeDomain::combine([Tai, Unanchored, Unknown]), Unanchored);
    assert_eq!(TimeDomain::combine([]), Unknown);
}

// --- mixed frame rates ----------------------------------------------------

/// The spike's mixed-rate production: 50, 25, 29.97 and 60 fps, each angle
/// started at its own time (own phase).
fn mixed() -> (ReplayTimeline, Vec<Arc<PlayerStats>>) {
    let ps = vec![
        player("cam1", BASE + 15 * MS, BASE + 300 * S, Some((50, 1))),
        player("cam2", BASE + 3_310 * MS, BASE + 300 * S, Some((25, 1))),
        player(
            "cam3",
            BASE + 7_707 * MS,
            BASE + 300 * S,
            Some((30000, 1001)),
        ),
        player("cam4", BASE + 12_101 * MS, BASE + 300 * S, Some((60, 1))),
    ];
    (timeline_of(&ps), ps)
}

#[test]
fn reference_is_the_finest_grid_whatever_reports_first() {
    let (tl, ps) = mixed();
    let g = tl.reference().unwrap();
    assert_eq!((g.fps_n, g.fps_d), (60, 1));
    // Reverse registration order: the same reference.
    let rev: Vec<_> = ps.iter().rev().cloned().collect();
    assert_eq!(timeline_of(&rev).reference(), Some(g));
    // Same rate, different phase: the smallest phase wins.
    let a = FrameGrid::new(50, 1, BASE + 15 * MS).unwrap();
    let b = FrameGrid::new(50, 1, BASE + MS).unwrap();
    assert_eq!(reference_grid([a, b]), Some(b));
    assert_eq!(reference_grid([b, a]), Some(b));
}

/// Frames each player shows on the ticks of an output at `out_fps`, playing
/// at `speed` from `from` (paused there at rt 0).
fn ticks(
    tl: &ReplayTimeline,
    grid: &FrameGrid,
    from: i64,
    speed: f64,
    out_fps: f64,
    n: usize,
) -> Vec<i64> {
    tl.apply_at(seek_tai(from), 0).unwrap();
    tl.apply_at(Command::Speed { speed }, 0).unwrap();
    let od = 1e9 / out_fps;
    (0..n)
        .map(|k| {
            let (f, t) = frame_on(tl, grid, (k as f64 * od) as i64).unwrap();
            // Sample and hold: the frame started at or before the cursor and
            // the next one had not started yet.
            assert!(grid.tai_of_frame(f) <= t && t < grid.tai_of_frame(f + 1));
            f
        })
        .collect()
}

fn steps(frames: &[i64]) -> Vec<i64> {
    frames.windows(2).map(|w| w[1] - w[0]).collect()
}

#[test]
fn every_angle_shows_its_frame_for_the_group_time() {
    let (tl, ps) = mixed();
    // One group instant: each angle shows the frame that holds it.
    let t = BASE + 20 * S + 7 * MS;
    tl.apply_at(seek_tai(t), 0).unwrap();
    for p in &ps {
        let g = p.grid().unwrap();
        let (f, cursor) = frame_on(&tl, &g, 0).unwrap();
        assert_eq!(cursor, t);
        assert!(g.tai_of_frame(f) <= t, "{}", p.block_id);
        assert!(t < g.tai_of_frame(f + 1), "{}", p.block_id);
    }
    // 25 fps from +3.310 s: frames at .310 + k*40 ms; 20.007 s is in the frame
    // that starts at 19.990 s.
    let g2 = ps[1].grid().unwrap();
    assert_eq!(
        g2.tai_of_frame(frame_on(&tl, &g2, 0).unwrap().0),
        BASE + 19_990 * MS
    );
}

#[test]
fn output_rate_against_source_rate_at_1x() {
    let (tl, ps) = mixed();
    let from = BASE + 30 * S;
    // 50p source on a 50p output: every frame once.
    let g = ps[0].grid().unwrap();
    assert!(steps(&ticks(&tl, &g, from, 1.0, 50.0, 200))
        .iter()
        .all(|d| *d == 1));
    // 25p on 50p: each frame twice.
    let g = ps[1].grid().unwrap();
    let s = steps(&ticks(&tl, &g, from, 1.0, 50.0, 200));
    assert_eq!(s.iter().filter(|d| **d == 0).count(), 100);
    assert!(s.iter().all(|d| *d == 0 || *d == 1));
    // 60p on 50p: one frame in six dropped, never two in a row.
    let g = ps[3].grid().unwrap();
    let f = ticks(&tl, &g, from, 1.0, 50.0, 251);
    let s = steps(&f);
    assert!(s.iter().all(|d| *d == 1 || *d == 2));
    assert_eq!(f[250] - f[0], 300);
    // 29.97p on its own rate: every frame once.
    let g = ps[2].grid().unwrap();
    assert!(steps(&ticks(&tl, &g, from, 1.0, 30000.0 / 1001.0, 300))
        .iter()
        .all(|d| *d == 1));
}

#[test]
fn slow_motion_from_high_frame_rate_shows_every_source_frame() {
    // A 100p angle at 50 % on a 50p output: one new source frame per tick.
    let p = player("hfr", BASE + 3 * MS, BASE + 60 * S, Some((100, 1)));
    let tl = timeline_of(std::slice::from_ref(&p));
    let g = p.grid().unwrap();
    // From a frame centre (where a jog or frame seek rests) and from an
    // instant exactly on a frame start.
    for from in [
        frame_centre(&g, g.floor_frame(BASE + 10 * S)),
        BASE + 10 * S + 3 * MS,
    ] {
        let s = steps(&ticks(&tl, &g, from, 0.5, 50.0, 500));
        assert!(s.iter().all(|d| *d == 1), "from {}", from - BASE);
    }
    // 60p at 5/6 on 50p: also every frame.
    let (tl, ps) = mixed();
    let g = ps[3].grid().unwrap();
    let from = frame_centre(&g, g.floor_frame(BASE + 40 * S));
    assert!(steps(&ticks(&tl, &g, from, 50.0 / 60.0, 50.0, 500))
        .iter()
        .all(|d| *d == 1));
    // 50p at 50 % on 50p: each frame twice, none skipped.
    let g = ps[0].grid().unwrap();
    let s = steps(&ticks(&tl, &g, from, 0.5, 50.0, 400));
    assert_eq!(s.iter().filter(|d| **d == 1).count(), 200);
    assert!(s.iter().all(|d| *d == 0 || *d == 1));
}

#[test]
fn reverse_walks_back_one_frame_per_tick() {
    let (tl, ps) = mixed();
    for (i, out) in [(0usize, 50.0), (3, 60.0), (2, 30000.0 / 1001.0)] {
        let g = ps[i].grid().unwrap();
        let from = frame_centre(&g, g.floor_frame(BASE + 100 * S));
        let s = steps(&ticks(&tl, &g, from, -1.0, out, 300));
        assert!(s.iter().all(|d| *d == -1), "{}", ps[i].block_id);
    }
}

#[test]
fn jog_steps_reference_frames_and_never_skips_an_angle() {
    let (tl, ps) = mixed();
    let g = tl.reference().unwrap();
    tl.apply_at(seek_tai(BASE + 50 * S), 0).unwrap();
    let mut last: Vec<i64> = ps
        .iter()
        .map(|p| frame_on(&tl, &p.grid().unwrap(), 0).unwrap().0)
        .collect();
    let start = g.floor_frame(tl.tai_at(0).unwrap());
    let g1 = ps[0].grid().unwrap();
    let f1_start = g1.floor_frame(frame_centre(&g, start));
    for i in 1..=120 {
        let a = tl.apply_at(Command::Jog { frames: 1 }, 0).unwrap();
        assert_eq!(g.floor_frame(a.tai), start + i);
        for (p, l) in ps.iter().zip(last.iter_mut()) {
            let f = frame_on(&tl, &p.grid().unwrap(), 0).unwrap().0;
            assert!(
                f - *l == 0 || f - *l == 1,
                "{} jumped {}",
                p.block_id,
                f - *l
            );
            *l = f;
        }
    }
    // 120 reference frames at 60 fps = 2 s: 100 frames of the 50p angle.
    let f1 = frame_on(&tl, &g1, 0).unwrap().0;
    assert_eq!(f1 - f1_start, 100);
}

#[test]
fn frames_across_segment_boundaries() {
    // 29.97 fps segments do not start on a frame of a whole second: one
    // second holds 29.97 frames. The angle's grid is anchored on its media,
    // so the instants where segments meet are frame starts like any other.
    let start = BASE + 7_707 * MS;
    let p = player("ntsc", start, start + 60 * S, Some((30000, 1001)));
    let tl = timeline_of(std::slice::from_ref(&p));
    let g = p.grid().unwrap();
    for sec in 9..60 {
        // First frame at or after each whole TAI second (where TAMS Output
        // cuts a segment), and the instant just before it.
        let cut = g.tai_of_frame(g.ceil_frame(BASE + sec * S));
        tl.apply_at(seek_tai(cut), 0).unwrap();
        let (f, _) = frame_on(&tl, &g, 0).unwrap();
        assert_eq!(g.tai_of_frame(f), cut);
        tl.apply_at(seek_tai(cut - 1), 0).unwrap();
        assert_eq!(frame_on(&tl, &g, 0).unwrap().0, f - 1);
    }
}

#[test]
fn group_start_is_frame_zero_on_every_grid() {
    let (tl, ps) = mixed();
    tl.bounds();
    for p in &ps {
        let g = p.grid().unwrap();
        assert_eq!(tl.origin_on(&g), Some(g.floor_frame(BASE + 15 * MS)));
    }
}

#[test]
fn a_new_run_resets_the_group_even_while_old_stats_live_on() {
    let old = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let tl = timeline_of(std::slice::from_ref(&old));
    tl.apply_at(seek_tai(BASE + 30 * S), 0).unwrap();
    assert_eq!(tl.origin_tai(), BASE);
    // The flow stops: the player unregisters, but its stats are still held
    // (a fetch thread winding down).
    tl.unregister_player(&old);
    // The rebuilt flow's player has another source.
    let new = player("cam0", BASE + 100 * S, BASE + 160 * S, Some((25, 1)));
    tl.register_player(&new);
    assert_eq!(tl.origin_tai(), UNKNOWN);
    let b = tl.bounds().unwrap();
    assert_eq!(b.start, BASE + 100 * S, "the old extent no longer counts");
    assert_eq!(tl.reference().map(|g| g.fps_n), Some(25));
    // The cursor starts over at the new start.
    assert_eq!(tl.tai_at(0), Some(BASE + 100 * S));
    drop(old);
}

#[test]
fn huge_client_values_clamp_instead_of_overflowing() {
    let (tl, _keep) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let end = BASE + 60 * S - 1;
    let jog = |frames| tl.apply_at(Command::Jog { frames }, 0).unwrap().tai;
    assert_eq!(jog(i64::MAX), end);
    assert_eq!(jog(i64::MIN), BASE);
    assert_eq!(tl.apply_at(seek_tai(i64::MAX), 0).unwrap().tai, end);
    assert_eq!(tl.apply_at(seek_tai(i64::MIN), 0).unwrap().tai, BASE);
    let rel = |ms| {
        tl.apply_at(
            seek_at(ReplayTarget {
                rel_ms: Some(ms),
                ..Default::default()
            }),
            0,
        )
        .unwrap()
        .tai
    };
    assert_eq!(rel(i64::MAX), end);
    assert_eq!(rel(i64::MIN), BASE);
}

#[test]
fn a_timeline_nobody_holds_is_dropped() {
    let flow = uuid::Uuid::new_v4();
    let tl = get_or_create(flow, "prune-test");
    assert!(get(flow, "prune-test").is_some());
    assert!(Arc::ptr_eq(&tl, &get_or_create(flow, "prune-test")));
    drop(tl);
    assert!(get(flow, "prune-test").is_none());
}

// --- command schema ------------------------------------------------------

fn parse(s: &str) -> Result<Command, CommandError> {
    parse_command(serde_json::from_str(s).unwrap()).map(|r| r.command)
}

#[test]
fn commands_parse_with_seq_and_refuse_unknown_fields() {
    assert_eq!(
        parse(r#"{"cmd":"jog","frames":-2,"seq":7}"#).unwrap(),
        Command::Jog { frames: -2 }
    );
    // A typo is an error, not a silently ignored field.
    let e = parse(r#"{"cmd":"seek","too":{"t":"3"}}"#).unwrap_err();
    assert!(matches!(e, CommandError::Invalid(_)), "{:?}", e);
    assert!(e.message().contains("too"), "{}", e);
    assert!(matches!(
        parse(r#"{"cmd":"fly"}"#),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        parse(r#"{"cmd":"jog"}"#),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(parse(r#"[1]"#), Err(CommandError::Invalid(_))));
    // seek_rel is gone: a relative seek is `rel_ms`.
    assert!(parse(r#"{"cmd":"seek_rel","ms":100}"#).is_err());
    // The schema round-trips.
    let c = Command::Live {
        margin_ms: Some(500),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, serde_json::json!({"cmd": "live", "margin_ms": 500}));
    assert_eq!(parse_command(v).unwrap().command, c);
}

#[test]
fn client_ref_is_taken_from_any_command_and_checked() {
    let r = parse_command(serde_json::json!({"cmd": "pause", "client_ref": "clip-17", "seq": 3}))
        .unwrap();
    assert_eq!(r.command, Command::Pause {});
    assert_eq!(r.client_ref.as_deref(), Some("clip-17"));
    let long = "x".repeat(MAX_CLIENT_REF_LEN + 1);
    assert!(matches!(
        parse_command(serde_json::json!({"cmd": "pause", "client_ref": long})),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        parse_command(serde_json::json!({"cmd": "pause", "client_ref": 5})),
        Err(CommandError::Invalid(_))
    ));
    // Echoed in the result.
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let a = tl.apply_req_at(r, 0).unwrap();
    assert_eq!(tl.result(&a).client_ref.as_deref(), Some("clip-17"));
}

#[test]
fn errors_tell_invalid_from_conflict() {
    // No media yet: valid commands do not apply.
    let tl = ReplayTimeline::new();
    assert!(matches!(
        tl.apply_at(parse(r#"{"cmd":"play"}"#).unwrap(), 0),
        Err(CommandError::Conflict(_))
    ));
    // Media but no frame rate yet: a jog does not apply.
    let p = player("cam1", BASE, BASE + 10 * S, None);
    let tl = timeline_of(std::slice::from_ref(&p));
    assert!(matches!(
        tl.apply_at(Command::Jog { frames: 1 }, 0),
        Err(CommandError::Conflict(_))
    ));
    assert!(tl.apply_at(seek_tai(BASE + S), 0).is_ok());
    // Bad values are invalid whatever the state.
    assert!(matches!(
        tl.apply_at(Command::Speed { speed: 20.0 }, 0),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        tl.apply_at(seek_utc("yesterday"), 0),
        Err(CommandError::Invalid(_))
    ));
}

#[test]
fn result_is_the_motion_with_the_cursor_frame() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    let a = tl.apply_at(seek(&tl, 195), 0).unwrap();
    let r = tl.result(&a);
    assert_eq!(r.motion.cursor.at.t, a.tai.to_string());
    let f = r.motion.cursor.frame.unwrap();
    assert_eq!(f.utc.as_deref(), Some("2026-11-24T13:21:59.980Z"));
    // The exact time is never replaced by the frame's start.
    assert_ne!(f.t, r.motion.cursor.at.t);
    assert_eq!(r.motion.speed, 0.0);
    assert!(!r.motion.live && r.motion.cue.is_none() && r.motion.ramp.is_none());
}

#[test]
fn seek_from_live_with_speed_is_one_step() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S), (BASE, BASE + 50 * S)]);
    tl.apply_at(Command::Pause {}, 0).unwrap();
    let c = parse(r#"{"cmd":"seek","to":{"from_live_ms":8000},"speed":0.5}"#).unwrap();
    let a = tl.apply_at(c, 0).unwrap();
    // 8 s before the last common frame (50 s), already at 50 %.
    let last = BASE + 50 * S - 1;
    assert_eq!(a.tai, last - 8 * S);
    assert_eq!(a.speed, 0.5);
    assert_eq!(tl.tai_at(S), Some(last - 8 * S + S / 2));
    // Without speed the speed is kept.
    let a = tl
        .apply_at(
            parse(r#"{"cmd":"seek","to":{"from_live_ms":1000}}"#).unwrap(),
            0,
        )
        .unwrap();
    assert_eq!((a.tai, a.speed), (last - S, 0.5));
    // A bad speed refuses the whole command: the cursor does not move.
    let bad = parse(r#"{"cmd":"seek","to":{"from_live_ms":0},"speed":12}"#).unwrap();
    assert!(matches!(tl.apply_at(bad, 0), Err(CommandError::Invalid(_))));
    assert_eq!(tl.tai_at(0), Some(last - S));
    // Two targets, or a negative distance, are invalid.
    assert!(matches!(
        parse(r#"{"cmd":"seek","to":{"from_live_ms":10,"t":"1"}}"#).map(|c| tl.apply_at(c, 0)),
        Ok(Err(CommandError::Invalid(_)))
    ));
    assert!(parse(r#"{"cmd":"seek","to":{"from_live_ms":-5}}"#).is_err());
    // Further back than the media: clamped to the start.
    let far = parse(r#"{"cmd":"seek","to":{"from_live_ms":18446744073709551615}}"#).unwrap();
    assert_eq!(tl.apply_at(far, 0).unwrap().tai, BASE);
}

// --- cue and bounded play -------------------------------------------------

fn cue(start: i64, end: i64, end_action: Option<EndAction>) -> Command {
    Command::Cue {
        in_point: at_tai(start),
        out: at_tai(end),
        end_action,
        source: None,
        block: None,
    }
}

fn play(speed: Option<f64>, until: Option<i64>, end_action: Option<EndAction>) -> Command {
    Command::Play {
        speed,
        until: until.map(at_tai),
        end_action,
    }
}

#[test]
fn cue_parks_paused_at_in_and_parses_in_and_out() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let c = parse(&format!(
        r#"{{"cmd":"cue","in":{{"t":"{}"}},"out":{{"t":"{}"}}}}"#,
        BASE + 10 * S,
        BASE + 12 * S
    ))
    .unwrap();
    assert_eq!(c, cue(BASE + 10 * S, BASE + 12 * S, None));
    let a = tl.apply_at(c, 0).unwrap();
    assert_eq!((a.tai, a.speed), (BASE + 10 * S, 0.0));
    let r = tl.result(&a).motion.cue.unwrap();
    assert_eq!(r.end_action, EndAction::Freeze);
    assert_eq!(r.out.t, (BASE + 12 * S).to_string());
    // out before in, or a cue outside the media, change nothing.
    let bad = cue(BASE + 12 * S, BASE + 10 * S, None);
    assert!(matches!(tl.apply_at(bad, 0), Err(CommandError::Invalid(_))));
    let outside = cue(BASE + 70 * S, BASE + 80 * S, None);
    let e = tl.apply_at(outside, 0).unwrap_err();
    assert!(matches!(e, CommandError::OutOfRange(_)), "{:?}", e);
    assert_eq!(e.code(), strom_types::replay::ReplayErrorCode::OutOfRange);
    assert_eq!(tl.cue().map(|r| r.start), Some(BASE + 10 * S));
}

#[test]
fn play_freezes_on_the_last_frame_before_out() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, None), 0)
        .unwrap();
    tl.apply_at(play(Some(0.5), None, None), 0).unwrap();
    assert_eq!(tl.cursor_tai(2 * S), Some((BASE + 11 * S, 0.5)));
    // 2 s of clip at 50 % take 4 s; after that the cursor holds out - 1.
    let (t, sp) = tl.cursor_tai(5 * S).unwrap();
    assert_eq!((t, sp), (BASE + 12 * S - 1, 0.0));
    let g = tl.reference().unwrap();
    assert_eq!(g.tai_of_frame(g.floor_frame(t)), BASE + 12 * S - 20 * MS);
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::CueEnd);
    assert!(
        matches!(&ev.kind, ReplayEventKind::Ended { at } if at.t == (BASE + 12 * S - 1).to_string())
    );
    // Frozen for good: paused, one event, the cue still held.
    assert_eq!(tl.cursor_tai(9 * S), Some((BASE + 12 * S - 1, 0.0)));
    assert!(events.try_recv().is_err());
    assert!(tl.cue().is_some());
    // Play again at the end: from in.
    let a = tl.apply_at(play(None, None, None), 9 * S).unwrap();
    assert_eq!((a.tai, a.speed), (BASE + 10 * S, 1.0));
}

#[test]
fn reverse_play_freezes_at_in() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, None), 0)
        .unwrap();
    tl.apply_at(seek_tai(BASE + 11 * S), 0).unwrap();
    tl.apply_at(play(Some(-1.0), None, None), 0).unwrap();
    assert_eq!(tl.cursor_tai(3 * S), Some((BASE + 10 * S, 0.0)));
    // At in, reverse play starts again from out.
    let a = tl.apply_at(play(Some(-1.0), None, None), 3 * S).unwrap();
    assert_eq!(a.tai, BASE + 12 * S - 1);
}

#[test]
fn loop_wraps_inside_the_cue() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, Some(EndAction::Loop)), 0)
        .unwrap();
    tl.apply_at(play(None, None, None), 0).unwrap();
    assert_eq!(tl.tai_at(3 * S), Some(BASE + 11 * S));
    assert_eq!(tl.tai_at(4 * S + 500 * MS), Some(BASE + 10 * S + 500 * MS));
    assert_eq!(tl.cursor_tai(4 * S).unwrap().1, 1.0);
    // Reverse at in starts from out, and wraps to out again.
    let a = tl.apply_at(play(Some(-2.0), None, None), 10 * S).unwrap();
    assert_eq!(a.tai, BASE + 12 * S - 1);
    assert_eq!(tl.tai_at(10 * S + 500 * MS), Some(BASE + 11 * S - 1));
    assert_eq!(
        tl.tai_at(11 * S + 250 * MS),
        Some(BASE + 11 * S + 500 * MS - 1)
    );
    assert!(events.try_recv().is_err());
}

#[test]
fn end_action_live_goes_live_with_an_event() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, Some(EndAction::Live)), 0)
        .unwrap();
    tl.apply_at(play(Some(2.0), None, None), 0).unwrap();
    // Reaches out after 1 s: live, 1.5 s behind the edge, at 1x, no cue.
    let (t, sp) = tl.cursor_tai(S + 1).unwrap();
    assert_eq!((t, sp), (BASE + 60 * S - 1500 * MS, 1.0));
    assert!(tl.cue().is_none());
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::CueEnd);
    assert!(matches!(ev.kind, ReplayEventKind::Live { .. }));
    assert_eq!(tl.tai_at(2 * S + 1), Some(BASE + 60 * S - 500 * MS));
}

#[test]
fn play_until_bounds_without_a_cue() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
    // until behind the cursor does not apply.
    assert!(matches!(
        tl.apply_at(play(None, Some(BASE + 19 * S), None), 0),
        Err(CommandError::Conflict(_))
    ));
    // end_action without until or cue is invalid.
    assert!(matches!(
        tl.apply_at(play(None, None, Some(EndAction::Loop)), 0),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        tl.apply_at(play(Some(0.0), None, None), 0),
        Err(CommandError::Invalid(_))
    ));
    let a = tl
        .apply_at(play(None, Some(BASE + 21 * S), Some(EndAction::Loop)), 0)
        .unwrap();
    let r = a.region.unwrap();
    assert_eq!((r.start, r.end), (BASE + 20 * S, BASE + 21 * S));
    assert_eq!(tl.tai_at(S + 250 * MS), Some(BASE + 20 * S + 250 * MS));
}

#[test]
fn leaving_the_cue_drops_it() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, None), 0)
        .unwrap();
    // Moves inside keep it, speed and pause keep it.
    tl.apply_at(Command::Jog { frames: 3 }, 0).unwrap();
    tl.apply_at(Command::Speed { speed: 0.5 }, 0).unwrap();
    tl.apply_at(Command::Pause {}, 0).unwrap();
    assert!(tl.cue().is_some());
    // A seek outside drops it, and play runs on unbounded.
    tl.apply_at(seek_tai(BASE + 30 * S), 0).unwrap();
    assert!(tl.cue().is_none());
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, None), 0)
        .unwrap();
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert!(tl.cue().is_none());
    tl.apply_at(cue(BASE + 10 * S, BASE + 12 * S, None), 0)
        .unwrap();
    let a = tl.apply_at(Command::Uncue {}, 0).unwrap();
    assert!(a.region.is_none() && tl.cue().is_none());
    assert_eq!(a.tai, BASE + 10 * S);
}

// --- catching up with the live edge ----------------------------------------

#[test]
fn fast_play_that_catches_up_goes_live_with_an_event() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    // 10 s behind the edge at 2x: the live point (1.5 s behind the edge) is
    // 8.5 s of media ahead, reached after 4.25 s.
    tl.apply_at(seek_tai(BASE + 50 * S), 0).unwrap();
    tl.apply_at(Command::Speed { speed: 2.0 }, 0).unwrap();
    assert_eq!(tl.cursor_tai(4 * S), Some((BASE + 58 * S, 2.0)));
    assert!(events.try_recv().is_err());
    // The edge grows meanwhile: a segment arrives.
    p[0].end_tai.store(BASE + 61 * S, Ordering::Relaxed);
    let (t, sp) = tl.cursor_tai(5 * S).unwrap();
    // Caught up: goes on from where it is, at 1x, no jump back.
    assert_eq!((t, sp), (BASE + 60 * S, 1.0));
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::CaughtUp);
    assert!(matches!(ev.kind, ReplayEventKind::Live { .. }));
    p[0].end_tai.store(BASE + 62 * S, Ordering::Relaxed);
    assert_eq!(tl.cursor_tai(6 * S), Some((BASE + 61 * S, 1.0)));
    assert!(tl.state().motion.unwrap().live);
    // Following live: no second event.
    tl.cursor_tai(9 * S);
    assert!(events.try_recv().is_err());
}

/// `live` sent right after a start, while one angle's index still ends where
/// the previous run ended: the line starts from that stale edge. Once the
/// angle catches up, the group jumps to the live point instead of playing the
/// hole between the runs for good (seen live: a frozen picture for 40 s).
#[test]
fn following_live_jumps_forward_when_a_lagging_edge_catches_up() {
    // cam1 has the new run up to 100 s; cam2's index still ends at 60 s.
    let (tl, p) = timeline_with(&[(BASE, BASE + 100 * S), (BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    let a = tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert_eq!(a.tai, BASE + 60 * S - 1500 * MS);
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Live);
    // Still lagging: the group plays on and holds at the common edge.
    assert_eq!(tl.cursor_tai(S), Some((BASE + 59_500 * MS, 1.0)));
    assert!(events.try_recv().is_err());
    // cam2's index catches up: 1.5 s behind the new edge, not 40 s.
    p[1].end_tai.store(BASE + 100 * S, Ordering::Relaxed);
    assert_eq!(tl.cursor_tai(2 * S), Some((BASE + 98_500 * MS, 1.0)));
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::EdgeMoved);
    assert!(matches!(ev.kind, ReplayEventKind::Live { .. }));
    assert!(tl.state().motion.unwrap().live);
    // Then it follows at 1x along the new line.
    assert_eq!(tl.cursor_tai(3 * S), Some((BASE + 99_500 * MS, 1.0)));
    p[0].end_tai.store(BASE + 101 * S, Ordering::Relaxed);
    p[1].end_tai.store(BASE + 101 * S, Ordering::Relaxed);
    assert_eq!(tl.cursor_tai(4 * S), Some((BASE + 100_500 * MS, 1.0)));
    assert!(events.try_recv().is_err());
}

/// The edge grows a segment at a time while the cursor moves smoothly: a
/// cursor up to one step further behind than the margin is on its line.
#[test]
fn following_live_rides_out_the_edge_steps() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    // `live` just before the edge steps: right after each step the cursor is
    // almost a whole step further behind the live point than the margin.
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Live);
    for k in 1..=10 {
        p[0].end_tai.store(BASE + (60 + k) * S, Ordering::Relaxed);
        let rt = (k - 1) * S + 10 * MS;
        assert_eq!(
            tl.cursor_tai(rt),
            Some((BASE + 58_500 * MS + rt, 1.0)),
            "second {}",
            k
        );
    }
    assert!(events.try_recv().is_err());
}

#[test]
fn play_inside_the_live_margin_does_not_go_live() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    // Already within the margin when the command comes: slow motion stays
    // slow motion.
    let c = parse(r#"{"cmd":"seek","to":{"from_live_ms":1000},"speed":0.5}"#).unwrap();
    tl.apply_at(c, 0).unwrap();
    assert_eq!(tl.cursor_tai(S).unwrap().1, 0.5);
    assert!(events.try_recv().is_err());
    // `live` is live with an event caused by the command; another command
    // leaves it.
    let a = tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert!(a.live && tl.result(&a).motion.live);
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Live);
    assert!(events.try_recv().is_err());
    assert!(!tl.apply_at(Command::Pause {}, 0).unwrap().live);
    // A cue's bounds win over catching up.
    tl.apply_at(cue(BASE + 50 * S, BASE + 59 * S, None), 0)
        .unwrap();
    tl.apply_at(play(Some(4.0), None, None), 0).unwrap();
    assert_eq!(tl.cursor_tai(3 * S), Some((BASE + 59 * S - 1, 0.0)));
    assert!(matches!(
        next(&mut events).unwrap().kind,
        ReplayEventKind::Ended { .. }
    ));
}

// --- speed ramps -----------------------------------------------------------

#[test]
fn ramp_changes_speed_linearly_on_the_server() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let from = BASE + 10 * S;
    tl.apply_at(seek_tai(from), 0).unwrap();
    let c = parse(r#"{"cmd":"ramp","to":1.0,"duration_ms":1000}"#).unwrap();
    assert_eq!(
        c,
        Command::Ramp {
            to: 1.0,
            duration_ms: 1000
        }
    );
    tl.apply_at(c, 0).unwrap();
    // Halfway: half speed, a quarter of the way (v t / 2).
    assert_eq!(tl.cursor_tai(S / 2), Some((from + 125 * MS, 0.5)));
    assert_eq!(tl.cursor_tai(S), Some((from + S / 2, 1.0)));
    assert_eq!(tl.cursor_tai(2 * S), Some((from + 3 * S / 2, 1.0)));
    // A ramp from the speed it is at: 1x down to 0.25x.
    tl.apply_at(
        Command::Ramp {
            to: 0.25,
            duration_ms: 2000,
        },
        2 * S,
    )
    .unwrap();
    let (t, v) = tl.cursor_tai(3 * S).unwrap();
    assert_eq!(v, 0.625);
    assert_eq!(t, from + 3 * S / 2 + 812_500_000);
    // A speed command ends the ramp where it is.
    let a = tl.apply_at(Command::Speed { speed: 2.0 }, 3 * S).unwrap();
    assert_eq!(a.tai, t);
    assert_eq!(tl.cursor_tai(4 * S), Some((t + 2 * S, 2.0)));
}

#[test]
fn ramp_is_validated_and_keeps_the_cue() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let bad = |to, duration_ms| {
        matches!(
            tl.apply_at(Command::Ramp { to, duration_ms }, 0),
            Err(CommandError::Invalid(_))
        )
    };
    assert!(bad(9.0, 100));
    assert!(bad(f64::NAN, 100));
    assert!(bad(1.0, MAX_RAMP_MS + 1));
    // Zero duration is a plain speed change.
    let a = tl
        .apply_at(
            Command::Ramp {
                to: 0.5,
                duration_ms: 0,
            },
            0,
        )
        .unwrap();
    assert_eq!(a.speed, 0.5);
    // Ramping up inside a cue still freezes at out.
    tl.apply_at(cue(BASE + 10 * S, BASE + 11 * S, None), 0)
        .unwrap();
    tl.apply_at(
        Command::Ramp {
            to: 4.0,
            duration_ms: 1000,
        },
        0,
    )
    .unwrap();
    assert!(tl.cue().is_some());
    assert_eq!(tl.cursor_tai(5 * S), Some((BASE + 11 * S - 1, 0.0)));
    assert!(tl.state().motion.unwrap().ramp.is_none());
}

// --- marks -----------------------------------------------------------------

#[test]
fn mark_stamps_the_pushed_frame_not_the_cursor() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    let g = p.grid().unwrap();
    let a = tl.apply_at(seek(&tl, 195), 0).unwrap();
    // Nothing went out yet: a mark does not apply.
    assert!(matches!(
        tl.apply_at(
            Command::Mark {
                label: None,
                at: None
            },
            0
        ),
        Err(CommandError::Conflict(_))
    ));
    // The players are 5 frames behind the cursor (a seek in flight).
    let pushed = a.tai - 5 * 20 * MS;
    p.shown_tai
        .store(g.tai_of_frame(g.floor_frame(pushed)), Ordering::Relaxed);
    tl.note_pushed(1, pushed);
    let c = parse(r#"{"cmd":"mark","label":"goal"}"#).unwrap();
    let m = tl.apply_at(c, 0).unwrap();
    // The cursor did not move.
    assert_eq!((m.tai, m.speed), (a.tai, 0.0));
    let r = tl.result(&m);
    let mark = r.mark.unwrap();
    assert_eq!(mark.label.as_deref(), Some("goal"));
    // The exact time of the tick, and its output frame for the timecode.
    assert_eq!(mark.at.t, pushed.to_string());
    let f = mark.frame.unwrap();
    assert_eq!(f.utc.as_deref(), Some("2026-11-24T13:21:59.880Z"));
    assert_eq!(mark.angles.len(), 1);
    assert_eq!(mark.angles[0].block, "cam1");
    assert_eq!(mark.angles[0].shown.t, f.t);
    // A cue from the mark's `t` starts on the same picture.
    let c = tl.apply_at(cue(pushed, pushed + S, None), 0).unwrap();
    assert_eq!(c.tai, pushed);
    assert_eq!(g.floor_frame(c.tai), g.floor_frame(pushed));
    // A mark leaves the motion alone: play goes on.
    tl.apply_at(Command::Uncue {}, 0).unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 0).unwrap();
    tl.apply_at(
        Command::Mark {
            label: None,
            at: None,
        },
        S,
    )
    .unwrap();
    assert_eq!(tl.cursor_tai(2 * S), Some((pushed + 2 * S, 1.0)));
}

#[test]
fn mark_at_stamps_another_point_and_refuses_one_outside() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    let g = p.grid().unwrap();
    let at = INGEST + 10 * S + 7 * MS;
    // Nothing has gone out: a mark with `at` still applies.
    let m = tl
        .apply_at(
            Command::Mark {
                label: Some("x".into()),
                at: Some(ReplayTarget::t(at)),
            },
            0,
        )
        .unwrap();
    let mark = m.mark.unwrap();
    assert_eq!(mark.at.t, at.to_string());
    assert_eq!(
        mark.angles[0].shown.t,
        g.tai_of_frame(g.floor_frame(at)).to_string()
    );
    // Relative to the cursor.
    let m = tl
        .apply_at(
            Command::Mark {
                label: None,
                at: Some(ReplayTarget {
                    rel_ms: Some(2000),
                    ..Default::default()
                }),
            },
            0,
        )
        .unwrap();
    assert_eq!(m.mark.unwrap().at.t, (INGEST + 2 * S).to_string());
    let outside = Command::Mark {
        label: None,
        at: Some(ReplayTarget::t(INGEST + 61 * S)),
    };
    assert!(matches!(
        tl.apply_at(outside, 0),
        Err(CommandError::OutOfRange(_))
    ));
}

// --- live edge estimate ----------------------------------------------------

#[test]
fn live_edge_estimate_runs_smoothly_between_steps() {
    let mut t = EdgeTracker::default();
    // Not growing yet: the known edge.
    t.observe(0, BASE);
    assert_eq!(t.estimate(500 * MS, BASE), (BASE, false));
    // 1 s segments, seen 0..60 ms late (polling).
    for (k, late) in [(1, 40), (2, 0), (3, 60), (4, 20)] {
        t.observe(k * S + late * MS, BASE + k * S);
    }
    let edge = BASE + 4 * S;
    // Never behind the known edge.
    assert_eq!(
        t.estimate(4 * S + 10 * MS, edge).0,
        edge.max(BASE + 4 * S + 10 * MS)
    );
    // The line set by the step seen with no delay (2 s): edge = BASE + m.
    assert_eq!(
        t.estimate(4 * S + 300 * MS, edge),
        (BASE + 4 * S + 300 * MS, true)
    );
    // Never more than a step ahead of the known edge.
    assert_eq!(t.estimate(5 * S + 500 * MS, edge), (edge + S, true));
    // No step for over three step lengths: the edge stopped growing; the
    // estimate does not step back.
    assert_eq!(t.estimate(8 * S, edge), (edge + S, false));
    // Repeats change nothing; a jump back starts the steps over, but the
    // estimate does not go back with it.
    t.observe(9 * S, edge);
    t.observe(9 * S, BASE);
    assert_eq!(t.estimate(9 * S + 1, BASE), (edge + S, false));
    assert_eq!(t.step(), None);
}

#[test]
fn live_edge_estimate_is_monotone_and_updated_every_tick() {
    // 1 s segments seen with jitter: the raw line moves when the step with
    // the least delay drops out of the window, and the known edge sits a
    // step behind the estimate before each step.
    let mut t = EdgeTracker::default();
    let lates = [0, 90, 30, 120, 10, 200, 60, 5, 150, 40];
    let mut last = i64::MIN;
    let mut seen = 0;
    for ms in (0..60_000i64).step_by(20) {
        let mono = ms * MS;
        let k = ms / 1000;
        let late = lates[(k as usize) % lates.len()] * MS;
        let edge = if mono >= k * S + late { k } else { k - 1 };
        let edge = BASE + edge.max(0) * S;
        t.observe(mono, edge);
        let (e, _) = t.estimate(mono, edge);
        assert!(e >= last, "stepped back at {} ms: {} < {}", ms, e, last);
        assert!(e >= edge && e <= edge + S);
        if e > last {
            seen += 1;
        }
        last = e;
    }
    // It moves on (nearly) every tick, not once a step.
    assert!(seen > 2500, "moved on {} of 3000 ticks", seen);
}

#[test]
fn state_lists_extents_gaps_and_the_estimate() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    p.set_index_view(IndexView {
        listed: vec![(INGEST, INGEST + 60 * S)],
        gaps: vec![(INGEST + 10 * S, INGEST + 12 * S)],
    });
    let s = tl.state();
    let a0 = &s.angles[0];
    assert_eq!(a0.extent.as_ref().unwrap().start.t, INGEST.to_string());
    assert_eq!(a0.gaps[0].duration_ms, 2000);
    assert_eq!(a0.gaps[0].start.t, (INGEST + 10 * S).to_string());
    assert_eq!(a0.offset_ms, 0);
    assert_eq!(a0.ready, ReplayReady::Warming);
    let from = a0.available_from.as_ref().unwrap();
    assert_eq!(from.t, INGEST.to_string());
    assert!(from.utc.is_some(), "an instant: utc on real TAI");
    let d = tl.diagnostics();
    assert_eq!(d.angles[0].indexed[0].duration_ms, 60_000);
    assert_eq!(d.angles[0].gone_segments, 0);
    // Retention deletes the first 5 s: the buffer starts later.
    p.start_tai.store(INGEST + 5 * S, Ordering::Relaxed);
    let s2 = tl.state();
    let from = s2.angles[0].available_from.as_ref().unwrap();
    assert_eq!(from.t, (INGEST + 5 * S).to_string());
    // A recording that does not grow: the estimate is the known edge.
    let r = s.range.unwrap();
    assert!(!r.growing);
    assert_eq!(r.live_edge_estimate.t, (INGEST + 60 * S).to_string());
    assert_eq!(r.end.t, (INGEST + 60 * S).to_string());
}

#[test]
fn following_live_holds_at_the_common_edge_when_it_stalls() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S), (BASE, BASE + 58 * S)]);
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    // The edge (58 s, where both angles have media) does not grow: hold
    // there, not at the longer angle's end.
    assert_eq!(tl.tai_at(5 * S), Some(BASE + 58 * S - 1));
    // It grows again (by what was captured meanwhile): play on along the
    // same line.
    p[1].end_tai.store(BASE + 63 * S, Ordering::Relaxed);
    assert_eq!(tl.tai_at(5 * S), Some(BASE + 60 * S - 1));
    p[0].end_tai.store(BASE + 63 * S, Ordering::Relaxed);
    assert_eq!(tl.tai_at(5 * S), Some(BASE + 61_500 * MS));
}

#[test]
fn live_edge_steps_of_several_angles_count_as_one() {
    // Four angles whose segments end 1 ms apart, stepping 100 ms apart:
    // each second the common edge moves 1 ms three times, then to the
    // first angle's new end. (Seen live with the split ingest; the median
    // step was then 1 ms and the estimate fell back to the known edge.)
    let mut t = EdgeTracker::default();
    t.observe(0, BASE);
    for k in 1..5 {
        for j in 1..=3 {
            t.observe(k * S - (4 - j) * 100 * MS, BASE + (k - 1) * S + j * MS);
        }
        t.observe(k * S, BASE + k * S);
    }
    let edge = BASE + 4 * S;
    // Still growing 1 s per second, and smooth between steps.
    let (e1, g1) = t.estimate(4 * S + 100 * MS, edge);
    let (e2, g2) = t.estimate(4 * S + 850 * MS, edge);
    assert!(g1 && g2);
    assert_eq!(e2 - e1, 750 * MS);
}

#[test]
fn the_origin_waits_for_every_angle_whatever_answers_first() {
    // cam1 starts 9.26 s before cam0, but its index answers later.
    let cam0 = player("cam0", BASE + 9260 * MS, BASE + 60 * S, Some((50, 1)));
    let cam1 = player("cam1", UNKNOWN, UNKNOWN, Some((50, 1)));
    let tl = timeline_of(&[cam0.clone(), cam1.clone()]);
    let b = tl.bounds().unwrap();
    assert_eq!(b.start, BASE + 9260 * MS);
    assert_eq!(tl.origin_tai(), UNKNOWN, "cam1 has not reported");
    tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
    assert_eq!(tl.ready(), ReplayReady::Indexing);
    cam1.start_tai.store(BASE, Ordering::Relaxed);
    cam1.end_tai.store(BASE + 60 * S, Ordering::Relaxed);
    tl.bounds();
    assert_eq!(tl.origin_tai(), BASE);
    assert_eq!(frame_no(&tl, tl.tai_at(0).unwrap()), 1000);
}

#[test]
fn frame_zero_is_fixed_without_an_angle_that_never_answers() {
    let cam0 = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let dead = player("dead", UNKNOWN, UNKNOWN, None);
    let tl = timeline_of(&[cam0.clone(), dead.clone()]);
    let t0 = Instant::now();
    tl.bounds_at(t0);
    assert_eq!(tl.origin_tai(), UNKNOWN);
    tl.bounds_at(t0 + ORIGIN_WAIT);
    assert_eq!(tl.origin_tai(), BASE);
}

#[test]
fn shown_time_is_the_frame_own_time_after_a_phase_change() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    let g = p.grid().unwrap();
    tl.bounds();
    // After a restart the source's frames start 8.8 ms later in their
    // period than the player's grid (anchored on the first run).
    let own = g.tai_of_frame(g.floor_frame(INGEST) + 190) + 8_800_000;
    p.shown_tai.store(own, Ordering::Relaxed);
    let s = tl.state();
    let shown = s.angles[0].shown.as_ref().unwrap();
    assert_eq!(shown.t, own.to_string());
    assert_eq!(shown.utc.as_deref(), Some("2026-11-24T13:21:59.889Z"));
    assert_eq!(shown.frame_in_second, 44);
}

// --- locking ---------------------------------------------------------------

/// A flow restarts (its player registers again, resetting the group) while a
/// client sends marks. Registering held `players` and took `anchor`; a mark
/// held `anchor` and took `players`: the two could lock each other for good.
#[test]
fn marks_during_a_restart_do_not_deadlock() {
    let tl = Arc::new(ReplayTimeline::new());
    let p = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    tl.register_player(&p);
    tl.note_pushed(0, BASE + S);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restarts = {
        let (tl, p, stop) = (tl.clone(), p.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                tl.unregister_player(&p);
                tl.register_player(&p);
                tl.note_pushed(0, BASE + S);
            }
        })
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let tl = tl.clone();
        std::thread::spawn(move || {
            for _ in 0..200_000 {
                let _ = tl.apply_at(
                    Command::Mark {
                        label: None,
                        at: None,
                    },
                    0,
                );
            }
            let _ = done_tx.send(());
        });
    }
    let finished = done_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .is_ok();
    stop.store(true, Ordering::Relaxed);
    assert!(finished, "mark and register_player deadlocked");
    restarts.join().unwrap();
}

// --- media bounds, new runs and time base changes --------------------------

#[test]
fn an_out_past_the_media_ends_where_the_media_does() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    tl.apply_at(cue(BASE + 50 * S, BASE + 70 * S, None), 0)
        .unwrap();
    tl.apply_at(play(None, None, None), 0).unwrap();
    // The media ends at 60 s: frozen there, with the event.
    assert_eq!(tl.cursor_tai(11 * S), Some((BASE + 60 * S - 1, 0.0)));
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::MediaEnd);
    // The same for play until.
    tl.apply_at(seek_tai(BASE + 55 * S), 20 * S).unwrap();
    tl.apply_at(play(None, Some(BASE + 90 * S), None), 20 * S)
        .unwrap();
    assert_eq!(tl.cursor_tai(30 * S), Some((BASE + 60 * S - 1, 0.0)));
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::MediaEnd);
}

#[test]
fn reverse_play_until_the_cursor_is_refused_as_forward_is() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
    for speed in [1.0, -1.0] {
        assert!(matches!(
            tl.apply_at(play(Some(speed), Some(BASE + 20 * S), None), 0),
            Err(CommandError::Conflict(_))
        ));
    }
}

#[test]
fn a_new_run_resets_margin_follow_live_edge_and_latency() {
    let old = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let tl = timeline_of(std::slice::from_ref(&old));
    tl.apply_at(
        Command::Live {
            margin_ms: Some(5000),
        },
        0,
    )
    .unwrap();
    tl.apply_at(Command::FollowLive { on: false }, 0).unwrap();
    tl.cmd_to_push_us.store(1234, Ordering::Relaxed);
    lock(&tl.edge).observe(0, BASE + 60 * S);
    tl.unregister_player(&old);
    let new = player_with(
        "cam0",
        BASE + 100 * S,
        BASE + 160 * S,
        Some((50, 1)),
        PlayerConfig::default(),
    );
    tl.register_player(&new);
    assert_eq!(tl.live_margin().1, ReplayLiveMarginSource::Computed);
    assert!(tl.follow_live());
    assert_eq!(tl.cmd_to_push_us.load(Ordering::Relaxed), 0);
    assert!(lock(&tl.edge).last_seen().is_none());
}

#[test]
fn a_command_before_the_flow_plays_keeps_its_place_on_the_new_time_base() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    // Given 5 s into the stand-in time base, before the flow plays.
    tl.apply_at(seek_tai(BASE + 10 * S), 5 * S).unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 5 * S).unwrap();
    tl.note_pushed(5 * S, BASE + 10 * S);
    // The flow plays: its running time starts near 0.
    tl.shift_rt(-5 * S);
    assert_eq!(tl.cursor_tai(0), Some((BASE + 10 * S, 1.0)));
    assert_eq!(tl.tai_at(S), Some(BASE + 11 * S));
    assert_eq!(lock(&tl.last_pushed).map(|(rt, _)| rt), Some(0));
}

#[test]
fn an_out_past_growing_media_holds_at_the_edge_until_the_media_reaches_it() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    // The edge steps 1 s a second: the media grows.
    let now = mono_ns();
    for i in 0..3 {
        lock(&tl.edge).observe(now - (2 - i) * S, BASE + (58 + i) * S);
    }
    let mut events = tl.subscribe();
    tl.apply_at(cue(BASE + 50 * S, BASE + 70 * S, None), 0)
        .unwrap();
    tl.apply_at(play(Some(2.0), None, None), 0).unwrap();
    // Past today's end at 2x: held at the edge, still playing, no end.
    assert_eq!(tl.cursor_tai(6 * S), Some((BASE + 60 * S - 1, 2.0)));
    assert!(events.try_recv().is_err());
    // The media reaches the out: play ends there, not at the old end.
    p[0].end_tai.store(BASE + 80 * S, Ordering::Relaxed);
    assert_eq!(tl.cursor_tai(7 * S), Some((BASE + 64 * S, 2.0)));
    assert_eq!(tl.cursor_tai(11 * S), Some((BASE + 70 * S - 1, 0.0)));
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::CueEnd);
}

#[test]
fn a_resume_after_a_pause_keeps_the_cursor_going() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 600 * S)]);
    let base = 1000 * S;
    *lock(&tl.clock_source) = Some(ClockSource {
        element: gst::glib::WeakRef::new(),
        base: Some(base),
    });
    tl.apply_at(seek_tai(BASE + 10 * S), 10 * S).unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 10 * S).unwrap();
    tl.note_pushed(20 * S, BASE + 20 * S);
    // Paused at 20 s for 30 s: the running time read from the clock kept
    // growing, and a command given during the pause is on that time.
    tl.apply_at(seek_tai(BASE + 100 * S), 35 * S).unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 35 * S).unwrap();
    // PLAYING again: the base moves 30 s on, running time falls back by 30 s.
    tl.observe_base(base + 30 * S);
    tl.observe_base(base + 30 * S);
    // Resumed at 20 s: 15 s after the command, going forward.
    assert_eq!(tl.cursor_tai(20 * S), Some((BASE + 115 * S, 1.0)));
    assert_eq!(tl.tai_at(21 * S), Some(BASE + 116 * S));
    // The first tick after the resume is the latest pushed.
    tl.note_pushed(21 * S, BASE + 116 * S);
    assert_eq!(tl.pushed_tai(), Some(BASE + 116 * S));
}

#[test]
fn a_base_is_only_observed_on_a_clock_source() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 600 * S)]);
    tl.apply_at(seek_tai(BASE + 10 * S), 10 * S).unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 10 * S).unwrap();
    // On the stand-in there is no base to move from.
    tl.observe_base(5 * S);
    assert_eq!(tl.tai_at(20 * S), Some(BASE + 20 * S));
}

#[test]
fn a_new_run_resets_under_the_anchor_lock() {
    let old = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let tl = Arc::new(timeline_of(std::slice::from_ref(&old)));
    tl.unregister_player(&old);
    // Hold the anchor as a command does: the new player is not registered
    // (so no command can see it) until the reset has happened.
    let a = lock(&tl.anchor);
    let new = player("cam0", BASE + 100 * S, BASE + 160 * S, Some((50, 1)));
    let t = {
        let tl = tl.clone();
        let new = new.clone();
        std::thread::spawn(move || tl.register_player(&new))
    };
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(tl.players().is_empty());
    drop(a);
    t.join().unwrap();
    assert_eq!(tl.players().len(), 1);
}

/// The source frames an offline output shows for `clip` (frame starts on a
/// 50p grid), one per tick, until play stops: how an offline player walks
/// the clip.
fn export_frames(tl: &ReplayTimeline, clip: &Clip) -> Vec<i64> {
    let g = tl.reference().unwrap();
    tl.use_counter();
    tl.start_clip(clip, 0).unwrap();
    let mut out = Vec::new();
    for k in 0.. {
        let rt = tick_rt(k, 50, 1);
        tl.set_counter(rt);
        assert_eq!(tl.now_rt(), rt);
        let Some(t) = tl.playing_at(rt) else {
            break;
        };
        out.push(g.floor_frame(t) - g.floor_frame(BASE));
        assert!(k < 100_000, "the clip never ended");
    }
    out
}

fn clip(from: i64, to: i64, speed: f64) -> Clip {
    Clip {
        in_point: ReplayTarget::t(from),
        out: ReplayTarget::t(to),
        speed,
    }
}

#[test]
fn ticks_are_exact_whole_nanoseconds() {
    assert_eq!(tick_rt(0, 50, 1), 0);
    assert_eq!(tick_rt(1, 50, 1), 20 * MS);
    assert_eq!(tick_rt(3, 30000, 1001), 100_100_000);
    // No drift over a day of 59.94p.
    let k = 24 * 3600 * 60_000 / 1001;
    assert_eq!(tick_rt(k, 60000, 1001), k * 1001 * S / 60000);
}

#[test]
fn a_counter_drives_now_and_the_clock_does_not() {
    gst::init().unwrap();
    let (tl, _p) = timeline_with(&[(BASE, BASE + 30 * S)]);
    assert_eq!(tl.time_source(), TimeSource::Clock);
    tl.use_counter();
    assert_eq!(tl.time_source(), TimeSource::Counter);
    assert_eq!(tl.now_rt(), 0);
    // A clock source does not take over the counter's "now".
    let el = gst::ElementFactory::make("identity").build().unwrap();
    tl.set_clock_source(&el);
    tl.set_counter(7 * S);
    assert_eq!(tl.now_rt(), 7 * S);
    // Commands at "now" are at the counter's running time.
    tl.apply(Request {
        command: Command::Speed { speed: 1.0 },
        client_ref: None,
    })
    .unwrap();
    tl.set_counter(9 * S);
    assert_eq!(tl.cursor_now().unwrap().0, BASE + 2 * S);
    // A new run of the group starts on the clock again.
    drop(_p);
    let p = player("cam", BASE, BASE + 30 * S, Some((50, 1)));
    tl.register_player(&p);
    assert_eq!(tl.time_source(), TimeSource::Clock);
}

#[test]
fn a_clip_on_the_counter_shows_each_frame_once_at_1x() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 30 * S)]);
    let frames = export_frames(&tl, &clip(BASE + 5 * S, BASE + 15 * S, 1.0));
    assert_eq!(frames, (250..750).collect::<Vec<_>>());
}

#[test]
fn a_clip_on_the_counter_shows_each_frame_twice_at_half_speed() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 30 * S)]);
    let frames = export_frames(&tl, &clip(BASE + 5 * S, BASE + 15 * S, 0.5));
    let want: Vec<i64> = (250..750).flat_map(|f| [f, f]).collect();
    assert_eq!(frames, want);
}

#[test]
fn a_clip_must_play_forward() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 30 * S)]);
    assert!(tl
        .start_clip(&clip(BASE + 5 * S, BASE + 15 * S, -1.0), 0)
        .is_err());
}

mod v2;

mod queue;
