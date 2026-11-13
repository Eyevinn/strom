//! What the SRT output blocks share around their `srtsink`.
//!
//! The leaky queue in front of a calling `srtsink` (#1259) is a targeted fix
//! for the SRT outputs in caller mode. The root cause is wider: Strom links a
//! block output to several blocks through a `tee` without a queue per branch,
//! so any network sink that blocks in `render` (RTMP, WHIP, ...) can stall the
//! whole flow. That is tracked separately.

use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::element::ElementPadRef;
use tracing::{debug, info, warn};

/// Headroom above the pipeline latency in the queue in front of a calling
/// `srtsink`. See [`queue_max_size_time`].
pub(crate) const QUEUE_LATENCY_MARGIN: gst::ClockTime = gst::ClockTime::from_seconds(2);

/// The least `max-size-time` of that queue, whatever the pipeline latency.
pub(crate) const QUEUE_MIN_SIZE_TIME: gst::ClockTime = gst::ClockTime::from_seconds(3);

/// How often drops are logged per output.
const DROP_REPORT_INTERVAL: gst::ClockTime = gst::ClockTime::from_seconds(5);

/// Whether `srtsink` dials out (caller or rendezvous) rather than listens.
///
/// Reads the `mode` the sink parsed from its `uri`, so call it after setting
/// the URI. An SRT URI with no host and no `mode` is opened as a listener, so
/// that counts as listening too.
pub(crate) fn dials_out(srtsink: &gst::Element) -> bool {
    let mode = srtsink.property_value("mode");
    if gst::glib::EnumValue::from_value(&mode).is_some_and(|(_, v)| v.nick() == "listener") {
        return false;
    }
    let uri: String = srtsink.property("uri");
    let host = uri
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&uri)
        .split(['?', '/'])
        .next()
        .unwrap_or("");
    !host.is_empty() && !host.starts_with(':')
}

/// `max-size-time` for the queue in front of a calling `srtsink`, given the
/// latency the pipeline configured: `max(latency + 2 s, 3 s)`.
///
/// A syncing sink holds every buffer until its timestamp plus the pipeline
/// latency. The pipeline latency is the largest minimum latency over all
/// sinks, so another branch of the flow (an encoder with lookahead, a mixer,
/// another output) sets it, not this output. While the sink is connected and
/// keeping up, the queue therefore holds about the pipeline latency minus what
/// this branch adds itself. A limit below that leaks media all the time while
/// connected; the old fixed 1 s limit did as soon as the flow had more than
/// about 1 s of latency. The 2 s margin covers jitter and timestamp jumps; the
/// 3 s floor covers a flow whose latency is not known yet (or not live, where
/// no latency is configured) and the jitter of a low-latency flow.
pub(crate) fn queue_max_size_time(latency: Option<gst::ClockTime>) -> gst::ClockTime {
    let latency = latency.unwrap_or(gst::ClockTime::ZERO);
    latency
        .saturating_add(QUEUE_LATENCY_MARGIN)
        .max(QUEUE_MIN_SIZE_TIME)
}

