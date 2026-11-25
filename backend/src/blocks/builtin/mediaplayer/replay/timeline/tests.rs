use super::*;

const BASE: i64 = 1_800_000_000_000_000_000;
const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;

fn seek(frame: i64) -> Command {
    seek_at(ReplayTimeRef {
        frame: Some(frame),
        ..Default::default()
    })
}

fn seek_at(at: ReplayTimeRef) -> Command {
    Command::Seek {
        frame: at.frame,
        utc: at.utc,
        tai: at.tai,
        from_live_ms: at.from_live_ms,
        speed: None,
    }
}

fn seek_tai(tai: i64) -> Command {
    seek_at(ReplayTimeRef {
        tai: Some(tai_string(tai)),
        ..Default::default()
    })
}

/// A player with an extent and, if given, a frame rate anchored at its start.
fn player(name: &str, start: i64, end: i64, fps: Option<(u32, u32)>) -> Arc<PlayerStats> {
    let p = Arc::new(PlayerStats::new(name));
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
    let a = tl.apply_at(seek(123), rt).unwrap();
    assert_eq!(tl.reference_time(a.tai).unwrap().frame, 123);
    assert_eq!(a.speed, 0.0);
    let a = tl.apply_at(Command::Jog { frames: -3 }, rt).unwrap();
    assert_eq!(tl.reference_time(a.tai).unwrap().frame, 120);
    // The cursor rests at the centre of the frame.
    assert_eq!(a.tai, BASE + 120 * 20 * MS + 10 * MS);
    assert_eq!(g.floor_frame(a.tai) - g.floor_frame(BASE), 120);
    // Clamped to the range.
    let a = tl.apply_at(seek(-50), rt).unwrap();
    assert_eq!(a.tai, BASE);
    // `end` goes to the live edge (the last instant every angle has).
    let a = tl.apply_at(Command::End, rt).unwrap();
    assert_eq!(tl.reference_time(a.tai).unwrap().frame, 499);
}

#[test]
fn end_is_the_common_edge_not_the_longest_angle() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 10 * S), (BASE, BASE + 8 * S)]);
    let a = tl.apply_at(Command::End, 0).unwrap();
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
    let a = tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert_eq!(a.tai, BASE + 12 * S - 1500 * MS);
}

#[test]
fn command_parses_live() {
    let c: Command = serde_json::from_str(r#"{"cmd":"live"}"#).unwrap();
    assert!(matches!(c, Command::Live { margin_ms: None }));
    let c: Command = serde_json::from_str(r#"{"cmd":"seek","frame":-5}"#).unwrap();
    assert_eq!(c, seek(-5));
}

/// TAI start of the spike's live ingest (off the 50 fps second grid).
const INGEST: i64 = 1_795_526_553_080_598_792;

fn one_player(base: i64, domain: TimeDomain) -> (ReplayTimeline, Arc<PlayerStats>) {
    let p = player("cam1", base, base + 60 * S, Some((50, 1)));
    p.set_time_domain(domain);
    (timeline_of(std::slice::from_ref(&p)), p)
}

fn seek_utc(utc: &str) -> Command {
    seek_at(ReplayTimeRef {
        utc: Some(utc.to_string()),
        ..Default::default()
    })
}

#[test]
fn frame_time_is_utc_of_the_frame_start() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    tl.bounds();
    let t = tl.reference_time(INGEST).unwrap();
    assert_eq!(t.frame, 0);
    assert_eq!(t.tai_ns, INGEST);
    assert_eq!(t.utc_ns, INGEST - 37 * S);
    assert_eq!(format_utc_ms(t.utc_ns), "2026-11-24T13:21:56.080Z");
    // Frames start at .000598792, .020598792, ... in each second.
    assert_eq!(t.frame_in_second, 4);
    let j = t.to_json();
    assert_eq!(j["tai"], "1795526553:80598792");
    assert_eq!(j["tai_ns"], "1795526553080598792");
    assert_eq!(j["tai_clock"], "2026-11-24T13:22:33.080");
    let at = |k: i64| tl.reference_time(INGEST + k * 20 * MS).unwrap();
    assert_eq!(at(45).frame_in_second, 49);
    assert_eq!(at(46).frame_in_second, 0);
}

#[test]
fn seek_by_utc_or_tai_is_exact() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    // 13:22:00.000 UTC lies in the frame that starts at 13:21:59.980598792.
    let a = tl
        .apply_at(seek_utc("2026-11-24T13:22:00.000Z"), 0)
        .unwrap();
    assert_eq!(
        a.tai,
        utc_to_tai_ns(parse_utc("2026-11-24T13:22:00Z").unwrap())
    );
    let t = tl.reference_time(a.tai).unwrap();
    assert_eq!(t.frame, 195);
    assert_eq!(format_utc_ms(t.utc_ns), "2026-11-24T13:21:59.980Z");
    let a2 = tl
        .apply_at(seek_utc("2026-11-24T14:22:00.000+01:00"), 0)
        .unwrap();
    assert_eq!(a2.tai, a.tai);
    let a3 = tl
        .apply_at(
            seek_at(ReplayTimeRef {
                tai: Some("1795526557:0".to_string()),
                ..Default::default()
            }),
            0,
        )
        .unwrap();
    assert_eq!(a3.tai, 1_795_526_557 * S);
    // Bad input changes nothing.
    assert!(tl.apply_at(seek_utc("13:22:00"), 0).is_err());
    let both = seek_at(ReplayTimeRef {
        frame: Some(1),
        utc: Some("2026-11-24T13:22:00Z".to_string()),
        ..Default::default()
    });
    assert!(tl.apply_at(both, 0).is_err());
    assert_eq!(tl.tai_at(0), Some(1_795_526_557 * S));
}

