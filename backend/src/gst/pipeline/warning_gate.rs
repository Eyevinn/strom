//! Lets a repeated bus warning through once per interval.
//!
//! Some elements post the same warning in a loop. An aggregator whose latency
//! cannot be configured posts "Impossible to configure latency" on every
//! aggregation cycle, which on a mixer is thousands per second. Logging each
//! one and broadcasting it to every client costs more CPU than the flow itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a warning is held back after it was last let through.
pub(super) const WARNING_REPEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Distinct warnings tracked before stale ones are dropped. A warning whose
/// text changes every time (a timestamp in the debug string) would otherwise
/// grow the map without bound.
const MAX_TRACKED: usize = 256;

struct Seen {
    last_reported: Instant,
    suppressed: u64,
}

#[derive(Default)]
pub(super) struct WarningGate {
    seen: HashMap<String, Seen>,
}

impl WarningGate {
    /// Whether to report this warning now. `Some(n)` lets it through, with `n`
    /// identical warnings held back since it was last reported. `None` holds it
    /// back.
    pub(super) fn check(&mut self, key: String, now: Instant) -> Option<u64> {
        if let Some(seen) = self.seen.get_mut(&key) {
            if now.duration_since(seen.last_reported) < WARNING_REPEAT_INTERVAL {
                seen.suppressed += 1;
                return None;
            }
            let suppressed = seen.suppressed;
            seen.last_reported = now;
            seen.suppressed = 0;
            return Some(suppressed);
        }
        if self.seen.len() >= MAX_TRACKED {
            self.seen
                .retain(|_, seen| now.duration_since(seen.last_reported) < WARNING_REPEAT_INTERVAL);
        }
        self.seen.insert(
            key,
            Seen {
                last_reported: now,
                suppressed: 0,
            },
        );
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_warning_passes_and_repeats_are_held_back() {
        let mut gate = WarningGate::default();
        let t0 = Instant::now();
        assert_eq!(gate.check("a".into(), t0), Some(0));
        for i in 1..=1000 {
            assert_eq!(gate.check("a".into(), t0 + Duration::from_millis(i)), None);
        }
    }

    #[test]
    fn a_repeat_after_the_interval_passes_with_the_held_back_count() {
        let mut gate = WarningGate::default();
        let t0 = Instant::now();
        gate.check("a".into(), t0);
        gate.check("a".into(), t0 + Duration::from_secs(1));
        gate.check("a".into(), t0 + Duration::from_secs(2));
        let later = t0 + WARNING_REPEAT_INTERVAL;
        assert_eq!(gate.check("a".into(), later), Some(2));
        assert_eq!(gate.check("a".into(), later + Duration::from_secs(1)), None);
    }

    #[test]
    fn different_warnings_are_gated_separately() {
        let mut gate = WarningGate::default();
        let t0 = Instant::now();
        assert_eq!(gate.check("a".into(), t0), Some(0));
        assert_eq!(gate.check("b".into(), t0), Some(0));
        assert_eq!(gate.check("a".into(), t0), None);
    }

    #[test]
    fn stale_warnings_are_dropped_when_the_map_is_full() {
        let mut gate = WarningGate::default();
        let t0 = Instant::now();
        for i in 0..MAX_TRACKED {
            gate.check(format!("w{i}"), t0);
        }
        gate.check("new".into(), t0 + WARNING_REPEAT_INTERVAL);
        assert_eq!(gate.seen.len(), 1);
    }
}