/// Link the muxer's `src` pad to `srtsink`, through a leaky queue when the
/// sink dials out.
///
/// An `srtsink` in caller mode with `auto-reconnect` reconnects inside
/// `render`, on the streaming thread, with blocking connect attempts of about
/// 3 s each, for as long as it has no peer. Strom links one block output to
/// several blocks through a `tee` without queues, so a sink that blocks there
/// holds up everything upstream: a live mixer stalls, every other output stops,
/// and when a peer comes back the backlog bursts out and SRT inputs that were
/// not read in the meantime break (#1259).
///
/// The queue gives the sink a thread of its own and drops the oldest data
/// while the sink is blocked (`leaky=downstream`), so an output without a peer
/// only loses its own data. This is the documented requirement the queue rule
/// in CLAUDE.md asks for. The queue is bounded by time only, because a drop
/// while the sink is connected and keeping up is a bug, and only time can be
/// set so that it never happens:
///
/// - `max-size-time` follows the pipeline latency, see
///   [`queue_max_size_time`]. It starts at the 3 s floor and is updated from
///   every LATENCY event the sink forwards upstream: the pipeline distributes
///   its latency to its sinks as a LATENCY event, and `basesink` passes it on
///   upstream once it has reached PAUSED, through this queue's `src` pad.
///   With `sync=false` the sink holds nothing for the latency, the queue
///   stays near empty while connected, and the floor applies.
/// - `max-size-buffers=0` (default 200). `mpegtsmux` emits one 1316-byte
///   buffer per 7 TS packets, so 200 buffers is ~100-250 ms at broadcast
///   bitrates.
/// - `max-size-bytes=0` (default 10 MB). 10 MB is 80 Mbit: 1.6 s at
///   50 Mbit/s, 0.4 s at 200 Mbit/s. How many bytes a second of media takes
///   depends on the bitrate the operator chose, so a byte cap that never binds
///   while connected cannot be chosen here.
///
/// With bytes and buffers unbounded, the time level is the only bound, and a
/// queue measures it from buffer timestamps (DTS, else PTS). Buffers without
/// either do not move it, so a stream of them would grow the queue without
/// bound. `mpegtsmux` stamps every buffer it pushes, and `efpmux` copies the
/// timestamps of its input frame; `srt_output_latency_test` checks both. A
/// probe on the queue's `sink` pad drops any buffer that arrives with neither
/// anyway, and a warning is logged, so the bound holds whatever the muxer
/// does.
///
/// When a peer connects after a stretch without one, the sink first sends
/// what the queue holds: up to `max-size-time` of stale media, rendered at
/// once because it is late. `srtsink` in caller mode has no signal for
/// "connected" to flush the queue on (no `caller-added`, which only a listener
/// emits, and its `stats` keep no connection state), so that is the price of
/// a limit that never binds while connected. Receivers resync on it.
///
/// What the queue drops is counted, and logged per output every 5 s from a
/// timer: as a warning when the sink was connected and sending on time (an
/// unexpected drop), at info level when it could not send (no peer, or a link
/// that cannot keep up). Buffers a flush or a stop discards are not drops.
/// See `QueueDropMonitor`.
///
/// A listener does not block without a peer as long as `wait-for-connection`
/// is false (the block default); with it true the operator asked for blocking.
/// So a listener keeps its direct link and no extra thread.
pub(crate) fn link_mux_to_srtsink(
    instance_id: &str,
    mux_id: &str,
    sink_id: &str,
    srtsink: &gst::Element,
    elements: &mut Vec<(String, gst::Element)>,
    internal_links: &mut Vec<(ElementPadRef, ElementPadRef)>,
) -> Result<(), BlockBuildError> {
    if !dials_out(srtsink) {
        internal_links.push((
            ElementPadRef::pad(mux_id, "src"),
            ElementPadRef::pad(sink_id, "sink"),
        ));
        return Ok(());
    }

    let (queue, _monitor) = caller_queue(instance_id)?;
    let queue_id = queue.name().to_string();
    info!(
        "SRT output {}: caller mode, srtsink behind a leaky queue so a missing peer only drops this output's data (#1259)",
        instance_id
    );

    internal_links.push((
        ElementPadRef::pad(mux_id, "src"),
        ElementPadRef::pad(&queue_id, "sink"),
    ));
    internal_links.push((
        ElementPadRef::pad(&queue_id, "src"),
        ElementPadRef::pad(sink_id, "sink"),
    ));
    elements.push((queue_id, queue));
    Ok(())
}

/// The leaky queue in front of a calling `srtsink`, with its probes, signal
/// handler and drop report timer installed. See [`link_mux_to_srtsink`].
fn caller_queue(
    instance_id: &str,
) -> Result<(gst::Element, Arc<QueueDropMonitor>), BlockBuildError> {
    let queue = gst::ElementFactory::make("queue")
        .name(format!("{}:srtsink_queue", instance_id))
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", queue_max_size_time(None).nseconds())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("srtsink queue: {}", e)))?;
    let monitor = install_queue_monitor(instance_id, &queue);
    Ok((queue, monitor))
}

/// Counters for the queue in front of a calling `srtsink`.
///
/// The queue does not count what it leaks, so drops are derived: buffers in,
/// minus buffers out, minus what the queue holds, minus what was discarded for
/// other reasons (a flush, a stop). The work is split by thread so that the
/// buffer probes stay cheap (CLAUDE.md, "GStreamer Pad Probes"):
///
/// - The buffer probes only add to counters and store timestamps (atomics).
/// - The accounting reads the queue's level, a property read that takes the
///   queue's lock. It runs on the upstream streaming thread where the queue's
///   lock is not held: in the `overrun` handler (gstqueue.c emits `overrun`
///   from its chain function after unlocking, right before
///   `gst_queue_leak_downstream`), and in the `sink` pad's event probe at EOS.
///   At an overrun the leak of the overrun before is complete and the buffer
///   being chained is not queued yet, so the count covers every leak up to the
///   previous overrun. A leak is accounted at the next overrun, or at EOS.
/// - FLUSH_STOP and STREAM_START rebase the counters: whatever is unaccounted
///   then was flushed (FLUSH_STOP, or a stop through READY, which flushes the
///   queue on pad deactivation), not leaked. A leak that was not accounted
///   yet is dropped from the count with it, so a flush can hide a drop but
///   never invent one.
/// - The logging runs on a 5 s timer on the system clock thread and only reads
///   counters; it logs what is left when the queue goes away, too.
///
/// One error is left: a buffer the queue's `src` thread dequeued but did not
/// push yet is missing from both the level and `out`, so an accounting that
/// falls in that window counts it as dropped. The total is a high-water mark,
/// so it is not counted twice, but it may be reported one overrun early.
///
/// Whether a drop was expected is decided at the `overrun`, from where the
/// sink stands. A syncing sink that took its last buffer and is waiting for
/// that buffer's time (running time + pipeline latency still ahead) delivered
/// the buffer before it, so it is connected and keeping up: a drop then means
/// the limit is too small, which is a bug. A sink still holding a buffer whose
/// time has passed is stuck in `render`: no peer, or a link that cannot keep
/// up, and dropping is what the queue is for. The drops counted at an overrun
/// come from the leak of the overrun before, so they take its verdict.
#[derive(Debug)]
struct QueueDropMonitor {
    instance_id: String,

