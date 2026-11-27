//! The v2 API: the command table, follow_live, client_ref, out_of_range,
//! offsets, the margin, grids, readiness, range events and discovery.

use super::*;

/// The states the table starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum From {
    Paused,
    Playing,
    Live,
    Cue,
    Ramp,
}

/// A group in state `from` at running time 0, `follow_live` as given. One
/// angle of 60 s at 50p, live margin 1.5 s: the live point is at 58.5 s.
fn group_in(from: From, follow: bool) -> (ReplayTimeline, Vec<Arc<PlayerStats>>) {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    if !follow {
        tl.apply_at(Command::FollowLive { on: false }, 0).unwrap();
    }
    match from {
        From::Paused => {
            tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
        }
        From::Playing => {
            tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
            tl.apply_at(Command::Speed { speed: 1.0 }, 0).unwrap();
        }
        From::Live => {
            tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
        }
        From::Cue => {
            tl.apply_at(cue(BASE + 10 * S, BASE + 30 * S, None), 0)
                .unwrap();
            tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
            tl.apply_at(Command::Speed { speed: 1.0 }, 0).unwrap();
        }
        From::Ramp => {
            tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
            tl.apply_at(Command::Speed { speed: 1.0 }, 0).unwrap();
            tl.apply_at(
                Command::Ramp {
                    to: 2.0,
                    duration_ms: 10_000,
                },
                0,
            )
            .unwrap();
        }
    }
    (tl, p)
}

/// Each command from each state, with `follow_live` on and off: the cursor,
/// speed, cue, live and ramp afterwards, as the table in the API definition
/// says.
#[test]
fn every_command_from_every_state_does_what_the_table_says() {
    let froms = [
        From::Paused,
        From::Playing,
        From::Live,
        From::Cue,
        From::Ramp,
    ];
    let commands: Vec<Command> = vec![
        seek_tai(BASE + 25 * S),
        Command::Jog { frames: 1 },
        Command::Speed { speed: 0.5 },
        play(None, None, None),
        Command::Pause {},
        Command::Ramp {
            to: 0.25,
            duration_ms: 2000,
        },
        cue(BASE + 40 * S, BASE + 50 * S, None),
        Command::Uncue {},
        Command::Live { margin_ms: None },
        Command::End {},
        Command::Mark {
            label: None,
            at: Some(ReplayTarget::t(BASE + 5 * S)),
        },
        Command::FollowLive { on: true },
        Command::FollowLive { on: false },
        // At the timeline, without a player that switches: the motion stays.
        Command::Source {
            source: "cam2".into(),
            block: None,
        },
        // The queue: the motion stays.
        Command::CueNext {
            in_point: ReplayTarget::t(BASE + 40 * S),
            out: ReplayTarget::t(BASE + 50 * S),
            speed: None,
            end_action: None,
            source: None,
            block: None,
            replace: None,
        },
        Command::ClearQueue {},
    ];
    let rt = S;
    for follow in [true, false] {
        for from in froms {
            for cmd in &commands {
                let what = format!("{:?} from {:?}, follow_live {}", cmd, from, follow);
                let (tl, _p) = group_in(from, follow);
                let (cur, cur_speed) = tl.cursor_tai(rt).unwrap();
                let before = *lock(&tl.anchor);
                let held = before.region;
                let a = tl.apply_at(cmd.clone(), rt).expect(&what);
                let ramp_left = before.ramp_left(rt).map(|r| r.duration_ns);
                // (cursor, speed, cue, live, ramp time left)
                let want: (i64, f64, Option<(i64, i64)>, bool, Option<i64>) = match cmd {
                    Command::Seek { .. } => (
                        BASE + 25 * S,
                        cur_speed,
                        held.filter(|r| r.contains(BASE + 25 * S))
                            .map(|r| (r.start, r.end)),
                        false,
                        None,
                    ),
                    Command::Jog { .. } => {
                        let g = tl.reference().unwrap();
                        (
                            frame_centre(&g, g.floor_frame(cur) + 1),
                            0.0,
                            held.map(|r| (r.start, r.end)),
                            false,
                            None,
                        )
                    }
                    Command::Speed { .. } => {
                        (cur, 0.5, held.map(|r| (r.start, r.end)), false, None)
                    }
                    Command::Play { .. } => (cur, 1.0, held.map(|r| (r.start, r.end)), false, None),
                    Command::Pause {} => (cur, 0.0, held.map(|r| (r.start, r.end)), false, None),
                    Command::Ramp { .. } => (
                        cur,
                        cur_speed,
                        held.map(|r| (r.start, r.end)),
                        false,
                        Some(2000 * MS),
                    ),
                    Command::Cue { .. } => (
                        BASE + 40 * S,
                        0.0,
                        Some((BASE + 40 * S, BASE + 50 * S)),
                        false,
                        None,
                    ),
                    // The ramp goes on; a group with a cue is not live.
                    Command::Uncue {} => (cur, cur_speed, None, before.live, ramp_left),
                    Command::Live { .. } => (BASE + 58_500 * MS, 1.0, None, true, None),
                    Command::End {} => (BASE + 60 * S - 1, 0.0, None, false, None),
                    Command::Mark { .. }
                    | Command::Source { .. }
                    | Command::CueNext { .. }
                    | Command::ClearQueue {} => (
                        cur,
                        cur_speed,
                        held.map(|r| (r.start, r.end)),
                        before.live,
                        ramp_left,
                    ),
                    Command::FollowLive { on } => (
                        cur,
                        cur_speed,
                        held.map(|r| (r.start, r.end)),
                        before.live && *on,
                        ramp_left,
                    ),
                };
                let got = (
                    a.tai,
                    a.speed,
                    a.region.map(|r| (r.start, r.end)),
                    a.live,
                    a.ramp.map(|r| r.3),
                );
                assert_eq!(got, want, "{}", what);
                // The state says the same as the result.
                let m = tl.result(&a).motion;
                assert_eq!(m.live, a.live, "{}", what);
                assert_eq!(m.cue.is_some(), a.region.is_some(), "{}", what);
            }
        }
    }
}

