//! Consumer-side adaptation of a WHEP Output video input.
//!
//! The convention is that a block emits the memory type it naturally produces
//! and the *consuming* block adapts its own input, so a GPU-to-GPU flow is
//! never forced through a download and re-upload. Most consumers can pick
//! their input front at build time. This module covers the case where they
//! cannot: what arrives on a WHEP Output video pad — which memory it lives in
//! and which pixel format it carries — depends on which decoder `decodebin`
//! autoplugged upstream, or on how a mixer was configured, and is only known
//! once caps are negotiated.
//!
//! Two things are adapted, both from the first CAPS event, both by splicing
//! elements between the watched pad and its peer:
//!
//! * **GL memory** is downloaded to system memory. `whepserversink` advertises
//!   `video/x-raw(memory:GLMemory)` on its video request pads, so GL frames
//!   negotiate all the way to the sink and its encoder discovery then finds no
//!   encoder that can take them — video goes missing while audio keeps working.
//!   Only GL memory is bridged. CUDA, NVMM, D3D11 and VA memory are left in
//!   place: the consumers that advertise those really do encode them, and
//!   downloading would cost a full round trip per frame.
//!
//! * **The pixel format** is converted to one the encoders take natively.
//!   `webrtcsink` builds a full encoding chain per consumer, each starting with
//!   its own `videoconvert`, so an RGBA input is converted once per viewer —
//!   and into whatever the encoder's caps happen to prefer, which for
//!   VideoToolbox is 16-bit `RGBA64_LE` and for `vp9enc` is 4:4:4 `Y444`.
//!   Converting once here leaves every consumer's converter in passthrough.
//!
//! Relinking from inside a push-event probe is a supported pattern:
//! `gst_pad_push_event_unchecked` resolves the peer and rechecks pending sticky
//! events *after* running its probes, so the event that triggered the splice
//! lands on the newly inserted elements.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use tracing::{info, warn};

/// The caps feature that marks a buffer as living in GL memory.
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// The caps feature negotiated caps carry for plain system memory.
const SYSTEM_MEMORY_FEATURE: &str = "memory:SystemMemory";

/// The format the input is converted to when it needs converting at all.
///
/// Every encoder a WHEP consumer can land on takes it without a further
/// conversion: `vtenc_h264`/`vtenc_h265` and the NVENC and VA encoders take it
/// natively, and `x264enc` lists it alongside I420. Only the VP9 and AV1
/// software encoders prefer I420, and there the difference is a couple of
/// points against an encode that costs fifteen.
const NATIVE_FORMAT: &str = "NV12";

/// Formats left alone: 8-bit 4:2:0, which every encoder in the offer takes
/// without a conversion of its own. Converting I420 to NV12 would buy nothing
/// and would cost a pass over every frame.
const NATIVE_FORMATS: &[&str] = &["NV12", "I420"];

/// True when `caps` describe raw video in GL memory.
///
/// Encoded video (`video/x-h264` and friends) is never a candidate: there is
/// nothing to download, and `gldownload` would not even link. Other GPU memory
/// types are not candidates either — see the module docs.
pub fn needs_gl_download(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    caps.features(0)
        .is_some_and(|features| features.contains(GL_MEMORY_FEATURE))
}

/// True when the input should be converted to [`NATIVE_FORMAT`] before the
/// sink fans it out to its consumers.
///
/// GL memory says yes, because the download that precedes the converter lands
/// the frame in system memory in the same format it had on the GPU. Every
/// other memory feature says no: the encoder behind that pad consumes it
/// directly, and a `videoconvert` could not even link to it.
///
/// A format carrying more than 8 bits per component is left alone as well.
/// Converting it would silently drop the input's precision, and the encoders
/// that can use the extra bits would have kept them.
pub fn needs_format_conversion(caps: &gst::CapsRef) -> bool {
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    if structure.name() != "video/x-raw" {
        return false;
    }
    // GL memory qualifies because the download spliced in front of the
    // converter lands the frame in system memory in the same pixel format.
    let convertible_memory = match caps.features(0) {
        None => true,
        Some(features) if features.is_any() => false,
        Some(features) => {
            features.is_empty()
                || features
                    .iter()
                    .all(|f| f == SYSTEM_MEMORY_FEATURE || f == GL_MEMORY_FEATURE)
        }
    };
    if !convertible_memory {
        return false;
    }
    let Ok(format) = structure.get::<String>("format") else {
        // An unfixed format means negotiation has not settled on one; there is
        // nothing to compare against and nothing to convert.
        return false;
    };
    if NATIVE_FORMATS.contains(&format.as_str()) {
        return false;
    }
    let Ok(parsed) = format.parse::<gst_video::VideoFormat>() else {
        return false;
    };
    let info = gst_video::VideoFormatInfo::from_format(parsed);
    info.depth().iter().copied().max().unwrap_or(8) <= 8
}