    // Written by the buffer probes.
    in_buffers: AtomicU64,
    out_buffers: AtomicU64,
    /// Buffers in the chain call that passed the `sink` pad last: counted in,
    /// not queued yet while `overrun` runs.
    in_chain: AtomicU64,
    /// Timestamp (DTS, else PTS) of the first and the last buffer in, and of
    /// the last buffer out, in ns; `u64::MAX` for none yet.
    first_in_ts: AtomicU64,
    in_ts: AtomicU64,
    out_ts: AtomicU64,
    /// Buffers dropped at the `sink` pad for carrying no timestamp.
    untimestamped: AtomicU64,

    // Written by event probes and the `overrun` handler.
    /// The segment of the buffers going out, to turn `out_ts` into running
    /// time. Set per SEGMENT event, read only at an overrun.
    out_segment: Mutex<Option<gst::FormattedSegment<gst::ClockTime>>>,
    /// The pipeline latency, from the last LATENCY event, and the queue's
    /// `max-size-time` it led to, in ns.
    latency: AtomicU64,
    limit: AtomicU64,
    /// Buffers that left the count without a leak (flushed), see `rebase`.
    discarded: AtomicU64,
    /// Every drop counted so far: the high-water mark of the estimate.
    accounted: AtomicU64,
    /// `accounted`, split by verdict.
    dropped_on_time: AtomicU64,
    dropped_late: AtomicU64,
    /// The verdict of the last overrun.
    last_overrun_on_time: AtomicBool,
    /// The limit was lowered, which leaks without an `overrun`: whatever the
    /// next accounting finds is an expected drop.
    limit_lowered: AtomicBool,

    // Written by the reporter (the timer, or `drop`).
    reported_on_time: AtomicU64,
    reported_late: AtomicU64,
    reported_untimestamped: AtomicU64,
    report_in_ts: AtomicU64,
    report_in_buffers: AtomicU64,
}

/// What [`QueueDropMonitor::take_report`] found since the last report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DropReport {
    /// Buffers dropped while the sink was connected and on time.
    on_time: u64,
    /// Buffers dropped while the sink could not send.
    late: u64,
    /// Media time dropped, estimates: see `take_report`.
    on_time_duration: gst::ClockTime,
    late_duration: gst::ClockTime,
    /// Buffers dropped at the `sink` pad for carrying no timestamp.
    untimestamped: u64,
}

impl QueueDropMonitor {
    fn new(instance_id: &str) -> Self {
        Self {
            instance_id: instance_id.to_string(),
            in_buffers: AtomicU64::new(0),
            out_buffers: AtomicU64::new(0),
            in_chain: AtomicU64::new(0),
            first_in_ts: AtomicU64::new(u64::MAX),
            in_ts: AtomicU64::new(u64::MAX),
            out_ts: AtomicU64::new(u64::MAX),
            untimestamped: AtomicU64::new(0),
            out_segment: Mutex::new(None),
            latency: AtomicU64::new(0),
            limit: AtomicU64::new(queue_max_size_time(None).nseconds()),
            discarded: AtomicU64::new(0),
            accounted: AtomicU64::new(0),
            dropped_on_time: AtomicU64::new(0),
            dropped_late: AtomicU64::new(0),
            last_overrun_on_time: AtomicBool::new(false),
            limit_lowered: AtomicBool::new(false),
            reported_on_time: AtomicU64::new(0),
            reported_late: AtomicU64::new(0),
            reported_untimestamped: AtomicU64::new(0),
            report_in_ts: AtomicU64::new(u64::MAX),
            report_in_buffers: AtomicU64::new(0),
        }
    }

    /// Buffers entered the queue (call before they are queued). Atomics only.
    fn buffer_in(&self, count: u64, ts: gst::ClockTime) {
        self.in_chain.store(count, Ordering::Relaxed);
        if self.in_buffers.fetch_add(count, Ordering::Relaxed) == 0 {
            self.first_in_ts.store(ts.nseconds(), Ordering::Relaxed);
        }
        self.in_ts.store(ts.nseconds(), Ordering::Relaxed);
    }

    /// Buffers without a timestamp were dropped at the `sink` pad. Atomics
    /// only; the timer logs them.
    fn untimestamped_dropped(&self, count: u64) {
        self.untimestamped.fetch_add(count, Ordering::Relaxed);
    }

    /// The sink is about to take buffers. Atomics only.
    fn buffer_out(&self, count: u64, ts: Option<gst::ClockTime>) {
        if let Some(ts) = ts {
            self.out_ts.store(ts.nseconds(), Ordering::Relaxed);
        }
        self.out_buffers.fetch_add(count, Ordering::Relaxed);
    }

    fn set_out_segment(&self, segment: gst::FormattedSegment<gst::ClockTime>) {
        *self.out_segment.lock().unwrap_or_else(|e| e.into_inner()) = Some(segment);
    }