/// `Live*`: forward play without a cue that reaches the live point goes live
/// by itself (1x, no ramp, `caught_up`) only with `follow_live`. Without it
/// the group stops on the last frame every angle has, at its speed, until
/// media grows.
#[test]
fn forward_play_goes_live_by_itself_only_with_follow_live() {
    let commands: Vec<Command> = vec![
        Command::Seek {
            to: ReplayTarget::t(BASE + 50 * S),
            speed: Some(2.0),
        },
        Command::Speed { speed: 2.0 },
        play(Some(2.0), None, None),
        Command::Ramp {
            to: 4.0,
            duration_ms: 1000,
        },
    ];
    for follow in [true, false] {
        for cmd in &commands {
            let what = format!("{:?}, follow_live {}", cmd, follow);
            let (tl, _p) = group_in(From::Playing, follow);
            let mut events = tl.subscribe();
            tl.apply_at(cmd.clone(), S).unwrap();
            let (t, speed) = tl.cursor_tai(60 * S).unwrap();
            let live = lock(&tl.anchor).live;
            assert_eq!(live, follow, "{}", what);
            if follow {
                assert_eq!(speed, 1.0, "{}", what);
                assert!(lock(&tl.anchor).ramp.is_none(), "{}", what);
                assert_eq!(next(&mut events).unwrap().cause, ReplayCause::CaughtUp);
            } else {
                assert_eq!(t, BASE + 60 * S - 1, "{}", what);
                assert!(speed >= 2.0, "{}", what);
                assert!(events.try_recv().is_err(), "{}", what);
            }
        }
    }
    // Uncue while playing forward: Live* too.
    for follow in [true, false] {
        let (tl, _p) = group_in(From::Cue, follow);
        tl.apply_at(Command::Speed { speed: 2.0 }, S).unwrap();
        tl.apply_at(Command::Uncue {}, S).unwrap();
        tl.cursor_tai(60 * S);
        assert_eq!(lock(&tl.anchor).live, follow);
    }
    // Turning follow_live on while play is behind: it goes live when it
    // gets there.
    let (tl, _p) = group_in(From::Playing, false);
    tl.apply_at(Command::Speed { speed: 2.0 }, S).unwrap();
    tl.apply_at(Command::FollowLive { on: true }, 2 * S)
        .unwrap();
    tl.cursor_tai(60 * S);
    assert!(lock(&tl.anchor).live);
    // Turning it off while live: no longer live, still playing.
    let (tl, _p) = group_in(From::Live, true);
    let a = tl.apply_at(Command::FollowLive { on: false }, S).unwrap();
    assert!(!a.live && a.speed == 1.0);
    assert!(!tl.follow_live());
}

#[test]
fn follow_live_property_off_on_any_player_turns_it_off() {
    let on = player("a", BASE, BASE + 60 * S, Some((50, 1)));
    let off = player_with(
        "b",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig {
            follow_live: false,
            ..Default::default()
        },
    );
    let tl = timeline_of(&[on.clone(), off.clone()]);
    assert!(!tl.follow_live());
    assert!(!tl.state().follow_live);
    // The command overrides it for the run.
    tl.apply_at(Command::FollowLive { on: true }, 0).unwrap();
    assert!(tl.follow_live());
}

#[test]
fn uncue_keeps_a_running_ramp() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(cue(BASE + 10 * S, BASE + 40 * S, None), 0)
        .unwrap();
    tl.apply_at(
        Command::Ramp {
            to: 1.0,
            duration_ms: 2000,
        },
        0,
    )
    .unwrap();
    // Halfway through the ramp (at 0.5x), the cue is dropped.
    let a = tl.apply_at(Command::Uncue {}, S).unwrap();
    assert!(a.region.is_none());
    assert_eq!(a.speed, 0.5);
    assert_eq!(a.ramp.map(|r| (r.1, r.3)), Some((1.0, S)));
    // The same curve as without the uncue: 1x after 2 s.
    assert_eq!(tl.cursor_tai(2 * S).unwrap().1, 1.0);
    assert_eq!(
        tl.tai_at(2 * S),
        Some(BASE + 10 * S + S) // 0 -> 1x over 2 s covers 1 s of media
    );
}

