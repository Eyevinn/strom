//! Shared helpers for decoded-video chains in source blocks.
//!
//! Source blocks that decode H.264 to raw video feed the GL-based vision mixer,
//! which rejects interlaced frames at `glupload`. Interlaced broadcast feeds
//! (e.g. 1080i50) must therefore be deinterlaced in the source.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::debug;

/// Link a decoded raw-video source pad into the block's video output `identity`
/// through a `deinterlace` element.
///
/// The element starts in `mode=auto` and a one-shot CAPS-event probe upgrades it
/// to `mode=interlaced` (force) when the negotiated `interlace-mode` is
/// `interleaved` or `mixed`. Forcing is required because H.264 `mixed` streams
/// report mixed caps without flagging individual buffers as interlaced, so
/// `mode=auto` would silently pass everything through (relabelling to
/// progressive) and leave visible combing. Genuinely progressive sources keep
/// `mode=auto`, i.e. passthrough, so they are untouched.
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
        if let Some(element) = pad.parent().and_then(|p| p.downcast::<gst::Element>().ok()) {
            if interlaced {
                element.set_property_from_str("mode", "interlaced");
            }
            debug!(
                "{}: deinterlace mode={} (source interlace-mode={:?})",
                element.name(),
                if interlaced {
                    "interlaced (forced)"
                } else {
                    "auto (passthrough)"
                },
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
