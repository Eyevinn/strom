//! The clip queue (`cue_next`, `clear_queue`): the swap on the running time
//! the out point is reached, each clip's speed, the end action of the last
//! clip, clearing and replacing, and the checks of a queued clip.

use super::*;
use strom_types::replay::{ReplayCause, ReplayEventKind, MAX_QUEUE_LEN};

const TICK: i64 = 20 * MS;

fn cue_next(start: i64, end: i64, speed: Option<f64>, end_action: Option<EndAction>) -> Command {
    Command::CueNext {
        in_point: at_tai(start),
        out: at_tai(end),
        speed,
        end_action,
        source: None,
        block: None,
        replace: None,
    }
}

fn req(cmd: Command, client_ref: &str) -> Request {
    Request {
        command: cmd,
        client_ref: Some(client_ref.to_string()),
    }
}

/// Every event sent so far: (cause, client_ref, kind).
fn drain(
    events: &mut tokio::sync::broadcast::Receiver<ReplayEvent>,
) -> Vec<(ReplayCause, Option<String>, ReplayEventKind)> {
    std::iter::from_fn(|| next(events))
        .map(|e| (e.cause, e.client_ref, e.kind))
        .collect()
}

/// A 50p group playing clip "a" = [10 s, 10.2 s) at 1x from running time 0.
fn playing_a() -> (ReplayTimeline, Vec<Arc<PlayerStats>>) {
    let (tl, p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(Command::FollowLive { on: false }, 0).unwrap();
    tl.apply_req_at(req(cue(BASE + 10 * S, BASE + 10_200 * MS, None), "a"), 0)
        .unwrap();
    tl.apply_req_at(req(play(None, None, None), "a"), 0)
        .unwrap();
    (tl, p)
}

/// The next clip starts on the tick the out point is reached: the old
/// clip's last frame on the tick before, the new clip's in point on that
/// tick, no frame of the old clip past its out and none skipped. Every
/// caller agrees, whichever looks first, and a tick from before the swap
/// that asks after it still gets the old clip.
#[test]
fn the_next_clip_starts_on_the_tick_the_out_point_is_reached() {
    let (tl, _p) = playing_a();
    let mut events = tl.subscribe();
    tl.apply_req_at(
        req(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), "b"),
        0,
    )
    .unwrap();
    assert_eq!(tl.queued(), 1);
    let ticks: Vec<(i64, u64)> = (0..14).map(|k| tl.cursor_item(k * TICK).unwrap()).collect();
    for (k, (t, item)) in ticks.iter().enumerate() {
        let k = k as i64;
        if k < 10 {
            assert_eq!(*t, BASE + 10 * S + k * TICK, "tick {}", k);
            assert_eq!(*item, 0, "tick {}: clip a came from a cue", k);
        } else {
            assert_eq!(*t, BASE + 30 * S + (k - 10) * TICK, "tick {}", k);
            assert_eq!(*item, 1, "tick {}", k);
        }
    }
    assert_eq!(tl.queued(), 0);
    let evs = drain(&mut events);
    assert_eq!(
        evs.iter()
            .map(|(c, r, k)| (*c, r.clone(), std::mem::discriminant(k)))
            .collect::<Vec<_>>(),
        vec![
            (
                ReplayCause::CueEnd,
                Some("a".into()),
                std::mem::discriminant(&ReplayEventKind::Ended {
                    at: tl.view().instant(0)
                })
            ),
            (
                ReplayCause::CueEnd,
                Some("b".into()),
                std::mem::discriminant(&ReplayEventKind::ItemStarted {
                    at: tl.view().instant(0),
                    waiting: 0
                })
            ),
        ]
    );
    match &evs[1].2 {
        ReplayEventKind::ItemStarted { at, waiting } => {
            assert_eq!(at.t, (BASE + 30 * S).to_string());
            assert_eq!(*waiting, 0);
        }
        other => panic!("{:?}", other),
    }
    // The new clip is the group's cue, with its ref.
    let r = tl.cue().unwrap();
    assert_eq!((r.start, r.end), (BASE + 30 * S, BASE + 31 * S));
    let s = tl.state();
    assert_eq!(s.queue.clip_ref.as_deref(), Some("b"));
    assert_eq!(s.queue.waiting, 0);

    // Whoever looks first: the swap is at the running time of the out
    // point, not at the time of the look.
    let (tl2, _p2) = playing_a();
    tl2.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
        .unwrap();
    // The state looks first, between two ticks, past the out point.
    assert_eq!(tl2.tai_at(205 * MS), Some(BASE + 30 * S + 5 * MS));
    assert_eq!(tl2.tai_at(200 * MS), Some(BASE + 30 * S));
    // A tick from before the swap, late: the old clip's frame.
    assert_eq!(
        tl2.cursor_item(180 * MS),
        Some((BASE + 10 * S + 180 * MS, 0))
    );
    assert_eq!(tl2.tai_at(220 * MS), Some(BASE + 30 * S + 20 * MS));
}

