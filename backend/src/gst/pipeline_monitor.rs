//! Periodic monitor for GStreamer pipeline health.
//!
//! Warns when queue/queue2 elements accumulate too much data: once per
//! episode, and above a queue's own `min-threshold-time` (a delay line).
//!
//! Buffer age measurement is handled exclusively by manual pad probes
//! (`buffer_age_probe.rs`) which measure real buffer PTS on the streaming
//! thread. Polling segment positions from a timer is unreliable (stale
//! data, clock domain mismatches) and produces false warnings.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use strom_types::FlowId;
use tracing::{debug, warn};

use crate::state::AppState;

/// Nanoseconds per millisecond.
const NS_PER_MS: u64 = 1_000_000;

/// Default threshold for queue `current-level-time` (2 s), on top of the
/// queue's own `min-threshold-time`.
const QUEUE_THRESHOLD_NS: u64 = 2000 * NS_PER_MS;

/// How often the monitor polls.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Queue-family factory names (multiqueue excluded — it lacks current-level-time).
const QUEUE_FACTORIES: &[&str] = &["queue", "queue2"];

/// Start the background monitor task.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        let mut over = OverQueues::default();
        loop {
            interval.tick().await;
            check_all(&state, &mut over).await;
        }
    });
}

async fn check_all(state: &AppState, over: &mut OverQueues) {
    let pipelines = state.pipelines_read().await;

    let mut levels = HashMap::new();
    for (flow_id, manager) in pipelines.iter() {
        for level in queues_over(manager.pipeline()) {
            levels.insert((*flow_id, level.name.clone()), level);
        }
    }

    let (entered, left) = over.update(levels.keys().cloned().collect());

    // Once per episode: a queue that stays full would otherwise warn every poll.
    for key in entered {
        let level = &levels[&key];
        warn!(
            flow = %key.0,
            queue = %level.name,
            kind = %level.kind,
            current_ms = level.current_ns / NS_PER_MS,
            threshold_ms = level.threshold_ns / NS_PER_MS,
            buffers = level.buffers,
            bytes = level.bytes,
            "Queue level exceeds threshold"
        );
    }
    for (flow_id, queue) in left {
        debug!(flow = %flow_id, queue = %queue, "Queue level back below threshold, or queue gone");
    }
}

/// A queue holding more than its threshold.
#[derive(Debug)]
struct QueueLevel {
    name: String,
    kind: String,
    current_ns: u64,
    threshold_ns: u64,
    buffers: u32,
    bytes: u32,
}

/// The queues in `pipeline` that hold more than their threshold.
///
/// A queue with `min-threshold-time` holds that much by design (a delay
/// line), so its threshold is [`QUEUE_THRESHOLD_NS`] on top of it:
/// a 5 s delay warns above 7 s, not on every poll.
fn queues_over(pipeline: &gst::Pipeline) -> Vec<QueueLevel> {
    let mut over = Vec::new();
    for element in pipeline.iterate_recurse().into_iter().flatten() {
        let factory_name = match element.factory() {
            Some(f) => f.name().to_string(),
            None => continue,
        };

        if !QUEUE_FACTORIES.contains(&factory_name.as_str()) {
            continue;
        }

        let delay_ns: u64 = if element.has_property("min-threshold-time") {
            element.property("min-threshold-time")
        } else {
            0
        };
        let threshold_ns = QUEUE_THRESHOLD_NS.saturating_add(delay_ns);
        let current_ns: u64 = element.property("current-level-time");
        if current_ns <= threshold_ns {
            continue;
        }

        over.push(QueueLevel {
            // The path, not the name: names are unique only within a bin.
            name: element.path_string().to_string(),
            kind: factory_name,
            current_ns,
            threshold_ns,
            buffers: element.property("current-level-buffers"),
            bytes: element.property("current-level-bytes"),
        });
    }
    over
}

/// A queue, by flow and element path.
type QueueKey = (FlowId, String);

/// The queues over threshold at the last poll.
#[derive(Default)]
struct OverQueues(HashSet<QueueKey>);

