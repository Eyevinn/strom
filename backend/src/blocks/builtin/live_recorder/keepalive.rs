//! Keeping every track of a live recording moving.
//!
//! The muxer is a live aggregator, so a quiet track does not stop it. But it
//! only finishes a fragment once each stream has reached the fragment's end,
//! and a stream's position only moves with its data. A quiet video therefore
//! keeps every other track in memory until it returns. GAP events move a quiet
//! track's position on without data, so fragments keep being written.
//!
//! One thread per recorder looks at the tracks every [`POLL`]. It holds weak
//! references only and returns once the muxer is gone.
//!
//! The buffer probe on each track is the hot path: atomics only.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How often the keepalive thread looks at the tracks.
const POLL: Duration = Duration::from_millis(100);

/// A track counts as quiet after this long without a buffer. Well above the
/// gap between two audio buffers or two video frames, and above a WHIP
/// jitter buffer's normal hiccups.
const QUIET: Duration = Duration::from_millis(500);

/// GAPs stop this far short of the pipeline's running time, so a buffer still
/// in flight is not overtaken by a GAP that claims its time.
const GAP_MARGIN: gst::ClockTime = gst::ClockTime::from_mseconds(300);

/// A connected track that has had no caps by the time another track has carried
/// data for this long is released from the muxer, which cannot write its header
/// until every pad has caps. Generous on purpose: a WHIP guest's video has been
/// seen arriving 7–9 s after their audio, and a track released here is not
/// recorded at all. The queues in front of the muxer hold the early tracks for
/// longer than this (see `TRACK_QUEUE_TIME`), so waiting holds nothing upstream.
pub const NO_DATA_TIMEOUT: Duration = Duration::from_secs(20);

const NONE: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    pub fn name(self) -> &'static str {
        match self {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
        }
    }
}

/// One track of the recording, shared by its probes and the keepalive thread.
pub struct Track {
    pub label: String,
    pub kind: TrackKind,
    input: gst::glib::WeakRef<gst::Element>,
    queue: gst::glib::WeakRef<gst::Element>,
    /// The muxer pad this track holds, set when the flow starts.
    mux_pad: Mutex<Option<gst::glib::WeakRef<gst::Pad>>>,
    epoch: Instant,
    has_caps: AtomicBool,
    /// Out of the recording: the input drops everything.
    retired: AtomicBool,
    /// Milliseconds since `epoch` when the queue last took a buffer. 0 = never.
    last_buffer_ms: AtomicU64,
    /// End of the last buffer, in the track's segment time (ns).
    last_end_ns: AtomicU64,
    /// End of the GAPs sent so far, in segment time (ns). A buffer that starts
    /// before it arrived too late: its time is already declared empty.
    gap_until_ns: AtomicU64,
    /// Video only: a buffer was dropped, so drop until the next keyframe.
    wait_keyframe: AtomicBool,
    /// The queue's input segment, from its sink pad's segment events.
    segment: Mutex<Option<gst::FormattedSegment<gst::ClockTime>>>,
}

impl Track {
    pub fn new(
        label: String,
        kind: TrackKind,
        input: &gst::Element,
        queue: &gst::Element,
    ) -> Arc<Self> {
        Arc::new(Track {
            label,
            kind,
            input: input.downgrade(),
            queue: queue.downgrade(),
            mux_pad: Mutex::new(None),
            epoch: Instant::now(),
            has_caps: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            last_buffer_ms: AtomicU64::new(0),
            last_end_ns: AtomicU64::new(NONE),
            gap_until_ns: AtomicU64::new(0),
            wait_keyframe: AtomicBool::new(false),
            segment: Mutex::new(None),
        })
    }

    /// Where a stop sends this track's EOS.
    pub fn queue_sink_pad(&self) -> Option<gst::glib::WeakRef<gst::Pad>> {
        self.queue
            .upgrade()?
            .static_pad("sink")
            .map(|p| p.downgrade())
    }

    pub fn is_connected(&self) -> bool {
        self.mux_pad.lock().unwrap().is_some()
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
    }

    pub fn mark_caps(&self) {
        self.last_buffer_ms.store(
            self.epoch.elapsed().as_millis().max(1) as u64,
            Ordering::SeqCst,
        );
        self.has_caps.store(true, Ordering::SeqCst);
    }