/// An out point between ticks: the swap is where play reached it, so the
/// next clip has moved on by the time since, at its own speed.
#[test]
fn each_clip_plays_at_its_own_speed_from_where_the_out_was_reached() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(cue(BASE + 10 * S, BASE + 10_100 * MS, None), 0)
        .unwrap();
    tl.apply_at(play(Some(2.0), None, None), 0).unwrap();
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, Some(0.5), None), 0)
        .unwrap();
    // 2x reaches the out (100 ms of media) at 50 ms.
    assert_eq!(tl.tai_at(40 * MS), Some(BASE + 10_080 * MS));
    assert_eq!(tl.tai_at(60 * MS), Some(BASE + 30 * S + 5 * MS));
    assert_eq!(tl.cursor_tai(80 * MS).unwrap().1, 0.5);
    assert_eq!(tl.tai_at(80 * MS), Some(BASE + 30 * S + 15 * MS));
    // `speed` changes the clip playing only: the next one has its own.
    tl.apply_at(cue_next(BASE + 40 * S, BASE + 41 * S, Some(4.0), None), 0)
        .unwrap();
    tl.apply_at(Command::Speed { speed: 1.0 }, 100 * MS)
        .unwrap();
    // [30.025 s, 31 s) at 1x: the out at 1075 ms.
    assert_eq!(tl.cursor_tai(1_075 * MS), Some((BASE + 40 * S, 4.0)));
}

/// Several clips: each starts where the one before reached its out, also
/// when one look covers several swaps.
#[test]
fn a_playlist_runs_clip_after_clip() {
    let (tl, _p) = playing_a();
    let mut events = tl.subscribe();
    for (i, r) in ["b", "c", "d"].iter().enumerate() {
        let start = BASE + (20 + 10 * i as i64) * S;
        tl.apply_req_at(req(cue_next(start, start + 100 * MS, None, None), r), 0)
            .unwrap();
    }
    assert_eq!(tl.state().queue.waiting, 3);
    // a ends at 200 ms, b at 300, c at 400, d at 500 (frozen there).
    assert_eq!(tl.tai_at(450 * MS), Some(BASE + 40 * S + 50 * MS));
    assert_eq!(tl.tai_at(600 * MS), Some(BASE + 40 * S + 100 * MS - 1));
    let started: Vec<(Option<String>, usize)> = drain(&mut events)
        .into_iter()
        .filter_map(|(_, r, k)| match k {
            ReplayEventKind::ItemStarted { waiting, .. } => Some((r, waiting)),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![
            (Some("b".into()), 2),
            (Some("c".into()), 1),
            (Some("d".into()), 0)
        ]
    );
}

/// With nothing queued when a clip ends, its own end action applies:
/// freeze (the default), loop or live; the clip's ref goes with it.
#[test]
fn an_empty_queue_leaves_the_clip_its_end_action() {
    for action in [None, Some(EndAction::Loop), Some(EndAction::Live)] {
        let (tl, _p) = playing_a();
        tl.apply_req_at(
            req(
                cue_next(BASE + 30 * S, BASE + 30_100 * MS, None, action),
                "b",
            ),
            0,
        )
        .unwrap();
        // b runs 200 .. 300 ms, then its end action.
        assert_eq!(tl.tai_at(250 * MS), Some(BASE + 30_050 * MS));
        let mut events = tl.subscribe();
        let t = tl.tai_at(S).unwrap();
        let evs = drain(&mut events);
        let refs_b = |want: fn(&ReplayEventKind) -> bool| {
            evs.iter()
                .any(|(c, r, k)| *c == ReplayCause::CueEnd && r.as_deref() == Some("b") && want(k))
        };
        match action {
            None => {
                assert_eq!(t, BASE + 30_100 * MS - 1);
                assert!(
                    refs_b(|k| matches!(k, ReplayEventKind::Ended { .. })),
                    "{:?}",
                    evs
                );
            }
            Some(EndAction::Loop) => {
                // 800 ms into a 100 ms loop: at its in point.
                assert_eq!(t, BASE + 30 * S);
                assert!(evs.is_empty(), "{:?}", evs);
            }
            _ => {
                // Live from the live point (60 s - 1.5 s margin).
                assert_eq!(t, BASE + 58_500 * MS);
                assert!(
                    refs_b(|k| matches!(k, ReplayEventKind::Live { .. })),
                    "{:?}",
                    evs
                );
            }
        }
    }
}

/// A clip queued after a looping clip ends the loop: the clip waiting is
/// played at the out point instead of going round again.
#[test]
fn a_queued_clip_ends_a_loop() {
    let (tl, _p) = timeline_with(&[(BASE, BASE + 60 * S)]);
    tl.apply_at(
        cue(BASE + 10 * S, BASE + 10_100 * MS, Some(EndAction::Loop)),
        0,
    )
    .unwrap();
    tl.apply_at(play(None, None, None), 0).unwrap();
    // Round once (0 .. 100 ms, 100 .. 200 ms).
    assert_eq!(tl.tai_at(150 * MS), Some(BASE + 10_050 * MS));
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 150 * MS)
        .unwrap();
    assert_eq!(tl.tai_at(240 * MS), Some(BASE + 30 * S + 40 * MS));
}

