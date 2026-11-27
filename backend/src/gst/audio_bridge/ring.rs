//! A FIFO measured in time, written by one flow and read by another.
//!
//! Nothing here knows what a chunk holds. Depth is tracked in the chunk's own
//! unit (audio frames, for the audio bridge) so it stays exact, and converted
//! to nanoseconds only on the way out.

use std::collections::VecDeque;

/// One piece of media in the ring.
pub trait Chunk {
    /// Length still unread, in the ring's units.
    fn units(&self) -> u64;
    /// Mark `units` (at most [`Chunk::units`]) as read. A chunk that cannot be
    /// split, such as a video frame, treats any advance as consuming it whole.
    fn advance(&mut self, units: u64);
}

pub struct Ring<T> {
    items: VecDeque<T>,
    depth: u64,
    units_per_second: u64,
    capacity: u64,
}

impl<T: Chunk> Ring<T> {
    pub fn new(units_per_second: u64, capacity_ns: u64) -> Self {
        let mut ring = Self {
            items: VecDeque::new(),
            depth: 0,
            units_per_second,
            capacity: 0,
        };
        ring.capacity = ring.ns_to_units(capacity_ns);
        ring
    }

    pub fn units_to_ns(&self, units: u64) -> u64 {
        (units as u128 * 1_000_000_000 / self.units_per_second as u128) as u64
    }

    pub fn ns_to_units(&self, ns: u64) -> u64 {
        (ns as u128 * self.units_per_second as u128 / 1_000_000_000) as u64
    }

    pub fn depth(&self) -> u64 {
        self.depth
    }

    pub fn depth_ns(&self) -> u64 {
        self.units_to_ns(self.depth)
    }

    /// Append a chunk. When that takes the ring past its capacity the oldest
    /// content is discarded; returns how many units that was.
    pub fn push(&mut self, chunk: T) -> u64 {
        let units = chunk.units();
        if units == 0 {
            return 0;
        }
        self.depth += units;
        self.items.push_back(chunk);
        if self.depth > self.capacity {
            self.discard(self.depth - self.capacity)
        } else {
            0
        }
    }

    /// Discard up to `units` from the front; returns how many were discarded.
    pub fn discard(&mut self, units: u64) -> u64 {
        let mut left = units;
        while left > 0 {
            let Some(front) = self.items.front_mut() else {
                break;
            };
            let before = front.units();
            front.advance(left.min(before));
            let taken = before - front.units();
            if taken == 0 {
                break;
            }
            self.depth -= taken;
            left -= taken.min(left);
            if front.units() == 0 {
                self.items.pop_front();
            }
        }
        units - left
    }

    /// Lend the front chunk to `f`, which reads from it and advances it.
    /// Depth follows whatever `f` consumed, and an emptied chunk is dropped.
    pub fn read_front<R>(&mut self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let front = self.items.front_mut()?;
        let before = front.units();
        let result = f(front);
        let after = front.units();
        self.depth -= before.saturating_sub(after);
        if after == 0 {
            self.items.pop_front();
        }
        Some(result)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.depth = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Span(u64);

    impl Chunk for Span {
        fn units(&self) -> u64 {
            self.0
        }
        fn advance(&mut self, units: u64) {
            self.0 -= units;
        }
    }

    struct Frame(bool);

    impl Chunk for Frame {
        fn units(&self) -> u64 {
            if self.0 {
                0
            } else {
                1
            }
        }
        fn advance(&mut self, _units: u64) {
            self.0 = true;
        }
    }

    #[test]
    fn depth_follows_pushes_and_partial_reads() {
        let mut ring = Ring::new(1000, 10_000_000_000);
        ring.push(Span(30));
        ring.push(Span(20));
        assert_eq!(ring.depth(), 50);
        assert_eq!(ring.depth_ns(), 50_000_000);
        ring.read_front(|c| c.advance(10));
        assert_eq!(ring.depth(), 40);
        ring.read_front(|c| c.advance(20));
        assert_eq!(ring.depth(), 20);
        assert_eq!(ring.items.len(), 1, "an emptied chunk is dropped");
    }

    #[test]
    fn capacity_discards_the_oldest() {
        let mut ring = Ring::new(1000, 100_000_000);
        assert_eq!(ring.push(Span(80)), 0);
        assert_eq!(ring.push(Span(40)), 20);
        assert_eq!(ring.depth(), 100);
        ring.read_front(|c| assert_eq!(c.0, 60, "front was trimmed, not dropped"));
    }

    #[test]
    fn discard_spans_chunks() {
        let mut ring = Ring::new(1000, 10_000_000_000);
        ring.push(Span(10));
        ring.push(Span(10));
        ring.push(Span(10));
        assert_eq!(ring.discard(25), 25);
        assert_eq!(ring.depth(), 5);
        assert_eq!(ring.discard(50), 5);
        assert_eq!(ring.depth(), 0);
    }

    #[test]
    fn unsplittable_chunks_are_consumed_whole() {
        let mut ring = Ring::new(25, 10_000_000_000);
        ring.push(Frame(false));
        ring.push(Frame(false));
        assert_eq!(ring.depth_ns(), 80_000_000);
        ring.discard(1);
        assert_eq!(ring.depth(), 1);
    }
}
