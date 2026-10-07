//! Caps-driven GL download in front of a consumer that reads system memory.
//!
//! A `videoconvert` with an encoder behind it advertises `video/x-raw(ANY)` on
//! its sink pad template, so it accepts GL memory and only then finds that
//! nothing downstream takes it: the stream stops `not-negotiated` one element
//! past the link. The linker's GL-download retry ([`crate::gst::gl_link`])
//! does not see this, because the link itself succeeds. It happens when the
//! producer settles on GL memory without asking the consumer: `decodebin`
//! answers the autoplug-query of a decoder whose pad is not linked yet, so a
//! decoder such as `vtdechw` can choose GL memory before the consumer is in the
//! path, and its CAPS event arrives afterwards.
//!
//! [`install`] watches the source pad in front of the consumer. A CAPS event in
//! GL memory that the consumer cannot take splices `gldownload` in front of
//! it, as [`video_adapt::decide`] calls for. Once spliced, the download stays:
//! `gldownload` passes system memory through untouched, so a producer that
//! later switches back needs nothing new. A consumer that takes GL memory
//! itself (`autovideoconvert` on an NVIDIA build) gets nothing.
//!
//! Relinking from inside a push-event probe is the pattern
//! [`crate::gst::video_input_bridge`] uses: the CAPS event is serialized with
//! the buffers on the streaming thread, so nothing is in flight across the
//! link while the probe runs, and the event lands on the new elements.

use crate::gst::video_adapt::{self, Adapter, Consumer};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Watch `src_pad` and splice a GL download in front of its peer when what
/// arrives is GL memory the peer cannot take. `name_prefix` names the inserted
/// elements and the input in logs and error messages.
///
/// The probe fires per event, never per buffer.
pub fn install(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();
    let spliced = AtomicBool::new(false);

    // EVENT_DOWNSTREAM, not BUFFER: this fires per event.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        if spliced.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Ok;
        }
        let caps = caps_event.caps();
        let Some(peer) = pad.peer() else {
            return gst::PadProbeReturn::Ok;
        };
        if !needs_download(caps, &peer) {
            return gst::PadProbeReturn::Ok;
        }
        match splice(pad, &peer, &name_prefix) {
            Ok(()) => {
                info!(
                    "{}: input {} is in GL memory, downloading it to system memory",
                    name_prefix, caps
                );
                spliced.store(true, Ordering::Release);
            }
            Err(e) => post_splice_failure(pad, caps, &name_prefix, &e),
        }
        gst::PadProbeReturn::Ok
    });
}

/// True when `caps` are GL memory that `peer` cannot take, and a download
/// would give it something it can.
///
/// `peer`'s caps query answer, not its ACCEPT_CAPS answer: `videoconvert`
/// accepts GL memory on its template alone.
fn needs_download(caps: &gst::CapsRef, peer: &gst::Pad) -> bool {
    let takes = peer.query_caps(None);
    if takes.can_intersect(caps) {
        return false;
    }
    matches!(
        video_adapt::decide(caps, Consumer::Accepts(&takes), video_adapt::factory_available),
        Ok(adapters) if adapters == [Adapter::GlDownload]
    )
}

/// Put `gldownload ! capsfilter(video/x-raw)` between `src_pad` and `target`.
///
/// The capsfilter pins the download's output to system memory: `gldownload`
/// also offers GL memory on its source pad. On failure the direct link is put
/// back. The caller makes sure no buffer moves across the link meanwhile.
fn splice(src_pad: &gst::Pad, target: &gst::Pad, name_prefix: &str) -> Result<(), String> {
    // Strong references stay local to this function — never captured in a
    // closure, so no reference cycle can outlive the pipeline.
    let bin = src_pad
        .parent_element()
        .and_then(|element| element.parent())
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| "source pad's element has no parent bin".to_string())?;

    let mut chain = video_adapt::build_elements(&[Adapter::GlDownload], name_prefix)?;
    chain.push(
        gst::ElementFactory::make("capsfilter")
            .name(format!("{}_system_memory", name_prefix))
            .property("caps", gst::Caps::new_empty_simple("video/x-raw"))
            .build()
            .map_err(|e| format!("capsfilter could not be created: {}", e))?,
    );
    let chain_in = chain[0]
        .static_pad("sink")
        .expect("gldownload has a sink pad");
    let chain_out = chain[chain.len() - 1]
        .static_pad("src")
        .expect("capsfilter has a src pad");

    bin.add_many(&chain)
        .map_err(|e| format!("could not add the download to {}: {}", bin.name(), e))?;
    let linked = gst::Element::link_many(&chain)
        .map_err(|e| format!("could not link the download: {}", e))
        .and_then(|_| {
            src_pad
                .unlink(target)
                .map_err(|e| format!("could not unlink {}: {}", src_pad.name(), e))
        })
        .and_then(|_| {
            chain_out
                .link(target)
                .map(|_| ())
                .map_err(|e| format!("could not link to {}: {}", target.name(), e))
        })
        .and_then(|_| {
            src_pad
                .link(&chain_in)
                .map(|_| ())
                .map_err(|e| format!("could not link {}: {}", src_pad.name(), e))
        })
        .and_then(|_| {
            // State last, so the elements negotiate against a complete topology.
            for element in &chain {
                element.sync_state_with_parent().map_err(|e| {
                    format!(
                        "{} could not reach the pipeline state: {}",
                        element.name(),
                        e
                    )
                })?;
            }
            Ok(())
        });

    if let Err(e) = linked {
        let _ = src_pad.unlink(&chain_in);
        let _ = chain_out.unlink(target);
        for element in &chain {
            let _ = element.set_state(gst::State::Null);
            let _ = bin.remove(element);
        }
        let _ = src_pad.link(target);
        return Err(e);
    }
    Ok(())
}

/// Fail the flow because the download `caps` need could not be put in place.
fn post_splice_failure(pad: &gst::Pad, caps: &gst::CapsRef, name_prefix: &str, reason: &str) {
    let Some(element) = pad.parent_element() else {
        warn!(
            "{}: input {} could not be downloaded from GL memory: {}",
            name_prefix, caps, reason
        );
        return;
    };
    gst::element_error!(
        element,
        gst::CoreError::Negotiation,
        (
            "{}: could not download the GL memory video input to system memory",
            name_prefix
        ),
        [
            "Arrived: {}. This input reads raw video in system memory, and \
             inserting gldownload failed: {}",
            caps,
            reason
        ]
    );
}
