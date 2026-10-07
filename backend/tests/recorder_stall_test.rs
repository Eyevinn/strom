//! The recorder's track stall watchdog, built through the real
//! `RecorderBuilder` and fed from live test sources.
//!
//! A binary of its own because it shortens the recorder's stall timeout for
//! the whole process (`set_track_stall_timeout_for_tests`), which the other
//! recorder tests should not run with.

pub mod common;
#[path = "common/recorder.rs"]
pub mod recorder;

use std::path::Path;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use recorder::*;

/// `splitmuxsink` releases a GOP only once every one of its sink pads has
/// advanced past it, so one track that stops delivering freezes the whole
/// recording — and, through the tee that feeds the recorder, every other branch
/// of that source with it. A participant whose microphone dies mid-session
/// takes their video and their recording down with it, and the program output
/// too if they were the only live source.
///
/// EOS is what takes a pad out of that wait; a GAP event does not, splitmuxsink
/// ignores it on a non-reference stream. So the recorder ends the track rather
/// than trying to keep it idling.
///
/// The first test asserts that a stopped track does not freeze the rest; the
/// other two, that tracks which are still running are left alone — both when
/// the recording is healthy and when the muxer itself stops for a while. Ending
/// every track on a timer would satisfy the first and destroy every recording.
/// The first also pins down *which* track is ended: if the recorder ended the
/// video track — the one still delivering — no video would reach the muxer
/// either.
///
/// The sleeps are fixed because the recorder's stall timeout is: tracks have to
/// be watched for longer than it to show that nothing was ended. They are
/// counted in multiples of `STALL_TIMEOUT`, which this binary shortens the
/// recorder's five seconds to.
mod stalled_track {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// The recorder's track stall timeout in these tests.
    const STALL_TIMEOUT: Duration = Duration::from_secs(1);

    /// Fewest buffers a running track hands the muxer per second of a
    /// measuring window: under a third of either source's rate (30 video
    /// frames, ~43 AAC frames), so only a track that stopped falls short.
    const MIN_PER_SECOND: u64 = 9;

    /// The window the tests measure a running track over.
    const WINDOW: Duration = Duration::from_secs(2);
    const MIN_IN_WINDOW: u64 = MIN_PER_SECOND * WINDOW.as_secs();