/// Watch `src_pad` and adapt what arrives on it to what `whepserversink` can
/// encode, by splicing elements in front of its peer.
///
/// The probe is one-shot: it removes itself as soon as the first CAPS event has
/// been classified, whichever way it went, so nothing is left behind on the
/// data path. `name_prefix` names the inserted elements and is only used for
/// logging and debugging. `convert_factory` is the converter to splice, picked
/// by the caller so the choice stays where the rest of the project makes it.
pub fn install_video_input_bridge(src_pad: &gst::Pad, name_prefix: &str, convert_factory: &str) {
    let name_prefix = name_prefix.to_string();
    let convert_factory = convert_factory.to_string();

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps();

        let adapters = match build_adapters(caps, &name_prefix, &convert_factory) {
            Ok(adapters) => adapters,
            Err(e) => {
                warn!(
                    "{}: input could not be adapted ({}), leaving it as it is — a downstream encoder may not accept it",
                    name_prefix, e
                );
                return gst::PadProbeReturn::Remove;
            }
        };

        // System memory in a format the encoders take: nothing to do, and
        // nothing to keep watching for.
        if adapters.is_empty() {
            return gst::PadProbeReturn::Remove;
        }

        // Remove the probe on every path from here on: the splice either
        // succeeds, or it failed for a reason that retrying will not fix.
        if let Err(e) = splice(pad, &adapters) {
            warn!(
                "{}: input could not be adapted ({}), leaving it as it is — a downstream encoder may not accept it",
                name_prefix, e
            );
        } else {
            info!(
                "{}: adapted input {} with {}",
                name_prefix,
                caps,
                adapters
                    .iter()
                    .map(|e| e.name().to_string())
                    .collect::<Vec<_>>()
                    .join(" ! ")
            );
        }
        gst::PadProbeReturn::Remove
    });
}

/// Build the elements that adapt `caps`, in the order they must be linked.
///
/// A download comes first: the converter takes system memory only, and the
/// format it has to produce is the same either way.
fn build_adapters(
    caps: &gst::CapsRef,
    name_prefix: &str,
    convert_factory: &str,
) -> Result<Vec<gst::Element>, String> {
    let mut adapters = Vec::new();

    if needs_gl_download(caps) {
        adapters.push(
            gst::ElementFactory::make("gldownload")
                .name(format!("{}_gldownload", name_prefix))
                .build()
                .map_err(|e| format!("gldownload could not be created: {}", e))?,
        );
    }

    if needs_format_conversion(caps) {
        adapters.push(
            gst::ElementFactory::make(convert_factory)
                .name(format!("{}_videoconvert", name_prefix))
                .build()
                .map_err(|e| format!("{} could not be created: {}", convert_factory, e))?,
        );
        adapters.push(
            gst::ElementFactory::make("capsfilter")
                .name(format!("{}_format", name_prefix))
                .property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("format", NATIVE_FORMAT)
                        .build(),
                )
                .build()
                .map_err(|e| format!("capsfilter could not be created: {}", e))?,
        );
    }

    Ok(adapters)
}

