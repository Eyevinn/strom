//! Consumer-side adaptation of a WHEP Output audio input.
//!
//! `whepserversink` takes any raw audio on its audio request pads, so what it
//! receives is whatever upstream happens to negotiate. Upstream is often an
//! audio mixer whose output format is not fixed: it can settle on one format
//! and switch to another a moment later, once the sink's chain has linked
//! and asked for a renegotiation. In practice a flow started while an SRT
//! input is already receiving negotiates `F32LE` at 44.1 kHz, then switches to
//! `S32LE` within about 100 ms.
//!
//! `webrtcsink` handles that badly. Its first audio buffer starts a codec
//! discovery for the caps it has, the new caps start a second one, and it
//! feeds every buffer to both. Around the switch the timestamps step back a
//! little, and the Opus encoder in one of the discovery pipelines fails with
//! "buffer going too far back in time". A failed discovery is fatal: the sink
//! posts "There is no codec present that can handle the stream's type", and
//! the flow does not reach Playing.
//!
//! The bridge pins raw audio to one format before it reaches the sink, so a
//! renegotiation upstream is absorbed by the converters and the sink sees a
//! single set of caps for the life of the pad. Encoded audio (Opus) passes
//! through untouched, as the block contract asks of output blocks. Which of
//! the two arrives is only known from the negotiated caps, so the decision is
//! made on the first CAPS event, by splicing `audioconvert ! audioresample !
//! capsfilter` between the watched pad and its peer.
//!
//! The pinned format is what WebRTC carries anyway: Opus at 48 kHz, two
//! channels (RFC 7587). Converting once here also leaves each consumer's own
//! converters in passthrough instead of resampling once per viewer.
//!
//! Relinking from inside a push-event probe is the same pattern
//! [`super::video_input_bridge`] uses, and its notes apply: the CAPS event
//! that triggered the splice lands on the newly inserted elements, and no
//! buffer is in flight across the link while the probe runs.

use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// The format raw audio is pinned to before it reaches the sink.
pub const PINNED_RAW_AUDIO: &str =
    "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";

#[derive(Default)]
enum Mode {
    /// No caps seen yet.
    #[default]
    Undecided,
    /// Raw audio: the converters are spliced in.
    Pinned,
    /// Encoded audio: linked straight to the sink.
    Passthrough,
}

/// Watch `src_pad` and, when raw audio arrives on it, pin the format before
/// it reaches the pad's peer. `name_prefix` names the spliced elements.
pub fn install_audio_input_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();
    // Taken per CAPS event only; never on the buffer path.
    let mode = Arc::new(Mutex::new(Mode::default()));

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps();
        let is_raw = caps
            .structure(0)
            .is_some_and(|s| s.name().as_str() == "audio/x-raw");
        let mut mode = mode.lock().unwrap_or_else(|p| p.into_inner());
        match (&*mode, is_raw) {
            (Mode::Undecided, true) => match splice(pad, &name_prefix) {
                Ok(()) => {
                    info!(
                        "{}: raw audio ({}) pinned to {} for the WHEP sink",
                        name_prefix, caps, PINNED_RAW_AUDIO
                    );
                    *mode = Mode::Pinned;
                }
                Err(reason) => post_error(
                    pad,
                    &format!("{name_prefix}: could not pin raw audio ({caps}): {reason}"),
                ),
            },
            (Mode::Undecided, false) => *mode = Mode::Passthrough,
            // The converters take any raw format, and the sink sees no change.
            (Mode::Pinned, true) | (Mode::Passthrough, false) => {}
            // webrtcsink cannot switch a running pad between raw and encoded
            // audio either, so say so instead of failing not-negotiated.
            (Mode::Pinned, false) | (Mode::Passthrough, true) => post_error(
                pad,
                &format!(
                    "{name_prefix}: audio switched between raw and encoded mid-stream ({caps}); \
                     a WHEP output keeps the kind of audio it started with"
                ),
            ),
        }
        gst::PadProbeReturn::Ok
    });
}