    /// Swallow EOS on `element`'s src pad, so a source that runs out of buffers
    /// looks like a track that simply stopped arriving. This is what the
    /// recorder actually sees: EOS does not cross a WebRTC hop, so a publisher
    /// whose microphone dies just stops sending RTP.
    fn drop_eos(element: &gst::Element) {
        element.static_pad("src").expect("src pad").add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM,
            |_pad, info| match info.data.as_ref() {
                Some(gst::PadProbeData::Event(e)) if e.type_() == gst::EventType::Eos => {
                    // Handled, not Drop: before 1.24.8 GStreamer frees a dropped
                    // event twice and logs a CRITICAL.
                    gst::PadProbeReturn::Handled
                }
                _ => gst::PadProbeReturn::Ok,
            },
        );
    }

    /// Link `source` into `target` through an `identity` that swallows EOS.
    fn link_without_eos(pipeline: &gst::Pipeline, source: &gst::Element, target: &gst::Element) {
        let gate = gst::ElementFactory::make("identity")
            .build()
            .expect("identity");
        drop_eos(&gate);
        pipeline.add(&gate).unwrap();
        source.link(&gate).unwrap();
        gate.link(target).expect("link source into recorder");
    }

    /// A one-video, one-audio recorder with live sources on both inputs that
    /// never send EOS. `audio_buffers` of -1 keeps the audio running.
    fn start_recorder(
        pipeline: &gst::Pipeline,
        instance_id: &str,
        media_root: &Path,
        audio_buffers: i32,
    ) {
        strom::blocks::builtin::recorder::set_track_stall_timeout_for_tests(STALL_TIMEOUT);
        let rec = add_mp4_recorder(pipeline, instance_id, media_root, 1, 1);
        let video = video_source(pipeline, -1, true);
        link_without_eos(pipeline, &video, &rec.input("video_input_0"));
        let audio = audio_source(pipeline, audio_buffers, true);
        link_without_eos(pipeline, &audio, &rec.input("audio_input_0"));
        rec.run_setups();

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline accepts PLAYING");
        let _ = pipeline.state(gst::ClockTime::from_seconds(15));
    }

    /// Buffers reaching splitmuxsink's `pad`: what the muxer is actually
    /// consuming, which is what stops when it is waiting on another pad.
    fn count_into_muxer(pipeline: &gst::Pipeline, instance_id: &str, pad: &str) -> Arc<AtomicU64> {
        let counter = Arc::new(AtomicU64::new(0));
        let sink = pipeline
            .by_name(&format!("{}:splitmuxsink", instance_id))
            .expect("splitmuxsink in pipeline");
        let sink_pad = sink
            .static_pad(pad)
            .unwrap_or_else(|| panic!("splitmuxsink has a {} pad", pad));
        let c = Arc::clone(&counter);
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
            c.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        counter
    }

    /// Video keeps arriving, audio stops after a second. The recording must
    /// go on without the audio rather than freeze on it.
    ///
    /// Reverting the fix pins the counted video at zero: with no watchdog to
    /// end the audio track, splitmuxsink never releases another GOP.
    #[test]
    fn a_track_that_stops_does_not_freeze_the_recording() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_stall", media_root, 43); // ~1 s at 1024 samples / 44.1 kHz
        let video_into_muxer = count_into_muxer(&pipeline, "rec_stall", "video");

        // Audio stops at ~1 s and the watchdog ends that track a stall timeout
        // later. Measure the window after that, so this is about recovery, not
        // the stall.
        std::thread::sleep(Duration::from_secs(1) + STALL_TIMEOUT * 3);
        let (buffers_before, bytes_before) = (
            video_into_muxer.load(Ordering::Relaxed),
            total_bytes(&recordings(media_root, "")),
        );
        std::thread::sleep(WINDOW);
        let (buffers_after, bytes_after) = (
            video_into_muxer.load(Ordering::Relaxed),
            total_bytes(&recordings(media_root, "")),
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        // Anything near the frame rate means the muxer is running rather than
        // waiting on the dead track.
        assert!(
            buffers_after - buffers_before >= MIN_IN_WINDOW,
            "the recording froze on the track that stopped: {} video buffers reached the muxer in {:?} ({} bytes written)",
            buffers_after - buffers_before,
            WINDOW,
            bytes_after - bytes_before
        );
    }

    /// Every track still delivering: none of them may be ended.
    ///
    /// Counterpart to the test above — ending tracks on a timer regardless of
    /// whether they are live would satisfy that one and lose the audio of every
    /// recording.
    #[test]
    fn tracks_that_keep_running_are_left_alone() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_live", media_root, -1);
        let audio_into_muxer = count_into_muxer(&pipeline, "rec_live", "audio_0");
        let video_into_muxer = count_into_muxer(&pipeline, "rec_live", "video");

        // Well past the stall timeout, so a watchdog that ignored liveness has fired.
        std::thread::sleep(STALL_TIMEOUT * 3);
        let (audio_before, video_before) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(WINDOW);
        let (audio_after, video_after) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert!(
            audio_after - audio_before >= MIN_IN_WINDOW,
            "the audio track was ended while it was still delivering: {} buffers reached the muxer in {:?}",
            audio_after - audio_before,
            WINDOW
        );
        assert!(
            video_after - video_before >= MIN_IN_WINDOW,
            "the video track was ended while it was still delivering: {} buffers reached the muxer in {:?}",
            video_after - video_before,
            WINDOW
        );
    }

    /// The muxer stops writing for longer than the stall timeout — a disk or
    /// network share that stalls — while both sources keep delivering. Every
    /// track goes quiet at the muxer, exactly as when one of them dies, but none
    /// of them is at fault, and once the write goes through both have to keep
    /// recording.
    ///
    /// A watchdog that trusts a frozen recording alone ends one track here, and
    /// that track never comes back: it fails the assertion on the track it
    /// picked.
    #[test]
    fn a_muxer_that_stalls_for_every_track_ends_none() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_disk", media_root, -1);
        let audio_into_muxer = count_into_muxer(&pipeline, "rec_disk", "audio_0");
        let video_into_muxer = count_into_muxer(&pipeline, "rec_disk", "video");
        std::thread::sleep(Duration::from_secs(1));

        // Park the muxer's output where its file write would block.
        let splitmuxsink = pipeline
            .by_name("rec_disk:splitmuxsink")
            .expect("splitmuxsink in pipeline")
            .downcast::<gst::Bin>()
            .expect("splitmuxsink is a bin");
        let file_sink_pad = splitmuxsink
            .iterate_sinks()
            .into_iter()
            .filter_map(Result::ok)
            .find_map(|sink| sink.static_pad("sink"))
            .expect("splitmuxsink has created its file sink");
        let block = file_sink_pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
                gst::PadProbeReturn::Ok
            })
            .expect("block the file sink");

        // Long enough for the stall to reach every input, and then the timeout.
        std::thread::sleep(Duration::from_secs(3));
        let stalled_from = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(STALL_TIMEOUT * 3);
        let stalled_to = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        file_sink_pad.remove_probe(block);

        // Give the backlog time to drain, then measure.
        std::thread::sleep(Duration::from_secs(2));
        let (audio_before, video_before) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        std::thread::sleep(WINDOW);
        let (audio_after, video_after) = (
            audio_into_muxer.load(Ordering::Relaxed),
            video_into_muxer.load(Ordering::Relaxed),
        );
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        // Without a real stall at the muxer pads this test proves nothing.
        assert_eq!(
            stalled_to, stalled_from,
            "blocking the file sink did not stop the muxer taking buffers (audio, video)"
        );
        assert!(
            audio_after - audio_before >= MIN_IN_WINDOW,
            "the audio track was ended during a stall that was not its fault: {} buffers reached the muxer in {:?} after it cleared",
            audio_after - audio_before,
            WINDOW
        );
        assert!(
            video_after - video_before >= MIN_IN_WINDOW,
            "the video track was ended during a stall that was not its fault: {} buffers reached the muxer in {:?} after it cleared",
            video_after - video_before,
            WINDOW
        );
    }

    /// The same stall, with the stopped track's chain already coming apart: its
    /// parser no longer active, so it refuses the EOS that would end the track,
    /// as a pad that is flushing, inactive or out of its parent does. The
    /// recording must still get out of the muxer's wait.
    ///
    /// Without the fallback to the splitmuxsink pad, the refused EOS leaves the
    /// muxer waiting on the audio track for good and the counted video stays at
    /// zero.
    #[test]
    fn a_stalled_track_whose_chain_refuses_the_eos_still_ends() {
        if !plugins_available() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let media_root = tmp.path();
        let pipeline = gst::Pipeline::new();
        start_recorder(&pipeline, "rec_refuse", media_root, 43); // ~1 s at 1024 samples / 44.1 kHz
        let video_into_muxer = count_into_muxer(&pipeline, "rec_refuse", "video");

        // Audio has stopped by now, and the watchdog cannot have ended it yet:
        // every track has to be quiet at the muxer for a stall timeout first.
        std::thread::sleep(Duration::from_millis(1300));
        let parser_sink = pipeline
            .by_name("rec_refuse:audio_0_parser")
            .expect("the audio parser is in the pipeline")
            .static_pad("sink")
            .expect("parser sink pad");
        assert!(
            !parser_sink.pad_flags().contains(gst::PadFlags::EOS),
            "the watchdog ended the audio track before its chain was broken, so this test proves nothing"
        );
        parser_sink
            .set_active(false)
            .expect("deactivate the audio parser's sink pad");

        // Past the stall timeout with room for a poll or two, then measure the
        // window after it: this is about recovery, not about the stall.
        std::thread::sleep(STALL_TIMEOUT * 4);
        let buffers_before = video_into_muxer.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < deadline
            && video_into_muxer.load(Ordering::Relaxed) - buffers_before < MIN_IN_WINDOW
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let buffers_after = video_into_muxer.load(Ordering::Relaxed);
        pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");

        assert!(
            buffers_after - buffers_before >= MIN_IN_WINDOW,
            "the recording stayed frozen on a track whose chain refused the EOS: {} video buffers reached the muxer within 20 s",
            buffers_after - buffers_before
        );
    }
}
