//! Re-anchors `tsdemux` output that runs ahead of the arrival timestamps.
//!
//! With `ignore-pcr=true`, `tsdemux` maps PTS to output time from a single
//! reference: the upstream timestamp of the buffer carrying the first PES it
//! parses (`mpegts_packetizer_pts_to_ts_internal`, "Take the first base time we
//! see as a reference"). Every later PTS is placed relative to that one and the
//! reference is never taken again.
//!
//! That is only right when the first PES is current. An SRT caller that has
//! been encoding before it could connect (listener not up yet, caller retrying)
//! delivers a few frames from the start of its encode, then jumps to live PTS.
//! The stale frames become the reference, so every live frame is stamped the
//! length of that PTS jump into the future — for the life of the connection.
//! Downstream sinks and aggregators hold each buffer until its running time,
//! so the source is delayed by however long the caller waited to connect.
//!
//! `srtsrc` stamps each buffer with its arrival time less the SRT transit
//! delay, so healthy demuxed output never runs ahead of the latest input by
//! more than the few frames one SRT message carries. This watcher compares the
//! two, and when the smallest lead over a window exceeds [`MAX_LEAD`] it shifts
//! all of the demuxer's source pads back by the window's mean lead with a pad
//! offset. One correction is shared by every pad, so audio and video stay
//! aligned.
//!
//! The shift belongs to the caller it was measured on: `srtsrc` keeps listening
//! after a caller leaves and `tsdemux` takes a fresh reference for the next one,
//! so the shift is dropped when the caller changes. Output that sits *behind*
//! arrival is left alone, whatever the shift: a sender that stalls and resumes
//! looks the same from here, and giving the shift back would throw its stream
//! forward by that much in one buffer.
//!
//! Timing comes only from buffer timestamps, never the clock: the probes are on
//! the streaming thread for every demuxed frame and every SRT message, so they
//! use atomics only and do no locking or allocation outside the rare
//! correction.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tracing::info;

/// Smallest lead over one window above which the output is re-anchored.
///
/// Healthy lead is a sawtooth: every PES completed within one SRT message is
/// placed from the same reference, while the input time is that of the whole
/// message. A 1316-byte message of 128 kbit/s AAC spans about 100 ms, and lower
/// bitrates span more.
pub const MAX_LEAD: gst::ClockTime = gst::ClockTime::from_mseconds(200);

/// Input time covered by one measurement window.
pub const WINDOW: gst::ClockTime = gst::ClockTime::from_mseconds(500);

const NONE: u64 = u64::MAX;

struct State {
    instance_id: String,
    /// Running time of the most recent input buffer, or `NONE`.
    last_input: AtomicU64,
    /// Input running time at which the current window started, or `NONE`.
    window_start: AtomicU64,
    /// Smallest lead seen in the current window (ns).
    window_min_lead: AtomicI64,
    /// Sum and count of the leads seen in the current window.
    window_lead_sum: AtomicI64,
    window_samples: AtomicI64,
    /// Offset every source pad of the demuxer should carry (ns, never positive).
    correction: AtomicI64,
}

impl State {
    /// Drop the correction and the window in progress.
    ///
    /// The window counters are re-initialised by the first buffer after this,
    /// which takes the `window_start == NONE` branch of [`on_output_buffer`].
    fn reset(&self) {
        self.correction.store(0, Ordering::Relaxed);
        self.window_start.store(NONE, Ordering::Relaxed);
    }
}

/// Watches one `tsdemux` and the element feeding it.
#[derive(Clone)]
pub struct TsDemuxAnchor {
    state: Arc<State>,
}

impl TsDemuxAnchor {
    pub fn new(instance_id: &str) -> Self {
        Self {
            state: Arc::new(State {
                instance_id: instance_id.to_string(),
                last_input: AtomicU64::new(NONE),
                window_start: AtomicU64::new(NONE),
                window_min_lead: AtomicI64::new(i64::MAX),
                window_lead_sum: AtomicI64::new(0),
                window_samples: AtomicI64::new(0),
                correction: AtomicI64::new(0),
            }),
        }
    }

