//! Time offset per source, for a whole flow: a source's media sits this much
//! later on a replay group's axis in every player that shows it, in any
//! group, before and after a source switch. The offset belongs to the source
//! (the camera), not to the player that happens to show it.
//!
//! The flow property `source_offsets_ms` is the record; this registry is its
//! copy for running players, set when a flow is saved or started
//! ([`set_flow`]). A player resolves its offset from here by the source it
//! shows ([`lookup`]) and falls back to its own `offset_ms`.

use super::timeline::{self, SourceIdent};
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, RwLock};
use strom_types::replay::MAX_SOURCE_OFFSET_MS;
use strom_types::FlowId;
use tracing::warn;

const MS: i64 = 1_000_000;

/// Offsets (ns) by source key, per flow.
static OFFSETS: LazyLock<RwLock<HashMap<FlowId, HashMap<String, i64>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Why an entry cannot be a source offset, if it cannot.
pub fn check(key: &str, offset_ms: i64) -> Result<(), String> {
    if key.trim().is_empty() {
        return Err("source_offsets_ms: empty source".into());
    }
    if !(-MAX_SOURCE_OFFSET_MS..=MAX_SOURCE_OFFSET_MS).contains(&offset_ms) {
        return Err(format!(
            "source_offsets_ms: '{}' {} ms, want -{} .. {} (24 h)",
            key, offset_ms, MAX_SOURCE_OFFSET_MS, MAX_SOURCE_OFFSET_MS
        ));
    }
    Ok(())
}

/// Take the flow's offsets (its `source_offsets_ms`), and move the players
/// of the flow that run now if theirs changed. An entry that fails
/// [`check`] is left out.
pub fn set_flow(flow_id: FlowId, offsets_ms: &BTreeMap<String, i64>) {
    let map: HashMap<String, i64> = offsets_ms
        .iter()
        .filter_map(|(k, &ms)| match check(k, ms) {
            Ok(()) => Some((k.trim().to_string(), ms * MS)),
            Err(e) => {
                warn!("Flow {}: ignoring {}", flow_id, e);
                None
            }
        })
        .collect();
    {
        let mut all = OFFSETS.write().unwrap_or_else(|e| e.into_inner());
        let changed = match all.get(&flow_id) {
            Some(old) => *old != map,
            None => !map.is_empty(),
        };
        if !changed {
            return;
        }
        if map.is_empty() {
            all.remove(&flow_id);
        } else {
            all.insert(flow_id, map);
        }
    }
    for (_, _, tl) in timeline::running(Some(flow_id)) {
        tl.refresh_offsets();
    }
}

/// Forget a deleted flow's offsets.
pub fn remove_flow(flow_id: FlowId) {
    OFFSETS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&flow_id);
}

