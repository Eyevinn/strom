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
//! delay, so healthy demuxed output runs ahead of the latest input only by what
//! the sender packs together: the frames one SRT message carries, multi-frame
//! audio PES, and a video burst at connect that can hold the lead at 440 ms.
//! This watcher compares the two, and when the smallest lead over a window
//! exceeds [`MAX_LEAD`] it shifts all of the demuxer's source pads back by the
//! window's mean lead with a pad offset. One correction is shared by every pad,
//! so audio and video stay aligned.
//!
//! A stale head only happens at connect, so a correction may only be made in
//! the first [`ARM_PERIOD`] of a caller's input, and only once. Shifting back
//! mid-stream steps running time backwards, which recorders write as
//! decreasing timestamps; a window of a healthy stream that crosses the
//! threshold later on must not be able to do that.
//!
//! The shift belongs to the caller it was measured on: `srtsrc` keeps listening
//! after a caller leaves and `tsdemux` takes a fresh reference for the next one,
//! so the shift is dropped, and the watcher re-armed, when the caller changes.
//!
//! Timing comes only from buffer timestamps, never the clock: the probes are on
//! the streaming thread for every demuxed frame and every SRT message, so they
//! use atomics only and do no locking or allocation outside the rare
//! correction. Once disarmed, the output probe only keeps the offset applied.

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
/// bitrates span more. ffmpeg's default TS muxing packs about 11 AAC frames per
/// PES, which leads by 100-230 ms, and the video burst at connect holds video
/// 290-440 ms ahead; windows of such a stream have reached 205 ms. A stale
/// head leads by however long the caller waited to connect, seconds in
/// practice, so one that waited less than this goes uncorrected.
pub const MAX_LEAD: gst::ClockTime = gst::ClockTime::from_mseconds(500);

/// Input time after a caller's first buffer within which a correction may be
/// made. The first window closes over the stale head itself, so the correction
/// lands about two windows in.
pub const ARM_PERIOD: gst::ClockTime = gst::ClockTime::from_seconds(3);

/// Input time covered by one measurement window.
pub const WINDOW: gst::ClockTime = gst::ClockTime::from_mseconds(500);

const NONE: u64 = u64::MAX;

/// `arm_deadline` before the caller's first input buffer.
const ARM_PENDING: u64 = u64::MAX;
/// `arm_deadline` once the arm period is over or the correction is made.
const DISARMED: u64 = 0;

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
    /// Input running time after which no correction is made, or `ARM_PENDING`
    /// or `DISARMED`.
    arm_deadline: AtomicU64,
}

impl State {
    /// Drop the correction and the window in progress, and re-arm.
    ///
    /// The window counters are re-initialised by the first buffer after this,
    /// which takes the `window_start == NONE` branch of [`on_output_buffer`].
    /// The arm period starts at the next caller's first input buffer, so the
    /// input time of the caller that left is forgotten too.
    fn reset(&self) {
        self.correction.store(0, Ordering::Relaxed);
        self.window_start.store(NONE, Ordering::Relaxed);
        self.last_input.store(NONE, Ordering::Relaxed);
        self.arm_deadline.store(ARM_PENDING, Ordering::Relaxed);
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
                arm_deadline: AtomicU64::new(ARM_PENDING),
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
                on_input_buffer(&state, pts.nseconds());
            }
            gst::PadProbeReturn::Ok
        });
    }

    /// Drop the correction and re-arm whenever the SRT caller changes.
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

fn on_input_buffer(state: &State, ts: u64) {
    state.last_input.store(ts, Ordering::Relaxed);
    if state.arm_deadline.load(Ordering::Relaxed) == ARM_PENDING {
        state
            .arm_deadline
            .store(ts + ARM_PERIOD.nseconds(), Ordering::Relaxed);
    }
}