impl OverQueues {
    /// Record this poll's set; return the queues that crossed over and the
    /// ones that dropped back (or went away) since the last poll.
    fn update(&mut self, now: HashSet<QueueKey>) -> (Vec<QueueKey>, Vec<QueueKey>) {
        let entered = now.difference(&self.0).cloned().collect();
        let left = self.0.difference(&now).cloned().collect();
        self.0 = now;
        (entered, left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Buffers pushed into each test queue: 30 x 100 ms = 3 s, over the 2 s threshold.
    const BUFFERS: u64 = 30;
    const BUFFER_NS: u64 = 100 * NS_PER_MS;
    /// Level at which the queue counts as filled (2.5 s; the sink may hold one buffer).
    const FILLED_NS: u64 = 2500 * NS_PER_MS;

    /// appsrc ! queue ! fakesink, paused with 3 s of buffers in the queue.
    /// The queue keeps `min_threshold_ns` as its delay.
    fn filled_queue(min_threshold_ns: u64) -> gst::Pipeline {
        gst::init().expect("GStreamer init");
        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .format(gst::Format::Time)
            .caps(&gst::Caps::new_empty_simple("application/x-test"))
            .build();
        let queue = gst::ElementFactory::make("queue")
            .name("q")
            .property("max-size-time", 0u64)
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("min-threshold-time", min_threshold_ns)
            .build()
            .expect("queue");
        let sink = gst::ElementFactory::make("fakesink")
            .build()
            .expect("fakesink");
        pipeline
            .add_many([src.upcast_ref(), &queue, &sink])
            .expect("add");
        gst::Element::link_many([src.upcast_ref(), &queue, &sink]).expect("link");

        for i in 0..BUFFERS {
            let mut buffer = gst::Buffer::with_size(1).expect("buffer");
            let b = buffer.get_mut().expect("writable");
            b.set_pts(gst::ClockTime::from_nseconds(i * BUFFER_NS));
            b.set_duration(gst::ClockTime::from_nseconds(BUFFER_NS));
            src.push_buffer(buffer).expect("push");
        }
        let _ = pipeline.set_state(gst::State::Paused);

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let level: u64 = queue.property("current-level-time");
            if level >= FILLED_NS {
                break;
            }
            assert!(Instant::now() < deadline, "queue only reached {} ns", level);
            std::thread::sleep(Duration::from_millis(10));
        }
        pipeline
    }

    #[test]
    fn an_overloaded_queue_is_over() {
        let pipeline = filled_queue(0);
        let over = queues_over(&pipeline);
        let _ = pipeline.set_state(gst::State::Null);
        assert_eq!(over.len(), 1, "{:?}", over);
        assert_eq!(over[0].threshold_ns, QUEUE_THRESHOLD_NS);
    }

    /// A send-delay queue holding 10 s by design held 3 s here and warned
    /// every 5 s before its own delay counted towards the threshold.
    #[test]
    fn a_delay_queue_within_its_delay_is_not_over() {
        let pipeline = filled_queue(10_000 * NS_PER_MS);
        let over = queues_over(&pipeline);
        let _ = pipeline.set_state(gst::State::Null);
        assert!(over.is_empty(), "{:?}", over);
    }

    /// A queue that stays full warns once, not on every poll.
    #[test]
    fn a_queue_that_stays_over_is_reported_once() {
        let key = (FlowId::nil(), "q".to_string());
        let mut over = OverQueues::default();

        let (entered, left) = over.update(HashSet::from([key.clone()]));
        assert_eq!((entered.len(), left.len()), (1, 0));

        for _ in 0..3 {
            let (entered, left) = over.update(HashSet::from([key.clone()]));
            assert_eq!((entered.len(), left.len()), (0, 0));
        }

        let (entered, left) = over.update(HashSet::new());
        assert_eq!((entered.len(), left.len()), (0, 1));

        let (entered, _) = over.update(HashSet::from([key]));
        assert_eq!(entered.len(), 1, "a new episode warns again");
    }
}
