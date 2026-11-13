//! What the SRT output blocks share around their `srtsink`.

use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use strom_types::element::ElementPadRef;
use tracing::{debug, info, warn};

/// Headroom above the pipeline latency in the queue in front of a calling
/// `srtsink`. See [`queue_max_size_time`].
pub(crate) const QUEUE_LATENCY_MARGIN: gst::ClockTime = gst::ClockTime::from_seconds(2);

/// The least `max-size-time` of that queue, whatever the pipeline latency.
pub(crate) const QUEUE_MIN_SIZE_TIME: gst::ClockTime = gst::ClockTime::from_seconds(3);

/// How often a drop report may be logged per output.
const DROP_REPORT_INTERVAL_NS: u64 = 5_000_000_000;

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
/// anyway, and warns, so the bound holds whatever the muxer does.
///
/// When a peer connects after a stretch without one, the sink first sends
/// what the queue holds: up to `max-size-time` of stale media, rendered at
/// once because it is late. `srtsink` in caller mode has no signal for
/// "connected" to flush the queue on (no `caller-added`, which only a listener
/// emits, and its `stats` keep no connection state), so that is the price of
/// a limit that never binds while connected. Receivers resync on it.
///
/// What the queue drops is counted and logged per output, at most every 5 s:
/// as a warning when the sink was connected and sending on time (an
/// unexpected drop), at info level when it could not send (no peer, or a link
/// that cannot keep up). See `QueueDropMonitor`.
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

    let queue_id = format!("{}:srtsink_queue", instance_id);
    let queue = gst::ElementFactory::make("queue")
        .name(&queue_id)
        .property_from_str("leaky", "downstream")
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", queue_max_size_time(None).nseconds())
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("srtsink queue: {}", e)))?;
    install_queue_monitor(instance_id, &queue);
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

/// Counters for the queue in front of a calling `srtsink`.
///
/// Updated from buffer probes, so atomics only on that path. The queue does
/// not count what it leaks, so drops are derived: buffers in, minus buffers
/// out, minus what the queue holds. That estimate is only looked at after the
/// queue reported itself full (`overrun`, emitted right before it leaks), so a
/// buffer in flight between the queue and the sink never shows up as a drop on
/// its own.
///
/// Whether a drop was expected is decided at the `overrun`, from where the
/// sink stands. A syncing sink that took its last buffer and is waiting for
/// that buffer's time (running time + pipeline latency still ahead) delivered
/// the buffer before it, so it is connected and keeping up: a drop then means
/// the limit is too small, which is a bug. A sink still holding a buffer whose
/// time has passed is stuck in `render`: no peer, or a link that cannot keep
/// up, and dropping is what the queue is for.
#[derive(Debug)]
struct QueueDropMonitor {
    in_buffers: AtomicU64,
    out_buffers: AtomicU64,
    /// Timestamp (DTS, else PTS) of the first and the last buffer in, and of
    /// the last buffer out, in ns; `u64::MAX` for none yet.
    first_in_ts: AtomicU64,
    in_ts: AtomicU64,
    out_ts: AtomicU64,
    /// The segment of the buffers going out, to turn `out_ts` into running
    /// time. Set per SEGMENT event, read only at an overrun.
    out_segment: Mutex<Option<gst::FormattedSegment<gst::ClockTime>>>,
    /// The pipeline latency, from the last LATENCY event, in ns.
    latency: AtomicU64,
    /// Buffers dropped at the `sink` pad for carrying no timestamp.
    untimestamped: AtomicU64,
    /// Set on `overrun`: the queue is about to leak.
    overran: AtomicBool,
    /// Set on `overrun` while the sink was waiting for its buffer's time.
    overran_while_on_time: AtomicBool,
    /// Drops already reported, and the state at the last report.
    reported_dropped: AtomicU64,
    reported_untimestamped: AtomicU64,
    report_in_ts: AtomicU64,
    report_in_buffers: AtomicU64,
    /// When the last report was due, in ns since `epoch`.
    epoch: Instant,
    last_report_ns: AtomicU64,
}

