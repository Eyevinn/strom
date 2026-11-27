//! Switching an angle's source at runtime (`cue {source}`, `source`): the
//! checks a switch must pass before anything changes, and the events that
//! say how it ended. The player does the switch itself (see
//! `player::ReplayPlayer`): a new index and fetcher behind the same decoders
//! and output, no rebuild of the flow.

use super::super::source::SourceIndex;
use super::stats::SourceIdent;
use super::{CommandError, PlayerStats, ReplayTimeline, TimeDomain, TimeSource};
use std::sync::Arc;
use strom_types::replay::{ReplayCause, ReplayEventKind};

/// A source opened for a switch, not yet used.
pub struct OpenedSource {
    pub index: Arc<dyn SourceIndex>,
    /// The URI as given (may carry credentials: never shown or logged). Tells
    /// a switch to the source already shown.
    pub uri: String,
    pub ident: SourceIdent,
}

/// What a player offers for a source switch.
pub trait SourceControl: Send + Sync {
    /// Open `uri` for a switch, or say why it cannot be. Changes nothing.
    fn open_source(&self, uri: &str) -> Result<OpenedSource, String>;
    /// Start the switch: the player holds its last picture until the new
    /// source has the frame for the cursor, then shows it
    /// ([`ReplayTimeline::source_changed`]), or gives up and keeps the old
    /// source ([`ReplayTimeline::source_failed`]).
    fn begin_switch(&self, source: OpenedSource, client_ref: Option<String>);
}

/// A switch that passed its checks, to start once the command applied.
pub struct Switch {
    control: Arc<dyn SourceControl>,
    source: OpenedSource,
}

impl Switch {
    pub fn begin(self, client_ref: Option<String>) {
        self.control.begin_switch(self.source, client_ref);
    }
}

impl ReplayTimeline {
    /// The switch a command asks for, checked and with its source opened,
    /// or `None` when it asks for none. An error refuses the whole command.
    ///
    /// A group with no common time (`unanchored`) is refused: there each
    /// angle's times are on a base of their own, and a new source would
    /// move the group's axis. On real TAI the new source lines up by itself.
    pub fn prepare_switch(
        &self,
        source: Option<&str>,
        block: Option<&str>,
    ) -> Result<Option<Switch>, CommandError> {
        let Some(uri) = source else {
            if block.is_some() {
                return Err(CommandError::Invalid(
                    "block: names the angle that switches, needs source".into(),
                ));
            }
            return Ok(None);
        };
        if uri.trim().is_empty() {
            return Err(CommandError::Invalid("source: empty".into()));
        }
        if self.time_source() != TimeSource::Clock {
            return Err(CommandError::Conflict(
                "source: an export (pacing offline) cannot switch source".into(),
            ));
        }
        match self.time_domain() {
            TimeDomain::Tai => {}
            TimeDomain::Unanchored => {
                return Err(CommandError::Conflict(
                    "source: the group has no common time (time_domain unanchored); a new \
                     source would move its time axis"
                        .into(),
                ))
            }
            TimeDomain::Unknown => {
                return Err(CommandError::Conflict(
                    "source: the group's time domain is not known yet".into(),
                ))
            }
        }
        let player = self.angle(block)?;
        let control = player.control().ok_or_else(|| {
            CommandError::Conflict(format!(
                "source: angle '{}' cannot switch source",
                player.block_id
            ))
        })?;
        let source = control
            .open_source(uri.trim())
            .map_err(|e| CommandError::Invalid(format!("source: {}", e)))?;
        Ok(Some(Switch { control, source }))
    }

    /// The angle `block`, or the group's only angle.
    fn angle(&self, block: Option<&str>) -> Result<Arc<PlayerStats>, CommandError> {
        let players = self.players();
        match block {
            Some(b) => players
                .into_iter()
                .find(|p| p.block_id == b)
                .ok_or_else(|| CommandError::Invalid(format!("block: no angle '{}'", b))),
            None if players.len() == 1 => Ok(players.into_iter().next().expect("one")),
            None => Err(CommandError::Invalid(format!(
                "block: the group has {} angles, name the one that switches",
                players.len()
            ))),
        }
    }

    /// Angle `block` shows its new source now.
    pub fn source_changed(&self, block: &str, source: Option<String>, client_ref: Option<String>) {
        self.emit(
            ReplayCause::Source,
            client_ref,
            ReplayEventKind::SourceChanged {
                block: block.to_string(),
                source,
            },
        );
    }