#[test]
fn from_live_0_and_end_land_on_the_same_frame() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S), (BASE + S, BASE + 58 * S + 7 * MS)]);
    let end = tl.apply_at(Command::End {}, 0).unwrap();
    let from_live = tl
        .apply_at(
            seek_at(ReplayTarget {
                from_live_ms: Some(0),
                ..Default::default()
            }),
            0,
        )
        .unwrap();
    assert_eq!(end.tai, from_live.tai);
    assert_eq!(end.tai, BASE + 58 * S + 7 * MS - 1);
    let v = tl.view();
    assert_eq!(v.frame(end.tai), v.frame(from_live.tai));
}

#[test]
fn a_range_wholly_outside_the_media_is_out_of_range_partly_is_clamped() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(seek_tai(BASE + 20 * S), 0).unwrap();
    // A cue before the media.
    assert!(matches!(
        tl.apply_at(cue(BASE - 20 * S, BASE - 10 * S, None), 0),
        Err(CommandError::OutOfRange(_))
    ));
    // Partly before: clamped to the start.
    let a = tl
        .apply_at(cue(BASE - 5 * S, BASE + 5 * S, None), 0)
        .unwrap();
    assert_eq!(a.region.map(|r| r.start), Some(BASE));
    // Play until a point past the media: the range from the cursor is
    // partly inside, so it is taken (and play ends where the media does).
    tl.apply_at(Command::Uncue {}, 0).unwrap();
    tl.apply_at(seek_tai(BASE + 50 * S), 0).unwrap();
    let a = tl
        .apply_at(play(None, Some(BASE + 90 * S), None), 0)
        .unwrap();
    assert_eq!(a.region.map(|r| r.end), Some(BASE + 90 * S));
    tl.apply_at(Command::Uncue {}, 0).unwrap();
    tl.apply_at(seek_tai(BASE), 0).unwrap();
    // A seek clamps instead.
    assert_eq!(tl.apply_at(seek_tai(BASE - 10 * S), 0).unwrap().tai, BASE);
    // `play {until}` with `rel_ms`: from the cursor at the command.
    let a = tl
        .apply_at(
            Command::Play {
                speed: None,
                until: Some(ReplayTarget {
                    rel_ms: Some(3000),
                    ..Default::default()
                }),
                end_action: None,
            },
            0,
        )
        .unwrap();
    assert_eq!(a.region.map(|r| r.end), Some(BASE + 3 * S));
}

#[test]
fn client_ref_comes_back_in_a_later_ended() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    let req = |command, r: &str| Request {
        command,
        client_ref: Some(r.to_string()),
    };
    tl.apply_req_at(req(cue(BASE + 10 * S, BASE + 12 * S, None), "cue-1"), 0)
        .unwrap();
    let a = tl
        .apply_req_at(req(play(None, None, None), "clip-17"), 0)
        .unwrap();
    assert_eq!(tl.result(&a).client_ref.as_deref(), Some("clip-17"));
    // Nothing yet; at the out the clip ends, with the play's ref.
    assert!(events.try_recv().is_err());
    tl.cursor_tai(3 * S);
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::CueEnd);
    assert_eq!(ev.client_ref.as_deref(), Some("clip-17"));
    assert!(matches!(ev.kind, ReplayEventKind::Ended { .. }));
    // Events are numbered one by one.
    tl.apply_req_at(req(Command::Live { margin_ms: None }, "go"), 4 * S)
        .unwrap();
    let ev2 = next(&mut events).unwrap();
    assert_eq!(ev2.event_seq, ev.event_seq + 1);
    assert_eq!(ev2.client_ref.as_deref(), Some("go"));
    assert_eq!(ev2.cause, ReplayCause::Live);
}

#[test]
fn offset_ms_moves_an_angle_on_the_group_axis() {
    // cam1's clock runs 2.5 s late: its picture of an instant has a source
    // time 2.5 s earlier. Offset +2500 ms moves it onto the group's axis.
    let cam0 = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let cam1 = player_with(
        "cam1",
        BASE - 2500 * MS,
        BASE + 57_500 * MS,
        Some((50, 1)),
        PlayerConfig {
            offset_ns: 2500 * MS,
            live_margin_ms: Some(1500),
            ..Default::default()
        },
    );
    let tl = timeline_of(&[cam0.clone(), cam1.clone()]);
    // Both angles cover the same span of the group's axis.
    let b = tl.bounds().unwrap();
    assert_eq!(
        (b.start, b.end, b.live_end),
        (BASE, BASE + 60 * S, BASE + 60 * S)
    );
    assert_eq!(cam1.extent(), Some((BASE, BASE + 60 * S)));
    assert_eq!(
        cam1.source_extent(),
        Some((BASE - 2500 * MS, BASE + 57_500 * MS))
    );
    // The group's grid is cam1's grid moved by the offset.
    let g1 = cam1.group_grid().unwrap();
    assert_eq!(g1.tai_of_frame(g1.floor_frame(BASE)), BASE);
    // What cam1 pushes (its own time) is on the axis 2.5 s later.
    cam1.shown_tai.store(BASE + 7_500 * MS, Ordering::Relaxed);
    let s = tl.state();
    let a1 = s.angles.iter().find(|a| a.block == "cam1").unwrap();
    assert_eq!(a1.offset_ms, 2500);
    assert_eq!(a1.shown.as_ref().unwrap().t, (BASE + 10 * S).to_string());
    assert_eq!(a1.extent.as_ref().unwrap().start.t, BASE.to_string());
}

