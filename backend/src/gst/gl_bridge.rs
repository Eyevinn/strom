//! Consumer-side GPU-memory adaptation for video inputs.
//!
//! The convention is that a block emits the memory type it naturally produces
//! and the *consuming* block adapts its own input, so a GPU-to-GPU flow is
//! never forced through a download and re-upload. Most consumers can pick
//! their input front at build time. This module covers the case where they
//! cannot: whether frames arriving on a pad live in GL memory depends on which
//! decoder `decodebin` autoplugged upstream, which is only known once caps are
//! negotiated.
//!
//! [`install_gl_download_bridge`] watches an already-linked source pad and, on
//! the first CAPS event that carries `memory:GLMemory`, splices a `gldownload`
//! between that pad and its peer. Relinking from inside a push-event probe is
//! a supported pattern: `gst_pad_push_event_unchecked` resolves the peer and
//! rechecks pending sticky events *after* running its probes, so the event that
//! triggered the splice lands on the newly inserted element.
//!
//! Only GL memory is bridged. CUDA, NVMM, D3D11 and VA memory are left in
//! place — the consumers that advertise those memory types really do encode
//! them, and downloading would cost a full round trip per frame.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The caps feature that marks a buffer as living in GL memory.
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

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

/// Watch `src_pad` and insert a `gldownload` in front of its peer if the
/// negotiated caps turn out to carry GL memory.
///
/// The probe is one-shot: it removes itself as soon as the first CAPS event has
/// been classified, whichever way it went, so nothing is left behind on the
/// data path. `name_prefix` names the inserted element and is only used for
/// logging and debugging.
pub fn install_gl_download_bridge(src_pad: &gst::Pad, name_prefix: &str) {
    let name_prefix = name_prefix.to_string();

    // EVENT_DOWNSTREAM, not BLOCK/BUFFER: this fires per event, not per buffer.
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };

        if !needs_gl_download(caps_event.caps()) {
            // System memory, another GPU memory type, or already-encoded video:
            // nothing to do, and nothing to keep watching for.
            return gst::PadProbeReturn::Remove;
        }

        // Remove the probe on every path from here on: the splice either
        // succeeds, or it failed for a reason that retrying will not fix.
        if let Err(e) = splice_gl_download(pad, &name_prefix) {
            warn!(
                "{}: GL-memory input could not be bridged ({}), leaving it in GL memory — a downstream encoder may not accept it",
                name_prefix, e
            );
        }
        gst::PadProbeReturn::Remove
    });
}

/// Insert a `gldownload` between `src_pad` and its current peer.
fn splice_gl_download(src_pad: &gst::Pad, name_prefix: &str) -> Result<(), String> {
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

    let name = format!("{}_gldownload", name_prefix);
    let gldownload = gst::ElementFactory::make("gldownload")
        .name(&name)
        .build()
        .map_err(|e| format!("gldownload could not be created: {}", e))?;

    bin.add(&gldownload)
        .map_err(|e| format!("gldownload could not be added to {}: {}", bin.name(), e))?;

    let sink = gldownload
        .static_pad("sink")
        .ok_or_else(|| "gldownload has no sink pad".to_string())?;
    let src = gldownload
        .static_pad("src")
        .ok_or_else(|| "gldownload has no src pad".to_string())?;

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
        .map_err(|e| format!("could not link gldownload to {}: {}", peer.name(), e))?;
    src_pad
        .link(&sink)
        .map_err(|e| format!("could not link {} to gldownload: {}", src_pad.name(), e))?;

    // State last, so the element negotiates against a complete topology.
    gldownload
        .sync_state_with_parent()
        .map_err(|e| format!("gldownload could not reach the pipeline state: {}", e))?;

    info!(
        "{}: input carries GL memory, inserted {} in front of {}",
        name_prefix,
        name,
        peer.name()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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
}
