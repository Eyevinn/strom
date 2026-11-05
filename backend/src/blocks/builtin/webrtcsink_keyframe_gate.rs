//! Keeps a viewer's video decodable when webrtcsink drops frames before its
//! payloader.
//!
//! webrtcsink feeds each viewer from a leaky `appsrc` (StreamProducer's
//! consumer, 500 ms deep). Until the viewer's ICE and DTLS are up, webrtcbin
//! does not take data, the queue fills and the oldest frames are dropped. The
//! viewer then gets the first keyframe, a frame or two, and after the
//! handshake frames that refer to frames it never got. The payloader numbers
//! the packets without a gap, so the browser cannot see that anything is
//! missing and decodes the broken frames:
//! - Chrome's H.265 decoder (VideoToolbox on macOS) fails, falls back to a
//!   null decoder that has no software fallback for H.265, and the picture
//!   never comes back. Chrome sends no PLI for it.
//! - AV1 shows a frame and freezes until a keyframe.
//! - H.264 and VP9 decoders conceal the damage and ask for a keyframe.
//!
//! `appsrc` marks the first buffer after a drop DISCONT. This gate drops a
//! DISCONT delta frame and every delta frame after it until the next keyframe,
//! and asks upstream for one, so a viewer only gets frames it can decode. In
//! a stream whose keyframes carry their codec headers, a frame without them
//! does not end the wait: svtav1enc emits frames that av1parse does not mark
//! as delta but that are no keyframe either.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use tracing::debug;

/// Gate every viewer's payloader of `webrtcsink` (`whepserversink`,
/// `whipclientsink`).
pub(crate) fn install(webrtcsink: &gst::Element) {
    webrtcsink.connect("payloader-setup", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let payloader = values[3].get::<gst::Element>().unwrap();
        // Discovery pipelines have no viewer to protect.
        if super::webrtcsink_pt_restore::session_webrtcbin(&payloader).is_some() {
            if let Some(sink) = payloader.static_pad("sink") {
                debug!(
                    "WebRTC viewer {}: keyframe gate on {}",
                    consumer_id,
                    payloader.name()
                );
                gate(&sink);
            }
        }
        // Let the other handlers and webrtcsink's defaults run.
        Some(false.to_value())
    });
}

/// Drop delta frames on `pad` from a DISCONT delta frame up to the next
/// keyframe, and request that keyframe upstream.
///
/// BUFFER probe, so it fires per frame: it reads the buffer flags and one
/// atomic. Only the rare drop path builds and sends an event.
pub(crate) fn gate(pad: &gst::Pad) {
    let state = Arc::new(GateState::default());
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        match decide(buffer.flags(), &state) {
            Gate::Pass => gst::PadProbeReturn::Ok,
            Gate::Drop => gst::PadProbeReturn::Drop,
            Gate::DropAndRequest => {
                pad.push_event(
                    gst_video::UpstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
                gst::PadProbeReturn::Drop
            }
        }
    });
}

#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Pass,
    Drop,
    DropAndRequest,
}

/// Per-viewer gate state, atomics only: the probe runs per frame.
#[derive(Default)]
struct GateState {
    /// Dropping until a frame the viewer can start from.
    waiting: AtomicBool,
    /// The stream's keyframes carry their codec headers (SPS/PPS, AV1
    /// sequence header), so a non-delta frame without them is no place to
    /// start from.
    keyframes_have_headers: AtomicBool,
    /// Non-delta frames without headers skipped while waiting.
    headerless_skipped: AtomicU32,
}

/// Non-delta frames without headers to skip while waiting before taking one
/// anyway, in case the encoder stopped repeating its headers.
const MAX_HEADERLESS_SKIPS: u32 = 2;