#[test]
fn speed_is_validated() {
    let (tl, _p) = one_player(BASE, TimeDomain::Unanchored);
    assert!(tl.apply_at(Command::Speed { speed: 9.0 }, 0).is_err());
    assert!(tl.apply_at(Command::Speed { speed: f64::NAN }, 0).is_err());
    assert_eq!(tl.speed(), 0.0);
    assert!(tl.apply_at(Command::Speed { speed: -8.0 }, 0).is_ok());
}

#[test]
fn telemetry_gives_utc_of_the_pushed_frame() {
    let (tl, p) = one_player(INGEST, TimeDomain::Tai);
    let g = p.grid().unwrap();
    let a = tl.apply_at(seek(195), tl.now_rt()).unwrap();
    // Nothing pushed yet: no "pushed" time.
    assert!(tl.telemetry().get("pushed").is_none());
    // The player pushed a different instant from the cursor: "pushed" is
    // what went out, not what was asked for.
    let pushed = a.tai - 5 * 20 * MS;
    p.shown_frame
        .store(g.floor_frame(pushed), Ordering::Relaxed);
    tl.note_pushed(1, pushed);
    let v = tl.telemetry();
    assert_eq!(v["time_domain"], "tai");
    assert_eq!(v["cursor"]["frame"], 195);
    assert_eq!(v["cursor"]["utc"], "2026-11-24T13:21:59.980Z");
    assert_eq!(v["pushed"]["frame"], 190);
    assert_eq!(v["pushed"]["utc"], "2026-11-24T13:21:59.880Z");
    assert_eq!(v["pushed"]["frame_in_second"], 44);
    assert_eq!(
        v["pushed"]["behind_live_ms"],
        (3000 - 190) as f64 * 20.0 - 10.0
    );
    // Captured in the past, so the picture's age is positive.
    assert!(v["pushed"]["age_ms"].as_i64().unwrap() > 0);
    assert_eq!(v["players"][0]["shown"]["utc"], "2026-11-24T13:21:59.880Z");
    assert_eq!(v["players"][0]["fps"], "50/1");
    assert_eq!(v["start"]["utc"], "2026-11-24T13:21:56.080Z");
    assert_eq!(v["live_edge"]["utc"], "2026-11-24T13:22:56.080Z");
    assert_eq!(v["live_edge"]["frame"], 3000);
    assert!(v["now_utc"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn unanchored_source_still_shows_its_times() {
    let (tl, _p) = one_player(BASE, TimeDomain::Unanchored);
    tl.note_pushed(0, BASE);
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
            let (f, t) = tl.frame_on(grid, (k as f64 * od) as i64).unwrap();
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
        let (f, cursor) = tl.frame_on(&g, 0).unwrap();
        assert_eq!(cursor, t);
        assert!(g.tai_of_frame(f) <= t, "{}", p.block_id);
        assert!(t < g.tai_of_frame(f + 1), "{}", p.block_id);
    }
    // 25 fps from +3.310 s: frames at .310 + k*40 ms; 20.007 s is in the frame
    // that starts at 19.990 s.
    let g2 = ps[1].grid().unwrap();
    assert_eq!(
        g2.tai_of_frame(tl.frame_on(&g2, 0).unwrap().0),
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
        .map(|p| tl.frame_on(&p.grid().unwrap(), 0).unwrap().0)
        .collect();
    let start = g.floor_frame(tl.tai_at(0).unwrap());
    let g1 = ps[0].grid().unwrap();
    let f1_start = g1.floor_frame(frame_centre(&g, start));
    for i in 1..=120 {
        let a = tl.apply_at(Command::Jog { frames: 1 }, 0).unwrap();
        assert_eq!(g.floor_frame(a.tai), start + i);
        for (p, l) in ps.iter().zip(last.iter_mut()) {
            let f = tl.frame_on(&p.grid().unwrap(), 0).unwrap().0;
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
    let f1 = tl.frame_on(&g1, 0).unwrap().0;
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
        let (f, _) = tl.frame_on(&g, 0).unwrap();
        assert_eq!(g.tai_of_frame(f), cut);
        tl.apply_at(seek_tai(cut - 1), 0).unwrap();
        assert_eq!(tl.frame_on(&g, 0).unwrap().0, f - 1);
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
    assert_eq!(tl.apply_at(seek(i64::MAX), 0).unwrap().tai, end);
    assert_eq!(tl.apply_at(seek(i64::MIN), 0).unwrap().tai, BASE);
    let rel = |ms| tl.apply_at(Command::SeekRel { ms }, 0).unwrap().tai;
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
    parse_command(serde_json::from_str(s).unwrap())
}

#[test]
fn commands_parse_with_seq_and_refuse_unknown_fields() {
    assert_eq!(
        parse(r#"{"cmd":"jog","frames":-2,"seq":7}"#).unwrap(),
        Command::Jog { frames: -2 }
    );
    // A typo is an error, not a silently ignored field.
    let e = parse(r#"{"cmd":"seek","frmae":3}"#).unwrap_err();
    assert!(matches!(e, CommandError::Invalid(_)), "{:?}", e);
    assert!(e.message().contains("frmae"), "{}", e);
    assert!(matches!(
        parse(r#"{"cmd":"fly"}"#),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        parse(r#"{"cmd":"jog"}"#),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(parse(r#"[1]"#), Err(CommandError::Invalid(_))));
    // The schema round-trips.
    let c = Command::Live {
        margin_ms: Some(500),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v, serde_json::json!({"cmd": "live", "margin_ms": 500}));
    assert_eq!(parse_command(v).unwrap(), c);
}

#[test]
fn errors_tell_invalid_from_conflict() {
    // No media yet: valid commands do not apply.
    let tl = ReplayTimeline::new();
    assert!(matches!(
        tl.apply_at(parse(r#"{"cmd":"play"}"#).unwrap(), 0),
        Err(CommandError::Conflict(_))
    ));
    // Media but no frame rate yet: frame addressing does not apply.
    let p = player("cam1", BASE, BASE + 10 * S, None);
    let tl = timeline_of(std::slice::from_ref(&p));
    assert!(matches!(
        tl.apply_at(seek(5), 0),
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
fn result_names_the_cursor_frame() {
    let (tl, _p) = one_player(INGEST, TimeDomain::Tai);
    let a = tl.apply_at(seek(195), 0).unwrap();
    let r = tl.result(&a);
    assert_eq!(r.frame, Some(195));
    assert_eq!(r.time.unwrap().utc, "2026-11-24T13:21:59.980Z");
    assert_eq!(r.cursor_tai.tai_ns, a.tai.to_string());
}

#[test]
fn seek_from_live_with_speed_is_one_step() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S), (BASE, BASE + 50 * S)]);
    tl.apply_at(Command::Pause, 0).unwrap();
    let c = parse(r#"{"cmd":"seek","from_live_ms":8000,"speed":0.5}"#).unwrap();
    let a = tl.apply_at(c, 0).unwrap();
    // 8 s before the common edge (50 s), already at 50 %.
    assert_eq!(a.tai, BASE + 42 * S);
    assert_eq!(a.speed, 0.5);
    assert_eq!(tl.tai_at(S), Some(BASE + 42 * S + S / 2));
    // Without speed the speed is kept.
    let a = tl
        .apply_at(parse(r#"{"cmd":"seek","from_live_ms":1000}"#).unwrap(), 0)
        .unwrap();
    assert_eq!((a.tai, a.speed), (BASE + 49 * S, 0.5));
    // A bad speed refuses the whole command: the cursor does not move.
    let bad = parse(r#"{"cmd":"seek","from_live_ms":0,"speed":12}"#).unwrap();
    assert!(matches!(tl.apply_at(bad, 0), Err(CommandError::Invalid(_))));
    assert_eq!(tl.tai_at(0), Some(BASE + 49 * S));
    // Two targets, or a negative distance, are invalid.
    assert!(matches!(
        parse(r#"{"cmd":"seek","from_live_ms":10,"frame":1}"#).map(|c| tl.apply_at(c, 0)),
        Ok(Err(CommandError::Invalid(_)))
    ));
    assert!(parse(r#"{"cmd":"seek","from_live_ms":-5}"#).is_err());
    // Further back than the media: clamped to the start.
    let far = parse(r#"{"cmd":"seek","from_live_ms":18446744073709551615}"#).unwrap();
    assert_eq!(tl.apply_at(far, 0).unwrap().tai, BASE);
}

// --- cue and bounded play -------------------------------------------------

fn at_tai(t: i64) -> ReplayTimeRef {
    ReplayTimeRef {
        tai: Some(tai_string(t)),
        ..Default::default()
    }
}

fn cue(start: i64, end: i64, end_action: Option<EndAction>) -> Command {
    Command::Cue {
        in_point: at_tai(start),
        out: at_tai(end),
        end_action,
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
        r#"{{"cmd":"cue","in":{{"tai":"{}"}},"out":{{"tai":"{}"}}}}"#,
        tai_string(BASE + 10 * S),
        tai_string(BASE + 12 * S)
    ))
    .unwrap();
    assert_eq!(c, cue(BASE + 10 * S, BASE + 12 * S, None));
    let a = tl.apply_at(c, 0).unwrap();
    assert_eq!((a.tai, a.speed), (BASE + 10 * S, 0.0));
    let r = tl.result(&a).cue.unwrap();
    assert_eq!(r.end_action, EndAction::Freeze);
    assert_eq!(r.out.tai_ns, (BASE + 12 * S).to_string());
    // out before in, or a cue outside the media, change nothing.
    let bad = cue(BASE + 12 * S, BASE + 10 * S, None);
    assert!(matches!(tl.apply_at(bad, 0), Err(CommandError::Invalid(_))));
    let outside = cue(BASE + 70 * S, BASE + 80 * S, None);
    assert!(matches!(
        tl.apply_at(outside, 0),
        Err(CommandError::Conflict(_))
    ));
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
    assert!(matches!(
        events.try_recv(),
        Ok(ReplayEvent::Ended { at }) if at.tai_ns == (BASE + 12 * S - 1).to_string()
    ));
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
    assert!(matches!(
        events.try_recv(),
        Ok(ReplayEvent::Live {
            reason: ReplayLiveReason::EndAction,
            ..
        })
    ));
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
    tl.apply_at(Command::Pause, 0).unwrap();
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
    let a = tl.apply_at(Command::Uncue, 0).unwrap();
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
    assert!(matches!(
        events.try_recv(),
        Ok(ReplayEvent::Live {
            reason: ReplayLiveReason::CaughtUp,
            ..
        })
    ));
    p[0].end_tai.store(BASE + 70 * S, Ordering::Relaxed);
    assert_eq!(tl.cursor_tai(6 * S), Some((BASE + 61 * S, 1.0)));
    assert!(tl.telemetry()["live"].as_bool().unwrap());
    // Following live: no second event.
    tl.cursor_tai(9 * S);
    assert!(events.try_recv().is_err());
}

#[test]
fn play_inside_the_live_margin_does_not_go_live() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    // Already within the margin when the command comes: slow motion stays
    // slow motion.
    let c = parse(r#"{"cmd":"seek","from_live_ms":1000,"speed":0.5}"#).unwrap();
    tl.apply_at(c, 0).unwrap();
    assert_eq!(tl.cursor_tai(S).unwrap().1, 0.5);
    assert!(events.try_recv().is_err());
    // `live` is live without an event; another command leaves it.
    let a = tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert!(a.live && tl.result(&a).live);
    assert!(events.try_recv().is_err());
    assert!(!tl.apply_at(Command::Pause, 0).unwrap().live);
    // A cue's bounds win over catching up.
    tl.apply_at(cue(BASE + 50 * S, BASE + 59 * S, None), 0)
        .unwrap();
    tl.apply_at(play(Some(4.0), None, None), 0).unwrap();
    assert_eq!(tl.cursor_tai(3 * S), Some((BASE + 59 * S - 1, 0.0)));
    assert!(matches!(events.try_recv(), Ok(ReplayEvent::Ended { .. })));
}