/// What [`QueueDropMonitor::take_report`] found since the last report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DropReport {
    buffers: u64,
    /// Media time dropped, an estimate: see `take_report`.
    duration: gst::ClockTime,
    /// Some of it while the sink was connected and on time.
    while_on_time: bool,
}

impl QueueDropMonitor {
    fn new() -> Self {
        Self {
            in_buffers: AtomicU64::new(0),
            out_buffers: AtomicU64::new(0),
            first_in_ts: AtomicU64::new(u64::MAX),
            in_ts: AtomicU64::new(u64::MAX),
            out_ts: AtomicU64::new(u64::MAX),
            out_segment: Mutex::new(None),
            latency: AtomicU64::new(0),
            untimestamped: AtomicU64::new(0),
            overran: AtomicBool::new(false),
            overran_while_on_time: AtomicBool::new(false),
            reported_dropped: AtomicU64::new(0),
            reported_untimestamped: AtomicU64::new(0),
            report_in_ts: AtomicU64::new(u64::MAX),
            report_in_buffers: AtomicU64::new(0),
            epoch: Instant::now(),
            last_report_ns: AtomicU64::new(0),
        }
    }

    /// Buffers entered the queue (call before they are queued).
    fn buffer_in(&self, count: u64, ts: gst::ClockTime) {
        if self.in_buffers.fetch_add(count, Ordering::Relaxed) == 0 {
            self.first_in_ts.store(ts.nseconds(), Ordering::Relaxed);
        }
        self.in_ts.store(ts.nseconds(), Ordering::Relaxed);
    }

    fn untimestamped_dropped(&self, count: u64) {
        self.untimestamped.fetch_add(count, Ordering::Relaxed);
    }

    /// The sink is about to take buffers.
    fn buffer_out(&self, count: u64, ts: Option<gst::ClockTime>) {
        if let Some(ts) = ts {
            self.out_ts.store(ts.nseconds(), Ordering::Relaxed);
        }
        self.out_buffers.fetch_add(count, Ordering::Relaxed);
    }

    fn set_out_segment(&self, segment: gst::FormattedSegment<gst::ClockTime>) {
        *self.out_segment.lock().unwrap_or_else(|e| e.into_inner()) = Some(segment);
    }