    /// Angle `block` gave up switching to `source` and stays on its old one.
    pub fn source_failed(
        &self,
        block: &str,
        source: Option<String>,
        error: String,
        client_ref: Option<String>,
    ) {
        self.emit(
            ReplayCause::Source,
            client_ref,
            ReplayEventKind::SourceFailed {
                block: block.to_string(),
                source,
                error,
            },
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::super::source::{Segment, SourceInfo};
    use super::super::{get_or_create, parse_command, PlayerConfig};
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;
    use strom_types::replay::ReplayEventKind;

    const BASE: i64 = 1_800_000_000_000_000_000;
    const S: i64 = 1_000_000_000;

    /// An index that is never read.
    struct NoIndex;

    #[async_trait::async_trait]
    impl SourceIndex for NoIndex {
        fn describe(&self) -> String {
            "none".into()
        }
        fn may_grow(&self) -> bool {
            false
        }
        async fn info(&self) -> anyhow::Result<SourceInfo> {
            anyhow::bail!("not read")
        }
        async fn list(&self, _: i64, _: i64) -> anyhow::Result<Vec<Segment>> {
            anyhow::bail!("not read")
        }
        async fn first(&self) -> anyhow::Result<Option<Segment>> {
            anyhow::bail!("not read")
        }
        async fn last(&self) -> anyhow::Result<Option<Segment>> {
            anyhow::bail!("not read")
        }
        async fn fetch(&self, _: &Segment) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("not read")
        }
    }

    /// A player's switch control that records what it was asked.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub begun: Mutex<Vec<(String, Option<String>)>>,
    }

    impl SourceControl for Recorder {
        fn open_source(&self, uri: &str) -> Result<OpenedSource, String> {
            if uri.starts_with("bad") {
                return Err(format!("cannot open {}", uri));
            }
            Ok(OpenedSource {
                index: Arc::new(NoIndex),
                uri: uri.to_string(),
                ident: SourceIdent {
                    source: Some(uri.to_string()),
                    ..Default::default()
                },
            })
        }

        fn begin_switch(&self, source: OpenedSource, client_ref: Option<String>) {
            self.begun.lock().unwrap().push((source.uri, client_ref));
        }
    }

    /// A player of 60 s of media from `BASE` in `domain`, with a recorder
    /// as its control (kept alive by the caller).
    pub(crate) fn angle(
        tl: &ReplayTimeline,
        block: &str,
        domain: TimeDomain,
    ) -> (Arc<PlayerStats>, Arc<Recorder>) {
        let p = Arc::new(PlayerStats::with_config(block, PlayerConfig::default()));
        p.start_tai.store(BASE, Ordering::Relaxed);
        p.end_tai.store(BASE + 60 * S, Ordering::Relaxed);
        p.init_grid(50, 1, BASE);
        p.set_time_domain(domain);
        let rec = Arc::new(Recorder::default());
        let control: std::sync::Weak<dyn SourceControl> =
            Arc::downgrade(&rec) as std::sync::Weak<Recorder>;
        p.set_control(control);
        tl.register_player(&p);
        (p, rec)
    }

    fn apply(tl: &ReplayTimeline, cmd: serde_json::Value) -> Result<i64, CommandError> {
        tl.apply(parse_command(cmd).unwrap()).map(|a| a.tai)
    }

    fn begun(rec: &Recorder) -> Vec<(String, Option<String>)> {
        rec.begun.lock().unwrap().clone()
    }

    #[test]
    fn a_cue_with_a_source_switches_once_the_cue_applied() {
        let tl = get_or_create(uuid::Uuid::new_v4(), "g");
        let (_p, rec) = angle(&tl, "p1", TimeDomain::Tai);
        let cue = |out: i64, r: &str| {
            serde_json::json!({"cmd": "cue", "in": {"t": (BASE + 10 * S).to_string()},
                "out": {"t": out.to_string()}, "source": "cam2", "client_ref": r})
        };
        // A cue refused for its range starts no switch.
        assert!(matches!(
            apply(&tl, cue(BASE + 5 * S, "x")),
            Err(CommandError::Invalid(_))
        ));
        assert!(begun(&rec).is_empty());
        assert_eq!(apply(&tl, cue(BASE + 12 * S, "c1")), Ok(BASE + 10 * S));
        assert_eq!(begun(&rec), vec![("cam2".into(), Some("c1".into()))]);
        // `source` leaves the motion as it is.
        let before = tl.cursor_now().unwrap();
        let t = apply(
            &tl,
            serde_json::json!({"cmd": "source", "source": "cam3", "block": "p1"}),
        )
        .unwrap();
        assert_eq!(t, before.0);
        assert!(tl.cue().is_some());
        assert_eq!(begun(&rec)[1], ("cam3".into(), None));
    }

