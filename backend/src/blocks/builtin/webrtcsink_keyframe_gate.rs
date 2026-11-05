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
//! as delta but that are no keyframe either, and starting a viewer on one
//! breaks its decoder for good. While it waits, the gate asks again every
//! [`REREQUEST_INTERVAL`], so a request that got lost on the way (an encoder
//! busy with another one, a parser that swallowed it) does not leave the
//! viewer waiting for the next scheduled keyframe.
//!
//! Only video payloaders are gated: an audio frame has no keyframe to wait
//! for.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use tracing::debug;

/// The consumer id webrtcsink passes to its signals for a discovery
/// pipeline, which has no viewer.
pub(crate) const DISCOVERY_CONSUMER: &str = "discovery";

/// How often the gate repeats its keyframe request while it waits.
const REREQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// Gate every viewer's video payloader of `webrtcsink` (`whepserversink`,
/// `whipclientsink`).
pub(crate) fn install(webrtcsink: &gst::Element) {
    webrtcsink.connect("payloader-setup", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        if let Ok(payloader) = values[3].get::<gst::Element>() {
            gate_payloader(&consumer_id, &payloader);
        }
        // Let the other handlers and webrtcsink's defaults run.
        Some(false.to_value())
    });
}

/// Whether `payloader` packs video, from its sink pad template.
pub(crate) fn is_video_payloader(payloader: &gst::Element) -> bool {
    payloader.static_pad("sink").is_some_and(|pad| {
        pad.pad_template_caps()
            .iter()
            .any(|s| s.name().starts_with("video/"))
    })
}

/// Gate `payloader` when it packs video for a viewer. `true` when gated.
fn gate_payloader(consumer_id: &str, payloader: &gst::Element) -> bool {
    // Discovery pipelines have no viewer to protect, and audio has no
    // keyframes to wait for.
    if consumer_id == DISCOVERY_CONSUMER || !is_video_payloader(payloader) {
        return false;
    }
    let Some(sink) = payloader.static_pad("sink") else {
        return false;
    };
    debug!(
        "WebRTC viewer {}: keyframe gate on {}",
        consumer_id,
        payloader.name()
    );
    gate(&sink);
    true
}