    /// Record the timestamps of buffers entering the demuxer.
    ///
    /// `pad` must carry a TIME segment starting at zero with timestamps that are
    /// already running times, as `srtsrc` produces.
    pub fn watch_input(&self, pad: &gst::Pad) {
        let state = self.state.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|b| b.pts()) {
                state.last_input.store(pts.nseconds(), Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// Drop the correction whenever the SRT caller changes.
    ///
    /// Listener mode only: `srtsrc` emits these while accepting and dropping a
    /// caller, and neither fires in caller mode.
    pub fn watch_caller(&self, srtsrc: &gst::Element) {
        for signal in ["caller-added", "caller-removed"] {
            let state = self.state.clone();
            srtsrc.connect(signal, false, move |_| {
                state.reset();
                None
            });
        }
    }

    /// Watch every source pad the demuxer adds.
    ///
    /// The handler captures only the shared state, never the element.
    pub fn watch_demuxer(&self, tsdemux: &gst::Element) {
        let anchor = self.clone();
        tsdemux.connect_pad_added(move |_, pad| {
            if pad.direction() == gst::PadDirection::Src {
                anchor.watch_output(pad);
            }
        });
    }

    fn watch_output(&self, pad: &gst::Pad) {
        let state = self.state.clone();
        let segment = Arc::new(PadSegment::default());
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => {
                        if let Some(ts) = buffer.dts_or_pts() {
                            on_output_buffer(&state, &segment, pad, ts.nseconds());
                        }
                    }
                    Some(gst::PadProbeData::Event(event)) => {
                        if let gst::EventView::Segment(e) = event.view() {
                            segment.record(e.segment());
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }
}

/// The last TIME segment seen on one source pad, as plain integers.
struct PadSegment {
    /// `start + offset` of the segment (ns), or `NONE` when unusable.
    origin: AtomicU64,
    base: AtomicU64,
    /// Pad offset already folded into that segment by `gst_pad_push_event`.
    embedded_offset: AtomicI64,
    /// Offset this pad currently carries (ns).
    applied_offset: AtomicI64,
}

impl Default for PadSegment {
    fn default() -> Self {
        Self {
            origin: AtomicU64::new(NONE),
            base: AtomicU64::new(0),
            embedded_offset: AtomicI64::new(0),
            applied_offset: AtomicI64::new(0),
        }
    }
}

impl PadSegment {
    fn record(&self, segment: &gst::Segment) {
        // Pad offsets are applied to a SEGMENT before downstream probes run, so
        // this segment already includes whatever offset the pad carries.
        let usable = segment
            .downcast_ref::<gst::format::Time>()
            .filter(|s| s.rate() == 1.0)
            .and_then(|s| Some((s.start()?, s.offset()?, s.base()?)));
        match usable {
            Some((start, offset, base)) => {
                self.base.store(base.nseconds(), Ordering::Relaxed);
                self.embedded_offset.store(
                    self.applied_offset.load(Ordering::Relaxed),
                    Ordering::Relaxed,
                );
                self.origin
                    .store(start.nseconds() + offset.nseconds(), Ordering::Relaxed);
            }
            None => self.origin.store(NONE, Ordering::Relaxed),
        }
    }

    /// Running time of `ts` with this pad's current offset, ignoring clipping.
    fn running_time(&self, ts: u64) -> Option<i64> {
        let origin = self.origin.load(Ordering::Relaxed);
        if origin == NONE {
            return None;
        }
        let base = self.base.load(Ordering::Relaxed) as i64;
        let unshifted =
            ts as i64 - origin as i64 + base - self.embedded_offset.load(Ordering::Relaxed);
        Some(unshifted + self.applied_offset.load(Ordering::Relaxed))
    }
}

fn on_output_buffer(state: &State, segment: &PadSegment, pad: &gst::Pad, ts: u64) {
    let correction = state.correction.load(Ordering::Relaxed);
    if segment.applied_offset.load(Ordering::Relaxed) != correction {
        apply(segment, pad, correction);
    }

    let input = state.last_input.load(Ordering::Relaxed);
    let Some(running_time) = segment.running_time(ts) else {
        return;
    };
    if input == NONE {
        return;
    }
    let lead = running_time - input as i64;

    let window_start = state.window_start.load(Ordering::Relaxed);
    if window_start == NONE || input < window_start {
        state.window_start.store(input, Ordering::Relaxed);
        state.window_min_lead.store(lead, Ordering::Relaxed);
        state.window_lead_sum.store(lead, Ordering::Relaxed);
        state.window_samples.store(1, Ordering::Relaxed);
        return;
    }
    let min_lead = lead.min(state.window_min_lead.load(Ordering::Relaxed));
    let lead_sum = state.window_lead_sum.load(Ordering::Relaxed) + lead;
    let samples = state.window_samples.load(Ordering::Relaxed) + 1;
    if input - window_start < WINDOW.nseconds() {
        state.window_min_lead.store(min_lead, Ordering::Relaxed);
        state.window_lead_sum.store(lead_sum, Ordering::Relaxed);
        state.window_samples.store(samples, Ordering::Relaxed);
        return;
    }

    state.window_start.store(NONE, Ordering::Relaxed);
    if min_lead <= MAX_LEAD.nseconds() as i64 {
        return;
    }

    // The minimum only detects the jump. Shifting by the mean puts the
    // sawtooth where a clean reference leaves it, centred on the arrival time.
    let mean_lead = lead_sum / samples;
    let correction = correction - mean_lead;
    state.correction.store(correction, Ordering::Relaxed);
    apply(segment, pad, correction);
    info!(
        "MPEGTSSRT Input {}: demuxed timestamps ran {} ms ahead of arrival, re-anchoring (total offset {} ms)",
        state.instance_id,
        mean_lead / 1_000_000,
        correction / 1_000_000
    );
}

fn apply(segment: &PadSegment, pad: &gst::Pad, offset: i64) {
    segment.applied_offset.store(offset, Ordering::Relaxed);
    // Takes effect for the buffer being pushed: gst_pad_push_data resends the
    // sticky SEGMENT, now offset, after the buffer probes and before the push.
    pad.set_offset(offset);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source pad carrying a segment that starts at zero, as `tsdemux`
    /// produces, with the timestamps left as running times.
    fn pad_and_segment() -> (gst::Pad, PadSegment) {
        gst::init().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src)
            .name("src")
            .build();
        let segment = PadSegment::default();
        let time = gst::FormattedSegment::<gst::format::Time>::new();
        segment.record(time.upcast_ref());
        (pad, segment)
    }

    /// One full window of buffers whose running time sits `lead` from arrival.
    fn window(state: &State, segment: &PadSegment, pad: &gst::Pad, lead: i64) {
        for step in 0..8u64 {
            let input = step * 100 * gst::ClockTime::MSECOND.nseconds();
            state.last_input.store(input, Ordering::Relaxed);
            // running_time(ts) is ts plus whatever offset the pad carries, so
            // ask for a timestamp that produces this lead against it.
            let applied = segment.applied_offset.load(Ordering::Relaxed);
            on_output_buffer(state, segment, pad, (input as i64 + lead - applied) as u64);
        }
    }

    /// A sender that stalls and resumes leaves its stream behind arrival with
    /// the offset still right for it. Giving the offset back here would throw
    /// the stream forward by that much in a single buffer.
    #[test]
    fn a_window_spent_behind_arrival_keeps_the_offset() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();
        let two_seconds = 2 * gst::ClockTime::SECOND.nseconds() as i64;

        window(&anchor.state, &segment, &pad, two_seconds);
        assert_eq!(
            pad.offset(),
            -two_seconds,
            "a window spent ahead of arrival is re-anchored"
        );

        window(&anchor.state, &segment, &pad, -two_seconds);
        assert_eq!(pad.offset(), -two_seconds, "the offset is left alone");
    }

    /// The caller the offset was measured on has gone.
    #[test]
    fn a_caller_change_drops_the_offset() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();
        let two_seconds = 2 * gst::ClockTime::SECOND.nseconds() as i64;

        window(&anchor.state, &segment, &pad, two_seconds);
        assert_eq!(pad.offset(), -two_seconds);

        anchor.state.reset();
        // A pad carries the offset it finds until something is pushed through
        // it, so the next caller's first buffer is what clears it.
        window(&anchor.state, &segment, &pad, 0);
        assert_eq!(pad.offset(), 0, "the next caller starts unshifted");
    }
}
