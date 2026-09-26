//! Named channels shared by a bridge sink and a bridge src, process-wide.
//!
//! A channel exists while either side holds it, so the two flows can start
//! and stop in any order. Each side claims its role; a second sink or a second
//! src on the same name is refused, because two writers would interleave and
//! two readers would each get half the audio.

use super::ring::{Chunk, Ring};
use super::{BPF, RATE, RING_CAPACITY_NS};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Instant;

/// Interleaved F32 frames, as written by the sink.
pub struct AudioChunk {
    data: Vec<u8>,
    offset: usize,
}

impl AudioChunk {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data, offset: 0 }
    }

    /// Copy up to `dst.len() / BPF` frames into `dst` and advance past them.
    /// Returns the number of frames copied.
    pub fn read_into(&mut self, dst: &mut [u8]) -> usize {
        let frames = (dst.len() / BPF).min(self.units() as usize);
        let bytes = frames * BPF;
        dst[..bytes].copy_from_slice(&self.data[self.offset..self.offset + bytes]);
        self.offset += bytes;
        frames
    }
}

impl Chunk for AudioChunk {
    fn units(&self) -> u64 {
        ((self.data.len() - self.offset) / BPF) as u64
    }

    fn advance(&mut self, units: u64) {
        self.offset = (self.offset + units as usize * BPF).min(self.data.len());
    }
}

/// What the writer saw of its producer.
#[derive(Default, Clone, Copy)]
pub struct InputStats {
    /// Gaps beyond the previous buffer's own length: ≥50, ≥100, ≥200, ≥400 ms.
    pub gaps: [u64; 4],
    pub longest_gap_ns: u64,
    pub overflow_ns: u64,
}

pub struct Shared {
    pub ring: Ring<AudioChunk>,
    pub input: InputStats,
    /// When the previous buffer arrived, and how long it was.
    last_arrival: Option<(Instant, u64)>,
}

impl Shared {
    /// Append one buffer's worth of audio, recording how late it came.
    pub fn write(&mut self, data: Vec<u8>) {
        let now = Instant::now();
        let chunk = AudioChunk::new(data);
        let duration_ns = self.ring.units_to_ns(chunk.units());
        if let Some((at, previous_ns)) = self.last_arrival {
            let gap = (now.duration_since(at).as_nanos() as u64).saturating_sub(previous_ns);
            self.input.longest_gap_ns = self.input.longest_gap_ns.max(gap);
            let ms = gap / 1_000_000;
            let bucket = match ms {
                400.. => Some(3),
                200.. => Some(2),
                100.. => Some(1),
                50.. => Some(0),
                _ => None,
            };
            if let Some(b) = bucket {
                self.input.gaps[b] += 1;
            }
        }
        self.last_arrival = Some((now, duration_ns));
        let dropped = self.ring.push(chunk);
        self.input.overflow_ns += self.ring.units_to_ns(dropped);
    }

    /// Forget the producer's last arrival, so the time a writer was stopped
    /// does not count as a gap.
    fn reset_arrival(&mut self) {
        self.last_arrival = None;
    }

    /// Whether the current writer has delivered any audio since it claimed
    /// the channel.
    pub fn writer_has_delivered(&self) -> bool {
        self.last_arrival.is_some()
    }
}

pub struct Channel {
    name: String,
    shared: Mutex<Shared>,
    writer: AtomicBool,
    reader: AtomicBool,
    /// Counts writer claims, so the reader can tell a new producer from the
    /// old one resuming.
    writer_generation: AtomicU64,
}

static CHANNELS: LazyLock<Mutex<HashMap<String, Weak<Channel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The channel called `name`, created if nobody holds it.
pub fn acquire(name: &str) -> Arc<Channel> {
    let mut channels = CHANNELS.lock().unwrap_or_else(|e| e.into_inner());
    channels.retain(|_, weak| weak.strong_count() > 0);
    if let Some(channel) = channels.get(name).and_then(Weak::upgrade) {
        return channel;
    }
    let channel = Arc::new(Channel {
        name: name.to_string(),
        shared: Mutex::new(Shared {
            ring: Ring::new(RATE, RING_CAPACITY_NS),
            input: InputStats::default(),
            last_arrival: None,
        }),
        writer: AtomicBool::new(false),
        reader: AtomicBool::new(false),
        writer_generation: AtomicU64::new(0),
    });
    channels.insert(name.to_string(), Arc::downgrade(&channel));
    channel
}