/// Put `audioconvert ! audioresample ! capsfilter` between `pad` and its peer.
fn splice(pad: &gst::Pad, name_prefix: &str) -> Result<(), String> {
    let peer = pad.peer().ok_or("the pad is not linked")?;
    let bin = pad
        .parent_element()
        .and_then(|e| e.parent())
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("the pad's element is not in a bin")?;
    let caps: gst::Caps = PINNED_RAW_AUDIO
        .parse()
        .map_err(|e| format!("pinned caps: {e}"))?;
    let make = |factory: &str, suffix: &str| {
        gst::ElementFactory::make(factory)
            .name(format!("{name_prefix}_pin_{suffix}"))
            .build()
            .map_err(|e| format!("{factory}: {e}"))
    };
    let convert = make("audioconvert", "convert")?;
    let resample = make("audioresample", "resample")?;
    let filter = make("capsfilter", "caps")?;
    filter.set_property("caps", &caps);

    let chain = [&convert, &resample, &filter];
    bin.add_many(chain).map_err(|e| format!("add: {e}"))?;
    gst::Element::link_many(chain).map_err(|e| format!("link chain: {e}"))?;
    let sink = convert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let src = filter
        .static_pad("src")
        .ok_or("capsfilter has no src pad")?;

    pad.unlink(&peer).map_err(|e| format!("unlink: {e}"))?;
    let relinked = src
        .link(&peer)
        .map_err(|e| format!("link to the sink: {e:?}"))
        .and_then(|_| {
            pad.link(&sink)
                .map(|_| ())
                .map_err(|e| format!("link from the queue: {e:?}"))
        });
    if let Err(reason) = relinked {
        // Leave the original link in place rather than a broken one.
        let _ = src.unlink(&peer);
        let _ = pad.link(&peer);
        let _ = bin.remove_many(chain);
        return Err(reason);
    }
    for element in chain {
        element
            .sync_state_with_parent()
            .map_err(|e| format!("sync state: {e}"))?;
    }
    Ok(())
}

fn post_error(pad: &gst::Pad, message: &str) {
    warn!("{}", message);
    if let Some(element) = pad.parent_element() {
        gst::element_error!(element, gst::StreamError::Format, ("{}", message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Runs `audiotestsrc ! capsfilter(first) ! queue ! fakesink`, switches the
    /// source caps to `second` mid-stream, and returns every caps the sink saw.
    fn caps_seen_by_sink(first: &str, second: &str, bridge: bool) -> Vec<gst::Caps> {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let filter = gst::ElementFactory::make("capsfilter")
            .property("caps", first.parse::<gst::Caps>().unwrap())
            .build()
            .unwrap();
        let queue = gst::ElementFactory::make("queue")
            .name("q")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &filter, &queue, &sink]).unwrap();
        gst::Element::link_many([&src, &filter, &queue, &sink]).unwrap();
        if bridge {
            install_audio_input_bridge(&queue.static_pad("src").unwrap(), "q");
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_in_probe = Arc::clone(&seen);
        sink.static_pad("sink").unwrap().add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if let gst::EventView::Caps(c) = event.view() {
                        seen_in_probe.lock().unwrap().push(c.caps_owned());
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        filter.set_property("caps", second.parse::<gst::Caps>().unwrap());
        std::thread::sleep(Duration::from_millis(300));
        let bus = pipeline.bus().unwrap();
        let error = bus.pop_filtered(&[gst::MessageType::Error]);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(error.is_none(), "pipeline error: {error:?}");
        let seen = seen.lock().unwrap().clone();
        seen
    }

    const F32_44K: &str = "audio/x-raw,format=F32LE,rate=44100,channels=2,layout=interleaved";
    const S32_44K: &str = "audio/x-raw,format=S32LE,rate=44100,channels=2,layout=interleaved";

    /// An upstream format switch must not reach the sink: it sees the pinned
    /// caps once. Without the bridge it sees both formats, which is what makes
    /// webrtcsink's codec discovery fail.
    #[test]
    fn upstream_format_switch_does_not_reach_the_sink() {
        let unbridged = caps_seen_by_sink(F32_44K, S32_44K, false);
        assert!(
            unbridged.len() >= 2,
            "the test must switch formats upstream; the sink saw {unbridged:?}"
        );

        let seen = caps_seen_by_sink(F32_44K, S32_44K, true);
        assert_eq!(seen.len(), 1, "the sink saw more than one caps: {seen:?}");
        let s = seen[0].structure(0).unwrap();
        assert_eq!(s.get::<&str>("format").ok(), Some("S16LE"), "{s}");
        assert_eq!(s.get::<i32>("rate").ok(), Some(48000), "{s}");
        assert_eq!(s.get::<i32>("channels").ok(), Some(2), "{s}");
    }

    /// Encoded audio is linked straight to the sink, with nothing spliced in.
    #[test]
    fn encoded_audio_passes_through() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        // opusenc ships in gstreamer1.0-plugins-base, which CI installs.
        let enc = gst::ElementFactory::make("opusenc")
            .build()
            .expect("opusenc (gst-plugins-base)");
        let queue = gst::ElementFactory::make("queue")
            .name("q")
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();
        pipeline.add_many([&src, &enc, &queue, &sink]).unwrap();
        gst::Element::link_many([&src, &enc, &queue, &sink]).unwrap();
        install_audio_input_bridge(&queue.static_pad("src").unwrap(), "q");
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let peer = queue.static_pad("src").unwrap().peer().unwrap();
        let error = pipeline
            .bus()
            .unwrap()
            .pop_filtered(&[gst::MessageType::Error]);
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(error.is_none(), "pipeline error: {error:?}");
        assert_eq!(
            peer.parent_element().as_ref(),
            Some(&sink),
            "something was spliced in front of the sink"
        );
    }
}