/// `clear_queue` empties the queue: the clip playing goes to its out and
/// its end action applies. `cue_next {replace}` empties it before adding;
/// `live` and `end` leave clip play and empty it, and `uncue` (no clip
/// left for the queue to follow) does too. Neither changes the motion.
#[test]
fn the_queue_is_cleared_and_replaced() {
    let (tl, _p) = playing_a();
    tl.apply_req_at(
        req(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), "b"),
        0,
    )
    .unwrap();
    tl.apply_req_at(
        req(cue_next(BASE + 40 * S, BASE + 41 * S, None, None), "c"),
        0,
    )
    .unwrap();
    let before = tl.cursor_tai(50 * MS).unwrap();
    let a = tl
        .apply_req_at(req(Command::ClearQueue {}, "x"), 50 * MS)
        .unwrap();
    assert_eq!((a.tai, a.speed), before);
    assert_eq!(a.client_ref.as_deref(), Some("x"));
    assert_eq!(tl.queued(), 0);
    // a freezes at its out.
    assert_eq!(tl.tai_at(300 * MS), Some(BASE + 10_200 * MS - 1));
    assert_eq!(tl.cue().map(|r| r.start), Some(BASE + 10 * S));

    // Replace: only the last clip stays.
    let (tl, _p) = playing_a();
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
        .unwrap();
    tl.apply_at(cue_next(BASE + 35 * S, BASE + 36 * S, None, None), 0)
        .unwrap();
    let mut replace = cue_next(BASE + 40 * S, BASE + 41 * S, None, None);
    if let Command::CueNext { replace: r, .. } = &mut replace {
        *r = Some(true);
    }
    tl.apply_req_at(req(replace, "r"), 0).unwrap();
    assert_eq!(tl.queued(), 1);
    assert_eq!(tl.queue_view().1.unwrap().client_ref.as_deref(), Some("r"));
    assert_eq!(tl.tai_at(200 * MS), Some(BASE + 40 * S));
    assert_eq!(tl.queue_view().0, 3, "the third clip queued plays");

    // `live`, `end` and `uncue` empty it.
    for leave in [
        Command::Live { margin_ms: None },
        Command::End {},
        Command::Uncue {},
    ] {
        let (tl, _p) = playing_a();
        tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
            .unwrap();
        tl.apply_at(leave.clone(), 10 * MS).unwrap();
        assert_eq!(tl.queued(), 0, "{:?}", leave);
    }

    // `uncue` keeps the cursor and the speed: play goes on past a's out,
    // and nothing is swapped in there.
    let (tl, _p) = playing_a();
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
        .unwrap();
    let before = tl.cursor_tai(100 * MS).unwrap();
    let a = tl.apply_at(Command::Uncue {}, 100 * MS).unwrap();
    assert_eq!((a.tai, a.speed), before);
    assert!(tl.cue().is_none());
    assert_eq!(tl.queue_view().0, 0);
    assert_eq!(tl.tai_at(300 * MS), Some(BASE + 10_300 * MS));

    // Other commands keep it: a pause in the clip, then play on to the out.
    let (tl, _p) = playing_a();
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
        .unwrap();
    tl.apply_at(Command::Pause {}, 100 * MS).unwrap();
    tl.apply_at(play(None, None, None), S).unwrap();
    assert_eq!(tl.tai_at(S + 100 * MS), Some(BASE + 30 * S));
}

/// The clip that plays from the queue stays the group's clip while its cue
/// is held: a pause keeps it, a new cue ends it.
#[test]
fn the_clip_from_the_queue_is_known_while_its_cue_is_held() {
    let (tl, _p) = playing_a();
    tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
        .unwrap();
    assert_eq!(tl.cursor_item(250 * MS).unwrap().1, 1);
    tl.apply_at(Command::Pause {}, 300 * MS).unwrap();
    assert_eq!(tl.queue_view().0, 1);
    tl.apply_at(cue(BASE + 5 * S, BASE + 6 * S, None), 400 * MS)
        .unwrap();
    assert_eq!(tl.queue_view().0, 0);
}

