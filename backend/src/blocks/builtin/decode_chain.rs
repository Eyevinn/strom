//! Shared helpers for decoded-video chains in source blocks.
//!
//! Source blocks that decode H.264 to raw video feed the GL-based vision mixer,
//! which rejects interlaced frames at `glupload`. Interlaced broadcast feeds
//! (e.g. 1080i50) must therefore be deinterlaced in the source.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{debug, warn};

/// Whether `caps` say the video is progressive.
///
/// No `interlace-mode` field means progressive, as in GStreamer's own video
/// info. No caps at all is not an answer: the caller cannot know yet.
fn is_progressive(caps: Option<&gst::CapsRef>) -> bool {
    let Some(structure) = caps.and_then(|c| c.structure(0)) else {
        return false;
    };
    match structure.get::<&str>("interlace-mode") {
        Ok(mode) => mode == "progressive",
        Err(gst::structure::GetError::FieldNotFound { .. }) => true,
        Err(_) => false,
    }
}

/// Link a decoded raw-video source pad into the block's video output
/// `identity`, deinterlacing only when the stream may need it.
///
/// `deinterlace` runs on the CPU and takes system memory only, so a decoder in
/// front of it (`nvh264dec`) downloads every frame - even in passthrough, for a
/// progressive stream. When `raw_src` already has progressive caps, which a
/// `decodebin` pad has when it is added, the pad goes straight to `identity`
/// and the decoder can keep its frames in GPU memory for the consumer to adapt
/// (see CLAUDE.md, GStreamer Memory Formats). Interlaced caps, and a pad with
/// no caps yet (an explicit decoder before data flows), go through
/// [`link_raw_video_through_deinterlace`] as before.
///
/// A stream that turns interlaced on the same pad later is not deinterlaced; a
/// warning says so, and restarting the flow picks `deinterlace` again.
pub(crate) fn link_raw_video(
    bin: &gst::Bin,
    raw_src: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
    pad_label: &str,
) -> Result<(), String> {
    if !is_progressive(raw_src.current_caps().as_deref()) {
        return link_raw_video_through_deinterlace(bin, raw_src, identity, instance_id, pad_label);
    }

    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;
    // EVENT probe: fires per event, never per buffer. Captures nothing but an
    // owned string.
    let label = format!("{}:{}", instance_id, pad_label);
    identity_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data {
            if let gst::EventView::Caps(caps_event) = event.view() {
                if !is_progressive(Some(caps_event.caps())) {
                    warn!(
                        "{}: the stream turned interlaced after it started progressive, and is \
                         not deinterlaced. Restart the flow to deinterlace it",
                        label
                    );
                    return gst::PadProbeReturn::Remove;
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    raw_src
        .link(&identity_sink)
        .map(|_| ())
        .map_err(|e| format!("link pad -> identity: {:?}", e))?;
    debug!(
        "{}: progressive video, linked {} straight to its output (no deinterlace)",
        instance_id, pad_label
    );
    Ok(())
}

/// Link a decoded raw-video source pad into the block's video output `identity`
/// through a `deinterlace` element.
///
/// A one-shot CAPS-event probe pins the deinterlace mode from the negotiated
/// `interlace-mode`: `interleaved`/`mixed` -> `mode=interlaced` (force),
/// everything else -> `mode=disabled` (explicit passthrough). Forcing is
/// required because H.264 `mixed` streams report mixed caps without flagging
/// individual buffers as interlaced, so `mode=auto` would silently pass
/// everything through (relabelling to progressive) and leave visible combing.
/// Progressive sources use `disabled` rather than `auto` so a source marked
/// progressive is never deinterlaced, even if a stray buffer is flagged.
///
/// The probe reads the element from the pad itself, so its closure captures no
/// GStreamer references — no circular-ref leak (see CLAUDE.md). EVENT probes
/// fire infrequently. Because the decision is driven by the CAPS event rather
/// than caps-at-link-time, this works for both decodebin pads (caps known at
/// pad-added) and explicit decoder pads (caps known only once data flows).
///
/// Links downstream first (`deinterlace -> identity`), syncs state, then links
/// `raw_src` last so data only flows once the chain is fully connected.
///
/// `pad_label` is used only to build a unique element name.
pub(crate) fn link_raw_video_through_deinterlace(
    bin: &gst::Bin,
    raw_src: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
    pad_label: &str,
) -> Result<(), String> {
    let name = format!("{}:deinterlace_{}", instance_id, pad_label);
    let deinterlace = gst::ElementFactory::make("deinterlace")
        .name(&name)
        .property_from_str("mode", "auto")
        // High-quality motion-adaptive method; the default `linear` softens
        // detail and leaves residual artifacts on broadcast content.
        .property_from_str("method", "yadif")
        .build()
        .map_err(|e| format!("deinterlace: {}", e))?;

    bin.add(&deinterlace)
        .map_err(|e| format!("add deinterlace: {}", e))?;

    let deinterlace_sink = deinterlace
        .static_pad("sink")
        .ok_or("deinterlace has no sink pad")?;
    let deinterlace_src = deinterlace
        .static_pad("src")
        .ok_or("deinterlace has no src pad")?;
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;

    // Force real deinterlacing for interlaced/mixed sources once caps arrive.
    deinterlace_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let interlace_mode = caps_event
            .caps()
            .structure(0)
            .and_then(|s| s.get::<String>("interlace-mode").ok());
        let interlaced = matches!(
            interlace_mode.as_deref(),
            Some("interleaved") | Some("mixed")
        );
        // Force deinterlacing for interlaced/mixed; explicit passthrough
        // otherwise (never deinterlace a source marked progressive).
        let mode = if interlaced { "interlaced" } else { "disabled" };
        if let Some(element) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
            element.set_property_from_str("mode", mode);
            debug!(
                "{}: deinterlace mode={} (source interlace-mode={:?})",
                element.name(),
                mode,
                interlace_mode
            );
        }
        gst::PadProbeReturn::Remove
    });

    // Link downstream first: deinterlace -> identity
    deinterlace_src
        .link(&identity_sink)
        .map_err(|e| format!("link deinterlace -> identity: {:?}", e))?;

    deinterlace
        .sync_state_with_parent()
        .map_err(|e| format!("sync deinterlace: {}", e))?;

    // Link source pad last to start data flow only when the chain is ready
    raw_src
        .link(&deinterlace_sink)
        .map_err(|e| format!("link pad -> deinterlace: {:?}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// A bin with the block's output `identity`, and a source pad carrying
    /// `caps` as a sticky event, the way a `decodebin` pad does when added.
    fn setup(caps: Option<&str>) -> (gst::Bin, gst::Pad, gst::Element) {
        gst::init().unwrap();
        let bin = gst::Bin::new();
        let identity = gst::ElementFactory::make("identity").build().unwrap();
        bin.add(&identity).unwrap();
        let src = gst::Pad::builder(gst::PadDirection::Src)
            .name("src_0")
            .build();
        src.set_active(true).unwrap();
        if let Some(caps) = caps {
            // Sticky events are stored on the pad even while it is unlinked.
            src.push_event(gst::event::StreamStart::new("test"));
            src.push_event(gst::event::Caps::new(&gst::Caps::from_str(caps).unwrap()));
            assert!(src.current_caps().is_some());
        }
        (bin, src, identity)
    }

    fn has_deinterlace(bin: &gst::Bin) -> bool {
        bin.children()
            .iter()
            .any(|e| e.factory().is_some_and(|f| f.name() == "deinterlace"))
    }

    fn peer_element(pad: &gst::Pad) -> Option<gst::Element> {
        pad.peer().and_then(|p| p.parent_element())
    }

    /// The fix: a progressive pad goes straight to the output, so nothing on
    /// the CPU sits between a GPU decoder and the consumer.
    #[test]
    fn progressive_video_skips_deinterlace() {
        for caps in [
            "video/x-raw, format=NV12, width=1280, height=720, interlace-mode=progressive",
            "video/x-raw(memory:CUDAMemory), format=NV12, interlace-mode=progressive",
            // No interlace-mode field means progressive.
            "video/x-raw, format=NV12, width=1280, height=720",
        ] {
            let (bin, src, identity) = setup(Some(caps));
            link_raw_video(&bin, &src, &identity, "test", "src_0").unwrap();
            assert!(!has_deinterlace(&bin), "{}", caps);
            assert_eq!(peer_element(&src).as_ref(), Some(&identity), "{}", caps);
        }
    }

    #[test]
    fn interlaced_video_is_deinterlaced() {
        for mode in ["interleaved", "mixed"] {
            let caps = format!("video/x-raw, format=NV12, interlace-mode={}", mode);
            let (bin, src, identity) = setup(Some(&caps));
            link_raw_video(&bin, &src, &identity, "test", "src_0").unwrap();
            assert!(has_deinterlace(&bin), "{}", mode);
            assert_ne!(peer_element(&src).as_ref(), Some(&identity), "{}", mode);
        }
    }

    /// An explicit decoder's pad has no caps until data flows, so it keeps
    /// `deinterlace`, which decides on the CAPS event as before.
    #[test]
    fn a_pad_without_caps_is_deinterlaced() {
        let (bin, src, identity) = setup(None);
        link_raw_video(&bin, &src, &identity, "test", "src_0").unwrap();
        assert!(has_deinterlace(&bin));
    }
}