/// Drop delta frames on `pad` from a DISCONT delta frame up to the next
/// keyframe, and request that keyframe upstream.
///
/// BUFFER probe, so it fires per frame: it reads the buffer flags and one
/// atomic. Only the drop path reads the clock (`Instant`), and only a
/// request builds and sends an event.
pub(crate) fn gate(pad: &gst::Pad) {
    let state = Arc::new(GateState::new());
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        match decide(buffer.flags(), &state, || state.elapsed_ns()) {
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
struct GateState {
    /// Clock origin for [`Self::last_request_ns`].
    epoch: Instant,
    /// Dropping until a frame the viewer can start from.
    waiting: AtomicBool,
    /// The stream's keyframes carry their codec headers (SPS/PPS, AV1
    /// sequence header), so a non-delta frame without them is no place to
    /// start from.
    keyframes_have_headers: AtomicBool,
    /// When the last keyframe request went out, in ns since [`Self::epoch`].
    last_request_ns: AtomicU64,
}

impl GateState {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            waiting: AtomicBool::new(false),
            keyframes_have_headers: AtomicBool::new(false),
            last_request_ns: AtomicU64::new(0),
        }
    }

    fn elapsed_ns(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// What to do with a frame with `flags`. `now_ns` reads the clock, in ns
/// since the state's epoch; it is called only while the gate waits.
fn decide(flags: gst::BufferFlags, state: &GateState, now_ns: impl FnOnce() -> u64) -> Gate {
    let delta = flags.contains(gst::BufferFlags::DELTA_UNIT);
    let header = flags.contains(gst::BufferFlags::HEADER);
    if !delta && header {
        state.keyframes_have_headers.store(true, Ordering::Relaxed);
    }
    // A frame the decoder can start from.
    let restart_point = !delta && (header || !state.keyframes_have_headers.load(Ordering::Relaxed));
    if restart_point {
        state.waiting.store(false, Ordering::Relaxed);
        return Gate::Pass;
    }
    let waiting = state.waiting.load(Ordering::Relaxed);
    if !waiting && !flags.contains(gst::BufferFlags::DISCONT) {
        return Gate::Pass;
    }
    let now = now_ns();
    if !waiting {
        state.waiting.store(true, Ordering::Relaxed);
        state.last_request_ns.store(now, Ordering::Relaxed);
        return Gate::DropAndRequest;
    }
    let last = state.last_request_ns.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= REREQUEST_INTERVAL.as_nanos() as u64 {
        state.last_request_ns.store(now, Ordering::Relaxed);
        Gate::DropAndRequest
    } else {
        Gate::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst::BufferFlags as F;

    const MS: u64 = 1_000_000;

    /// `decide` at a fixed time.
    fn at(flags: F, state: &GateState, ms: u64) -> Gate {
        decide(flags, state, || ms * MS)
    }

    #[test]
    fn delta_frames_after_a_drop_wait_for_a_keyframe() {
        let state = GateState::new();
        assert_eq!(at(F::DISCONT, &state, 0), Gate::Pass); // first keyframe
        assert_eq!(at(F::DELTA_UNIT, &state, 0), Gate::Pass);
        // appsrc dropped frames here.
        assert_eq!(
            at(F::DISCONT | F::DELTA_UNIT, &state, 10),
            Gate::DropAndRequest
        );
        assert_eq!(at(F::DELTA_UNIT, &state, 50), Gate::Drop);
        // Another drop soon after does not ask again.
        assert_eq!(at(F::DISCONT | F::DELTA_UNIT, &state, 100), Gate::Drop);
        assert_eq!(at(F::empty(), &state, 120), Gate::Pass); // keyframe
        assert_eq!(at(F::DELTA_UNIT, &state, 140), Gate::Pass);
    }

    /// svtav1enc via av1parse: keyframes carry the sequence header (HEADER),
    /// and some frames are neither delta nor keyframe. After a drop, such a
    /// frame is no place to start.
    #[test]
    fn a_headerless_non_delta_frame_is_no_restart_point() {
        let state = GateState::new();
        assert_eq!(at(F::DISCONT | F::HEADER, &state, 0), Gate::Pass);
        assert_eq!(at(F::DISCONT, &state, 10), Gate::DropAndRequest);
        assert_eq!(at(F::DELTA_UNIT, &state, 20), Gate::Drop);
        assert_eq!(at(F::HEADER, &state, 30), Gate::Pass);
        // In the stream, without a drop, it passes.
        assert_eq!(at(F::empty(), &state, 40), Gate::Pass);
        assert_eq!(at(F::DELTA_UNIT, &state, 50), Gate::Pass);
    }

    /// However many headerless frames arrive, none starts the viewer: the
    /// gate keeps waiting and repeats its request every
    /// [`REREQUEST_INTERVAL`] until a keyframe with headers comes.
    #[test]
    fn headerless_frames_never_start_a_viewer_and_the_request_repeats() {
        let state = GateState::new();
        assert_eq!(at(F::HEADER, &state, 0), Gate::Pass);
        assert_eq!(
            at(F::DISCONT | F::DELTA_UNIT, &state, 1000),
            Gate::DropAndRequest
        );
        let mut requests = 1;
        for ms in (1040..3000).step_by(40) {
            match at(F::empty(), &state, ms) {
                Gate::Drop => {}
                Gate::DropAndRequest => requests += 1,
                Gate::Pass => panic!("a headerless frame at {ms} ms started the viewer"),
            }
        }
        // 1000, 1520, 2040, 2560 ms: every 500 ms or so, not on every frame.
        assert_eq!(requests, 4);
        assert_eq!(at(F::HEADER, &state, 3000), Gate::Pass);
        assert_eq!(at(F::DELTA_UNIT, &state, 3040), Gate::Pass);
    }

    /// Push `frames` (index, flags) through an appsrc into a sink whose pad
    /// is gated; the indexes that got through and the keyframe requests.
    /// A frame with `Some(pause)` is pushed after sleeping that long.
    fn run_gated(frames: &[(u64, F, Option<Duration>)]) -> (Vec<u64>, u32) {
        use std::sync::atomic::AtomicU32;
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

        let requests = Arc::new(AtomicU32::new(0));
        let seen = requests.clone();
        src.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
                    if gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok() {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        pipeline.set_state(gst::State::Playing).unwrap();
        let mut got = Vec::new();
        for (n, flags, pause) in frames {
            if let Some(pause) = pause {
                std::thread::sleep(*pause);
            }
            let mut buffer = gst::Buffer::with_size(4).unwrap();
            {
                let b = buffer.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_mseconds(40 * n));
                b.set_flags(*flags);
            }
            src.push_buffer(buffer).unwrap();
        }
        src.end_of_stream().unwrap();
        while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(5)) {
            got.push(sample.buffer().unwrap().pts().unwrap().mseconds() / 40);
        }
        pipeline.set_state(gst::State::Null).unwrap();
        (got, requests.load(Ordering::SeqCst))
    }

    /// A pipeline that drops frames the way webrtcsink's appsrc does: the
    /// frames after the drop do not reach the payloader until a keyframe
    /// does, and a keyframe is requested upstream.
    #[test]
    fn gate_drops_to_the_next_keyframe_and_requests_one() {
        let (got, requests) = run_gated(&[
            (0, F::empty(), None),
            (1, F::DELTA_UNIT, None),
            (5, F::DISCONT | F::DELTA_UNIT, None),
            (6, F::DELTA_UNIT, None),
            (7, F::empty(), None),
            (8, F::DELTA_UNIT, None),
        ]);
        assert_eq!(got, vec![0, 1, 7, 8]);
        assert_eq!(requests, 1, "one keyframe request");
    }

    /// In a stream whose keyframes carry headers, headerless frames after a
    /// gap never reach the viewer, the request is repeated while the gate
    /// waits, and the next keyframe with headers starts the viewer.
    #[test]
    fn gate_never_passes_headerless_frames_after_a_gap() {
        let pause = Some(REREQUEST_INTERVAL + Duration::from_millis(100));
        let (got, requests) = run_gated(&[
            (0, F::HEADER, None),
            (1, F::DELTA_UNIT, None),
            (5, F::DISCONT | F::DELTA_UNIT, None),
            (6, F::empty(), None),
            (7, F::DELTA_UNIT, None),
            (8, F::empty(), pause),
            (9, F::empty(), None),
            (10, F::HEADER, None),
            (11, F::DELTA_UNIT, None),
        ]);
        assert_eq!(got, vec![0, 1, 10, 11]);
        assert_eq!(requests, 2, "the request is repeated after the interval");
    }

    /// Feed three frames flagged as after a drop (DISCONT, delta, HEADER)
    /// into `payloader` gated for `consumer_id`; how many reach the
    /// payloader past the gate.
    fn frames_past_gate(consumer_id: &str, payloader: &str, caps: &str) -> usize {
        use std::sync::atomic::AtomicUsize;
        gst::init().unwrap();
        let pay = gst::ElementFactory::make(payloader).build().unwrap();
        let gated = gate_payloader(consumer_id, &pay);
        // A probe added after the gate's sees only what the gate passed.
        let passed = Arc::new(AtomicUsize::new(0));
        let counter = passed.clone();
        pay.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            });
        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .caps(&caps.parse::<gst::Caps>().unwrap())
            .format(gst::Format::Time)
            .build();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline
            .add_many([src.upcast_ref::<gst::Element>(), &pay, &sink])
            .unwrap();
        gst::Element::link_many([src.upcast_ref::<gst::Element>(), &pay, &sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        for (n, flags) in [
            F::DISCONT | F::DELTA_UNIT,
            F::DELTA_UNIT,
            F::DELTA_UNIT | F::HEADER,
        ]
        .into_iter()
        .enumerate()
        {
            let mut buffer = gst::Buffer::from_slice([0u8, 0, 0, 1, 0x41, 0x9a, 0, 0]);
            {
                let b = buffer.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_mseconds(20 * n as u64));
                b.set_flags(flags);
            }
            let _ = src.push_buffer(buffer);
        }
        let _ = src.end_of_stream();
        let _ = pipeline.bus().unwrap().timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(
            gated,
            consumer_id != DISCOVERY_CONSUMER && caps.starts_with("video/"),
            "{payloader} for {consumer_id}"
        );
        passed.load(Ordering::SeqCst)
    }

    /// Audio frames flagged DISCONT, delta or HEADER pass untouched; video
    /// frames to a viewer are gated; a discovery pipeline is not.
    #[test]
    fn only_video_payloaders_of_viewers_are_gated() {
        let opus = "audio/x-opus, channel-mapping-family=(int)0, channels=(int)2, rate=(int)48000";
        assert_eq!(frames_past_gate("viewer", "rtpopuspay", opus), 3);
        let h264 = "video/x-h264, stream-format=(string)byte-stream, alignment=(string)au";
        assert_eq!(frames_past_gate("viewer", "rtph264pay", h264), 0);
        assert_eq!(frames_past_gate(DISCOVERY_CONSUMER, "rtph264pay", h264), 3);
    }
}
