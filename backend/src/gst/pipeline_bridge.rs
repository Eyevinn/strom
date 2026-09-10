//! The session → flow pipeline bridge: what crosses it, and with what timestamps.
//!
//! Two blocks run media in a *session* pipeline of their own, isolated from the
//! flow's main pipeline: a WHIP publisher, so that a publisher connecting or
//! dropping cannot restart the flow, and the Media Player, so that a file
//! switch or a seek stays local to the player. Both cross the boundary through
//! an `appsink`/`appsrc` pair: the session pipeline's appsink hands each sample
//! to this module, which pushes it into the block's appsrc in the main
//! pipeline.
//!
//! Two things have to happen at that boundary.
//!
//! **Timestamps have to be rebased.** The two pipelines have independent base
//! times, so a buffer's PTS only means anything in the pipeline it came from.
//! The bridge computes one offset per session — `main_running_time - pts`, from
//! the first buffer that carries a PTS — and adds it to every buffer on both
//! streams. One offset shared between audio and video, not one each, or the two
//! would drift apart from each other after the shift. Audio and video arrive on
//! their own streaming threads, so "once per session" is a `compare_exchange`:
//! whichever stream gets there first sets the offset and the other reads it
//! back, rather than both computing against clocks read milliseconds apart.
//!
//! **A buffer with no PTS has to stop here.** `qtmux` cannot place an
//! untimestamped buffer in a track and answers with `GST_FLOW_ERROR`:
//!
//! ```text
//! ERROR Could not multiplex stream.
//!   gst_qt_mux_add_buffer (): .../GstMP4Mux:rec_p2:mux: Buffer has no PTS.
//! ```
//!
//! That error propagates out of the recorder, up through the mixer, and takes
//! the whole flow down — every seat, not just the one whose publisher produced
//! the buffer. Observed on a live rig: a second participant joining killed the
//! first. So an unstamped buffer is dropped here and counted, never forwarded.
//!
//! Dropping beats inventing a stamp. The main pipeline's running time *now* is
//! a different time base from `pts + offset` and the two drift apart over a
//! session, so a synthesised stamp can land before its own predecessor — and a
//! muxer rejects backwards timestamps as hard as missing ones, the same cascade
//! by another route. A stamp that is merely a little wrong is worse still: it
//! desynchronises audio from video for the rest of the session, silently. A
//! dropped frame on a live source is the ordinary recoverable failure — the
//! receiver already tolerates loss, and [`crate::gst::keyframe_request`]
//! recovers a decoder that lost part of its GOP.
//!
//! **What is not covered.** [`shift`] clamps a shifted timestamp at zero, so a
//! stream whose PTS base sits far behind the offset-setting stream's pins its
//! first buffers at 0 — equal timestamps, which is the muxer's third complaint
//! after missing and backwards ones. Pre-existing, and it needs a policy for
//! what a stream that starts *before* the session's time origin should be
//! worth; not decided here.

use gstreamer as gst;
use gstreamer_app as gst_app;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// `offset_ns` value meaning "no buffer carrying a PTS has arrived yet".
const OFFSET_UNSET: i64 = i64::MIN;

/// What the bridge did with one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// Pushed, with its PTS shifted into the main pipeline's time base.
    Restamped,
    /// As [`Forwarded::Restamped`], and this is the buffer the session's offset
    /// was computed from. Worth one log line per session.
    OffsetComputed(i64),
    /// Pushed unchanged: the main pipeline is gone or has no clock yet, so
    /// there is no running time to rebase onto. The next buffer tries again.
    Unadjusted,
    /// Not pushed: the buffer had no PTS. `dropped` is the running total for
    /// this session.
    DroppedUnstamped { dropped: u64 },
    /// The appsrc refused the sample — normally `Flushing`, while the main
    /// pipeline is still coming up. The media is lost; the next buffer tries
    /// again. If this buffer was also the one that established the session
    /// offset, the offset is kept even though the buffer did not cross.
    PushFailed(gst::FlowError),
}

/// Per-session bridge state, shared by the audio and video appsink callbacks.
#[derive(Debug)]
pub struct SessionBridge {
    offset_ns: AtomicI64,
    dropped_unstamped: AtomicU64,
}

impl Default for SessionBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionBridge {
    pub fn new() -> Self {
        Self {
            offset_ns: AtomicI64::new(OFFSET_UNSET),
            dropped_unstamped: AtomicU64::new(0),
        }
    }

    /// Buffers dropped so far for want of a PTS, across both streams.
    pub fn dropped_unstamped(&self) -> u64 {
        self.dropped_unstamped.load(Ordering::Relaxed)
    }