    #[test]
    fn a_switch_is_refused_without_a_common_time() {
        for (domain, what) in [
            (TimeDomain::Unanchored, "unanchored"),
            (TimeDomain::Unknown, "not known"),
        ] {
            let tl = get_or_create(uuid::Uuid::new_v4(), "g");
            let (_p, rec) = angle(&tl, "p1", domain);
            let before = tl.cursor_now().unwrap();
            let r = apply(
                &tl,
                serde_json::json!({"cmd": "cue", "in": {"t": (BASE + 10 * S).to_string()},
                    "out": {"t": (BASE + 12 * S).to_string()}, "source": "cam2"}),
            );
            match r {
                Err(CommandError::Conflict(m)) => assert!(m.contains(what), "{}", m),
                other => panic!("{:?}", other),
            }
            // Refused whole: the cue did not apply either.
            assert_eq!(tl.cursor_now().unwrap(), before);
            assert!(tl.cue().is_none());
            assert!(begun(&rec).is_empty());
        }
    }

    #[test]
    fn a_switch_names_its_angle_and_a_source_that_opens() {
        let tl = get_or_create(uuid::Uuid::new_v4(), "g");
        let (_a, ra) = angle(&tl, "a", TimeDomain::Tai);
        let (_b, rb) = angle(&tl, "b", TimeDomain::Tai);
        let src = |v: serde_json::Value| apply(&tl, v);
        let invalid = |r: Result<i64, CommandError>, has: &str| match r {
            Err(CommandError::Invalid(m)) => assert!(m.contains(has), "{}", m),
            other => panic!("{:?}", other),
        };
        // Two angles: the command names one.
        invalid(
            src(serde_json::json!({"cmd": "source", "source": "x"})),
            "2 angles",
        );
        invalid(
            src(serde_json::json!({"cmd": "source", "source": "x", "block": "c"})),
            "no angle 'c'",
        );
        invalid(
            src(serde_json::json!({"cmd": "source", "source": "bad", "block": "a"})),
            "cannot open bad",
        );
        invalid(
            src(serde_json::json!({"cmd": "source", "source": " ", "block": "a"})),
            "empty",
        );
        // A block without a source says nothing to switch.
        invalid(
            src(
                serde_json::json!({"cmd": "cue", "in": {"t": BASE.to_string()},
                "out": {"t": (BASE + S).to_string()}, "block": "a"}),
            ),
            "needs source",
        );
        src(serde_json::json!({"cmd": "source", "source": "x", "block": "b"})).unwrap();
        assert!(begun(&ra).is_empty());
        assert_eq!(begun(&rb), vec![("x".into(), None)]);
        // An angle that cannot switch (its player is gone).
        drop(rb);
        assert!(matches!(
            src(serde_json::json!({"cmd": "source", "source": "x", "block": "b"})),
            Err(CommandError::Conflict(_))
        ));
    }

    #[test]
    fn an_export_does_not_switch() {
        let tl = get_or_create(uuid::Uuid::new_v4(), "g");
        let (_p, rec) = angle(&tl, "p1", TimeDomain::Tai);
        tl.use_counter();
        assert!(matches!(
            apply(&tl, serde_json::json!({"cmd": "source", "source": "x"})),
            Err(CommandError::Conflict(_))
        ));
        assert!(begun(&rec).is_empty());
    }

    /// The player's stats take over the new source's extent, grid (also
    /// when one was set), time domain and identity; its counters stay.
    #[test]
    fn a_player_adopts_the_new_sources_knowledge() {
        let p = PlayerStats::with_config(
            "p1",
            PlayerConfig {
                source: Some("cam1".into()),
                ..Default::default()
            },
        );
        p.init_grid(50, 1, 0);
        p.misses.store(3, Ordering::Relaxed);
        p.set_pending_source(Some("cam2".into()));
        let new = PlayerStats::new("p1");
        new.start_tai.store(5 * S, Ordering::Relaxed);
        new.end_tai.store(9 * S, Ordering::Relaxed);
        new.init_grid(25, 1, 5 * S);
        new.set_time_domain(TimeDomain::Tai);
        p.adopt_source(
            &new,
            SourceIdent {
                source: Some("cam2".into()),
                ..Default::default()
            },
        );
        assert_eq!(p.source_extent(), Some((5 * S, 9 * S)));
        assert_eq!(p.grid().map(|g| g.fps_n), Some(25));
        assert_eq!(p.time_domain(), TimeDomain::Tai);
        assert_eq!(p.source_ident().source.as_deref(), Some("cam2"));
        assert_eq!(p.pending_source(), None);
        assert_eq!(p.misses.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn switch_events_carry_the_ref() {
        let tl = get_or_create(uuid::Uuid::new_v4(), "g");
        let mut events = tl.subscribe();
        tl.source_changed("p1", Some("cam2".into()), Some("r".into()));
        let ev = events.try_recv().unwrap();
        assert_eq!(ev.cause, strom_types::replay::ReplayCause::Source);
        assert_eq!(ev.client_ref.as_deref(), Some("r"));
        assert!(matches!(ev.kind, ReplayEventKind::SourceChanged { .. }));
        tl.source_failed("p1", None, "no".into(), None);
        assert!(matches!(
            events.try_recv().unwrap().kind,
            ReplayEventKind::SourceFailed { .. }
        ));
    }
}