#[test]
fn the_margin_is_computed_from_the_edge_step_unless_set() {
    let p = player_with(
        "cam0",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig::default(),
    );
    let tl = timeline_of(std::slice::from_ref(&p));
    // No step seen yet: a 1 s step is assumed.
    assert_eq!(
        tl.live_margin(),
        (1400 * MS, ReplayLiveMarginSource::Computed)
    );
    // 200 ms segments: clamped up to 800 ms.
    for k in 0..4 {
        lock(&tl.edge).observe(k * S, BASE + k * 200 * MS);
    }
    assert_eq!(tl.live_margin().0, 800 * MS);
    // 4 s segments: clamped down to 3000 ms.
    let tl = timeline_of(std::slice::from_ref(&p));
    for k in 0..4 {
        lock(&tl.edge).observe(k * S, BASE + k * 4 * S);
    }
    assert_eq!(tl.live_margin().0, 3000 * MS);
    let s = tl.state();
    assert_eq!(s.live_margin.ms, 3000);
    assert_eq!(s.live_margin.source, ReplayLiveMarginSource::Computed);
    // The largest property wins; `live {margin_ms}` overrides for the run.
    let q = player_with(
        "cam1",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig {
            live_margin_ms: Some(1200),
            ..Default::default()
        },
    );
    let r = player_with(
        "cam2",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig {
            live_margin_ms: Some(900),
            ..Default::default()
        },
    );
    let keep = [p.clone(), q, r];
    let tl = timeline_of(&keep);
    assert_eq!(tl.live_margin(), (1200 * MS, ReplayLiveMarginSource::Set));
    tl.apply_at(
        Command::Live {
            margin_ms: Some(700),
        },
        0,
    )
    .unwrap();
    assert_eq!(tl.live_margin(), (700 * MS, ReplayLiveMarginSource::Set));
}

#[test]
fn jog_steps_the_output_grid_when_asked() {
    // A 50p source on a 25p output.
    let cfg = PlayerConfig {
        jog_grid: strom_types::replay::ReplayJogGrid::Output,
        live_margin_ms: Some(1500),
        ..Default::default()
    };
    let p = player_with("cam0", BASE, BASE + 60 * S, Some((50, 1)), cfg);
    p.set_output_rate(25, 1);
    let tl = timeline_of(std::slice::from_ref(&p));
    assert_eq!(tl.jog_grid(), strom_types::replay::ReplayJogGrid::Output);
    tl.apply_at(seek_tai(BASE + 10 * S), 0).unwrap();
    let a = tl.apply_at(Command::Jog { frames: 1 }, 0).unwrap();
    assert_eq!(a.tai, BASE + 10 * S + 40 * MS + 20 * MS);
    // The timecode follows the output grid: 25 frames a second.
    let s = tl.state();
    assert_eq!(s.grid.output.unwrap().fps, "25/1");
    assert_eq!(s.grid.source.unwrap().fps, "50/1");
    let f = s.motion.unwrap().cursor.frame.unwrap();
    assert_eq!(f.t, (BASE + 10 * S + 40 * MS).to_string());
    assert_eq!(f.frame_in_second, 1);
    // With the default (source) grid a jog is one 50p frame.
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(seek_tai(BASE + 10 * S), 0).unwrap();
    let a = tl.apply_at(Command::Jog { frames: 1 }, 0).unwrap();
    assert_eq!(a.tai, BASE + 10 * S + 30 * MS);
}

#[test]
fn readiness_goes_indexing_warming_ready_with_events() {
    let p = player("cam0", UNKNOWN, UNKNOWN, Some((50, 1)));
    let tl = timeline_of(std::slice::from_ref(&p));
    let mut events = tl.subscribe();
    assert!(tl.bounds().is_none());
    assert_eq!(tl.state().ready, ReplayReady::Indexing);
    p.start_tai.store(BASE, Ordering::Relaxed);
    p.end_tai.store(BASE + 60 * S, Ordering::Relaxed);
    tl.bounds();
    let ev = next(&mut events).unwrap();
    assert!(matches!(
        ev.kind,
        ReplayEventKind::Ready {
            ready: ReplayReady::Warming
        }
    ));
    for _ in 0..READY_TICKS {
        p.note_tick(true, None);
    }
    tl.bounds();
    assert_eq!(tl.ready(), ReplayReady::Ready);
    assert!(matches!(
        next(&mut events).unwrap().kind,
        ReplayEventKind::Ready {
            ready: ReplayReady::Ready
        }
    ));
    // Ready for the run: a miss later does not take it back.
    p.note_tick(false, None);
    tl.bounds();
    assert!(events.try_recv().is_err());
}