    /// Forget the session offset, so the next buffer with a PTS computes a new
    /// one. The Media Player calls this whenever the relationship between file
    /// time and main-pipeline running time breaks: a file switch, a seek, or a
    /// resume after a pause. The drop count is a lifetime total and survives.
    pub fn reset_offset(&self) {
        self.offset_ns.store(OFFSET_UNSET, Ordering::Relaxed);
    }

    /// Push one sample from the session pipeline into the main pipeline.
    ///
    /// Runs once per buffer on the appsink's streaming thread: atomics only, no
    /// locks, and no allocation beyond the buffer copy the restamp needs.
    ///
    /// `main_running_time` is consulted only while the offset is still unset —
    /// the caller uses it to upgrade its weak pipeline reference and read the
    /// clock, work not worth doing on every buffer. It may be called and its
    /// answer discarded, if the other stream set the offset first.
    ///
    /// Returns `None` if the sample carried no buffer; the caller decides what
    /// that means for the appsink.
    pub fn forward(
        &self,
        sample: &gst::Sample,
        appsrc: &gst_app::AppSrc,
        main_running_time: impl FnOnce() -> Option<gst::ClockTime>,
    ) -> Option<Forwarded> {
        let buffer = sample.buffer()?;

        // The whole point of this module: an unstamped buffer never crosses.
        let Some(pts) = buffer.pts() else {
            let dropped = self.dropped_unstamped.fetch_add(1, Ordering::Relaxed) + 1;
            return Some(Forwarded::DroppedUnstamped { dropped });
        };

        let mut offset_ns = self.offset_ns.load(Ordering::Relaxed);
        let mut just_computed = false;
        if offset_ns == OFFSET_UNSET {
            if let Some(running) = main_running_time() {
                let candidate = running.nseconds() as i64 - pts.nseconds() as i64;
                // Audio and video reach this on their own streaming threads and
                // can both have read OFFSET_UNSET above. Exactly one may win:
                // two offsets computed from clocks read milliseconds apart is
                // the A/V drift this shared offset exists to prevent.
                match self.offset_ns.compare_exchange(
                    OFFSET_UNSET,
                    candidate,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        offset_ns = candidate;
                        just_computed = true;
                    }
                    Err(winner) => offset_ns = winner,
                }
            }
        }

        if offset_ns == OFFSET_UNSET {
            return Some(match appsrc.push_sample(sample) {
                Ok(_) => Forwarded::Unadjusted,
                Err(e) => Forwarded::PushFailed(e),
            });
        }

        let mut adjusted = buffer.copy();
        {
            let Some(buf_ref) = adjusted.get_mut() else {
                // A fresh copy is always writable. If that ever stops being
                // true, forward the original rather than lose media.
                return Some(match appsrc.push_sample(sample) {
                    Ok(_) => Forwarded::Unadjusted,
                    Err(e) => Forwarded::PushFailed(e),
                });
            };
            buf_ref.set_pts(shift(pts, offset_ns));
            // DTS is in the session's time base too. It is unset on WHIP
            // traffic (WebRTC H.264 carries no B-frames) and set on a file the
            // Media Player passes through encoded. Leaving a session-base DTS
            // beside a main-base PTS is the muxer's other way to reject the
            // buffer.
            if let Some(dts) = buffer.dts() {
                buf_ref.set_dts(shift(dts, offset_ns));
            }
        }

        // `to_owned` on a caps ref is a refcount bump, not a deep copy.
        let caps = sample.caps().map(|c| c.to_owned());
        let mut builder = gst::Sample::builder().buffer(&adjusted);
        if let Some(caps) = &caps {
            builder = builder.caps(caps);
        }
        if let Err(e) = appsrc.push_sample(&builder.build()) {
            return Some(Forwarded::PushFailed(e));
        }

        Some(if just_computed {
            Forwarded::OffsetComputed(offset_ns)
        } else {
            Forwarded::Restamped
        })
    }
}

fn shift(ts: gst::ClockTime, offset_ns: i64) -> gst::ClockTime {
    let shifted = (ts.nseconds() as i64).saturating_add(offset_ns).max(0);
    gst::ClockTime::from_nseconds(shifted as u64)
}