/// The offset (ns) the flow gives `ident`: by its URI, else by its own id.
pub fn lookup(flow_id: Option<FlowId>, ident: &SourceIdent) -> Option<i64> {
    let flow_id = flow_id?;
    let all = OFFSETS.read().unwrap_or_else(|e| e.into_inner());
    let map = all.get(&flow_id)?;
    [ident.source.as_deref(), ident.source_id.as_deref()]
        .into_iter()
        .flatten()
        .find_map(|k| map.get(k).copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mediaplayer::replay::timeline::{
        get_or_create, PlayerConfig, PlayerStats,
    };
    use std::sync::Arc;

    fn offsets(entries: &[(&str, i64)]) -> BTreeMap<String, i64> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// A player of `flow` built on `source`, with its own `offset_ms`.
    fn player(flow: FlowId, block: &str, source: &str, own_ms: i64) -> Arc<PlayerStats> {
        Arc::new(PlayerStats::with_config(
            block,
            PlayerConfig {
                source: Some(format!("tams+https://gw/flows/{}", source)),
                source_id: Some(source.to_string()),
                offset_ns: own_ms * MS,
                flow_id: Some(flow),
                ..Default::default()
            },
        ))
    }

    fn ident(source: &str) -> SourceIdent {
        SourceIdent {
            source: Some(format!("tams+https://gw/flows/{}", source)),
            source_id: Some(source.to_string()),
            store: None,
        }
    }

    /// One offset per source for the whole flow: every player that shows
    /// the source uses it, in any group, and moves at once when it changes.
    /// Another flow is not touched.
    #[test]
    fn the_offset_applies_across_groups() {
        let flow = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        set_flow(flow, &offsets(&[("cam1", 2500)]));
        let g1 = get_or_create(flow, "g1");
        let g2 = get_or_create(flow, "g2");
        let a = player(flow, "a", "cam1", 0);
        let b = player(flow, "b", "cam1", 700);
        let c = player(flow, "c", "cam2", 0);
        let d = player(other, "d", "cam1", 0);
        g1.register_player(&a);
        g2.register_player(&b);
        g2.register_player(&c);
        assert_eq!(a.offset_ns(), 2500 * MS);
        assert_eq!(b.offset_ns(), 2500 * MS);
        assert_eq!(c.offset_ns(), 0);
        assert_eq!(d.offset_ns(), 0);
        // Changed while running, by its URI this time.
        set_flow(
            flow,
            &offsets(&[("tams+https://gw/flows/cam1", -400), ("cam2", 120)]),
        );
        assert_eq!(a.offset_ns(), -400 * MS);
        assert_eq!(b.offset_ns(), -400 * MS);
        assert_eq!(c.offset_ns(), 120 * MS);
        assert_eq!(d.offset_ns(), 0);
        // The URI wins over the id when both are given.
        set_flow(
            flow,
            &offsets(&[("tams+https://gw/flows/cam1", 10), ("cam1", 20)]),
        );
        assert_eq!(a.offset_ns(), 10 * MS);
        remove_flow(flow);
    }

    /// After a source switch the player takes the new source's offset; a
    /// source the flow does not list gets the player's own `offset_ms`.
    #[test]
    fn the_offset_follows_a_source_switch() {
        let flow = uuid::Uuid::new_v4();
        set_flow(flow, &offsets(&[("cam1", 1000), ("cam2", -300)]));
        let p = player(flow, "p", "cam1", 50);
        assert_eq!(p.offset_ns(), 1000 * MS);
        let new = PlayerStats::new("p");
        p.adopt_source(&new, ident("cam2"));
        assert_eq!(p.offset_ns(), -300 * MS);
        p.adopt_source(&new, ident("cam3"));
        assert_eq!(p.offset_ns(), 50 * MS);
        p.adopt_source(&new, ident("cam1"));
        assert_eq!(p.offset_ns(), 1000 * MS);
        remove_flow(flow);
    }

    /// A flow without source offsets (an old flow) keeps each player's own
    /// `offset_ms`; removing a source's offset falls back to it again.
    #[test]
    fn the_players_own_offset_is_the_fallback() {
        let flow = uuid::Uuid::new_v4();
        let g = get_or_create(flow, "g");
        let p = player(flow, "p", "cam1", 700);
        g.register_player(&p);
        assert_eq!(p.offset_ns(), 700 * MS);
        set_flow(flow, &offsets(&[("cam1", 0)]));
        assert_eq!(p.offset_ns(), 0);
        set_flow(flow, &BTreeMap::new());
        assert_eq!(p.offset_ns(), 700 * MS);
        // A player outside any flow has only its own.
        let q = PlayerStats::with_config(
            "q",
            PlayerConfig {
                offset_ns: 30 * MS,
                ..Default::default()
            },
        );
        assert_eq!(q.offset_ns(), 30 * MS);
    }

    /// A player's control whose lane being prepared (a switch, or a queued
    /// clip) has stats of its own.
    struct Preparing(Arc<PlayerStats>);

    impl timeline::SourceControl for Preparing {
        fn open_source(&self, _: &str) -> Result<timeline::OpenedSource, String> {
            Err("not used".into())
        }
        fn begin_switch(&self, _: timeline::OpenedSource, _: Option<String>) {}
        fn pending_stats(&self) -> Option<Arc<PlayerStats>> {
            Some(Arc::clone(&self.0))
        }
    }

    /// The lane being prepared for another source moves with the flow's
    /// offsets as well, not only the lane shown: it must fetch and decode
    /// the stretch the new offset asks for.
    #[test]
    fn a_prepared_lane_takes_a_changed_offset() {
        let flow = uuid::Uuid::new_v4();
        set_flow(flow, &offsets(&[("cam1", 100), ("cam2", 200)]));
        let g = get_or_create(flow, "g");
        let p = player(flow, "p", "cam1", 0);
        g.register_player(&p);
        let ctl = Arc::new(Preparing(player(flow, "p", "cam2", 0)));
        let weak: std::sync::Weak<dyn timeline::SourceControl> =
            Arc::downgrade(&ctl) as std::sync::Weak<Preparing>;
        p.set_control(weak);
        assert_eq!(ctl.0.offset_ns(), 200 * MS);
        set_flow(flow, &offsets(&[("cam1", 100), ("cam2", -900)]));
        assert_eq!(p.offset_ns(), 100 * MS);
        assert_eq!(ctl.0.offset_ns(), -900 * MS);
        remove_flow(flow);
    }

    #[test]
    fn bad_entries_are_refused_or_left_out() {
        assert!(check("cam1", MAX_SOURCE_OFFSET_MS).is_ok());
        assert!(check("cam1", -MAX_SOURCE_OFFSET_MS - 1).is_err());
        assert!(check(" ", 0).is_err());
        let flow = uuid::Uuid::new_v4();
        set_flow(
            flow,
            &offsets(&[("cam1", MAX_SOURCE_OFFSET_MS + 1), ("cam2", 5)]),
        );
        assert_eq!(lookup(Some(flow), &ident("cam1")), None);
        assert_eq!(lookup(Some(flow), &ident("cam2")), Some(5 * MS));
        remove_flow(flow);
        assert_eq!(lookup(Some(flow), &ident("cam2")), None);
    }
}