#[test]
fn start_mode_live_goes_live_once_ready() {
    let cfg = PlayerConfig {
        start_mode: StartMode::Live,
        live_margin_ms: Some(1500),
        ..Default::default()
    };
    let p = player_with("cam0", BASE, BASE + 60 * S, Some((50, 1)), cfg);
    let tl = timeline_of(std::slice::from_ref(&p));
    let mut events = tl.subscribe();
    // Warming: paused at the start.
    assert_eq!(tl.cursor_tai(0), Some((BASE, 0.0)));
    for _ in 0..READY_TICKS {
        p.note_tick(true, None);
    }
    tl.bounds();
    assert_eq!(tl.cursor_tai(S), Some((BASE + 58_500 * MS, 1.0)));
    assert!(lock(&tl.anchor).live);
    let causes: Vec<_> = std::iter::from_fn(|| next(&mut events))
        .map(|e| e.cause)
        .collect();
    assert!(causes.contains(&ReplayCause::StartMode), "{:?}", causes);
    // A command before ready wins over the start mode.
    let p = player_with(
        "cam0",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig {
            start_mode: StartMode::Live,
            ..Default::default()
        },
    );
    let tl = timeline_of(std::slice::from_ref(&p));
    tl.apply_at(seek_tai(BASE + 5 * S), 0).unwrap();
    for _ in 0..READY_TICKS {
        p.note_tick(true, None);
    }
    tl.bounds();
    assert_eq!(tl.cursor_tai(S), Some((BASE + 5 * S, 0.0)));
}

/// Retention removes the start under a paused cursor: the cursor shows the
/// new start, and a seek into what was removed lands on it.
#[test]
fn a_cursor_in_what_retention_removed_moves_to_the_start() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(Command::FollowLive { on: false }, 0).unwrap();
    assert_eq!(
        tl.apply_at(seek_tai(BASE + 5 * S), 0).unwrap().tai,
        BASE + 5 * S
    );
    p[0].start_tai.store(BASE + 10 * S, Ordering::Relaxed);
    assert_eq!(tl.tai_at(0), Some(BASE + 10 * S));
    assert_eq!(
        tl.apply_at(seek_tai(BASE + 2 * S), 0).unwrap().tai,
        BASE + 10 * S
    );
    // Play from there goes on from the new start.
    tl.apply_at(Command::Speed { speed: 1.0 }, 0).unwrap();
    assert_eq!(tl.tai_at(S), Some(BASE + 11 * S));
}

#[test]
fn range_changed_on_retention_and_angles_not_on_growth() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.bounds();
    let mut events = tl.subscribe();
    // The edge grows: no event (the state shows it).
    p[0].end_tai.store(BASE + 61 * S, Ordering::Relaxed);
    tl.bounds();
    assert!(events.try_recv().is_err());
    // Retention removes the start.
    p[0].start_tai.store(BASE + 10 * S, Ordering::Relaxed);
    tl.bounds();
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::Retention);
    match ev.kind {
        ReplayEventKind::RangeChanged { range } => {
            assert_eq!(range.start.t, (BASE + 10 * S).to_string())
        }
        k => panic!("{:?}", k),
    }
    // A hole fills.
    p[0].set_index_view(IndexView {
        listed: vec![],
        gaps: vec![(BASE + 20 * S, BASE + 21 * S)],
    });
    tl.bounds();
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Media);
    p[0].set_index_view(IndexView::default());
    tl.bounds();
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Media);
}

#[test]
fn discovery_names_the_angles_and_hides_credentials() {
    let p = player_with(
        "cam0",
        BASE,
        BASE + 60 * S,
        Some((50, 1)),
        PlayerConfig {
            name: Some("Camera 1".into()),
            source: Some("tams+https://gw.example.com/flows/f1".into()),
            source_id: Some("f1".into()),
            store: Some("tams+https://gw.example.com".into()),
            ..Default::default()
        },
    );
    let tl = timeline_of(std::slice::from_ref(&p));
    let flow = uuid::Uuid::nil();
    let info = tl.info(flow, "r1");
    assert_eq!(info.group, "r1");
    assert_eq!(info.angles[0].name.as_deref(), Some("Camera 1"));
    assert_eq!(info.angles[0].source_id.as_deref(), Some("f1"));
    assert_eq!(info.source.as_deref(), Some("tams+https://gw.example.com"));
    assert_eq!(info.ready, ReplayReady::Warming);
    assert_eq!(info.time_domain, ReplayTimeDomain::Unknown);
}

/// An angle that starts later than the group has nothing to show at the
/// group's start: ticks there count as in time, so a group paused at its
/// start (or waiting for `start_mode: live`) still becomes ready.
#[test]
fn ticks_where_an_angle_has_no_media_do_not_hold_readiness_back() {
    let late = player("cam1", BASE + 2 * S, BASE + 60 * S, Some((50, 1)));
    for _ in 0..READY_TICKS {
        late.note_tick(false, Some(BASE));
    }
    assert_eq!(late.ready(), ReplayReady::Ready);
    // A miss where it has media does.
    let p = player("cam2", BASE, BASE + 60 * S, Some((50, 1)));
    for _ in 0..READY_TICKS {
        p.note_tick(false, Some(BASE + S));
    }
    assert_eq!(p.ready(), ReplayReady::Warming);
}

