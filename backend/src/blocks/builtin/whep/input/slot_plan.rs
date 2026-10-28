//! Which incoming video stream feeds which video output.
//!
//! Bookkeeping only, no GStreamer: [`super::video`] reports arrivals,
//! activity and removals, and carries out the actions this returns.
//!
//! - A stream that holds a slot keeps it while it sends. Once it has been
//!   silent for the timeout, it gives the slot up and waits, still running.
//! - A stream that does not get a slot waits. Waiting streams take free
//!   slots in the order they became active.
//! - `Fill`: a new stream waits for a free slot. Streams that run in
//!   parallel keep their outputs, so a mixer downstream keeps its inputs.
//! - `Newest`: a new stream, or one that starts sending again, takes over the
//!   slot of the stream that has held one longest. A source that switches
//!   (an SFU's pinned or active-speaker video) shows on the output at once.

use std::collections::BTreeMap;

/// What happens when a stream becomes active and every slot is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Assignment {
    Fill,
    Newest,
}

impl Assignment {
    pub(super) fn from_property(value: Option<&str>) -> Self {
        match value {
            Some(strom_types::block::WHEP_VIDEO_ASSIGNMENT_NEWEST) => Assignment::Newest,
            _ => Assignment::Fill,
        }
    }
}

/// A change to carry out, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// Stop feeding `slot` from `stream`. The stream keeps running, unrouted.
    Release { stream: usize, slot: usize },
    /// Feed `slot` from `stream`.
    Assign { stream: usize, slot: usize },
}

struct Stream {
    /// Order in which the stream last became active. Lower is older.
    since: u64,
    /// When the stream last delivered data, in ms on the caller's clock.
    last_seen_ms: u64,
    /// Whether the stream counted as active at the last check.
    active: bool,
    slot: Option<usize>,
}

pub(super) struct SlotPlan {
    policy: Assignment,
    timeout_ms: u64,
    /// The stream each slot holds.
    holders: Vec<Option<usize>>,
    streams: BTreeMap<usize, Stream>,
    activations: u64,
}

impl SlotPlan {
    pub(super) fn new(slots: usize, policy: Assignment, timeout_ms: u64) -> Self {
        SlotPlan {
            policy,
            timeout_ms,
            holders: vec![None; slots],
            streams: BTreeMap::new(),
            activations: 0,
        }
    }

    pub(super) fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Every held slot, with the stream that holds it.
    pub(super) fn holders(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.holders
            .iter()
            .enumerate()
            .filter_map(|(slot, h)| h.map(|stream| (slot, stream)))
    }

    fn is_active(&self, s: &Stream, now_ms: u64) -> bool {
        now_ms.saturating_sub(s.last_seen_ms) <= self.timeout_ms
    }

    /// The slot `stream` holds, if any.
    pub(super) fn slot_of(&self, stream: usize) -> Option<usize> {
        self.streams.get(&stream).and_then(|s| s.slot)
    }

    /// A new stream, active from `now_ms`.
    pub(super) fn arrive(&mut self, stream: usize, now_ms: u64) -> Vec<Action> {
        let since = self.next_activation();
        self.streams.insert(
            stream,
            Stream {
                since,
                last_seen_ms: now_ms,
                active: true,
                slot: None,
            },
        );
        let mut actions = Vec::new();
        self.release_silent(now_ms, &mut actions);
        if self.policy == Assignment::Newest {
            self.take_over(stream, &mut actions);
        }
        self.fill_free(now_ms, &mut actions);
        actions
    }

    /// `stream` delivered data at `last_seen_ms`.
    pub(super) fn seen(&mut self, stream: usize, last_seen_ms: u64) {
        if let Some(s) = self.streams.get_mut(&stream) {
            s.last_seen_ms = s.last_seen_ms.max(last_seen_ms);
        }
    }

    /// Re-evaluate at `now_ms`: release silent streams, let streams that
    /// started sending again in, and fill free slots.
    pub(super) fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        self.release_silent(now_ms, &mut actions);

        let mut resumed = Vec::new();
        let timeout_ms = self.timeout_ms;
        for (&id, s) in self.streams.iter_mut() {
            let active = now_ms.saturating_sub(s.last_seen_ms) <= timeout_ms;
            if active && !s.active {
                resumed.push(id);
            }
            s.active = active;
        }
        for id in resumed {
            let since = self.next_activation();
            if let Some(s) = self.streams.get_mut(&id) {
                s.since = since;
            }
            if self.policy == Assignment::Newest {
                self.take_over(id, &mut actions);
            }
        }