    fn set_latency(&self, latency: gst::ClockTime) {
        self.latency.store(latency.nseconds(), Ordering::Relaxed);
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

    /// The queue is full and about to leak; `now` is the pipeline's running
    /// time.
    fn overrun(&self, now: Option<gst::ClockTime>) {
        if self.sink_on_time(now) {
            self.overran_while_on_time.store(true, Ordering::Relaxed);
        }
        self.overran.store(true, Ordering::Relaxed);
    }

    /// Mark that the queue may have leaked without an `overrun` (its limit
    /// was lowered).
    fn may_have_leaked(&self) {
        self.overran.store(true, Ordering::Relaxed);
    }

    /// Every buffer the queue has dropped so far, estimated from its counters
    /// and `level_buffers`, what it holds now. Only meaningful after an
    /// overrun; see the type's documentation.
    fn dropped_total(&self, level_buffers: u64) -> u64 {
        // Read `out` before `in`: `in` only moves on the thread calling this.
        let out = self.out_buffers.load(Ordering::Relaxed);
        let inn = self.in_buffers.load(Ordering::Relaxed);
        inn.saturating_sub(out).saturating_sub(level_buffers)
    }

    /// Whether a report is due, at most every 5 s. Cheap; per buffer.
    fn report_due(&self) -> bool {
        let now = self.epoch.elapsed().as_nanos() as u64;
        let last = self.last_report_ns.load(Ordering::Relaxed);
        if now.saturating_sub(last) < DROP_REPORT_INTERVAL_NS {
            return false;
        }
        self.last_report_ns.store(now, Ordering::Relaxed);
        true
    }

    /// The drops since the last report, given what the queue holds now.
    /// `None` when there were none.
    fn take_report(&self, level_buffers: u64) -> Option<DropReport> {
        let first_in_ts = self.first_in_ts.load(Ordering::Relaxed);
        let in_ts = self.in_ts.load(Ordering::Relaxed);
        let in_buffers = self.in_buffers.load(Ordering::Relaxed);
        let prev_in_ts = self.report_in_ts.swap(in_ts, Ordering::Relaxed);
        let prev_in_buffers = self.report_in_buffers.swap(in_buffers, Ordering::Relaxed);

        if !self.overran.swap(false, Ordering::Relaxed) {
            return None;
        }
        let while_on_time = self.overran_while_on_time.swap(false, Ordering::Relaxed);
        let total = self.dropped_total(level_buffers);
        let prev = self.reported_dropped.load(Ordering::Relaxed);
        if total <= prev {
            return None;
        }
        self.reported_dropped.store(total, Ordering::Relaxed);
        let dropped = total - prev;

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
        let came_in = in_buffers.saturating_sub(prev_in_buffers).max(dropped);
        let duration = (u128::from(span) * u128::from(dropped) / u128::from(came_in)) as u64;

        Some(DropReport {
            buffers: dropped,
            duration: gst::ClockTime::from_nseconds(duration),
            while_on_time,
        })
    }
}

/// Probes and signals on the queue in front of a calling `srtsink`: follow the
/// pipeline latency, drop buffers without a timestamp, count and log drops.
fn install_queue_monitor(instance_id: &str, queue: &gst::Element) {
    let monitor = Arc::new(QueueDropMonitor::new());
    let (Some(sink_pad), Some(src_pad)) = (queue.static_pad("sink"), queue.static_pad("src"))
    else {
        return;
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
                    mon.set_latency(latency);
                    let size = queue_max_size_time(Some(latency));
                    let old: u64 = queue.property("max-size-time");
                    if old != size.nseconds() {
                        queue.set_property("max-size-time", size.nseconds());
                        if size.nseconds() < old {
                            mon.may_have_leaked();
                        }
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

    // The sink takes a buffer (or a list: mpegtsmux pushes lists). Per buffer:
    // atomics only.
    let mon = monitor.clone();
    src_pad.add_probe(BUFFERS, move |_, info| {
        if let Some((count, ts)) = buffers_and_timestamp(info) {
            mon.buffer_out(count, ts);
        }
        gst::PadProbeReturn::Ok
    });

    // A buffer arrives. Per buffer: atomics and a clock read, plus a report
    // (two property reads and a log line) at most every 5 s.
    let queue_weak = queue.downgrade();
    let mon = monitor.clone();
    let id = instance_id.to_string();
    sink_pad.add_probe(BUFFERS, move |_, info| {
        if mon.report_due() {
            if let Some(queue) = queue_weak.upgrade() {
                report_drops(&id, &queue, &mon);
            }
        }
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

    // Emitted right before the queue leaks; per buffer only while it does.
    // The queue is the signal's first argument, so no reference is captured.
    queue.connect("overrun", false, move |args| {
        let now = args
            .first()
            .and_then(|v| v.get::<gst::Element>().ok())
            .and_then(|queue| queue.current_running_time());
        monitor.overrun(now);
        None
    });
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

fn report_drops(instance_id: &str, queue: &gst::Element, mon: &QueueDropMonitor) {
    let untimestamped = mon.untimestamped.load(Ordering::Relaxed);
    let prev_untimestamped = mon
        .reported_untimestamped
        .swap(untimestamped, Ordering::Relaxed);
    if untimestamped > prev_untimestamped {
        warn!(
            "SRT output {}: dropped {} buffer(s) from the muxer that had no timestamp ({} in all); the queue in front of srtsink is bounded by time and cannot hold them",
            instance_id,
            untimestamped - prev_untimestamped,
            untimestamped
        );
    }

    let level_buffers = u64::from(queue.property::<u32>("current-level-buffers"));
    let Some(report) = mon.take_report(level_buffers) else {
        return;
    };
    if report.while_on_time {
        let limit = gst::ClockTime::from_nseconds(queue.property::<u64>("max-size-time"));
        let latency = gst::ClockTime::from_nseconds(mon.latency.load(Ordering::Relaxed));
        warn!(
            "SRT output {}: the queue in front of srtsink dropped {} buffer(s), about {} of media, while srtsink was connected and sending on time. This is unexpected: the queue holds up to {} for a pipeline latency of {}; timestamps may have jumped",
            instance_id, report.buffers, report.duration, limit, latency
        );
    } else {
        info!(
            "SRT output {}: dropped {} buffer(s), about {} of media, while srtsink could not send (no peer, or the link cannot keep up)",
            instance_id, report.buffers, report.duration
        );
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
        let mon = QueueDropMonitor::new();
        for i in 0..10 {
            mon.buffer_in(1, gst::ClockTime::from_mseconds(i));
        }
        // 10 in, none out, 5 queued: 5 unaccounted for, but no overrun, so
        // they are in flight, not dropped.
        assert_eq!(mon.take_report(5), None);
    }

    #[test]
    fn report_counts_drops_once() {
        let mon = QueueDropMonitor::new();
        for i in 0..100 {
            mon.buffer_in(1, gst::ClockTime::from_mseconds(i * 10));
        }
        mon.overrun(None);
        // 100 in, none out, 40 queued: 60 dropped.
        let report = mon.take_report(40).expect("a report");
        assert_eq!(report.buffers, 60);
        // Nothing new: no second report.
        mon.overrun(None);
        assert_eq!(mon.take_report(40), None);
    }

    #[test]
    fn report_estimates_the_dropped_duration() {
        let mon = QueueDropMonitor::new();
        let ms = gst::ClockTime::from_mseconds;
        // 1001 buffers over 10 s of media, 201 out, 300 queued: 500 dropped,
        // so about 5 s.
        for i in 0..=1000 {
            mon.buffer_in(1, ms(i * 10));
        }
        mon.buffer_out(201, Some(ms(2000)));
        mon.overrun(None);
        let report = mon.take_report(300).expect("a report");
        assert_eq!(report.buffers, 500);
        assert_eq!(
            report.duration,
            gst::ClockTime::from_nseconds(10_000_000_000 * 500 / 1001)
        );

        // 5 s more in 500 buffers, 100 of them dropped: 1 s.
        for i in 1001..=1500 {
            mon.buffer_in(1, ms(i * 10));
        }
        mon.buffer_out(400, Some(ms(7000)));
        mon.overrun(None);
        let report = mon.take_report(300).expect("a report");
        assert_eq!(report.buffers, 100);
        assert_eq!(report.duration, ms(1000));
    }

    #[test]
    fn a_drop_is_unexpected_only_while_the_sink_is_on_time() {
        gst::init().unwrap();
        let ms = gst::ClockTime::from_mseconds;
        let mon = QueueDropMonitor::new();
        mon.set_latency(ms(2500));
        mon.buffer_in(1, ms(10_000));
        // The sink took the buffer stamped 10 s; with 2.5 s of latency it
        // sends it at 12.5 s.
        mon.buffer_out(1, Some(ms(10_000)));

        // At 12 s it is waiting for that time: connected and on time.
        assert!(mon.sink_on_time(Some(ms(12_000))));
        mon.overrun(Some(ms(12_000)));
        mon.buffer_in(1, ms(10_020));
        let report = mon.take_report(0).expect("a report");
        assert!(report.while_on_time);

        // At 20 s it still holds it: stuck in render, no peer.
        assert!(!mon.sink_on_time(Some(ms(20_000))));
        mon.overrun(Some(ms(20_000)));
        mon.buffer_in(1, ms(10_040));
        let report = mon.take_report(0).expect("a report");
        assert!(!report.while_on_time);

        // Running time comes from the segment of what goes out.
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(ms(10_000));
        mon.set_out_segment(segment);
        // Running time 0 + 2.5 s: at 2 s on time, at 3 s not.
        assert!(mon.sink_on_time(Some(ms(2000))));
        assert!(!mon.sink_on_time(Some(ms(3000))));

        // Nothing taken yet, or no clock: not on time.
        let fresh = QueueDropMonitor::new();
        assert!(!fresh.sink_on_time(Some(ms(1000))));
        assert!(!mon.sink_on_time(None));
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