    /// Request a muxer pad for this track and link its queue to it, if the
    /// track's input is linked. Called once, before the flow leaves NULL.
    pub fn connect_to_muxer(&self, block_id: &str, mux: &gst::Element, template: &str) {
        let linked = self
            .input
            .upgrade()
            .and_then(|i| i.static_pad("sink"))
            .is_some_and(|p| p.is_linked());
        if !linked {
            info!(
                "Live Recorder {}: {} is not connected, so it gets no muxer pad",
                block_id, self.label
            );
            return;
        }
        let Some(queue_src) = self.queue.upgrade().and_then(|q| q.static_pad("src")) else {
            return;
        };
        let Some(pad) = mux.request_pad_simple(template) else {
            error_no_pad(block_id, &self.label);
            return;
        };
        if let Err(e) = queue_src.link(&pad) {
            warn!(
                "Live Recorder {}: could not link {} to the muxer: {:?}",
                block_id, self.label, e
            );
            mux.release_request_pad(&pad);
            return;
        }
        *self.mux_pad.lock().unwrap() = Some(pad.downgrade());
    }

    /// Take the track out of the recording: the muxer stops waiting for it and
    /// its input drops what it is sent.
    pub fn release_from_muxer(&self, mux: &gst::Element) {
        self.retire();
        let Some(pad) = self
            .mux_pad
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.upgrade())
        else {
            return;
        };
        if let Some(peer) = pad.peer() {
            let _ = peer.unlink(&pad);
        }
        mux.release_request_pad(&pad);
    }

    /// The input's drop probe and the queue's position probes.
    pub fn install_probes(self: &Arc<Self>) {
        if let Some(input_sink) = self.input.upgrade().and_then(|i| i.static_pad("sink")) {
            // Without it, a retired track's branch would answer upstream with
            // not-linked, which stops a source that feeds other blocks through a tee.
            // Flushes still pass: they carry pad state the branch needs.
            let track = Arc::clone(self);
            input_sink.add_probe(
                gst::PadProbeType::BUFFER
                    | gst::PadProbeType::BUFFER_LIST
                    | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_pad, _info| {
                    if track.retired.load(Ordering::Relaxed) {
                        gst::PadProbeReturn::Drop
                    } else {
                        gst::PadProbeReturn::Ok
                    }
                },
            );
        }

        let Some(queue_sink) = self.queue.upgrade().and_then(|q| q.static_pad("sink")) else {
            return;
        };

        // An ALLOCATION query is serialized: the queue holds it until it has
        // drained, then the muxer holds it until it can aggregate, which is not
        // before every track has caps. An encoder asks one before its first
        // buffer, so a late track would stop the early ones at their source, and
        // through a tee every other user of that source. The muxer allocates
        // nothing for its inputs, so answer here with no proposal.
        queue_sink.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_pad, info| match info
            .query_mut()
            .map(|q| q.view_mut())
        {
            Some(gst::QueryViewMut::Allocation(_)) => gst::PadProbeReturn::Handled,
            _ => gst::PadProbeReturn::Ok,
        });

        let track = Arc::clone(self);
        queue_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
                if let gst::EventView::Segment(segment) = event.view() {
                    if let Some(segment) = segment.segment().downcast_ref::<gst::ClockTime>() {
                        *track.segment.lock().unwrap() = Some(segment.clone());
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        let track = Arc::clone(self);
        queue_sink.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
            let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
                return gst::PadProbeReturn::Ok;
            };
            let Some(pts) = buffer.pts() else {
                return gst::PadProbeReturn::Ok;
            };
            let pts = pts.nseconds();
            let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

            // Its time was already declared empty by a GAP.
            if pts < track.gap_until_ns.load(Ordering::Relaxed) {
                if track.kind == TrackKind::Video {
                    track.wait_keyframe.store(true, Ordering::Relaxed);
                }
                return gst::PadProbeReturn::Drop;
            }
            if track.wait_keyframe.load(Ordering::Relaxed) {
                if !keyframe {
                    return gst::PadProbeReturn::Drop;
                }
                track.wait_keyframe.store(false, Ordering::Relaxed);
            }

            let end = pts + buffer.duration().map(|d| d.nseconds()).unwrap_or(0);
            track.last_end_ns.store(end, Ordering::Relaxed);
            track.last_buffer_ms.store(
                track.epoch.elapsed().as_millis().max(1) as u64,
                Ordering::Relaxed,
            );
            gst::PadProbeReturn::Ok
        });
    }

    /// Send a GAP from where the track stands to `running_time` (less the
    /// margin). Returns whether one went out.
    fn fill_gap(&self, running_time: gst::ClockTime) -> bool {
        let Some(target) = running_time.checked_sub(GAP_MARGIN) else {
            return false;
        };
        let Some(position) = self
            .segment
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.position_from_running_time(target))
        else {
            return false;
        };
        let last_end = self.last_end_ns.load(Ordering::Relaxed);
        let gap_until = self.gap_until_ns.load(Ordering::Relaxed);
        let start = match (last_end, gap_until) {
            (NONE, 0) => return false,
            (NONE, g) => g,
            (e, g) => e.max(g),
        };
        let position = position.nseconds();
        if position <= start {
            return false;
        }
        let Some(queue) = self.queue.upgrade() else {
            return false;
        };
        // Only a track with nothing waiting is quiet. A track whose queue holds
        // buffers is being held back by the muxer, and its upstream thread may be
        // parked in a push into that queue, holding the stream lock this event
        // would wait for.
        if queue.property::<u32>("current-level-buffers") > 0 {
            return false;
        }
        let Some(queue_sink) = queue.static_pad("sink") else {
            return false;
        };
        let gap = gst::event::Gap::builder(gst::ClockTime::from_nseconds(start))
            .duration(gst::ClockTime::from_nseconds(position - start))
            .build();
        self.gap_until_ns.store(position, Ordering::Relaxed);
        queue_sink.send_event(gap)
    }
}