/// Insert `adapters`, linked in order, between `src_pad` and its current peer.
fn splice(src_pad: &gst::Pad, adapters: &[gst::Element]) -> Result<(), String> {
    let peer = src_pad
        .peer()
        .ok_or_else(|| "source pad has no peer".to_string())?;

    // Strong references stay local to this function — never captured in a
    // closure, so no reference cycle can outlive the pipeline.
    let bin = src_pad
        .parent_element()
        .and_then(|element| element.parent())
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| "source pad's element has no parent bin".to_string())?;

    for adapter in adapters {
        bin.add(adapter).map_err(|e| {
            format!(
                "{} could not be added to {}: {}",
                adapter.name(),
                bin.name(),
                e
            )
        })?;
    }

    let first = adapters.first().expect("at least one adapter");
    let last = adapters.last().expect("at least one adapter");
    let sink = first
        .static_pad("sink")
        .ok_or_else(|| format!("{} has no sink pad", first.name()))?;
    let src = last
        .static_pad("src")
        .ok_or_else(|| format!("{} has no src pad", last.name()))?;

    for pair in adapters.windows(2) {
        pair[0].link(&pair[1]).map_err(|e| {
            format!(
                "could not link {} to {}: {}",
                pair[0].name(),
                pair[1].name(),
                e
            )
        })?;
    }

    // Unlink before linking: the peer accepts one upstream link only.
    src_pad.unlink(&peer).map_err(|e| {
        format!(
            "could not unlink {} from {}: {}",
            src_pad.name(),
            peer.name(),
            e
        )
    })?;
    src.link(&peer)
        .map_err(|e| format!("could not link {} to {}: {}", last.name(), peer.name(), e))?;
    src_pad.link(&sink).map_err(|e| {
        format!(
            "could not link {} to {}: {}",
            src_pad.name(),
            first.name(),
            e
        )
    })?;

    // State last, so the elements negotiate against a complete topology.
    for adapter in adapters {
        adapter.sync_state_with_parent().map_err(|e| {
            format!(
                "{} could not reach the pipeline state: {}",
                adapter.name(),
                e
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn caps(s: &str) -> gst::Caps {
        // Caps parsing needs the type system registered.
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    #[test]
    fn gl_memory_raw_video_needs_a_download() {
        assert!(needs_gl_download(&caps(
            "video/x-raw(memory:GLMemory), format=NV12, width=1280, height=720"
        )));
    }

    #[test]
    fn system_memory_raw_video_does_not() {
        assert!(!needs_gl_download(&caps(
            "video/x-raw, format=NV12, width=1280, height=720"
        )));
    }

    /// The consumers that advertise these memory types encode them directly.
    /// Downloading would cost a GPU round trip per frame for nothing.
    #[test]
    fn other_gpu_memory_types_are_left_alone() {
        for feature in [
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            let c = caps(&format!("video/x-raw({}), format=NV12", feature));
            assert!(
                !needs_gl_download(&c),
                "{} should not be downloaded",
                feature
            );
        }
    }

    /// WHEP Output also takes pre-encoded video. gldownload cannot even link
    /// to it, so it must never be spliced in.
    #[test]
    fn encoded_video_is_not_a_candidate() {
        for c in [
            "video/x-h264, stream-format=avc, alignment=au",
            "video/x-h265",
            "video/x-vp9",
            "video/x-av1",
        ] {
            assert!(
                !needs_gl_download(&caps(c)),
                "{} should be passed through",
                c
            );
        }
    }

    #[test]
    fn audio_and_capsless_caps_are_not_candidates() {
        assert!(!needs_gl_download(&caps("audio/x-raw, rate=48000")));
        assert!(!needs_gl_download(&caps("ANY")));
        assert!(!needs_gl_download(&caps("EMPTY")));
    }

    /// The format every encoder in the offer takes without a conversion of its
    /// own, plus the one the sink's own per-consumer converter would produce.
    #[test]
    fn eight_bit_420_is_left_alone() {
        for format in ["NV12", "I420"] {
            assert!(
                !needs_format_conversion(&caps(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                ))),
                "{} should be passed through",
                format
            );
        }
    }

    /// Everything webrtcsink would otherwise convert once per consumer: packed
    /// RGB from a mixer or a screen capture, packed and planar YUV that is not
    /// 4:2:0, and gray.
    #[test]
    fn other_eight_bit_formats_are_converted() {
        for format in [
            "RGBA", "BGRA", "RGB", "BGRx", "UYVY", "YUY2", "Y42B", "Y444", "GRAY8",
        ] {
            assert!(
                needs_format_conversion(&caps(&format!(
                    "video/x-raw, format={}, width=1920, height=1080",
                    format
                ))),
                "{} should be converted",
                format
            );
        }
    }

    /// A download lands the frame in system memory in the format it had on the
    /// GPU, so a GL input is judged on its format like any other.
    #[test]
    fn gl_memory_is_judged_on_its_format() {
        assert!(needs_format_conversion(&caps(
            "video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080"
        )));
        assert!(!needs_format_conversion(&caps(
            "video/x-raw(memory:GLMemory), format=NV12, width=1920, height=1080"
        )));
    }

    /// The consumers that advertise these memory types encode them directly,
    /// and `videoconvert` could not link to them anyway.
    #[test]
    fn other_gpu_memory_is_not_converted() {
        for feature in [
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            assert!(
                !needs_format_conversion(&caps(&format!("video/x-raw({}), format=RGBA", feature))),
                "{} should not be converted",
                feature
            );
        }
    }

    /// Converting these would drop precision the encoder behind the pad may
    /// well be able to keep.
    #[test]
    fn deeper_than_eight_bit_is_left_alone() {
        for format in ["P010_10LE", "I420_10LE", "AYUV64", "RGBA64_LE", "v210"] {
            assert!(
                !needs_format_conversion(&caps(&format!("video/x-raw, format={}", format))),
                "{} should be passed through",
                format
            );
        }
    }

    /// WHEP Output also takes pre-encoded video, which no converter can touch.
    #[test]
    fn encoded_video_is_not_converted() {
        for c in [
            "video/x-h264, stream-format=avc, alignment=au",
            "video/x-h265",
            "video/x-vp9",
            "video/x-av1",
            "audio/x-raw, rate=48000",
        ] {
            assert!(
                !needs_format_conversion(&caps(c)),
                "{} should be passed through",
                c
            );
        }
    }

    /// Before negotiation settles there is nothing to compare against.
    #[test]
    fn unfixed_format_is_left_alone() {
        assert!(!needs_format_conversion(&caps(
            "video/x-raw, width=1920, height=1080"
        )));
        assert!(!needs_format_conversion(&caps("ANY")));
        assert!(!needs_format_conversion(&caps("EMPTY")));
    }

    /// GL memory in a format the encoders do not take needs both adaptations,
    /// and the download has to come first: the converter takes system memory.
    #[test]
    fn gl_rgba_is_downloaded_then_converted() {
        let _ = gst::init();
        let adapters = build_adapters(
            &caps("video/x-raw(memory:GLMemory), format=RGBA, width=1920, height=1080"),
            "video_queue",
            "videoconvert",
        )
        .expect("adapters");
        let factories: Vec<String> = adapters
            .iter()
            .map(|e| {
                e.factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(factories, ["gldownload", "videoconvert", "capsfilter"]);
    }

    /// A GPU mixer that already emits an encoder-native format only needs the
    /// download.
    #[test]
    fn gl_nv12_is_only_downloaded() {
        let _ = gst::init();
        let adapters = build_adapters(
            &caps("video/x-raw(memory:GLMemory), format=NV12, width=1920, height=1080"),
            "video_queue",
            "videoconvert",
        )
        .expect("adapters");
        let factories: Vec<String> = adapters
            .iter()
            .map(|e| {
                e.factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(factories, ["gldownload"]);
    }

    /// Nothing is spliced into a path that needs neither adaptation.
    #[test]
    fn native_system_memory_needs_no_adapters() {
        let _ = gst::init();
        assert!(build_adapters(
            &caps("video/x-raw, format=NV12, width=1920, height=1080"),
            "video_queue",
            "videoconvert",
        )
        .expect("adapters")
        .is_empty());
    }

    /// A pipeline through the bridge, reporting what the sink negotiated, how
    /// many buffers reached it, and how many elements the pipeline ended with.
    fn run_pipeline(format: &str) -> (String, usize, usize) {
        let _ = gst::init();

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property("num-buffers", 15i32)
            .build()
            .expect("videotestsrc");
        let filter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", format)
                    .field("width", 320i32)
                    .field("height", 240i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .expect("capsfilter");
        let queue = gst::ElementFactory::make("queue")
            .name("video_queue")
            .build()
            .expect("queue");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink");

        pipeline
            .add_many([&src, &filter, &queue, &sink])
            .expect("add");
        gst::Element::link_many([&src, &filter, &queue, &sink]).expect("link");

        let queue_src = queue.static_pad("src").expect("queue src pad");
        install_video_input_bridge(&queue_src, "video_queue", "videoconvert");

        let sink_pad = sink.static_pad("sink").expect("fakesink sink pad");
        let buffers = Arc::new(AtomicUsize::new(0));
        let counter = buffers.clone();
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

        pipeline.set_state(gst::State::Playing).expect("play");

        let bus = pipeline.bus().expect("bus");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(200)) {
                match msg.view() {
                    gst::MessageView::Eos(_) => break,
                    gst::MessageView::Error(e) => {
                        panic!("pipeline error: {} ({:?})", e.error(), e.debug())
                    }
                    _ => {}
                }
            }
        }

        let negotiated = sink_pad
            .current_caps()
            .and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok()))
            .unwrap_or_default();
        let elements = pipeline.iterate_elements().into_iter().flatten().count();

        pipeline.set_state(gst::State::Null).expect("null");
        (negotiated, buffers.load(Ordering::Relaxed), elements)
    }

    /// Without the conversion, RGBA reaches the sink and every consumer
    /// converts it for itself.
    #[test]
    fn rgba_is_converted_before_the_peer() {
        let (format, buffers, elements) = run_pipeline("RGBA");
        assert_eq!(format, "NV12", "the peer should see converted video");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(
            elements, 6,
            "expected a converter and a capsfilter to be spliced in"
        );
    }

    /// A path that already carries an encoder-native format must not pay for a
    /// conversion it does not need.
    #[test]
    fn nv12_reaches_the_peer_untouched() {
        let (format, buffers, elements) = run_pipeline("NV12");
        assert_eq!(format, "NV12");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(elements, 4, "nothing should have been spliced in");
    }

    /// I420 is native to the software encoders and accepted by the hardware
    /// ones, so it is passed through as it is.
    #[test]
    fn i420_reaches_the_peer_untouched() {
        let (format, buffers, elements) = run_pipeline("I420");
        assert_eq!(format, "I420");
        assert!(buffers > 0, "no buffers reached the peer");
        assert_eq!(elements, 4, "nothing should have been spliced in");
    }
}
