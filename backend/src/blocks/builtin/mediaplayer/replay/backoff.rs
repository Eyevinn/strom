//! Per-key retry backoff for the replay engine's failing work (listings,
//! fetches, decodes), so one bad segment or a gateway that is down is retried
//! with growing pauses instead of in a hot loop.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
struct Entry {
    next: Instant,
    attempts: u32,
}

/// Exponential backoff per key: after the n-th consecutive failure the key
/// waits `initial * 2^(n-1)`, at most `max`.
#[derive(Debug)]
pub struct Backoff<K> {
    entries: HashMap<K, Entry>,
    initial: Duration,
    max: Duration,
}

impl<K: Hash + Eq + Copy> Backoff<K> {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            initial,
            max,
        }
    }

    /// Whether `key` may be tried at `now`.
    pub fn ready(&self, key: &K, now: Instant) -> bool {
        self.entries.get(key).is_none_or(|e| now >= e.next)
    }

    /// Record a failure of `key` at `now`. Returns the number of consecutive
    /// failures, including this one.
    pub fn fail(&mut self, key: K, now: Instant) -> u32 {
        let e = self.entries.entry(key).or_insert(Entry {
            next: now,
            attempts: 0,
        });
        e.attempts = e.attempts.saturating_add(1);
        let shift = (e.attempts - 1).min(16);
        let wait = self.initial.saturating_mul(1 << shift).min(self.max);
        e.next = now + wait;
        e.attempts
    }

    /// Forget `key` after a success.
    pub fn succeed(&mut self, key: &K) {
        self.entries.remove(key);
    }

    /// Keep only the keys for which `f` is true (e.g. those still near the
    /// cursor), so the map does not grow without bound.
    pub fn retain(&mut self, mut f: impl FnMut(&K) -> bool) {
        self.entries.retain(|k, _| f(k));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Whether the `attempts`-th consecutive failure is worth a warning: the
/// first, then 2, 4, 8, ... The others go to debug.
pub fn worth_warning(attempts: u32) -> bool {
    attempts.is_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_grow_and_are_capped() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(350));
        let t0 = Instant::now();
        assert!(b.ready(&1, t0));
        assert_eq!(b.fail(1, t0), 1);
        assert!(!b.ready(&1, t0 + Duration::from_millis(99)));
        assert!(b.ready(&1, t0 + Duration::from_millis(100)));
        assert_eq!(b.fail(1, t0), 2);
        assert!(!b.ready(&1, t0 + Duration::from_millis(199)));
        assert!(b.ready(&1, t0 + Duration::from_millis(200)));
        b.fail(1, t0);
        // 400 ms capped to 350 ms.
        assert!(b.ready(&1, t0 + Duration::from_millis(350)));
        // Other keys are unaffected.
        assert!(b.ready(&2, t0));
        b.succeed(&1);
        assert!(b.ready(&1, t0));
        assert_eq!(b.fail(1, t0), 1);
    }

    #[test]
    fn many_failures_do_not_overflow() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let t0 = Instant::now();
        for _ in 0..100 {
            b.fail(7, t0);
        }
        assert!(b.ready(&7, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn retain_prunes() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(5));
        let t0 = Instant::now();
        b.fail(1, t0);
        b.fail(2, t0);
        b.retain(|k| *k == 2);
        assert_eq!(b.len(), 1);
        assert!(b.ready(&1, t0));
    }

    #[test]
    fn warnings_thin_out() {
        let warned: Vec<u32> = (1..=20).filter(|a| worth_warning(*a)).collect();
        assert_eq!(warned, vec![1, 2, 4, 8, 16]);
    }
}