// --- review of 2026-11-27 ----------------------------------------------------

/// Give the group an edge that has stepped 1 s a second up to `edge`: the
/// media grows.
fn seed_growth(tl: &ReplayTimeline, edge: i64) {
    let now = mono_ns();
    for i in 0..3 {
        lock(&tl.edge).observe(now - (2 - i) * S, edge - (2 - i) * S);
    }
}

/// Run the group in 20 ms ticks from `from` to `to` (running time), the
/// media growing a 1 s segment each whole second of running time from
/// `edge0` at rt 0. Returns each tick's (rt, cursor, speed, live, live_end).
fn run_growing(
    tl: &ReplayTimeline,
    p: &PlayerStats,
    edge0: i64,
    from: i64,
    to: i64,
) -> Vec<(i64, i64, f64, bool, i64)> {
    let mut out = vec![];
    let mut rt = from;
    while rt <= to {
        let edge = edge0 + rt.div_euclid(S) * S;
        p.end_tai.store(edge, Ordering::Relaxed);
        let (t, speed) = tl.cursor_tai(rt).unwrap();
        out.push((rt, t, speed, lock(&tl.anchor).live, edge));
        rt += 20 * MS;
    }
    out
}

/// Finding 1: live, then `play {speed: 2}` while the cursor is ahead of the
/// live point (the edge steps, the cursor moves smoothly): it catches up at
/// once and plays 1x live, on growing media. Before the fix it ran at 2x
/// into the edge and stuttered there, not live, for good.
#[test]
fn play_2x_from_live_ahead_of_the_live_point_goes_live_again() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
    seed_growth(&tl, BASE + 10 * S);
    let mut events = tl.subscribe();
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::Live);
    // 0.6 s on: the cursor is at 9.1 s, the live point at 8.5 s.
    let a = tl.apply_at(play(Some(2.0), None, None), 600 * MS).unwrap();
    assert!(a.tai > BASE + 8_500 * MS);
    let ticks = run_growing(&tl, &p[0], BASE + 10 * S, 620 * MS, 20 * S);
    let ev = next(&mut events).expect("no caught_up");
    assert_eq!(ev.cause, ReplayCause::CaughtUp);
    assert!(matches!(ev.kind, ReplayEventKind::Live { .. }));
    let (rt0, t0, ..) = ticks[0];
    for &(rt, t, speed, live, edge) in &ticks {
        assert!(live && speed == 1.0, "rt {} ms", rt / MS);
        // 1x on one line, never held at the edge.
        assert_eq!(t, t0 + (rt - rt0), "rt {} ms", rt / MS);
        assert!(t < edge - 1, "held at the edge at rt {} ms", rt / MS);
    }
    assert!(events.try_recv().is_err());
}

/// Finding 1, the other phase: the cursor behind the live point when the
/// command comes (the edge just stepped). Also goes 1x live.
#[test]
fn play_2x_from_live_behind_the_live_point_goes_live_again() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
    seed_growth(&tl, BASE + 10 * S);
    let mut events = tl.subscribe();
    tl.apply_at(Command::Live { margin_ms: None }, 900 * MS)
        .unwrap();
    next(&mut events);
    // The edge steps at rt 1 s: the live point is at 9.5 s, the cursor at
    // 8.6 s.
    p[0].end_tai.store(BASE + 11 * S, Ordering::Relaxed);
    let a = tl.apply_at(play(Some(2.0), None, None), S).unwrap();
    assert!(a.tai < BASE + 9_500 * MS);
    let ticks = run_growing(&tl, &p[0], BASE + 10 * S, S + 20 * MS, 10 * S);
    assert_eq!(next(&mut events).unwrap().cause, ReplayCause::CaughtUp);
    let &(_, _, speed, live, _) = ticks.last().unwrap();
    assert!(live && speed == 1.0);
    for &(rt, t, _, _, edge) in &ticks {
        assert!(t < edge - 1, "held at the edge at rt {} ms", rt / MS);
    }
}

/// Finding 1: `end`, then `play`, on growing media: live, back on the live
/// point (from the edge, 1x live would hold at the edge on every step).
/// `seek {from_live_ms: 0, speed: 1}` the same. A ramp towards 2x from live
/// goes live and the ramp ends.
#[test]
fn forward_play_from_the_edge_goes_live_at_the_live_point() {
    for cmd in [
        None,
        Some(Command::Seek {
            to: ReplayTarget {
                from_live_ms: Some(0),
                ..Default::default()
            },
            speed: Some(1.0),
        }),
    ] {
        let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
        seed_growth(&tl, BASE + 10 * S);
        let mut events = tl.subscribe();
        match cmd {
            None => {
                tl.apply_at(Command::End {}, 0).unwrap();
                tl.apply_at(play(None, None, None), 100 * MS).unwrap();
            }
            Some(c) => {
                tl.apply_at(c, 100 * MS).unwrap();
            }
        }
        let ticks = run_growing(&tl, &p[0], BASE + 10 * S, 120 * MS, 5 * S);
        assert_eq!(next(&mut events).unwrap().cause, ReplayCause::CaughtUp);
        for &(rt, t, speed, live, edge) in &ticks {
            assert!(live && speed == 1.0);
            assert!(t < edge - 1, "held at the edge at rt {} ms", rt / MS);
            // Not further from the edge than the margin and a step.
            assert!(t >= edge - 2_500 * MS, "rt {} ms", rt / MS);
        }
    }
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
    seed_growth(&tl, BASE + 10 * S);
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    tl.apply_at(
        Command::Ramp {
            to: 2.0,
            duration_ms: 2000,
        },
        600 * MS,
    )
    .unwrap();
    run_growing(&tl, &p[0], BASE + 10 * S, 620 * MS, 3 * S);
    let a = *lock(&tl.anchor);
    assert!(a.live && a.ramp.is_none() && a.speed == 1.0);
}