fn on_output_buffer(state: &State, segment: &PadSegment, pad: &gst::Pad, ts: u64) {
    let correction = state.correction.load(Ordering::Relaxed);
    if segment.applied_offset.load(Ordering::Relaxed) != correction {
        apply(segment, pad, correction);
    }

    let deadline = state.arm_deadline.load(Ordering::Relaxed);
    if deadline == DISARMED || deadline == ARM_PENDING {
        return;
    }
    let input = state.last_input.load(Ordering::Relaxed);
    if input == NONE {
        return;
    }
    if input > deadline {
        state.arm_deadline.store(DISARMED, Ordering::Relaxed);
        return;
    }
    let Some(running_time) = segment.running_time(ts) else {
        return;
    };
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
    state.arm_deadline.store(DISARMED, Ordering::Relaxed);
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

    fn ms(ms: i64) -> i64 {
        ms * gst::ClockTime::MSECOND.nseconds() as i64
    }

    /// Eight buffers 100 ms of input apart from `start`, enough to close one
    /// window, whose running time sits `leads[i % leads.len()]` from arrival.
    fn window(state: &State, segment: &PadSegment, pad: &gst::Pad, start: i64, leads: &[i64]) {
        for step in 0..8 {
            let input = start + ms(step * 100);
            on_input_buffer(state, input as u64);
            let lead = leads[step as usize % leads.len()];
            // running_time(ts) is ts plus whatever offset the pad carries, so
            // ask for a timestamp that produces this lead against it.
            let applied = segment.applied_offset.load(Ordering::Relaxed);
            on_output_buffer(state, segment, pad, (input + lead - applied) as u64);
        }
    }

    /// ffmpeg's multi-frame audio PES and the video burst at connect keep a
    /// healthy stream ahead of arrival, and the smallest lead in a window has
    /// reached 205 ms. Re-anchoring it steps running time back mid-stream, and
    /// recorders write decreasing DTS.
    #[test]
    fn a_healthy_stream_is_never_re_anchored() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();

        // Through the arm period: video at the top of its range, audio at the
        // top of its own.
        for start in (0..4).map(|i| ms(i * 800)) {
            window(&anchor.state, &segment, &pad, start, &[ms(440), ms(230)]);
        }
        assert_eq!(pad.offset(), 0, "the lead at connect is left alone");

        window(
            &anchor.state,
            &segment,
            &pad,
            ms(10_000),
            &[ms(440), ms(205)],
        );
        assert_eq!(pad.offset(), 0, "a high window mid-stream is left alone");
    }

    /// A stale head is corrected once, early in the connection.
    #[test]
    fn a_stale_head_is_re_anchored_once() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();

        window(&anchor.state, &segment, &pad, 0, &[ms(2000)]);
        assert_eq!(
            pad.offset(),
            -ms(2000),
            "a window spent ahead of arrival is re-anchored"
        );

        window(&anchor.state, &segment, &pad, ms(800), &[ms(1000)]);
        assert_eq!(pad.offset(), -ms(2000), "a second correction is not made");
    }

    /// Past the arm period even a large lead is left alone: shifting back
    /// there would step every recording's timestamps backwards.
    #[test]
    fn no_correction_after_the_arm_period() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();

        window(&anchor.state, &segment, &pad, 0, &[0]);
        let late = ms(ARM_PERIOD.mseconds() as i64 + 1000);
        window(&anchor.state, &segment, &pad, late, &[ms(2000)]);
        assert_eq!(pad.offset(), 0);
    }

    /// The caller the offset was measured on has gone.
    #[test]
    fn a_caller_change_drops_the_offset_and_re_arms() {
        let anchor = TsDemuxAnchor::new("test");
        let (pad, segment) = pad_and_segment();

        window(&anchor.state, &segment, &pad, 0, &[ms(2000)]);
        assert_eq!(pad.offset(), -ms(2000));

        anchor.state.reset();
        // A pad carries the offset it finds until something is pushed through
        // it, so the next caller's first buffer is what clears it. The next
        // caller connects long after the first one's arm period.
        window(&anchor.state, &segment, &pad, ms(60_000), &[0]);
        assert_eq!(pad.offset(), 0, "the next caller starts unshifted");

        anchor.state.reset();
        window(&anchor.state, &segment, &pad, ms(120_000), &[ms(1500)]);
        assert_eq!(
            pad.offset(),
            -ms(1500),
            "a later caller's stale head is corrected"
        );
    }
}
