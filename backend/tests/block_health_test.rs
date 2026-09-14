//! A pipeline branch can stop dead while the pipeline still reports Playing.
//!
//! When a downstream push fails with `not-negotiated`, `GstAggregator` marks
//! its sink pads flushing and pauses its source pad task. Nothing is posted to
//! the bus and no element changes state, so the failure is invisible in
//! `gst_state`. These tests pin both halves: that the stall is silent, and
//! that the block health scan reports it.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn init() {
    gst::init().unwrap();
}

/// Poll `condition` until true or `timeout` elapses. Returns whether it held.
fn wait_for(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

/// videotestsrc -> compositor -> capsfilter -> fakesink.
///
/// The capsfilter is the lever: retightening it to a caps the compositor
/// cannot produce makes the next push fail negotiation.
struct StallRig {
    pipeline: gst::Pipeline,
    compositor: gst::Element,
    capsfilter: gst::Element,
    bus_errors: Arc<Mutex<Vec<String>>>,
}

impl StallRig {
    fn build() -> Self {
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let compositor = gst::ElementFactory::make("compositor").build().unwrap();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .build(),
            )
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();

        pipeline
            .add_many([&src, &compositor, &capsfilter, &sink])
            .unwrap();
        src.link(&compositor).unwrap();
        gst::Element::link_many([&compositor, &capsfilter, &sink]).unwrap();

        let bus_errors = Arc::new(Mutex::new(Vec::new()));
        let collected = bus_errors.clone();
        let bus = pipeline.bus().unwrap();
        bus.add_signal_watch();
        bus.connect_message(Some("error"), move |_, msg| {
            if let gst::MessageView::Error(err) = msg.view() {
                collected.lock().unwrap().push(err.error().to_string());
            }
        });

        Self {
            pipeline,
            compositor,
            capsfilter,
            bus_errors,
        }
    }

    fn compositor_task_state(&self) -> gst::TaskState {
        self.compositor.static_pad("src").unwrap().task_state()
    }

    /// Force the next compositor push to fail negotiation.
    fn break_negotiation(&self) {
        self.capsfilter.set_property(
            "caps",
            gst::Caps::builder("video/x-bayer")
                .field("format", "bggr")
                .build(),
        );
    }
}

impl Drop for StallRig {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[test]
fn aggregator_stall_is_silent_and_leaves_pipeline_playing() {
    init();
    let rig = StallRig::build();
    rig.pipeline.set_state(gst::State::Playing).unwrap();

    // videotestsrc is live, so the transition is async. Block until it settles:
    // the compositor task starts before the pipeline finishes reaching Playing,
    // so waiting on the task state alone races the state change.
    let (res, current, _) = rig.pipeline.state(gst::ClockTime::from_seconds(10));
    assert_eq!(res, Ok(gst::StateChangeSuccess::Success));
    assert_eq!(current, gst::State::Playing);
    assert_eq!(rig.compositor_task_state(), gst::TaskState::Started);

    rig.break_negotiation();

    assert!(
        wait_for(
            || rig.compositor_task_state() == gst::TaskState::Paused,
            Duration::from_secs(10)
        ),
        "compositor task never stalled after negotiation was broken"
    );

    // The whole point: the stall reports nothing where a caller would look.
    let (_, current, _) = rig.pipeline.state(gst::ClockTime::from_seconds(1));
    assert_eq!(
        current,
        gst::State::Playing,
        "pipeline should still report Playing while the branch is dead"
    );
    let errors = rig.bus_errors.lock().unwrap();
    assert!(
        errors.is_empty(),
        "expected no bus error, got: {:?}",
        errors
    );
}