/// Finding 1, what must not change: slow motion, or a ramp down, from at or
/// ahead of the live point is not taken live (it falls behind the edge).
#[test]
fn slow_play_from_ahead_of_the_live_point_stays_slow() {
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
    seed_growth(&tl, BASE + 10 * S);
    tl.apply_at(Command::Live { margin_ms: None }, 0).unwrap();
    tl.apply_at(
        Command::Ramp {
            to: 0.25,
            duration_ms: 1000,
        },
        600 * MS,
    )
    .unwrap();
    let ticks = run_growing(&tl, &p[0], BASE + 10 * S, 620 * MS, 4 * S);
    let &(_, _, speed, live, _) = ticks.last().unwrap();
    assert!(!live && speed == 0.25);
    let (tl, p) = timeline_with(&[(BASE, BASE + 10 * S)]);
    seed_growth(&tl, BASE + 10 * S);
    tl.apply_at(Command::End {}, 0).unwrap();
    tl.apply_at(play(Some(0.5), None, None), 0).unwrap();
    let ticks = run_growing(&tl, &p[0], BASE + 10 * S, 20 * MS, 4 * S);
    let &(_, _, speed, live, _) = ticks.last().unwrap();
    assert!(!live && speed == 0.5);
}

/// Finding 2: the cue's `client_ref` comes back in its `ended`, also when
/// `play` has none and other commands with or without a ref come mid-clip.
#[test]
fn ended_carries_the_cue_ref_not_the_last_command_ref() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let mut events = tl.subscribe();
    let req = |command, r: Option<&str>| Request {
        command,
        client_ref: r.map(str::to_string),
    };
    tl.apply_req_at(
        req(cue(BASE + 10 * S, BASE + 14 * S, None), Some("clip-17")),
        0,
    )
    .unwrap();
    tl.apply_req_at(req(play(None, None, None), None), 0)
        .unwrap();
    tl.apply_req_at(
        req(
            Command::Ramp {
                to: 2.0,
                duration_ms: 500,
            },
            Some("studio"),
        ),
        S,
    )
    .unwrap();
    tl.apply_req_at(req(Command::FollowLive { on: true }, None), S)
        .unwrap();
    tl.apply_req_at(req(Command::Speed { speed: 1.0 }, None), S)
        .unwrap();
    tl.cursor_tai(10 * S);
    let ev = next(&mut events).unwrap();
    assert_eq!(ev.cause, ReplayCause::CueEnd);
    assert_eq!(ev.client_ref.as_deref(), Some("clip-17"));
    // Played again (from in, at the end), it ends with the same ref.
    tl.apply_req_at(req(play(None, None, None), None), 10 * S)
        .unwrap();
    tl.cursor_tai(20 * S);
    assert_eq!(
        next(&mut events).unwrap().client_ref.as_deref(),
        Some("clip-17")
    );
    // A new cue without a ref replaces it; a seek out of the cue drops it.
    tl.apply_req_at(req(cue(BASE + 20 * S, BASE + 21 * S, None), None), 20 * S)
        .unwrap();
    tl.apply_req_at(req(play(None, None, None), None), 20 * S)
        .unwrap();
    tl.cursor_tai(22 * S);
    assert_eq!(next(&mut events).unwrap().client_ref, None);
    tl.apply_req_at(
        req(cue(BASE + 30 * S, BASE + 31 * S, None), Some("clip-18")),
        22 * S,
    )
    .unwrap();
    tl.apply_req_at(req(seek_tai(BASE + 40 * S), None), 22 * S)
        .unwrap();
    assert!(lock(&tl.cue_ref).is_none());
}

/// Finding 3: `play` at the edge of growing media with a cue whose out is
/// past it goes on from there; it does not jump back to `in`. On media that
/// no longer grows it does (the end play runs to is the media's).
#[test]
fn play_at_a_growing_edge_with_the_out_past_it_does_not_jump_to_in() {
    for grows in [true, false] {
        let (tl, _p) = timeline_with(&[(BASE, BASE + 10 * S)]);
        if grows {
            seed_growth(&tl, BASE + 10 * S);
        }
        tl.apply_at(cue(BASE + 5 * S, BASE + 20 * S, None), 0)
            .unwrap();
        tl.apply_at(play(None, None, None), 0).unwrap();
        let (t, _) = tl.cursor_tai(8 * S).unwrap();
        assert_eq!(t, BASE + 10 * S - 1);
        let a = tl.apply_at(play(Some(0.5), None, None), 8 * S).unwrap();
        let want = if grows {
            BASE + 10 * S - 1
        } else {
            BASE + 5 * S
        };
        assert_eq!(a.tai, want, "grows {}", grows);
    }
}

