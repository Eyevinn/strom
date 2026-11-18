//! QoS for input frames the vision mixer drops as late.
//!
//! A live video aggregator (`compositor`, `glvideomixerelement`) drops every
//! queued input buffer that ends before the start of the next output frame
//! (`GstAggregatorPad::skip_buffer`, run each aggregate cycle). It does so
//! with nothing but a debug line: the aggregator's only QoS message is about
//! its own late output. An input that reaches the mixer later than the
//! mixer's latency window therefore vanishes from program without a signal.
//!
//! This module counts those drops itself and posts a standard GStreamer QoS
//! message per input, with the mixer's sink pad as the source, so they reach
//! the same QoS path as any element's.
//!
//! How a drop is detected. A BUFFER probe on each input pad of the dist mixer
//! converts the buffer's end to running time and compares it with the mixer's
//! output position, the end of the last frame the mixer pushed. A buffer that
//! ends before that position at arrival is dropped by the aggregator's skip
//! check (which uses the same comparison), since the position only grows.
//! The count can only err low: a buffer that turns late while queued, or a
//! frame the aggregator skipped producing, is not counted. Nothing is counted
//! that the mixer shows.
//!
//! Rejected alternatives: the aggregator pad's `buffer-consumed` signal fires
//! for used and dropped buffers alike and costs a signal emission per buffer;
//! comparing buffers in against frames out cannot tell a dropped frame from
//! one replaced by a newer frame within one output period (a 50 fps input on
//! a 25 fps mixer); and reading the aggregator's segments takes object locks.
//!
//! The probe callbacks only touch atomics. The segments they need are stored
//! from event probes (SEGMENT, FLUSH_STOP), the output position from a BUFFER
//! probe on the mixer's src pad (one per output frame). The message is built
//! and posted from the overlay timer thread, at most once per second per
//! input and only when the input dropped something since the last message.
//! Like GStreamer's own QoS stats, `processed` and `dropped` are totals since
//! the last flush.

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::gst::pipeline::effects::find_pad;

/// Sentinel for "no valid time" in the atomics below.
const NONE: u64 = u64::MAX;

/// The part of a pad's TIME segment needed to turn a timestamp into running
/// time at rate 1.0: `running = ts - (start + offset) + base`.
struct SegmentAtomics {
    /// False until a TIME segment at rate 1.0 arrives. Other segments are not
    /// tracked: a live mixer input never carries one.
    valid: AtomicBool,
    start_offset: AtomicU64,
    base: AtomicU64,
}

impl SegmentAtomics {
    fn new() -> Self {
        Self {
            valid: AtomicBool::new(false),
            start_offset: AtomicU64::new(0),
            base: AtomicU64::new(0),
        }
    }

    fn store(&self, segment: &gst::Segment) {
        let Some(seg) = segment.downcast_ref::<gst::ClockTime>() else {
            self.valid.store(false, Ordering::Relaxed);
            return;
        };
        let start = seg.start().map(|t| t.nseconds()).unwrap_or(0);
        let offset = seg.offset().map(|t| t.nseconds()).unwrap_or(0);
        let base = seg.base().map(|t| t.nseconds()).unwrap_or(0);
        if seg.rate() != 1.0 {
            self.valid.store(false, Ordering::Relaxed);
            return;
        }
        self.start_offset
            .store(start.saturating_add(offset), Ordering::Relaxed);
        self.base.store(base, Ordering::Relaxed);
        self.valid.store(true, Ordering::Relaxed);
    }

    /// Running time of `ts`, or `None` before the segment or without one.
    #[inline]
    fn running_time(&self, ts: u64) -> Option<u64> {
        if !self.valid.load(Ordering::Relaxed) {
            return None;
        }
        let start = self.start_offset.load(Ordering::Relaxed);
        let rel = ts.checked_sub(start)?;
        rel.checked_add(self.base.load(Ordering::Relaxed))
    }
}

/// Per-input counters. Written by that input's streaming thread, read by
/// the timer.
struct InputCounters {
    segment: SegmentAtomics,
    processed: AtomicU64,
    dropped: AtomicU64,
    /// Running time and lateness of the last buffer counted as dropped.
    last_late_rt: AtomicU64,
    last_late_by: AtomicI64,
    /// `dropped` as of the last message posted, so a message goes out only
    /// when it grew.
    posted_dropped: AtomicU64,
}

impl InputCounters {
    fn new() -> Self {
        Self {
            segment: SegmentAtomics::new(),
            processed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_late_rt: AtomicU64::new(NONE),
            last_late_by: AtomicI64::new(0),
            posted_dropped: AtomicU64::new(0),
        }
    }

    fn reset(&self) {
        self.processed.store(0, Ordering::Relaxed);
        self.dropped.store(0, Ordering::Relaxed);
        self.posted_dropped.store(0, Ordering::Relaxed);
        self.last_late_rt.store(NONE, Ordering::Relaxed);
    }
}

/// Late-drop accounting for the dist mixer's input pads: the video inputs
/// (`sink_0..N`) followed by the DSK inputs (`sink_N..N+D`).
pub struct LateInputQos {
    inputs: Vec<InputCounters>,
    /// Running time of the mixer's output position: the end of the last
    /// frame it pushed. [`NONE`] before the first one.
    output_end_rt: AtomicU64,
    output_segment: SegmentAtomics,
    /// The monitored sink pads, by input slot. Weak: the pads own the probes
    /// that hold `self`, and this is only read off the streaming threads.
    pads: Mutex<Vec<Option<glib::WeakRef<gst::Pad>>>>,
}