        self.fill_free(now_ms, &mut actions);
        actions
    }

    /// `stream` is gone (its source pad went away).
    pub(super) fn remove(&mut self, stream: usize, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(s) = self.streams.remove(&stream) {
            if let Some(slot) = s.slot {
                self.holders[slot] = None;
            }
        }
        self.fill_free(now_ms, &mut actions);
        actions
    }

    fn next_activation(&mut self) -> u64 {
        self.activations += 1;
        self.activations
    }

    /// Release every slot whose stream has been silent past the timeout.
    fn release_silent(&mut self, now_ms: u64, actions: &mut Vec<Action>) {
        for slot in 0..self.holders.len() {
            let Some(id) = self.holders[slot] else {
                continue;
            };
            let Some(s) = self.streams.get_mut(&id) else {
                continue;
            };
            if now_ms.saturating_sub(s.last_seen_ms) > self.timeout_ms {
                s.slot = None;
                s.active = false;
                self.holders[slot] = None;
                actions.push(Action::Release { stream: id, slot });
            }
        }
    }

    /// Give `stream` a slot now: the lowest free one, or else the slot of the
    /// stream that became active longest ago.
    fn take_over(&mut self, stream: usize, actions: &mut Vec<Action>) {
        if self.slot_of(stream).is_some() || self.holders.is_empty() {
            return;
        }
        let slot = match self.holders.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                let (slot, victim) = self
                    .holders
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, h)| h.map(|id| (slot, id)))
                    .min_by_key(|(_, id)| self.streams.get(id).map(|s| s.since))
                    .expect("every slot is held");
                if let Some(s) = self.streams.get_mut(&victim) {
                    s.slot = None;
                }
                actions.push(Action::Release {
                    stream: victim,
                    slot,
                });
                slot
            }
        };
        self.hand(stream, slot, actions);
    }

    /// Give free slots, lowest first, to waiting streams that are sending at
    /// `now_ms`, in the order they became active.
    fn fill_free(&mut self, now_ms: u64, actions: &mut Vec<Action>) {
        let mut waiting: Vec<(u64, usize)> = self
            .streams
            .iter()
            .filter(|(_, s)| s.slot.is_none() && self.is_active(s, now_ms))
            .map(|(&id, s)| (s.since, id))
            .collect();
        waiting.sort_unstable();
        let mut waiting = waiting.into_iter();
        for slot in 0..self.holders.len() {
            if self.holders[slot].is_some() {
                continue;
            }
            let Some((_, id)) = waiting.next() else {
                break;
            };
            self.hand(id, slot, actions);
        }
    }

    fn hand(&mut self, stream: usize, slot: usize, actions: &mut Vec<Action>) {
        self.holders[slot] = Some(stream);
        if let Some(s) = self.streams.get_mut(&stream) {
            s.slot = Some(slot);
        }
        actions.push(Action::Assign { stream, slot });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    const T: u64 = 1000;

    #[test]
    fn streams_fill_slots_in_arrival_order() {
        let mut plan = SlotPlan::new(2, Assignment::Fill, T);
        assert_eq!(plan.arrive(0, 0), vec![Assign { stream: 0, slot: 0 }]);
        assert_eq!(plan.arrive(1, 10), vec![Assign { stream: 1, slot: 1 }]);
        assert_eq!(plan.arrive(2, 20), vec![]);
        assert_eq!(plan.slot_of(2), None);
    }

    /// Fill: a stream that goes silent gives up its slot to the stream that
    /// has waited longest.
    #[test]
    fn fill_hands_a_silent_streams_slot_to_the_waiting_stream() {
        let mut plan = SlotPlan::new(1, Assignment::Fill, T);
        plan.arrive(0, 0);
        assert_eq!(plan.arrive(1, 100), vec![]);
        plan.seen(1, 900);
        assert_eq!(plan.tick(900), vec![], "stream 0 is within its timeout");
        // Stream 0 last sent at 0: past the timeout from 1001.
        plan.seen(1, 1500);
        assert_eq!(
            plan.tick(1500),
            vec![
                Release { stream: 0, slot: 0 },
                Assign { stream: 1, slot: 0 }
            ]
        );
    }

    /// Fill: a waiting stream that is itself silent does not get a free slot.
    #[test]
    fn fill_skips_a_silent_waiting_stream() {
        let mut plan = SlotPlan::new(1, Assignment::Fill, T);
        plan.arrive(0, 0);
        plan.arrive(1, 0);
        plan.seen(0, 1500);
        assert_eq!(plan.tick(1500), vec![]);
        assert_eq!(
            plan.remove(0, 1500),
            vec![],
            "stream 1 has been silent since 0"
        );
        plan.seen(1, 1600);
        assert_eq!(plan.tick(1600), vec![Assign { stream: 1, slot: 0 }]);
    }

    /// Newest: the new stream takes over at once (an SFU switching its
    /// pinned source to a new SSRC).
    #[test]
    fn newest_takes_over_the_oldest_slot_at_once() {
        let mut plan = SlotPlan::new(1, Assignment::Newest, T);
        plan.arrive(0, 0);
        assert_eq!(
            plan.arrive(1, 100),
            vec![
                Release { stream: 0, slot: 0 },
                Assign { stream: 1, slot: 0 }
            ]
        );
    }

    /// Newest: a stream that starts sending again after a silence counts as
    /// new, so switching back to an earlier source shows at once.
    #[test]
    fn newest_lets_a_resumed_stream_take_over() {
        let mut plan = SlotPlan::new(1, Assignment::Newest, T);
        plan.arrive(0, 0);
        plan.arrive(1, 100);
        // Stream 1 keeps sending; stream 0 is silent and then resumes.
        plan.seen(1, 1500);
        assert_eq!(plan.tick(1500), vec![]);
        plan.seen(0, 1600);
        plan.seen(1, 1600);
        assert_eq!(
            plan.tick(1600),
            vec![
                Release { stream: 1, slot: 0 },
                Assign { stream: 0, slot: 0 }
            ]
        );
    }

    /// Newest with free slots: no stream is pushed out.
    #[test]
    fn newest_uses_a_free_slot_first() {
        let mut plan = SlotPlan::new(2, Assignment::Newest, T);
        plan.arrive(0, 0);
        assert_eq!(plan.arrive(1, 10), vec![Assign { stream: 1, slot: 1 }]);
    }

    /// Newest over several slots replaces the stream that became active
    /// first, not the lowest slot.
    #[test]
    fn newest_replaces_the_longest_held_slot() {
        let mut plan = SlotPlan::new(2, Assignment::Newest, T);
        plan.arrive(0, 0);
        plan.arrive(1, 10);
        assert_eq!(
            plan.arrive(2, 20),
            vec![
                Release { stream: 0, slot: 0 },
                Assign { stream: 2, slot: 0 }
            ]
        );
        assert_eq!(
            plan.arrive(3, 30),
            vec![
                Release { stream: 1, slot: 1 },
                Assign { stream: 3, slot: 1 }
            ]
        );
    }

    /// A removed stream frees its slot for the next waiting stream.
    #[test]
    fn a_removed_stream_frees_its_slot() {
        let mut plan = SlotPlan::new(1, Assignment::Fill, T);
        plan.arrive(0, 0);
        plan.arrive(1, 10);
        assert_eq!(plan.remove(0, 20), vec![Assign { stream: 1, slot: 0 }]);
    }

    /// A waiting stream that went silent since the last check must not get
    /// a slot that frees up: activity is judged at the time of the call.
    #[test]
    fn a_freed_slot_skips_a_stream_that_went_silent_since_the_last_tick() {
        let mut plan = SlotPlan::new(1, Assignment::Fill, T);
        plan.arrive(0, 0);
        plan.arrive(1, 0);
        plan.seen(0, 500);
        assert_eq!(plan.tick(500), vec![]);
        // No tick between 500 and 1500: stream 1's flag still says active.
        assert_eq!(plan.remove(0, 1500), vec![]);
    }

    #[test]
    fn no_slots_never_assigns() {
        let mut plan = SlotPlan::new(0, Assignment::Newest, T);
        assert_eq!(plan.arrive(0, 0), vec![]);
        assert_eq!(plan.tick(5000), vec![]);
    }
}
