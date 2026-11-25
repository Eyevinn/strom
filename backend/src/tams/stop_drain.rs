//! Finish the open segment of a TAMS Output when its flow is stopped.
//!
//! Stopping a flow sets the pipeline to NULL without EOS. The fragment muxer
//! then drops what it has taken in but not yet written: the tail of the last
//! frame still in mpegtsmux (it packs 7 TS packets per buffer), or a buffer it
//! consumed while the pipeline went down. The last segment of the run then
//! holds a frame less than its timerange claims. [`MuxDrain::drain`] sends EOS
//! into the `splitmuxsink` before the state change and waits, bounded, until
//! the muxer has passed it on, so everything the muxer took in is in the file.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Longest a stop waits for the muxer to finish the open segment.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// How long after an EOS leaves the muxer a split may still open the next
/// fragment. An EOS also closes each fragment at a split; only one that no
/// new fragment follows is the end of the stream.
const SPLIT_GRACE: Duration = Duration::from_millis(200);

#[derive(Debug, Default)]
struct Counts {
    /// EOS events that left the muxer (one per closed fragment).
    eos: u64,
    /// Fragments `splitmuxsink` opened.
    opened: u64,
}

/// What a stop drain waits on: EOS events leaving the fragment muxer, and the
/// fragments `splitmuxsink` opens. Updated per event and per fragment, never
/// per buffer.
#[derive(Debug, Default)]
pub struct MuxDrain {
    counts: Mutex<Counts>,
    changed: Condvar,
}

impl MuxDrain {
    /// Count EOS events leaving `mux` through its src pad.
    pub fn watch_muxer(self: &Arc<Self>, mux: &gst::Element) {
        let Some(src) = mux.static_pad("src") else {
            return;
        };
        let this = Arc::downgrade(self);
        src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(ev)) = info.data.as_ref() {
                if ev.type_() == gst::EventType::Eos {
                    if let Some(this) = this.upgrade() {
                        this.update(|c| c.eos += 1);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// `splitmuxsink` opened a fragment.
    pub fn fragment_opened(&self) {
        self.update(|c| c.opened += 1);
    }

    fn update(&self, f: impl FnOnce(&mut Counts)) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut counts);
        self.changed.notify_all();
    }

    /// Send EOS into `splitmuxsink` and wait until the muxer has finished the
    /// open fragment, at most [`DRAIN_TIMEOUT`]. Returns whether it finished.
    ///
    /// Does nothing when no fragment was opened (no data yet, or the sink is
    /// still locked out of the pipeline's state).
    pub fn drain(&self, splitmuxsink: &gst::Element, label: &str) -> bool {
        if splitmuxsink.is_locked_state() || splitmuxsink.current_state() != gst::State::Playing {
            return true;
        }
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if counts.opened == 0 {
            return true;
        }
        let pads = splitmuxsink.sink_pads();
        if pads.is_empty() {
            return true;
        }
        // From another thread: a pad's stream lock is held while a buffer is
        // pushed into it, and a push can be parked (splitmuxsink waiting on
        // another track). The NULL state change that follows unparks it.
        let sent = std::thread::Builder::new()
            .name("tams-stop-eos".into())
            .spawn(move || {
                for pad in pads {
                    pad.send_event(gst::event::Eos::new());
                }
            });
        if let Err(e) = sent {
            warn!("TAMS {}: cannot send EOS on stop: {}", label, e);
            return false;
        }

        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut eos_seen = counts.eos;
        loop {
            // Wait for the next EOS out of the muxer.
            while counts.eos == eos_seen {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    warn!(
                        "TAMS {}: the muxer did not finish within {:?} of the stop; \
                         the last segment may end short",
                        label, DRAIN_TIMEOUT
                    );
                    return false;
                };
                counts = self
                    .changed
                    .wait_timeout(counts, left)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            // A fragment opened right after an EOS means a split, not the end.
            loop {
                eos_seen = counts.eos;
                let opened = counts.opened;
                let grace_end = (Instant::now() + SPLIT_GRACE).min(deadline);
                while counts.opened == opened && counts.eos == eos_seen {
                    let Some(left) = grace_end.checked_duration_since(Instant::now()) else {
                        debug!("TAMS {}: the last segment is complete", label);
                        return true;
                    };
                    counts = self
                        .changed
                        .wait_timeout(counts, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                if counts.opened != opened {
                    break; // a split: wait for the next EOS
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unopened_sink_is_not_drained() {
        gst::init().unwrap();
        let Ok(sink) = gst::ElementFactory::make("splitmuxsink").build() else {
            return;
        };
        let drain = MuxDrain::default();
        let started = Instant::now();
        assert!(drain.drain(&sink, "test"));
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