fn decide(flags: gst::BufferFlags, state: &GateState) -> Gate {
    let delta = flags.contains(gst::BufferFlags::DELTA_UNIT);
    let header = flags.contains(gst::BufferFlags::HEADER);
    if !delta && header {
        state.keyframes_have_headers.store(true, Ordering::Relaxed);
    }
    // A frame the decoder can start from. svtav1enc emits frames that
    // av1parse leaves unmarked as delta but that are no keyframe (no sequence
    // header); starting from one breaks the viewer's decoder for good.
    let waiting = state.waiting.load(Ordering::Relaxed);
    let restart_point = !delta
        && (header
            || !state.keyframes_have_headers.load(Ordering::Relaxed)
            || (waiting
                && state.headerless_skipped.fetch_add(1, Ordering::Relaxed)
                    >= MAX_HEADERLESS_SKIPS));
    if restart_point {
        state.waiting.store(false, Ordering::Relaxed);
        state.headerless_skipped.store(0, Ordering::Relaxed);
        return Gate::Pass;
    }
    if flags.contains(gst::BufferFlags::DISCONT) {
        return if state.waiting.swap(true, Ordering::Relaxed) {
            Gate::Drop
        } else {
            Gate::DropAndRequest
        };
    }
    if waiting {
        Gate::Drop
    } else {
        Gate::Pass
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::BufferFlags as F;

    #[test]
    fn delta_frames_after_a_drop_wait_for_a_keyframe() {
        let state = GateState::default();
        assert_eq!(decide(F::DISCONT, &state), Gate::Pass); // first keyframe
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Pass);
        // appsrc dropped frames here.
        assert_eq!(
            decide(F::DISCONT | F::DELTA_UNIT, &state),
            Gate::DropAndRequest
        );
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Drop);
        // Another drop while waiting does not ask again.
        assert_eq!(decide(F::DISCONT | F::DELTA_UNIT, &state), Gate::Drop);
        assert_eq!(decide(F::empty(), &state), Gate::Pass); // keyframe
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Pass);
    }

    /// svtav1enc via av1parse: keyframes carry the sequence header (HEADER),
    /// and some frames are neither delta nor keyframe. After a drop, such a
    /// frame is no place to start.
    #[test]
    fn a_headerless_non_delta_frame_is_no_restart_point() {
        let state = GateState::default();
        assert_eq!(decide(F::DISCONT | F::HEADER, &state), Gate::Pass);
        assert_eq!(decide(F::DISCONT, &state), Gate::DropAndRequest);
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Drop);
        assert_eq!(decide(F::HEADER, &state), Gate::Pass);
        // In the stream, without a drop, it passes.
        assert_eq!(decide(F::empty(), &state), Gate::Pass);
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Pass);
    }

    /// An encoder that stops repeating its headers does not stall the viewer:
    /// after a few headerless keyframes the gate takes one.
    #[test]
    fn headerless_keyframes_are_taken_after_a_few() {
        let state = GateState::default();
        assert_eq!(decide(F::HEADER, &state), Gate::Pass);
        assert_eq!(
            decide(F::DISCONT | F::DELTA_UNIT, &state),
            Gate::DropAndRequest
        );
        for _ in 0..MAX_HEADERLESS_SKIPS {
            assert_eq!(decide(F::empty(), &state), Gate::Drop);
        }
        assert_eq!(decide(F::empty(), &state), Gate::Pass);
        assert_eq!(decide(F::DELTA_UNIT, &state), Gate::Pass);
    }

    /// A pipeline that drops frames the way webrtcsink's appsrc does: the
    /// frames after the drop do not reach the payloader until a keyframe
    /// does, and a keyframe is requested upstream.
    #[test]
    fn gate_drops_to_the_next_keyframe_and_requests_one() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple("video/x-h265"))
            .format(gst::Format::Time)
            .build();
        let sink = gstreamer_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([src.upcast_ref::<gst::Element>(), sink.upcast_ref()])
            .unwrap();
        src.link(&sink).unwrap();
        gate(&sink.static_pad("sink").unwrap());

        let requested = Arc::new(AtomicBool::new(false));
        let seen = requested.clone();
        src.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
                    if gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok() {
                        seen.store(true, Ordering::SeqCst);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        pipeline.set_state(gst::State::Playing).unwrap();
        let frames: [(u64, F); 6] = [
            (0, F::empty()),
            (1, F::DELTA_UNIT),
            (5, F::DISCONT | F::DELTA_UNIT),
            (6, F::DELTA_UNIT),
            (7, F::empty()),
            (8, F::DELTA_UNIT),
        ];
        for (n, flags) in frames {
            let mut buffer = gst::Buffer::with_size(4).unwrap();
            {
                let b = buffer.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_mseconds(40 * n));
                b.set_flags(flags);
            }
            src.push_buffer(buffer).unwrap();
        }
        src.end_of_stream().unwrap();
        let mut got = Vec::new();
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            got.push(sample.buffer().unwrap().pts().unwrap().mseconds() / 40);
        }
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(got, vec![0, 1, 7, 8]);
        assert!(requested.load(Ordering::SeqCst), "no keyframe requested");
    }
}