fn error_no_pad(block_id: &str, label: &str) {
    tracing::error!(
        "Live Recorder {}: the muxer refused a pad for {} — it will not be recorded",
        block_id,
        label
    );
}

/// Start the keepalive thread for one recorder.
pub fn spawn(block_id: &str, mux: &gst::Element, tracks: Vec<Arc<Track>>) {
    let mux = mux.downgrade();
    let thread_block_id = block_id.to_string();
    let spawned = std::thread::Builder::new()
        .name(format!("liverec-{}", block_id))
        .spawn(move || run(&thread_block_id, mux, &tracks));
    if let Err(e) = spawned {
        tracing::error!(
            "Live Recorder {}: could not start the keepalive thread: {} — a quiet video will hold the other tracks in memory",
            block_id,
            e
        );
    }
}

fn run(block_id: &str, mux: gst::glib::WeakRef<gst::Element>, tracks: &[Arc<Track>]) {
    let mut first_data: Option<Instant> = None;
    let mut quiet_reported = vec![false; tracks.len()];

    loop {
        std::thread::sleep(POLL);
        let Some(mux) = mux.upgrade() else {
            return;
        };
        if first_data.is_none() && tracks.iter().any(|t| t.has_caps.load(Ordering::SeqCst)) {
            first_data = Some(Instant::now());
        }
        let running_time = mux.current_running_time();

        for (track, reported) in tracks.iter().zip(quiet_reported.iter_mut()) {
            if !track.is_connected() || track.is_retired() {
                continue;
            }

            if !track.has_caps.load(Ordering::SeqCst) {
                if first_data.is_some_and(|t| t.elapsed() >= NO_DATA_TIMEOUT) {
                    warn!(
                        "Live Recorder {}: {} has carried nothing for {}s while other tracks record — recording without it",
                        block_id,
                        track.label,
                        NO_DATA_TIMEOUT.as_secs()
                    );
                    track.release_from_muxer(&mux);
                }
                continue;
            }

            let now_ms = track.epoch.elapsed().as_millis() as u64;
            let quiet_ms = now_ms.saturating_sub(track.last_buffer_ms.load(Ordering::Relaxed));
            if quiet_ms < QUIET.as_millis() as u64 {
                if *reported {
                    info!("Live Recorder {}: {} is back", block_id, track.label);
                    *reported = false;
                }
                continue;
            }
            let Some(running_time) = running_time else {
                continue;
            };
            if track.fill_gap(running_time) && !*reported {
                info!(
                    "Live Recorder {}: {} has been quiet for {}ms — recording the other tracks past it",
                    block_id, track.label, quiet_ms
                );
                *reported = true;
            }
        }
    }
}