/// Rate limit for the drop warning. A publisher that sends nothing but
/// unstamped buffers must stay visible without filling the log at frame rate.
pub fn should_log_drop(dropped: u64) -> bool {
    dropped == 1 || dropped.is_multiple_of(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer::prelude::*;

    const CAPS: &str = "application/x-strom-bridge-test";
    const SEC: u64 = gst::ClockTime::SECOND.nseconds();

    /// A real `appsrc ! appsink` pair standing in for the main pipeline's slot:
    /// what comes out of `sink` is exactly what the bridge let across.
    struct Harness {
        pipeline: gst::Pipeline,
        src: gst_app::AppSrc,
        sink: gst_app::AppSink,
    }

    impl Harness {
        fn new() -> Self {
            let _ = gst::init();
            let src = gst_app::AppSrc::builder()
                .format(gst::Format::Time)
                .caps(&gst::Caps::builder(CAPS).build())
                .build();
            let sink = gst_app::AppSink::builder().sync(false).build();
            let pipeline = gst::Pipeline::new();
            pipeline
                .add_many([src.upcast_ref::<gst::Element>(), sink.upcast_ref()])
                .expect("add");
            src.link(&sink).expect("link");
            pipeline.set_state(gst::State::Playing).expect("playing");
            Self {
                pipeline,
                src,
                sink,
            }
        }

        /// PTS of the next buffer to arrive, or `None` if none does.
        fn next_pts(&self) -> Option<Option<gst::ClockTime>> {
            self.sink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
                .map(|s| s.buffer().expect("sample has a buffer").pts())
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    fn sample(pts: Option<gst::ClockTime>, dts: Option<gst::ClockTime>) -> gst::Sample {
        let mut buffer = gst::Buffer::with_size(4).expect("buffer");
        {
            let b = buffer.get_mut().unwrap();
            b.set_pts(pts);
            b.set_dts(dts);
        }
        gst::Sample::builder()
            .buffer(&buffer)
            .caps(&gst::Caps::builder(CAPS).build())
            .build()
    }

    fn at(secs: u64) -> Option<gst::ClockTime> {
        Some(gst::ClockTime::from_seconds(secs))
    }

    /// The regression this module exists for. A buffer with no PTS must not
    /// reach the main pipeline: downstream of that appsrc sits a recorder whose
    /// muxer answers `Buffer has no PTS` with `GST_FLOW_ERROR`, which tears the
    /// whole flow down — every seat, not just this one.
    ///
    /// Revert the drop and the unstamped buffer arrives at `sink` between the
    /// other two, and this fails.
    #[test]
    fn an_unstamped_buffer_never_crosses_the_bridge() {
        let h = Harness::new();
        let bridge = SessionBridge::new();
        let now = || Some(gst::ClockTime::from_seconds(10));

        assert_eq!(
            bridge.forward(&sample(at(1), None), &h.src, now),
            Some(Forwarded::OffsetComputed(9 * SEC as i64))
        );
        assert_eq!(
            bridge.forward(&sample(None, None), &h.src, now),
            Some(Forwarded::DroppedUnstamped { dropped: 1 })
        );
        assert_eq!(
            bridge.forward(&sample(at(3), None), &h.src, now),
            Some(Forwarded::Restamped)
        );

        assert_eq!(h.next_pts(), Some(at(10)), "first buffer, rebased");
        assert_eq!(
            h.next_pts(),
            Some(at(12)),
            "the unstamped buffer must not be here — the third one is next"
        );
        assert_eq!(h.next_pts(), None, "nothing else should have crossed");
        assert_eq!(bridge.dropped_unstamped(), 1);
    }

    /// The other way in: if the *first* buffer of a session has no PTS there is
    /// no offset yet, so the restamping path is not even reached. It still must
    /// not be forwarded, and it must not poison the offset for the buffer that
    /// does carry one.
    #[test]
    fn an_unstamped_first_buffer_is_dropped_and_does_not_fix_the_offset() {
        let h = Harness::new();
        let bridge = SessionBridge::new();

        assert_eq!(
            bridge.forward(&sample(None, None), &h.src, || panic!(
                "the clock must not be read for a buffer with no PTS"
            )),
            Some(Forwarded::DroppedUnstamped { dropped: 1 })
        );
        assert_eq!(
            bridge.forward(&sample(at(2), None), &h.src, || Some(
                gst::ClockTime::from_seconds(5)
            )),
            Some(Forwarded::OffsetComputed(3 * SEC as i64)),
            "the first buffer with a PTS establishes the offset"
        );

        assert_eq!(h.next_pts(), Some(at(5)));
        assert_eq!(h.next_pts(), None);
    }

    /// With no running time to rebase onto — the main pipeline is being torn
    /// down, or has not been given a clock yet — a stamped buffer still goes
    /// through unchanged, and the next one retries the offset.
    #[test]
    fn a_missing_running_time_forwards_unadjusted_and_retries() {
        let h = Harness::new();
        let bridge = SessionBridge::new();

        assert_eq!(
            bridge.forward(&sample(at(1), None), &h.src, || None),
            Some(Forwarded::Unadjusted)
        );
        assert_eq!(
            bridge.forward(&sample(at(2), None), &h.src, || Some(
                gst::ClockTime::from_seconds(20)
            )),
            Some(Forwarded::OffsetComputed(18 * SEC as i64))
        );

        assert_eq!(h.next_pts(), Some(at(1)), "pushed as it came");
        assert_eq!(h.next_pts(), Some(at(20)));
    }

    /// DTS is in the session's time base too. Leaving it unshifted beside a
    /// rebased PTS is the muxer's other way to reject the buffer.
    #[test]
    fn dts_is_shifted_with_pts() {
        let h = Harness::new();
        let bridge = SessionBridge::new();
        let now = || Some(gst::ClockTime::from_seconds(10));

        bridge.forward(&sample(at(2), at(1)), &h.src, now);
        let out = h
            .sink
            .try_pull_sample(gst::ClockTime::from_mseconds(500))
            .expect("sample");
        let buffer = out.buffer().unwrap();
        assert_eq!(buffer.pts(), at(10));
        assert_eq!(buffer.dts(), at(9));
    }

    /// The offset is shared so audio and video keep their relative timing. Both
    /// streams push through one `SessionBridge`; the second must reuse the
    /// offset the first computed rather than recomputing against a later clock.
    ///
    /// One thread, so this covers only the case where one stream's first buffer
    /// clearly precedes the other's. The concurrent case is
    /// [`two_streams_racing_for_the_offset_still_get_one`].
    #[test]
    fn both_streams_share_one_offset() {
        let h = Harness::new();
        let bridge = SessionBridge::new();

        bridge.forward(&sample(at(1), None), &h.src, || {
            Some(gst::ClockTime::from_seconds(10))
        });
        assert_eq!(
            bridge.forward(&sample(at(1), None), &h.src, || panic!(
                "the offset is computed once per session, not once per stream"
            )),
            Some(Forwarded::Restamped)
        );

        assert_eq!(h.next_pts(), Some(at(10)));
        assert_eq!(
            h.next_pts(),
            Some(at(10)),
            "same input time, same output time"
        );
    }

    /// Audio and video run on their own streaming threads, so both can find the
    /// offset unset and read the clock before either has stored anything. The
    /// barrier inside the clock closure forces that interleaving instead of
    /// hoping for it: both threads are past the `OFFSET_UNSET` load and inside
    /// `main_running_time` before either returns.
    ///
    /// With a plain store both callers report `OffsetComputed` and the two
    /// buffers leave shifted by clocks read milliseconds apart — the A/V drift
    /// the shared offset exists to prevent. With the `compare_exchange` exactly
    /// one wins and both buffers get the winner's offset.
    #[test]
    fn two_streams_racing_for_the_offset_still_get_one() {
        use std::sync::{Arc, Barrier};

        let h = Harness::new();
        let bridge = Arc::new(SessionBridge::new());
        let gate = Arc::new(Barrier::new(2));

        // Same input PTS on both streams, different clock readings: if the two
        // computed their own offsets, the outputs would differ by a second.
        let outcomes: Vec<_> = [10u64, 11u64]
            .into_iter()
            .map(|clock_secs| {
                let bridge = Arc::clone(&bridge);
                let gate = Arc::clone(&gate);
                let src = h.src.clone();
                std::thread::spawn(move || {
                    bridge.forward(&sample(at(1), None), &src, || {
                        gate.wait();
                        Some(gst::ClockTime::from_seconds(clock_secs))
                    })
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .collect();

        let computed: Vec<_> = outcomes
            .iter()
            .filter_map(|o| match o {
                Some(Forwarded::OffsetComputed(offset)) => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(
            computed.len(),
            1,
            "exactly one stream may establish the offset, got {:?}",
            outcomes
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, Some(Forwarded::Restamped)))
                .count(),
            1,
            "the loser reuses the winner's offset, got {:?}",
            outcomes
        );

        let winner = gst::ClockTime::from_nseconds(computed[0] as u64 + SEC);
        assert_eq!(h.next_pts(), Some(Some(winner)));
        assert_eq!(
            h.next_pts(),
            Some(Some(winner)),
            "both buffers shifted by the same offset"
        );
    }

    /// A shifted PTS cannot go negative: `ClockTime` is unsigned and
    /// `ClockTime::NONE` is what we are here to avoid producing.
    #[test]
    fn a_negative_shift_clamps_to_zero() {
        let h = Harness::new();
        let bridge = SessionBridge::new();

        bridge.forward(&sample(at(100), None), &h.src, || {
            Some(gst::ClockTime::from_seconds(1))
        });
        bridge.forward(&sample(at(1), None), &h.src, || unreachable!());

        assert_eq!(h.next_pts(), Some(at(1)));
        assert_eq!(h.next_pts(), Some(at(0)));
    }

    /// A publisher sending nothing but unstamped buffers must stay visible in
    /// the log without writing a line per frame.
    #[test]
    fn drops_are_logged_first_then_sparsely() {
        assert!(should_log_drop(1));
        assert!(!should_log_drop(2));
        assert!(!should_log_drop(99));
        assert!(should_log_drop(100));
        assert!(!should_log_drop(101));
        assert!(should_log_drop(1_000));
    }
}