    /// A LATENCY event set the queue's `max-size-time` to `limit`.
    fn set_latency(&self, latency: gst::ClockTime, limit: gst::ClockTime) {
        self.latency.store(latency.nseconds(), Ordering::Relaxed);
        let old = self.limit.swap(limit.nseconds(), Ordering::Relaxed);
        if limit.nseconds() < old {
            self.limit_lowered.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the sink waits for the time of the buffer it took last, given
    /// the pipeline's running time `now`. See the type's documentation.
    fn sink_on_time(&self, now: Option<gst::ClockTime>) -> bool {
        let out_ts = self.out_ts.load(Ordering::Relaxed);
        let (Some(now), true) = (now, out_ts != u64::MAX) else {
            return false;
        };
        let segment = self.out_segment.lock().unwrap_or_else(|e| e.into_inner());
        let out_rt = match segment.as_ref() {
            Some(segment) => segment.to_running_time(gst::ClockTime::from_nseconds(out_ts)),
            None => Some(gst::ClockTime::from_nseconds(out_ts)),
        };
        let latency = gst::ClockTime::from_nseconds(self.latency.load(Ordering::Relaxed));
        out_rt.is_some_and(|rt| now < rt.saturating_add(latency))
    }

    /// Buffers counted in and not accounted for otherwise, given `level`, what
    /// the queue holds, and `in_chain`, what is counted in but not queued yet.
    /// Read `level` before calling: `out` is read here, after it, so a buffer
    /// dequeued in between is missing from neither.
    fn unaccounted(&self, level: u64, in_chain: u64) -> u64 {
        let out = self.out_buffers.load(Ordering::Relaxed);
        // `in` and `discarded` only move on the thread calling this.
        let inn = self.in_buffers.load(Ordering::Relaxed);
        let discarded = self.discarded.load(Ordering::Relaxed);
        inn.saturating_sub(in_chain)
            .saturating_sub(out)
            .saturating_sub(level)
            .saturating_sub(discarded)
    }

    /// Count the drops so far. Upstream streaming thread, while it is not in
    /// the queue's chain function (or in the `overrun` it emits).
    fn account(&self, level: u64, in_chain: u64) {
        let total = self.unaccounted(level, in_chain);
        let lowered = self.limit_lowered.swap(false, Ordering::Relaxed);
        let prev = self.accounted.load(Ordering::Relaxed);
        if total <= prev {
            return;
        }
        self.accounted.store(total, Ordering::Relaxed);
        let new = total - prev;
        if self.last_overrun_on_time.load(Ordering::Relaxed) && !lowered {
            self.dropped_on_time.fetch_add(new, Ordering::Relaxed);
        } else {
            self.dropped_late.fetch_add(new, Ordering::Relaxed);
        }
    }

    /// The queue is full and about to leak; `now` is the pipeline's running
    /// time and `level` the buffers it holds.
    fn overrun(&self, now: Option<gst::ClockTime>, level: u64) {
        self.account(level, self.in_chain.load(Ordering::Relaxed));
        self.last_overrun_on_time
            .store(self.sink_on_time(now), Ordering::Relaxed);
    }

    /// EOS reached the queue; `level` is what it holds.
    fn end_of_stream(&self, level: u64) {
        self.account(level, 0);
    }

    /// The queue is about to drop what it holds but `level` buffers, or did:
    /// whatever is not accounted for now was flushed, not leaked.
    fn rebase(&self, level: u64) {
        let unaccounted = self.unaccounted(level, 0);
        let accounted = self.accounted.load(Ordering::Relaxed);
        let discarded = self.discarded.load(Ordering::Relaxed);
        self.discarded.store(
            discarded + unaccounted.saturating_sub(accounted),
            Ordering::Relaxed,
        );
        self.last_overrun_on_time.store(false, Ordering::Relaxed);
        self.limit_lowered.store(false, Ordering::Relaxed);
    }

    /// The drops since the last report. `None` when there were none. Only
    /// one reporter at a time: the timer, or `drop`.
    fn take_report(&self) -> Option<DropReport> {
        let first_in_ts = self.first_in_ts.load(Ordering::Relaxed);
        let in_ts = self.in_ts.load(Ordering::Relaxed);
        let in_buffers = self.in_buffers.load(Ordering::Relaxed);
        let prev_in_ts = self.report_in_ts.swap(in_ts, Ordering::Relaxed);
        let prev_in_buffers = self.report_in_buffers.swap(in_buffers, Ordering::Relaxed);

        let since = |total: &AtomicU64, reported: &AtomicU64| {
            let total = total.load(Ordering::Relaxed);
            total.saturating_sub(reported.swap(total, Ordering::Relaxed))
        };
        let on_time = since(&self.dropped_on_time, &self.reported_on_time);
        let late = since(&self.dropped_late, &self.reported_late);
        let untimestamped = since(&self.untimestamped, &self.reported_untimestamped);
        if on_time == 0 && late == 0 && untimestamped == 0 {
            return None;
        }

        // The media time that came in since the last report (before the first
        // one: since the first buffer), in the share of buffers dropped. Exact
        // for a constant bitrate; timestamps that jump make it a guess.
        let prev_in_ts = if prev_in_ts == u64::MAX {
            first_in_ts
        } else {
            prev_in_ts
        };
        let span = if prev_in_ts == u64::MAX || in_ts == u64::MAX {
            0
        } else {
            in_ts.saturating_sub(prev_in_ts)
        };
        let came_in = in_buffers
            .saturating_sub(prev_in_buffers)
            .max(on_time + late)
            .max(1);
        let share = |dropped: u64| {
            gst::ClockTime::from_nseconds(
                (u128::from(span) * u128::from(dropped) / u128::from(came_in)) as u64,
            )
        };

        Some(DropReport {
            on_time,
            late,
            on_time_duration: share(on_time),
            late_duration: share(late),
            untimestamped,
        })
    }

    /// Log the drops since the last report, if any.
    fn report(&self) {
        let Some(report) = self.take_report() else {
            return;
        };
        let id = &self.instance_id;
        if report.untimestamped > 0 {
            warn!(
                "SRT output {}: dropped {} buffer(s) from the muxer that had no timestamp ({} in all); the queue in front of srtsink is bounded by time and cannot hold them",
                id,
                report.untimestamped,
                self.untimestamped.load(Ordering::Relaxed)
            );
        }
        if report.on_time > 0 {
            let limit = gst::ClockTime::from_nseconds(self.limit.load(Ordering::Relaxed));
            let latency = gst::ClockTime::from_nseconds(self.latency.load(Ordering::Relaxed));
            warn!(
                "SRT output {}: the queue in front of srtsink dropped {} buffer(s), about {} of media, while srtsink was connected and sending on time. This is unexpected: the queue holds up to {} for a pipeline latency of {}; timestamps may have jumped",
                id, report.on_time, report.on_time_duration, limit, latency
            );
        }
        if report.late > 0 {
            info!(
                "SRT output {}: dropped {} buffer(s), about {} of media, while srtsink could not send (no peer, or the link cannot keep up)",
                id, report.late, report.late_duration
            );
        }
    }

    /// Every drop counted so far, both verdicts.
    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped_on_time.load(Ordering::Relaxed) + self.dropped_late.load(Ordering::Relaxed)
    }
}

impl Drop for QueueDropMonitor {
    /// The queue is gone (the probes and handler that held this): log what
    /// the timer did not get to.
    fn drop(&mut self) {
        self.report();
    }
}

/// Probes and signals on the queue in front of a calling `srtsink`: follow the
/// pipeline latency, drop buffers without a timestamp, count drops; and a
/// timer that logs them.
fn install_queue_monitor(instance_id: &str, queue: &gst::Element) -> Arc<QueueDropMonitor> {
    let monitor = Arc::new(QueueDropMonitor::new(instance_id));
    let (Some(sink_pad), Some(src_pad)) = (queue.static_pad("sink"), queue.static_pad("src"))
    else {
        return monitor;
    };

    // Follow the pipeline latency. The LATENCY event travels upstream from
    // srtsink through this pad; it fires per latency change, not per buffer.
    let queue_weak = queue.downgrade();
    let mon = monitor.clone();
    let id = instance_id.to_string();
    src_pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(ref event)) = info.data {
            if let gst::EventView::Latency(latency) = event.view() {
                if let Some(queue) = queue_weak.upgrade() {
                    let latency = latency.latency();
                    let size = queue_max_size_time(Some(latency));
                    let old: u64 = queue.property("max-size-time");
                    // Before the property: lowering it leaks at once.
                    mon.set_latency(latency, size);
                    if old != size.nseconds() {
                        queue.set_property("max-size-time", size.nseconds());
                        debug!(
                            "SRT output {}: pipeline latency {}, srtsink queue max-size-time {} -> {}",
                            id,
                            latency,
                            gst::ClockTime::from_nseconds(old),
                            size
                        );
                    }
                }
            }
        }
        gst::PadProbeReturn::Ok
    });

    // The segment of what goes out; per SEGMENT event.
    let mon = monitor.clone();
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(ref event)) = info.data {
            if let gst::EventView::Segment(segment) = event.view() {
                if let Ok(segment) = segment.segment().clone().downcast::<gst::ClockTime>() {
                    mon.set_out_segment(segment);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });

    // Events into the queue, on the upstream thread, before the queue handles
    // them (so its lock is free for the level read). Flush events reach only
    // a probe that asks for them.
    let mon = monitor.clone();
    sink_pad.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
        move |pad, info| {
            let Some(gst::PadProbeData::Event(ref event)) = info.data else {
                return gst::PadProbeReturn::Ok;
            };
            match event.type_() {
                // The queue flushes all it holds right after forwarding this;
                // its `src` thread is paused since FLUSH_START.
                gst::EventType::FlushStop => mon.rebase(0),
                // A new stream: after a stop through READY the queue was
                // flushed on pad deactivation, with no event to tell.
                gst::EventType::StreamStart => mon.rebase(level_buffers(pad)),
                // The last leak is complete; account for it before the queue
                // drains.
                gst::EventType::Eos => mon.end_of_stream(level_buffers(pad)),
                _ => {}
            }
            gst::PadProbeReturn::Ok
        },
    );

    // The sink takes a buffer (or a list: mpegtsmux pushes lists). Per buffer:
    // atomics only.
    let mon = monitor.clone();
    src_pad.add_probe(BUFFERS, move |_, info| {
        if let Some((count, ts)) = buffers_and_timestamp(info) {
            mon.buffer_out(count, ts);
        }
        gst::PadProbeReturn::Ok
    });

    // A buffer arrives. Per buffer: atomics only.
    let mon = monitor.clone();
    sink_pad.add_probe(BUFFERS, move |_, info| {
        let Some((count, ts)) = buffers_and_timestamp(info) else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(ts) = ts else {
            // Would not move the queue's time level: drop it, so time stays a
            // bound. No muxer this queue serves is known to send one.
            mon.untimestamped_dropped(count);
            return gst::PadProbeReturn::Drop;
        };
        mon.buffer_in(count, ts);
        gst::PadProbeReturn::Ok
    });

    // Emitted from the chain function on the upstream thread, with the
    // queue's lock released, right before the queue leaks; per buffer only
    // while it does. The queue is the signal's first argument, so no
    // reference is captured.
    let mon = monitor.clone();
    queue.connect("overrun", false, move |args| {
        if let Some(queue) = args.first().and_then(|v| v.get::<gst::Element>().ok()) {
            let level = u64::from(queue.property::<u32>("current-level-buffers"));
            mon.overrun(queue.current_running_time(), level);
        }
        None
    });

    start_report_timer(&monitor);
    monitor
}

