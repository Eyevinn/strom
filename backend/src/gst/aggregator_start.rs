//! Start-time handling for live aggregators (`audiomixer`, `compositor`,
//! `glvideomixerelement`) whose inputs start empty.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;

/// Stop an aggregator from rewinding its output to 0 when the first buffer
/// reaches one of its inputs.
///
/// `GstAggregator` selects its start time when the first buffer arrives on any
/// sink pad. With `start-time-selection=zero` that sets the output position to
/// `MIN(0, position)`. A `force-live` aggregator does not wait for that buffer:
/// while its inputs are empty it times out and outputs silence (or background)
/// on the pipeline clock. So in a flow that starts before anyone publishes to
/// a WHIP slot, the first real buffer arrives seconds later and moves the
/// position back to 0. The next output buffer is stamped 0, with a duration
/// that reaches forward to the correct end, and the stream then carries on.
/// One buffer, but `opusenc` rejects it ("buffer going too far back in time"),
/// and webrtcsink ends every WHEP viewer session connected at that moment.
///
/// `gst_aggregator_update_segment` turns that selection off. The segment passed
/// is the aggregator's initial one: start 0, position unset. Both
/// `GstAudioAggregator` and `GstVideoAggregator` read an unset position as the
/// segment start, so output still begins at 0, as zero start-time selection
/// would place it.
///
/// Call before the element leaves NULL. A PAUSED to READY change turns the
/// selection back on; flows build a new pipeline on every start, so that path
/// is not taken here.
pub(crate) fn disarm_start_time_selection(element: &gst::Element) {
    let Some(aggregator) = element.downcast_ref::<gst_base::Aggregator>() else {
        return;
    };
    let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
    segment.set_position(gst::ClockTime::NONE);
    aggregator.update_segment(&segment);
}

/// Drives a live aggregator through a late first input, for the regression
/// tests of each block that builds one.
#[cfg(test)]
pub(crate) mod test_support {
    use gstreamer as gst;
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// How long each wait gives the aggregator before the test gives up.
    /// Every wait ends as soon as its condition holds, so a healthy run never
    /// takes this long. The bound only has to separate "slow" from "never":
    /// a loaded CI runner can stall the streaming threads for hundreds of
    /// milliseconds, and an aggregator that stopped outputting stays stopped.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Output PTS seen by the appsink, and a signal for each new one.
    type Output = Arc<(Mutex<Vec<gst::ClockTime>>, Condvar)>;

    /// Block until `done` holds for the output so far, or `DEADLINE` passes.
    /// Returns whether it held.
    fn wait_for_output(output: &Output, done: impl Fn(&[gst::ClockTime]) -> bool) -> bool {
        let (lock, cvar) = &**output;
        let deadline = Instant::now() + DEADLINE;
        let mut pts = lock.lock().unwrap();
        while !done(&pts) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            pts = cvar.wait_timeout(pts, left).unwrap().0;
        }
        true
    }

    /// Run `aggregator` live with one input that stays empty for at least
    /// `idle`, and until it has output on its own timeouts. Then push a single
    /// `buffer_size`-byte buffer stamped at the current running time, and wait
    /// until the output reaches that time. Returns the pushed PTS and every
    /// output PTS, in order.
    ///
    /// Both waits are on the output itself, not on wall-clock sleeps, so a
    /// starved runner only makes the test slower. If a wait times out, the
    /// output so far is returned and `assert_no_rewind` reports it.
    pub(crate) fn output_pts_around_late_first_input(
        aggregator: &gst::Element,
        caps: &gst::Caps,
        buffer_size: usize,
        buffer_duration: gst::ClockTime,
        idle: Duration,
    ) -> (gst::ClockTime, Vec<gst::ClockTime>) {
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .caps(caps)
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let appsink = gst_app::AppSink::builder().caps(caps).sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref(), aggregator, appsink.upcast_ref()])
            .unwrap();
        let sink_pad = aggregator.request_pad_simple("sink_%u").unwrap();
        appsrc.static_pad("src").unwrap().link(&sink_pad).unwrap();
        aggregator.link(&appsink).unwrap();

        let output: Output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let output_cb = output.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(pts) = sample.buffer().and_then(|b| b.pts()) {
                        let (lock, cvar) = &*output_cb;
                        lock.lock().unwrap().push(pts);
                        cvar.notify_all();
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(idle);
        // The bug needs the output position to have moved past 0 before the
        // first input arrives, so the aggregator must have timed out at least
        // once on its own.
        wait_for_output(&output, |pts| !pts.is_empty());
        // Push between two output buffers, while the aggregator waits on the
        // clock. A push while it is inside `aggregate` can land between the
        // subclass reading the output position and writing it back, and that
        // write hides a rewind. So wait for a fresh output buffer, then for
        // half an input buffer duration (one output period in these tests).
        let seen = output.0.lock().unwrap().len();
        wait_for_output(&output, |pts| pts.len() > seen);
        std::thread::sleep(Duration::from_nanos(buffer_duration.nseconds() / 2));

        let now = pipeline
            .current_running_time()
            .expect("pipeline is running");
        let mut buffer = gst::Buffer::with_size(buffer_size).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(now);
            buffer.set_duration(buffer_duration);
        }
        appsrc.push_buffer(buffer).unwrap();
        // A rewound buffer would be the first output after the input arrived,
        // so it is already recorded by the time the output reaches `now`.
        wait_for_output(&output, |pts| pts.iter().any(|pts| *pts >= now));

        pipeline.set_state(gst::State::Null).unwrap();
        let pts = output.0.lock().unwrap().clone();
        (now, pts)
    }

    /// Panic unless the aggregator produced output before the input arrived
    /// and its output timestamps never went backwards.
    pub(crate) fn assert_no_rewind(pushed: gst::ClockTime, pts: &[gst::ClockTime]) {
        assert!(
            pts.first().is_some_and(|first| *first < pushed),
            "aggregator produced no output before its first input at {pushed}; output: {pts:?}"
        );
        assert!(
            pts.iter().any(|pts| *pts >= pushed),
            "the late input at {pushed} never reached the output; output: {pts:?}"
        );
        for pair in pts.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "output went back in time from {} to {} after the first input at {}",
                pair[0],
                pair[1],
                pushed
            );
        }
    }
}