impl LateInputQos {
    pub fn new(num_slots: usize) -> Self {
        Self {
            inputs: (0..num_slots).map(|_| InputCounters::new()).collect(),
            output_end_rt: AtomicU64::new(NONE),
            output_segment: SegmentAtomics::new(),
            pads: Mutex::new(vec![None; num_slots]),
        }
    }

    /// Account one buffer arriving on input `slot`. Called from a BUFFER
    /// probe: atomics only.
    #[inline]
    fn note_input_buffer(&self, slot: usize, buffer: &gst::BufferRef) {
        let Some(c) = self.inputs.get(slot) else {
            return;
        };
        let (Some(pts), Some(duration)) = (buffer.pts(), buffer.duration()) else {
            return;
        };
        let Some(start_rt) = c.segment.running_time(pts.nseconds()) else {
            return;
        };
        c.processed.fetch_add(1, Ordering::Relaxed);
        let end_rt = start_rt.saturating_add(duration.nseconds());
        let output_end = self.output_end_rt.load(Ordering::Relaxed);
        // Same comparison as the aggregator's skip check: a buffer that ends
        // before the next output frame starts is dropped.
        if output_end != NONE && end_rt < output_end {
            c.dropped.fetch_add(1, Ordering::Relaxed);
            c.last_late_rt.store(start_rt, Ordering::Relaxed);
            c.last_late_by
                .store((output_end - end_rt) as i64, Ordering::Relaxed);
        }
    }

    /// Record the end of an output frame. Called from a BUFFER probe on the
    /// mixer's src pad: atomics only.
    #[inline]
    fn note_output_buffer(&self, buffer: &gst::BufferRef) {
        let (Some(pts), Some(duration)) = (buffer.pts(), buffer.duration()) else {
            return;
        };
        if let Some(rt) = self
            .output_segment
            .running_time(pts.nseconds().saturating_add(duration.nseconds()))
        {
            self.output_end_rt.store(rt, Ordering::Relaxed);
        }
    }

    /// Post a QoS message for every input that dropped frames since the last
    /// call. Called from the overlay timer, at most once per second.
    pub fn post_dropped(&self) {
        let pads = match self.pads.lock() {
            Ok(p) => p.clone(),
            Err(_) => return,
        };
        for (slot, c) in self.inputs.iter().enumerate() {
            let dropped = c.dropped.load(Ordering::Relaxed);
            let posted = c.posted_dropped.load(Ordering::Relaxed);
            if dropped <= posted {
                // Also covers a flush that reset the counters.
                if dropped < posted {
                    c.posted_dropped.store(dropped, Ordering::Relaxed);
                }
                continue;
            }
            let Some(pad) = pads.get(slot).and_then(|w| w.as_ref()?.upgrade()) else {
                continue;
            };
            let Some(mixer) = pad.parent_element() else {
                continue;
            };
            c.posted_dropped.store(dropped, Ordering::Relaxed);
            let processed = c.processed.load(Ordering::Relaxed);
            let running_time = match c.last_late_rt.load(Ordering::Relaxed) {
                NONE => None,
                rt => Some(gst::ClockTime::from_nseconds(rt)),
            };
            let msg = gst::message::Qos::builder(true)
                .src(&pad)
                .running_time(running_time)
                .values(c.last_late_by.load(Ordering::Relaxed), 1.0, 1_000_000)
                .stats(
                    gst::format::Buffers::from_u64(processed),
                    gst::format::Buffers::from_u64(dropped),
                )
                .build();
            debug!(
                "Vision mixer {}: input pad {} dropped {} late frame(s) ({} processed)",
                mixer.name(),
                pad.name(),
                dropped - posted,
                processed
            );
            if mixer.post_message(msg).is_err() {
                debug!("Vision mixer {}: no bus for late-input QoS", mixer.name());
            }
        }
    }
}

/// Install the probes on the dist `mixer`: one per input pad (`sink_0` up to
/// `sink_{slots-1}`), and one on its src pad for the output position. Must run
/// after linking, when the request pads exist.
pub fn install(block_id: &str, mixer: &gst::Element, qos: &Arc<LateInputQos>) {
    let Some(src) = mixer.static_pad("src") else {
        warn!(
            "Vision mixer {}: mixer has no src pad, late input frames are not reported",
            block_id
        );
        return;
    };
    {
        let qos = Arc::clone(qos);
        // BUFFER fires once per output frame; EVENT_DOWNSTREAM once per event.
        src.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => qos.note_output_buffer(buffer),
                    Some(gst::PadProbeData::Event(ev)) => match ev.view() {
                        gst::EventView::Segment(s) => qos.output_segment.store(s.segment()),
                        gst::EventView::FlushStop(_) => {
                            qos.output_end_rt.store(NONE, Ordering::Relaxed)
                        }
                        _ => {}
                    },
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    let mut installed = 0;
    let mut pads = match qos.pads.lock() {
        Ok(p) => p,
        Err(_) => return,
    };
    for slot in 0..qos.inputs.len() {
        let Some(pad) = find_pad(mixer, &format!("sink_{}", slot)) else {
            continue;
        };
        pads[slot] = Some(pad.downgrade());
        let qos = Arc::clone(qos);
        // BUFFER: per input frame, atomics only. EVENT_DOWNSTREAM: segments
        // and flushes.
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_pad, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buffer)) => qos.note_input_buffer(slot, buffer),
                    Some(gst::PadProbeData::Event(ev)) => {
                        if let Some(c) = qos.inputs.get(slot) {
                            match ev.view() {
                                gst::EventView::Segment(s) => c.segment.store(s.segment()),
                                gst::EventView::FlushStop(_) => c.reset(),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
        installed += 1;
    }
    debug!(
        "Vision mixer {}: late-input QoS on {} mixer input pad(s)",
        block_id, installed
    );
}
