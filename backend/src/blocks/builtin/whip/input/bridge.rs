//! WHIP Input session-to-slot bridge: forwards a session's samples into its slot's appsrc.

use crate::gst::pipeline_bridge::{self, SessionBridge};
use crate::whip_session_manager::SessionActivity;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_rtp as gst_rtp;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// Wire one session stream's appsink into its slot's appsrc in the main
/// pipeline.
///
/// A function rather than an inline closure so a test can drive the callback:
/// the guard that matters is here, not in [`pipeline_bridge`]. A bare
/// `appsrc.push_sample(&sample)` in this body re-opens the cascade an unstamped
/// buffer starts and fails nothing in that module's tests.
#[allow(clippy::too_many_arguments)]
pub(super) fn install_slot_bridge(
    appsink: &gst_app::AppSink,
    appsrc: gst_app::AppSrc,
    bridge: Arc<SessionBridge>,
    main_pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    media_type: &str,
    slot: usize,
    activity: Arc<SessionActivity>,
    session_finished: Arc<AtomicBool>,
) {
    // Resolved here, not per buffer: the pad's media type is fixed.
    let pad_is_audio = media_type == "audio";
    let video_order = VideoPtsOrder::new();

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // Media arriving from the publisher. Half of this session's
                // liveness; the other half is stamped on the slot's output tee
                // in the main pipeline.
                activity.touch_ingress(pad_is_audio);

                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                forward_sample_to_slot(
                    &sample,
                    &appsrc,
                    &session_finished,
                    &bridge,
                    &main_pipeline_weak,
                    if pad_is_audio {
                        SlotMedia::Audio
                    } else {
                        SlotMedia::Video(&video_order)
                    },
                    slot,
                )?;

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// How far behind the previous frame's PTS a new frame's may be and still be
/// treated as landing on it. A run released on one PTS falls behind by 1 ns
/// per frame already moved; a frame interval is over 8 ms at up to 120 fps.
const MAX_PTS_NUDGE_NS: u64 = 1_000_000;

/// Gives each of a session's video frames its own PTS when several arrive on one.
///
/// The session's jitterbuffer can release a run of packets from different
/// frames all on one PTS, as it does when a publisher joins a slot. The
/// slot's `h264parse` takes a PTS only when it differs from the previous
/// buffer's, so every later frame of the run leaves it with none; a browser
/// stream has no framerate to derive one from, and the vision mixer's
/// compositor fails the whole flow on a frame without a timestamp.
///
/// Packets of one frame keep their shared PTS. A new frame whose PTS is at
/// most [`MAX_PTS_NUDGE_NS`] behind the previous frame's is moved 1 ns past it.
/// Any other PTS passes through: a larger step back is real timing (a B-frame,
/// sent before the frame it precedes, or a step in the sender's clock), and
/// holding later frames behind a PTS far ahead would freeze the seat. Only the
/// appsink's streaming thread touches it.
struct VideoPtsOrder {
    /// RTP timestamp of the last frame, `u64::MAX` before the first.
    last_rtptime: AtomicU64,
    last_pts: AtomicU64,
}

impl VideoPtsOrder {
    fn new() -> Self {
        Self {
            last_rtptime: AtomicU64::new(u64::MAX),
            last_pts: AtomicU64::new(0),
        }
    }

    fn order(&self, rtptime: u32, pts: u64) -> u64 {
        let last_rtptime = self.last_rtptime.load(Ordering::Relaxed);
        let last_pts = self.last_pts.load(Ordering::Relaxed);
        if last_rtptime == u64::from(rtptime) {
            return last_pts;
        }
        let duplicate =
            last_rtptime != u64::MAX && pts <= last_pts && last_pts - pts <= MAX_PTS_NUDGE_NS;
        let pts = if duplicate { last_pts + 1 } else { pts };
        self.last_rtptime
            .store(u64::from(rtptime), Ordering::Relaxed);
        self.last_pts.store(pts, Ordering::Relaxed);
        pts
    }
}

/// Which of a slot's streams a sample is bridged into.
#[derive(Clone, Copy)]
enum SlotMedia<'a> {
    Audio,
    Video(&'a VideoPtsOrder),
}

impl SlotMedia<'_> {
    fn name(self) -> &'static str {
        match self {
            SlotMedia::Audio => "audio",
            SlotMedia::Video(_) => "video",
        }
    }
}

