//! A WHIP session's audio and video pads appear one after the other within a
//! millisecond, each already carrying media, and each gets a
//! `tee -> fakesink + appsink` branch added to a session pipeline that is
//! already PLAYING.
//!
//! A sink that still wants a preroll answers that state change with ASYNC. Two
//! of those in quick succession on one pipeline leave the second branch below
//! PLAYING, where it takes a single buffer and then blocks its streaming thread
//! for good. From the outside that is a publisher whose audio or video never
//! starts, with nothing in the log to say so — and the seat is useless either
//! way, because the flow's recorder and mixer are waiting on both streams.
//!
//! `attach_session_branch` builds both sinks with `async` off, so there is no
//! ASYNC cycle for the second branch to land in.
//!
//! Because the pad is already carrying media, the branch must also be PLAYING
//! before the pad is linked to it. A buffer pushed into an element still in
//! NULL comes back as FLUSHING, and the upstream source pauses its streaming
//! task for good on that: the stream is gone after the first buffer, with the
//! sinks sitting in PLAYING and nothing in the log. `attach_session_branch`
//! links the pad last for that reason.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use strom::blocks::builtin::whip::attach_session_branch;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// Elements this test needs beyond core GStreamer. Missing on a bare CI image.
const REQUIRED: &[&str] = &["tee", "fakesink", "appsink", "videotestsrc", "audiotestsrc"];

/// One round is enough to strand a branch here — 20 runs of each arm, all 20
/// failing with the fix reverted and all 20 passing with it. A few rounds are
/// cheap insurance in case a faster or slower host makes it probabilistic
/// again, which is what it is on the rig.
const ROUNDS: usize = 3;

/// Skipping on a missing element passes green and guards nothing, so CI sets
/// `STROM_REQUIRE_GST_PLUGINS=1` to turn a skip into a failure.
fn plugins_available() -> bool {
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|e| gst::ElementFactory::find(e).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        strom_types::env::var_opt("STROM_REQUIRE_GST_PLUGINS").is_none(),
        "STROM_REQUIRE_GST_PLUGINS is set but these elements are missing: {}",
        missing.join(", ")
    );
    false
}

/// A running source standing in for one of whipserversrc's pads, which carry
/// media by the time they appear.
///
/// The pad handed out is a `tee`'s, not the source's own: `allow-not-linked`
/// lets it drop data until the branch arrives, where a source pushing into its
/// own unlinked pad would stop with a flow error first.
struct LiveSource {
    pad: gst::Pad,
}

impl LiveSource {
    fn new(pipeline: &gst::Pipeline, factory: &str, is_live: bool) -> Self {
        let element = gst::ElementFactory::make(factory)
            .property("is-live", is_live)
            .build()
            .unwrap_or_else(|_| panic!("{factory} available"));
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()
            .expect("tee");
        pipeline.add_many([&element, &tee]).expect("add source");
        element.link(&tee).expect("source -> tee");
        let pad = tee.request_pad_simple("src_%u").expect("tee src pad");
        Self { pad }
    }
}

/// Count what an appsink actually receives. Pulling is what the real bridge
/// callback does; without it the sink's queue would fill and hide a stall.
fn count_samples(appsink: &gst_app::AppSink) -> Arc<AtomicUsize> {
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_cb = Arc::clone(&seen);
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let _ = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                seen_cb.fetch_add(1, Ordering::Relaxed);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    seen
}

/// One session: two pads carrying media, each given a branch on a pipeline that
/// is already PLAYING. Reports each branch's sample count and the state its
/// appsink ended in, so a failure can name both.
///
/// A live source paces itself, so whether a buffer lands while a branch is
/// being linked is down to timing. A source that is not live pushes as fast as
/// it can and always has a buffer in flight, which makes that moment certain.
fn run_round(is_live: bool) -> ((usize, gst::State), (usize, gst::State)) {
    let pipeline = gst::Pipeline::new();
    let video = LiveSource::new(&pipeline, "videotestsrc", is_live);
    let audio = LiveSource::new(&pipeline, "audiotestsrc", is_live);

    pipeline
        .set_state(gst::State::Playing)
        .expect("pipeline accepts PLAYING");
    let (_, current, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    assert_eq!(
        current,
        gst::State::Playing,
        "the session pipeline must be PLAYING before its branches are attached"
    );

    // One after the other on one thread, as whipserversrc's pad-added fires.
    let sinks: Vec<gst_app::AppSink> = [(&video.pad, "video_0"), (&audio.pad, "audio_0")]
        .into_iter()
        .map(|(pad, name)| {
            attach_session_branch(&pipeline, pad, name)
                .unwrap_or_else(|| panic!("{name} branch attaches"))
        })
        .collect();

    let counts: Vec<Arc<AtomicUsize>> = sinks.iter().map(count_samples).collect();

    // A stranded branch takes one buffer and stops, so wait long enough that
    // "one" cannot be confused with "still starting".
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if counts.iter().all(|c| c.load(Ordering::Relaxed) > 5) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = (
        (
            counts[0].load(Ordering::Relaxed),
            sinks[0].upcast_ref::<gst::Element>().current_state(),
        ),
        (
            counts[1].load(Ordering::Relaxed),
            sinks[1].upcast_ref::<gst::Element>().current_state(),
        ),
    );

    pipeline
        .set_state(gst::State::Null)
        .expect("pipeline to NULL");
    out
}

/// The regression. Both of a session's branches must keep carrying data after
/// being attached, back to back, to a pipeline that is already running.
///
/// Revert the fix and the branch attached second is left in READY having taken
/// a single buffer, while the first runs normally. Which stream that is on the
/// rig depends on the order whipserversrc exposes its pads, and both orders
/// occur.
#[test]
fn both_session_branches_keep_running_when_attached_in_succession() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    assert_rounds_keep_running(true);
}

/// The branch must be up before the pad that is already carrying media is
/// linked to it. Link first and the buffer in flight meets a tee still in
/// NULL, comes back FLUSHING, and the source pauses for good: that branch ends
/// with 0 samples while its sinks report PLAYING.
///
/// Free-running sources keep a buffer in flight at every moment, so the window
/// between the link and the state change is hit far more often than with live
/// sources, where it took a loaded host to see it about one run in four.
#[test]
fn branch_attached_to_a_pad_already_carrying_media_keeps_its_stream() {
    gst::init().expect("gstreamer init");
    if !plugins_available() {
        eprintln!("skipping: required GStreamer elements missing");
        return;
    }

    assert_rounds_keep_running(false);
}

fn assert_rounds_keep_running(is_live: bool) {
    for round in 0..ROUNDS {
        let ((video_samples, video_state), (audio_samples, audio_state)) = run_round(is_live);
        assert!(
            video_samples > 5 && audio_samples > 5,
            "round {round}: a branch stopped carrying data — \
             video {video_samples} samples in {video_state:?}, \
             audio {audio_samples} samples in {audio_state:?}"
        );
        assert_eq!(
            (video_state, audio_state),
            (gst::State::Playing, gst::State::Playing),
            "round {round}: a branch was left below PLAYING"
        );
    }
}