/// Log the drops every 5 s, on the system clock's thread. Holds the monitor
/// weakly and stops once the queue is gone.
fn start_report_timer(monitor: &Arc<QueueDropMonitor>) {
    let clock = gst::SystemClock::obtain();
    let id = clock.new_periodic_id(clock.time() + DROP_REPORT_INTERVAL, DROP_REPORT_INTERVAL);
    let weak = Arc::downgrade(monitor);
    if let Err(e) = id.wait_async(move |_, _, id| match weak.upgrade() {
        Some(mon) => mon.report(),
        None => id.unschedule(),
    }) {
        warn!(
            "SRT output {}: could not start the drop report timer, drops will be logged when the output stops: {:?}",
            monitor.instance_id, e
        );
    }
}

/// The buffers the queue owning `pad` holds. Takes the queue's lock: not from
/// a buffer probe, nor with the lock held.
fn level_buffers(pad: &gst::Pad) -> u64 {
    pad.parent_element()
        .map(|queue| u64::from(queue.property::<u32>("current-level-buffers")))
        .unwrap_or(0)
}

const BUFFERS: gst::PadProbeType = gst::PadProbeType::BUFFER.union(gst::PadProbeType::BUFFER_LIST);

/// How many buffers a buffer or buffer-list probe sees, and the timestamp
/// (DTS, else PTS) the queue takes from them: the last valid one.
fn buffers_and_timestamp(info: &gst::PadProbeInfo) -> Option<(u64, Option<gst::ClockTime>)> {
    match info.data {
        Some(gst::PadProbeData::Buffer(ref buffer)) => Some((1, buffer.dts_or_pts())),
        Some(gst::PadProbeData::BufferList(ref list)) => {
            let ts = list.iter().rev().find_map(|b| b.dts_or_pts());
            Some((list.len() as u64, ts))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srtsink(uri: &str) -> gst::Element {
        gst::init().unwrap();
        let sink = gst::ElementFactory::make("srtsink")
            .build()
            .expect("srtsink");
        sink.set_property("uri", uri);
        sink
    }

    #[test]
    fn dials_out_follows_the_uri() {
        let cases = [
            ("srt://127.0.0.1:5000?mode=caller", true),
            ("srt://127.0.0.1:5000", true),
            ("srt://127.0.0.1:5000?mode=rendezvous", true),
            ("srt://127.0.0.1:5000?mode=listener", false),
            ("srt://:5000?mode=listener", false),
            ("srt://:5000", false),
        ];
        for (uri, expected) in cases {
            assert_eq!(dials_out(&srtsink(uri)), expected, "{uri}");
        }
    }

    #[test]
    fn queue_max_size_time_is_latency_plus_margin_with_a_floor() {
        let ms = gst::ClockTime::from_mseconds;
        let cases = [
            (None, ms(3000)),
            (Some(ms(0)), ms(3000)),
            (Some(ms(200)), ms(3000)),
            (Some(ms(1000)), ms(3000)),
            (Some(ms(1001)), ms(3001)),
            (Some(ms(2500)), ms(4500)),
            (Some(ms(10_000)), ms(12_000)),
            (Some(gst::ClockTime::MAX), gst::ClockTime::MAX),
        ];
        for (latency, expected) in cases {
            assert_eq!(queue_max_size_time(latency), expected, "{latency:?}");
        }
    }

    #[test]
    fn no_report_without_an_overrun() {
        let mon = QueueDropMonitor::new("t");
        for i in 0..10 {
            mon.buffer_in(1, gst::ClockTime::from_mseconds(i));
        }
        // 10 in, none out, 5 queued: 5 unaccounted for, but no overrun, so
        // they are in flight, not dropped.
        assert_eq!(mon.take_report(), None);
        assert_eq!(mon.dropped(), 0);
    }

    #[test]
    fn an_overrun_counts_the_drops_before_it_once() {
        let mon = QueueDropMonitor::new("t");
        for i in 0..100 {
            mon.buffer_in(1, gst::ClockTime::from_mseconds(i * 10));
        }
        // 100 in, the last one still in the chain call, none out, 40 queued:
        // 59 dropped.
        mon.overrun(None, 40);
        let report = mon.take_report().expect("a report");
        assert_eq!((report.on_time, report.late), (0, 59));
        // Nothing new: no second report.
        mon.overrun(None, 40);
        assert_eq!(mon.take_report(), None);
        assert_eq!(mon.dropped(), 59);
    }

    #[test]
    fn eos_counts_the_last_leak() {
        let mon = QueueDropMonitor::new("t");
        for i in 0..50 {
            mon.buffer_in(1, gst::ClockTime::from_mseconds(i * 10));
        }
        mon.overrun(None, 30);
        assert_eq!(mon.dropped(), 19);
        // The leak of that overrun took one more; at EOS nothing is in the
        // chain call.
        mon.end_of_stream(29);
        assert_eq!(mon.dropped(), 21);
    }

    #[test]
    fn report_estimates_the_dropped_duration() {
        let mon = QueueDropMonitor::new("t");
        let ms = gst::ClockTime::from_mseconds;
        // 1001 buffers over 10 s of media, 201 out, 300 queued, the last one
        // in the chain call: 499 dropped, so about 5 s.
        for i in 0..=1000 {
            mon.buffer_in(1, ms(i * 10));
        }
        mon.buffer_out(201, Some(ms(2000)));
        mon.overrun(None, 300);
        let report = mon.take_report().expect("a report");
        assert_eq!(report.late, 499);
        assert_eq!(
            report.late_duration,
            gst::ClockTime::from_nseconds(10_000_000_000 * 499 / 1001)
        );

        // 5 s more in 500 buffers, 100 of them dropped: 1 s.
        for i in 1001..=1500 {
            mon.buffer_in(1, ms(i * 10));
        }
        mon.buffer_out(400, Some(ms(7000)));
        mon.overrun(None, 300);
        let report = mon.take_report().expect("a report");
        assert_eq!(report.late, 100);
        assert_eq!(report.late_duration, ms(1000));
    }

    #[test]
    fn drops_take_the_verdict_of_the_overrun_that_leaked_them() {
        gst::init().unwrap();
        let ms = gst::ClockTime::from_mseconds;
        let mon = QueueDropMonitor::new("t");
        mon.set_latency(ms(2500), queue_max_size_time(Some(ms(2500))));
        mon.buffer_in(1, ms(10_000));
        // The sink took the buffer stamped 10 s; with 2.5 s of latency it
        // sends it at 12.5 s.
        mon.buffer_out(1, Some(ms(10_000)));

        // At 12 s it is waiting for that time: connected and on time. The
        // queue then leaks one buffer, counted at the next overrun.
        assert!(mon.sink_on_time(Some(ms(12_000))));
        mon.overrun(Some(ms(12_000)), 0);
        mon.buffer_in(1, ms(10_020));
        mon.buffer_in(1, ms(10_040));
        // At 20 s it still holds it: stuck in render, no peer.
        assert!(!mon.sink_on_time(Some(ms(20_000))));
        mon.overrun(Some(ms(20_000)), 0);
        let report = mon.take_report().expect("a report");
        assert_eq!((report.on_time, report.late), (1, 0));

        // What this overrun leaked is expected.
        mon.buffer_in(1, ms(10_060));
        mon.end_of_stream(0);
        let report = mon.take_report().expect("a report");
        assert_eq!((report.on_time, report.late), (0, 2));

        // Running time comes from the segment of what goes out.
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(ms(10_000));
        mon.set_out_segment(segment);
        // Running time 0 + 2.5 s: at 2 s on time, at 3 s not.
        assert!(mon.sink_on_time(Some(ms(2000))));
        assert!(!mon.sink_on_time(Some(ms(3000))));

        // Nothing taken yet, or no clock: not on time.
        let fresh = QueueDropMonitor::new("t");
        assert!(!fresh.sink_on_time(Some(ms(1000))));
        assert!(!mon.sink_on_time(None));
    }

    #[test]
    fn a_lowered_limit_leaks_as_expected() {
        let ms = gst::ClockTime::from_mseconds;
        let mon = QueueDropMonitor::new("t");
        mon.set_latency(ms(5000), ms(7000));
        mon.buffer_in(1, ms(0));
        mon.buffer_out(1, Some(ms(0)));
        mon.overrun(Some(ms(100)), 0);
        assert!(mon.last_overrun_on_time.load(Ordering::Relaxed));
        // The latency drops: the queue leaks on the property change.
        mon.set_latency(ms(200), ms(3000));
        for i in 1..=10 {
            mon.buffer_in(1, ms(i * 10));
        }
        mon.end_of_stream(4);
        let report = mon.take_report().expect("a report");
        assert_eq!((report.on_time, report.late), (0, 6));
    }

    #[test]
    fn a_flush_is_not_a_drop() {
        let ms = gst::ClockTime::from_mseconds;
        let mon = QueueDropMonitor::new("t");
        for i in 0..40 {
            mon.buffer_in(1, ms(i * 100));
        }
        mon.buffer_out(1, Some(ms(0)));
        // 40 in, 1 out, 30 queued, 1 in the chain call: 8 dropped.
        mon.overrun(None, 30);
        assert_eq!(mon.dropped(), 8);
        // The queue flushes what it holds.
        mon.rebase(0);
        // After the flush, 10 in, 1 out, 7 queued: 2 dropped, counted at EOS.
        for i in 0..10 {
            mon.buffer_in(1, ms(i * 100));
        }
        mon.buffer_out(1, Some(ms(0)));
        mon.end_of_stream(7);
        assert_eq!(mon.dropped(), 10);

        // A new stream after a stop: the queue is empty, nothing was leaked.
        for i in 0..5 {
            mon.buffer_in(1, ms(i * 100));
        }
        mon.rebase(0);
        mon.end_of_stream(0);
        assert_eq!(mon.dropped(), 10);
    }

    /// Fill the block's queue while the sink holds a buffer, flush it, then
    /// overrun it a little: only the buffers the overrun leaked are drops.
    /// The pushes, the flush and EOS go through a pad linked to the queue, so
    /// the real queue decides what it holds, leaks and flushes.
    #[test]
    fn the_queue_counts_a_flush_as_no_drop() {
        gst::init().unwrap();
        let (queue, mon) = caller_queue("t").expect("a queue");
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("signal-handoffs", true)
            .build()
            .unwrap();
        let rendered = Arc::new(AtomicU64::new(0));
        let counter = rendered.clone();
        fakesink.connect("handoff", false, move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            None
        });
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&queue, &fakesink]).unwrap();
        queue.link(&fakesink).unwrap();
        // Hold the sink's first buffer, so the queue fills.
        let block = fakesink
            .static_pad("sink")
            .unwrap()
            .add_probe(
                gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
                |_, _| gst::PadProbeReturn::Ok,
            )
            .unwrap();

        let src = gst::Pad::builder(gst::PadDirection::Src).build();
        src.link(&queue.static_pad("sink").unwrap()).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        src.set_active(true).unwrap();
        let start = |src: &gst::Pad| {
            assert!(
                src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                    gst::ClockTime,
                >::new()))
            );
        };
        let push = |src: &gst::Pad, range: std::ops::Range<u64>| {
            for i in range {
                let mut b = gst::Buffer::with_size(188).unwrap();
                b.get_mut()
                    .unwrap()
                    .set_pts(gst::ClockTime::from_mseconds(i * 100));
                assert_eq!(src.push(b), Ok(gst::FlowSuccess::Ok));
            }
        };
        assert!(src.push_event(gst::event::StreamStart::new("s")));
        assert!(
            src.push_event(gst::event::Caps::new(&gst::Caps::new_empty_simple(
                "video/mpegts"
            )))
        );
        start(&src);

        // 2.6 s of media: below the 3 s limit, nothing leaks.
        push(&src, 0..27);
        let wait_level = |level: u32| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while queue.property::<u32>("current-level-buffers") != level {
                assert!(
                    std::time::Instant::now() < deadline,
                    "queue level {}",
                    queue.property::<u32>("current-level-buffers")
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait_level(26);

        assert!(src.push_event(gst::event::FlushStart::new()));
        assert!(src.push_event(gst::event::FlushStop::new(true)));
        assert_eq!(queue.property::<u32>("current-level-buffers"), 0);
        start(&src);

        // 3.6 s of media into an empty queue whose sink holds one buffer:
        // the last few leak.
        let pushed = 37;
        push(&src, 0..pushed);
        assert!(src.push_event(gst::event::Eos::new()));
        fakesink.static_pad("sink").unwrap().remove_probe(block);

        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS");
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");

        // The buffer the sink held at the flush was not rendered, so every
        // rendered buffer came after it.
        let leaked = pushed - rendered.load(Ordering::Relaxed);
        assert!((1..10).contains(&leaked), "leaked {leaked}");
        assert_eq!(mon.dropped(), leaked, "the 26 flushed buffers are no drops");
    }

    /// The queue a caller gets, fed a buffer without a timestamp between two
    /// with one: the untimestamped one must not reach the sink, the others
    /// must. `srtsink` is swapped for a `fakesink`; the queue is the block's.
    #[test]
    fn the_queue_drops_buffers_without_a_timestamp() {
        let sink = srtsink("srt://127.0.0.1:5000?mode=caller");
        let mut elements = vec![("t:srtsink".to_string(), sink.clone())];
        let mut links = Vec::new();
        link_mux_to_srtsink("t", "t:mux", "t:srtsink", &sink, &mut elements, &mut links)
            .expect("links");
        let queue = elements
            .iter()
            .find(|(id, _)| id == "t:srtsink_queue")
            .map(|(_, e)| e.clone())
            .expect("a caller gets a queue");
        assert_eq!(queue.property::<u32>("max-size-bytes"), 0);
        assert_eq!(queue.property::<u32>("max-size-buffers"), 0);
        assert_eq!(
            queue.property::<u64>("max-size-time"),
            QUEUE_MIN_SIZE_TIME.nseconds()
        );

        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple("video/mpegts"))
            .format(gst::Format::Time)
            .build();
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline
            .add_many([src.upcast_ref(), &queue, &fakesink])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &queue, &fakesink]).unwrap();
        let seen = Arc::new(AtomicU64::new(0));
        let counter = seen.clone();
        fakesink
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();

        let stamped = |ms: u64| {
            let mut b = gst::Buffer::with_size(188).unwrap();
            b.get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_mseconds(ms));
            b
        };
        src.push_buffer(stamped(0)).unwrap();
        src.push_buffer(gst::Buffer::with_size(188).unwrap())
            .unwrap();
        src.push_buffer(stamped(20)).unwrap();
        src.end_of_stream().unwrap();
        let bus = pipeline.bus().unwrap();
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("EOS");
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
        assert_eq!(seen.load(Ordering::Relaxed), 2);
    }
}