/// `sample` with its PTS moved by `order`, or `None` when it stays as it is.
/// Copies the buffer only when the PTS changes. A buffer with no PTS, or one
/// that is not RTP, is left for the bridge to judge.
fn order_video_pts(sample: &gst::Sample, order: &VideoPtsOrder) -> Option<gst::Sample> {
    let buffer = sample.buffer()?;
    let pts = buffer.pts()?.nseconds();
    let rtptime = gst_rtp::RTPBuffer::from_buffer_readable(buffer)
        .ok()?
        .timestamp();
    let ordered = order.order(rtptime, pts);
    if ordered == pts {
        return None;
    }
    let mut new_buffer = buffer.copy();
    new_buffer
        .make_mut()
        .set_pts(gst::ClockTime::from_nseconds(ordered));
    let mut builder = gst::Sample::builder().buffer(&new_buffer);
    let caps = sample.caps_owned();
    if let Some(caps) = caps.as_ref() {
        builder = builder.caps(caps);
    }
    Some(builder.build())
}

/// Bridge one sample from a session's appsink into its slot's appsrc, shifting
/// its PTS by the offset shared across the session's audio and video.
///
/// The offset and the drop of unstamped buffers are [`SessionBridge`]'s: a
/// buffer with no PTS never reaches the slot, because a muxer downstream
/// answers it with `GST_FLOW_ERROR` and takes the whole flow down.
///
/// Nothing is pushed once `session_finished` is set. The slot's appsrc belongs to
/// whoever holds the slot, and takeover releases the slot while the displaced
/// session may still be delivering media: without this check, two sessions push
/// into one appsrc with different offsets until the old pipeline reaches NULL.
fn forward_sample_to_slot(
    sample: &gst::Sample,
    appsrc: &gst_app::AppSrc,
    session_finished: &AtomicBool,
    bridge: &SessionBridge,
    main_pipeline: &gst::glib::WeakRef<gst::Pipeline>,
    media: SlotMedia,
    slot: usize,
) -> Result<(), gst::FlowError> {
    if session_finished.load(Ordering::Relaxed) {
        return Ok(());
    }

    let ordered = match media {
        SlotMedia::Video(order) => order_video_pts(sample, order),
        SlotMedia::Audio => None,
    };
    let sample = ordered.as_ref().unwrap_or(sample);

    let outcome = bridge
        .forward(sample, appsrc, || {
            let main_pipeline = main_pipeline.upgrade()?;
            let clock = main_pipeline.clock()?;
            let base_time = main_pipeline.base_time()?;
            Some(clock.time().saturating_sub(base_time))
        })
        .ok_or(gst::FlowError::Error)?;

    match outcome {
        pipeline_bridge::Forwarded::OffsetComputed(offset) => {
            info!(
                "WHIP Input: Computed shared ts-offset={}ms from {} stream (slot {})",
                offset / 1_000_000,
                media.name(),
                slot
            );
        }
        pipeline_bridge::Forwarded::DroppedUnstamped { dropped } => {
            if pipeline_bridge::should_log_drop(dropped) {
                warn!(
                    "WHIP Input: dropped {} buffer(s) with no PTS on the {} stream (slot {}); forwarding one fails the downstream muxer and takes the whole flow with it",
                    dropped, media.name(), slot
                );
            }
        }
        pipeline_bridge::Forwarded::Restamped
        | pipeline_bridge::Forwarded::Unadjusted
        | pipeline_bridge::Forwarded::PushFailed(_) => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gst::pipeline_bridge::test_support::{at, Harness, SessionPipeline};
    use crate::whip_session_manager::SlotOutput;
    use std::time::Instant;

    /// The wiring guard for this block. [`pipeline_bridge`] can only promise
    /// that [`SessionBridge::forward`] drops an unstamped buffer; it cannot
    /// promise this block still calls it. So drive the real appsink callback:
    /// three buffers into a session pipeline, the middle one with no PTS, and
    /// only the two stamped ones may reach the slot's appsrc.
    ///
    /// Forwarding that buffer is what makes `qtmux` answer `Buffer has no PTS`
    /// with `GST_FLOW_ERROR`, which tears down the whole flow — every seat, not
    /// just this one.
    #[test]
    fn an_unstamped_buffer_never_reaches_the_slot_appsrc() {
        let slot_pipeline = Harness::new();
        let session = SessionPipeline::new();

        let appsink = gst_app::AppSink::builder().sync(false).build();
        session
            .pipeline
            .add(appsink.upcast_ref::<gst::Element>())
            .expect("add appsink");
        session
            .src
            .link(&appsink)
            .expect("link session appsrc to appsink");

        install_slot_bridge(
            &appsink,
            slot_pipeline.src.clone(),
            Arc::new(SessionBridge::new()),
            slot_pipeline.pipeline_weak(),
            "video",
            0,
            Arc::new(SessionActivity::new(
                Instant::now(),
                Arc::new(SlotOutput::new(Instant::now())),
            )),
            Arc::new(AtomicBool::new(false)),
        );

        session.start();
        session.push(at(1));
        session.push(None);
        session.push(at(3));

        let first = slot_pipeline.next_pts().expect("first buffer crossed");
        assert!(first.is_some(), "a stamped buffer must keep its stamp");
        let second = slot_pipeline
            .next_pts()
            .expect("the third buffer crossed too");
        assert!(
            second.is_some(),
            "the unstamped buffer must not be here — the third one is next"
        );
        assert!(second > first, "and it must follow the first");
        assert_eq!(
            slot_pipeline.next_pts(),
            None,
            "nothing else should have crossed"
        );
    }

    /// Takeover releases a slot while the displaced session may still be
    /// delivering media, and the slot's appsrc passes to the new session. Once
    /// the old session is marked finished, its samples must stop reaching that
    /// appsrc, or both sessions feed it with their own timestamp offsets.
    #[test]
    fn a_finished_session_stops_feeding_its_slot() {
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();
        appsrc.link(&appsink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let caps = gst::Caps::builder("application/x-test").build();
        let sample_at = |seconds| {
            let mut buffer = gst::Buffer::new();
            buffer
                .get_mut()
                .unwrap()
                .set_pts(gst::ClockTime::from_seconds(seconds));
            gst::Sample::builder().buffer(&buffer).caps(&caps).build()
        };
        // No main pipeline, so the bridge forwards both samples unadjusted.
        let bridge = SessionBridge::new();
        let no_main_pipeline = gst::glib::WeakRef::new();

        let displaced = AtomicBool::new(true);
        forward_sample_to_slot(
            &sample_at(1),
            &appsrc,
            &displaced,
            &bridge,
            &no_main_pipeline,
            SlotMedia::Audio,
            0,
        )
        .unwrap();

        let current = AtomicBool::new(false);
        forward_sample_to_slot(
            &sample_at(2),
            &appsrc,
            &current,
            &bridge,
            &no_main_pipeline,
            SlotMedia::Audio,
            0,
        )
        .unwrap();

        let first = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("the current session's sample must reach the slot");
        pipeline.set_state(gst::State::Null).unwrap();

        assert_eq!(
            first.buffer().unwrap().pts(),
            Some(gst::ClockTime::from_seconds(2)),
            "the first sample in the slot must be the current session's, not the displaced one's"
        );
    }

    /// Bridge RTP video packets, given as (RTP timestamp, PTS in ms), through
    /// one session's `forward_sample_to_slot` and return the PTS each reached
    /// the slot with, in ns.
    fn bridge_video_packets(packets: &[(u32, u64)]) -> Vec<u64> {
        use gst_rtp::prelude::RTPBufferExt;
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder().format(gst::Format::Time).build();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), appsink.upcast_ref()])
            .unwrap();
        appsrc.link(&appsink).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "video")
            .field("clock-rate", 90000i32)
            .field("encoding-name", "H264")
            .build();
        // No main pipeline, so the bridge forwards every PTS unadjusted and
        // only the ordering moves it.
        let bridge = SessionBridge::new();
        let no_main_pipeline = gst::glib::WeakRef::new();
        let session_finished = AtomicBool::new(false);
        let order = VideoPtsOrder::new();

        let mut out = Vec::new();
        for &(rtptime, pts_ms) in packets {
            let mut buffer = gst::Buffer::new_rtp_with_sizes(4, 0, 0).unwrap();
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(gst::ClockTime::from_mseconds(pts_ms));
                let mut rtp = gst_rtp::RTPBuffer::from_buffer_writable(buffer).unwrap();
                rtp.set_timestamp(rtptime);
            }
            let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
            forward_sample_to_slot(
                &sample,
                &appsrc,
                &session_finished,
                &bridge,
                &no_main_pipeline,
                SlotMedia::Video(&order),
                0,
            )
            .unwrap();
            let pulled = appsink
                .try_pull_sample(gst::ClockTime::from_seconds(5))
                .expect("every packet must reach the slot");
            out.push(pulled.buffer().unwrap().pts().unwrap().nseconds());
        }
        pipeline.set_state(gst::State::Null).unwrap();
        out
    }

    /// A session's jitterbuffer can release packets of several frames on one
    /// PTS. Each frame must still reach the slot with its own, increasing PTS,
    /// and the packets of one frame with a shared one.
    #[test]
    fn video_frames_on_one_pts_reach_the_slot_in_order() {
        // Two packets of frame A, then frames B and C released on A's PTS,
        // then frame D on its own later PTS.
        let out = bridge_video_packets(&[
            (1000, 1000),
            (1000, 1000),
            (4000, 1000),
            (7000, 1000),
            (10000, 1100),
        ]);

        let ms = 1_000_000;
        assert_eq!(out[0], 1000 * ms);
        assert_eq!(out[1], out[0], "packets of one frame share a PTS");
        assert!(out[2] > out[1], "frame B must follow frame A: {:?}", out);
        assert!(out[3] > out[2], "frame C must follow frame B: {:?}", out);
        assert_eq!(out[4], 1100 * ms, "a frame already in order keeps its PTS");
    }

    /// An encoder with B-frames sends a frame after the one it precedes, so
    /// its PTS is a frame or more behind the previous packet's. It must keep
    /// that PTS: moving it past the later frame bunches the decoded video.
    #[test]
    fn video_b_frames_keep_their_pts() {
        // Decode order I P B B P, 33 ms per frame: display order I B B P P.
        let out = bridge_video_packets(&[
            (0, 1000),
            (9000, 1100),
            (3000, 1033),
            (6000, 1066),
            (18000, 1200),
        ]);

        let ms = 1_000_000;
        assert_eq!(
            out,
            vec![1000 * ms, 1100 * ms, 1033 * ms, 1066 * ms, 1200 * ms],
            "B-frames must reach the slot with their own PTS"
        );
    }

    /// One frame with a PTS far ahead of the rest must not hold back the
    /// frames after it, or the seat freezes until real time catches up.
    #[test]
    fn video_frame_far_ahead_does_not_hold_back_later_frames() {
        let out = bridge_video_packets(&[
            (0, 1000),
            (3000, 1033),
            (6000, 10_000),
            (9000, 1100),
            (12000, 1133),
        ]);

        let ms = 1_000_000;
        assert_eq!(
            out,
            vec![1000 * ms, 1033 * ms, 10_000 * ms, 1100 * ms, 1133 * ms],
            "frames after an outlier must keep their own PTS"
        );
    }
}