/// What a queued clip is checked for: its range, its speed (forward), and
/// the size of the queue. Refused whole: the queue is as it was.
#[test]
fn a_queued_clip_is_checked() {
    let (tl, _p) = playing_a();
    let refused = |cmd: Command| tl.apply_at(cmd, 0).unwrap_err();
    assert!(matches!(
        refused(cue_next(BASE + 31 * S, BASE + 30 * S, None, None)),
        CommandError::Invalid(_)
    ));
    for speed in [0.0, -1.0, 9.0, f64::NAN] {
        assert!(matches!(
            refused(cue_next(BASE + 30 * S, BASE + 31 * S, Some(speed), None)),
            CommandError::Invalid(_)
        ));
    }
    assert!(matches!(
        refused(cue_next(BASE + 70 * S, BASE + 71 * S, None, None)),
        CommandError::OutOfRange(_)
    ));
    assert_eq!(tl.queued(), 0);
    for _ in 0..MAX_QUEUE_LEN {
        tl.apply_at(cue_next(BASE + 30 * S, BASE + 31 * S, None, None), 0)
            .unwrap();
    }
    assert!(matches!(
        refused(cue_next(BASE + 30 * S, BASE + 31 * S, None, None)),
        CommandError::Conflict(_)
    ));
    // A target relative to the cursor, from the wire.
    let (tl, _p) = playing_a();
    let r = parse_command(
        serde_json::json!({"cmd": "cue_next", "in": {"rel_ms": 1000},
        "out": {"rel_ms": 2000}, "client_ref": "n"}),
    )
    .unwrap();
    tl.apply_req_at(r, 0).unwrap();
    let item = tl.queue_view().1.unwrap();
    assert_eq!((item.start, item.end), (BASE + 11 * S, BASE + 12 * S));
    assert_eq!(item.client_ref.as_deref(), Some("n"));
    assert_eq!(item.speed, 1.0);
    assert_eq!(item.end_action, EndAction::Freeze);
}

/// A queued clip with a source is checked as a switch is, and keeps the
/// opened source for its angle (the player prepares it); one without keeps
/// the source each angle shows.
#[test]
fn a_queued_clip_with_a_source_names_its_angle() {
    use super::super::switch_tests::angle;
    let tl = get_or_create(uuid::Uuid::new_v4(), "g");
    let (_a, ra) = angle(&tl, "a", TimeDomain::Tai);
    let (_b, _rb) = angle(&tl, "b", TimeDomain::Tai);
    let cmd = |v: serde_json::Value| tl.apply(parse_command(v).unwrap());
    let range = |extra: serde_json::Value| {
        let mut v = serde_json::json!({"cmd": "cue_next",
            "in": {"t": (BASE + 10 * S).to_string()}, "out": {"t": (BASE + 11 * S).to_string()}});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    // Two angles: the clip names the one that switches.
    assert!(matches!(
        cmd(range(serde_json::json!({"source": "cam2"}))),
        Err(CommandError::Invalid(_))
    ));
    assert!(matches!(
        cmd(range(serde_json::json!({"source": "bad", "block": "a"}))),
        Err(CommandError::Invalid(_))
    ));
    assert_eq!(tl.queued(), 0);
    cmd(range(serde_json::json!({"source": "cam2", "block": "a"}))).unwrap();
    cmd(range(serde_json::json!({}))).unwrap();
    // Not switched now: when the clip starts, by the player.
    assert!(ra.begun.lock().unwrap().is_empty());
    let (_, next) = tl.queue_view();
    let next = next.unwrap();
    assert_eq!(next.source_for("a").map(|s| s.uri.as_str()), Some("cam2"));
    assert!(next.source_for("b").is_none());
    cmd(serde_json::json!({"cmd": "clear_queue"})).unwrap();
    assert_eq!(tl.queued(), 0);

    // No common time: a clip with a source is refused, one without is not.
    let loose = get_or_create(uuid::Uuid::new_v4(), "g");
    let (_c, _rc) = angle(&loose, "c", TimeDomain::Unanchored);
    let with = loose.apply(parse_command(range(serde_json::json!({"source": "cam2"}))).unwrap());
    assert!(matches!(with, Err(CommandError::Conflict(_))));
    loose
        .apply(parse_command(range(serde_json::json!({}))).unwrap())
        .unwrap();
    assert_eq!(loose.queued(), 1);
}