impl Channel {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claim the writer role; false if another sink holds it. The generation
    /// changes under the channel lock, so a reader holding it sees the new
    /// writer before any of its audio.
    pub fn claim_writer(&self) -> bool {
        let claimed = !self.writer.swap(true, Ordering::AcqRel);
        if claimed {
            let mut shared = self.lock();
            shared.reset_arrival();
            self.writer_generation.fetch_add(1, Ordering::AcqRel);
        }
        claimed
    }

    /// Changes every time a writer claims the channel.
    pub fn writer_generation(&self) -> u64 {
        self.writer_generation.load(Ordering::Acquire)
    }

    pub fn release_writer(&self) {
        self.writer.store(false, Ordering::Release);
    }

    /// Whether a sink holds the writer role: its flow is running, whether or
    /// not it is delivering audio.
    pub fn has_writer(&self) -> bool {
        self.writer.load(Ordering::Acquire)
    }

    /// Claim the reader role; false if another src holds it. A new reader
    /// starts from an empty ring: whatever piled up while nobody was reading
    /// is stale. Its input statistics start over with it, since the reader is
    /// what reports them and its own counters start from here too.
    pub fn claim_reader(&self) -> bool {
        let claimed = !self.reader.swap(true, Ordering::AcqRel);
        if claimed {
            let mut shared = self.lock();
            shared.ring.clear();
            shared.input = InputStats::default();
            shared.reset_arrival();
        }
        claimed
    }

    pub fn release_reader(&self) {
        self.reader.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_share_one_channel_until_both_let_go() {
        let a = acquire("channel-test-share");
        let b = acquire("channel-test-share");
        assert!(Arc::ptr_eq(&a, &b));
        drop(a);
        drop(b);
        let c = acquire("channel-test-share");
        assert!(c.claim_writer(), "a fresh channel has no writer");
    }

    #[test]
    fn roles_are_exclusive() {
        let ch = acquire("channel-test-roles");
        assert!(ch.claim_writer());
        assert!(!ch.claim_writer());
        ch.release_writer();
        assert!(ch.claim_writer());
        assert!(ch.claim_reader());
        assert!(!ch.claim_reader());
    }

    #[test]
    fn a_new_reader_discards_the_stale_backlog() {
        let ch = acquire("channel-test-stale");
        ch.lock().write(vec![0; 480 * BPF]);
        assert_eq!(ch.lock().ring.depth(), 480);
        assert!(ch.claim_reader());
        assert_eq!(ch.lock().ring.depth(), 0);
    }

    #[test]
    fn a_new_reader_starts_the_input_statistics_over() {
        let ch = acquire("channel-test-stats-reset");
        {
            let mut shared = ch.lock();
            shared.input.gaps[3] = 7;
            shared.input.longest_gap_ns = 900_000_000;
            shared.input.overflow_ns = 5_000_000_000;
        }
        assert!(ch.claim_reader());
        let input = ch.lock().input;
        assert_eq!(input.gaps, [0; 4], "gap histogram starts over");
        assert_eq!(input.longest_gap_ns, 0);
        assert_eq!(
            input.overflow_ns, 0,
            "audio dropped before this reader existed is not its overflow"
        );
    }

    #[test]
    fn a_new_writer_leaves_the_old_writers_tail_to_the_reader() {
        // Whether a previous writer's leftovers are stale is the reader's
        // call: only after an overrun are they. An ordinary restart keeps them.
        let ch = acquire("channel-test-writer-tail");
        assert!(ch.claim_writer());
        ch.lock().write(vec![0; 480 * BPF]);
        ch.release_writer();
        assert!(ch.claim_writer());
        assert_eq!(ch.lock().ring.depth(), 480);
    }

    #[test]
    fn each_writer_claim_is_a_new_generation() {
        let ch = acquire("channel-test-generation");
        let before = ch.writer_generation();
        assert!(ch.claim_writer());
        let first = ch.writer_generation();
        assert_ne!(first, before, "a writer claiming is a new producer");
        assert!(!ch.claim_writer());
        assert_eq!(
            ch.writer_generation(),
            first,
            "a refused claim changes nothing"
        );
        ch.release_writer();
        assert_eq!(
            ch.writer_generation(),
            first,
            "releasing is not a new producer"
        );
        assert!(ch.claim_writer());
        assert_ne!(ch.writer_generation(), first, "the next claim is");
    }

    #[test]
    fn partial_reads_advance_through_a_chunk() {
        let mut chunk = AudioChunk::new((0..(4 * BPF) as u8).collect());
        let mut dst = [0u8; 3 * BPF];
        assert_eq!(chunk.read_into(&mut dst), 3);
        assert_eq!(chunk.units(), 1);
        assert_eq!(dst[BPF], BPF as u8);
        assert_eq!(chunk.read_into(&mut dst), 1);
        assert_eq!(dst[0], (3 * BPF) as u8);
    }
}