/// Finding 4: a `utc` target lands on the output grid, where the API's
/// timecodes are: every output frame's UTC seeks back to that output frame,
/// also when the output's rate is not a multiple of the source's.
#[test]
fn utc_targets_land_on_the_output_grid() {
    for (out_n, out_d) in [(50, 1), (30000, 1001), (25, 1)] {
        let p = player("cam1", INGEST, INGEST + 60 * S, Some((25, 1)));
        p.set_time_domain(TimeDomain::Tai);
        p.set_output_rate(out_n, out_d);
        let tl = timeline_of(std::slice::from_ref(&p));
        let v = tl.view();
        let g = tl.output_grid().unwrap();
        assert_eq!((g.fps_n, g.fps_d), (out_n, out_d));
        for k in [0, 1, 2, 3, 49, 50, 51, 997, 1001] {
            let ft = g.tai_of_frame(g.ceil_frame(INGEST) + k);
            let f = v.frame(ft).unwrap();
            let a = tl.apply_at(seek_utc(f.utc.as_deref().unwrap()), 0).unwrap();
            let back = v.frame(a.tai).unwrap();
            assert_eq!(a.tai, ft, "{}/{} frame {}", out_n, out_d, k);
            assert_eq!(
                (back.utc, back.frame_in_second),
                (f.utc, f.frame_in_second),
                "{}/{} frame {}",
                out_n,
                out_d,
                k
            );
        }
    }
}

/// Finding 11: `live {margin_ms}` and `mark {label}` have upper bounds.
#[test]
fn margin_and_label_are_bounded() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    let live = |ms| Command::Live {
        margin_ms: Some(ms),
    };
    assert!(tl.apply_at(live(MAX_LIVE_MARGIN_SET_MS), 0).is_ok());
    assert!(matches!(
        tl.apply_at(live(MAX_LIVE_MARGIN_SET_MS + 1), 0),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        tl.apply_at(live(u64::MAX), 0),
        Err(CommandError::Invalid(_))
    ));
    let mark = |n: usize| Command::Mark {
        label: Some("é".repeat(n)),
        at: Some(ReplayTarget::t(BASE + S)),
    };
    assert!(tl.apply_at(mark(MAX_MARK_LABEL_LEN), 0).is_ok());
    assert!(matches!(
        tl.apply_at(mark(MAX_MARK_LABEL_LEN + 1), 0),
        Err(CommandError::Invalid(_))
    ));
}

/// The next `range_changed`, skipping other events.
fn next_range(events: &mut tokio::sync::broadcast::Receiver<ReplayEvent>) -> Option<ReplayEvent> {
    std::iter::from_fn(|| next(events))
        .find(|ev| matches!(ev.kind, ReplayEventKind::RangeChanged { .. }))
}

/// Finding 13: an angle swapped for another in one look, and a live edge
/// that goes back while the end stays, send `range_changed`; the live edge
/// estimate does not go back.
#[test]
fn range_changed_on_an_angle_swap_and_an_edge_that_goes_back() {
    let a = player("cam0", BASE, BASE + 60 * S, Some((50, 1)));
    let b = player("cam1", BASE, BASE + 60 * S, Some((50, 1)));
    b.end_tai.store(UNKNOWN, Ordering::Relaxed);
    let tl = timeline_of(&[a.clone(), b.clone()]);
    tl.bounds();
    let mut events = tl.subscribe();
    // cam0 goes, cam1 comes: still one angle with media.
    a.end_tai.store(UNKNOWN, Ordering::Relaxed);
    b.end_tai.store(BASE + 60 * S, Ordering::Relaxed);
    tl.bounds();
    let ev = next_range(&mut events).expect("no range_changed on the swap");
    assert!(matches!(ev.kind, ReplayEventKind::RangeChanged { .. }));
    assert_eq!(ev.cause, ReplayCause::Media);
    // Both have media; then cam0 has less: the end stays, the live edge goes
    // back.
    a.end_tai.store(BASE + 60 * S, Ordering::Relaxed);
    tl.bounds();
    // (Readiness events come too: the angles' extents changed.)
    while next(&mut events).is_some() {}
    let before = tl.state().range.unwrap().live_edge_estimate.t;
    a.end_tai.store(BASE + 50 * S, Ordering::Relaxed);
    tl.bounds();
    let ev = next_range(&mut events).expect("no range_changed on the edge going back");
    match ev.kind {
        ReplayEventKind::RangeChanged { range } => {
            assert_eq!(
                range.live_edge.t,
                (BASE + 50 * S).to_string(),
                "{:?}",
                range
            );
            assert_eq!(range.end.t, (BASE + 60 * S).to_string());
        }
        k => panic!("{:?}", k),
    }
    let after = tl.state().range.unwrap().live_edge_estimate.t;
    assert!(after.parse::<i64>().unwrap() >= before.parse::<i64>().unwrap());
}
