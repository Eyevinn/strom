//! Link-time adaptation for a producer that can only deliver GL memory.
//!
//! A block emits the memory type it naturally produces and the *consuming*
//! block adapts its own input. A consumer that can pick its input front at
//! build time does so; one that only learns the memory type from negotiated
//! caps splices adapters from a caps probe on a pad that is already linked.
//!
//! Neither reaches the case here. A consumer whose external video input is a
//! `videoconvert` or `videoscale` with an encoder behind it advertises system
//! memory only, so a `video/x-raw(memory:GLMemory)` producer shares no format
//! with it and the link is refused outright — before any caps event the bridge
//! could act on, and leaving no peer to splice against. Nothing retries a link
//! that failed for want of a common format, so that branch carries no data
//! while the flow reports Playing.
//!
//! A `queue` or `identity` front does not open the link: both answer a caps
//! query by proxying it downstream, so their sink pads advertise whatever the
//! converter behind them does.
//!
//! [`retry_link_with_gl_download`] (or its element-level counterpart, for a
//! link that leaves a pad unnamed) runs only after a plain link has been
//! refused for want of a common format, and only when the producer offers GL
//! memory and nothing else while the consumer takes raw video in system memory.
//! If the adaptation cannot be completed it is undone, so the pads are left as
//! the refused link left them. A path that links on its own never reaches
//! this code, so no flow pays for a download it did not need, and CUDA, NVMM,
//! D3D11 and VA producers are left alone.

use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The caps feature that marks a buffer as living in GL memory.
const GL_MEMORY_FEATURE: &str = "memory:GLMemory";

/// True when every format `caps` offers is raw video in GL memory.
///
/// A producer that also offers system memory would have linked without help,
/// so a download is not what it is missing.
fn offers_only_gl_memory(caps: &gst::Caps) -> bool {
    if caps.is_any() || caps.is_empty() {
        return false;
    }
    caps.iter_with_features().all(|(structure, features)| {
        // `video/x-raw(ANY)` contains every feature, GL included, but it offers
        // system memory too and would have linked on its own.
        structure.name() == "video/x-raw"
            && !features.is_any()
            && features.contains(GL_MEMORY_FEATURE)
    })
}

/// True when `caps` accept raw video in system memory.
///
/// This is the consumer half of the test: a `videoconvert` with an encoder
/// behind it advertises plain `video/x-raw`, which is what a `gldownload`
/// produces. A consumer that takes only GPU memory of any kind, or no raw
/// video at all, is refusing the link for a reason a download does not address.
fn accepts_system_memory_raw_video(caps: &gst::Caps) -> bool {
    if caps.is_empty() {
        return false;
    }
    if caps.is_any() {
        return true;
    }
    caps.iter_with_features().any(|(structure, features)| {
        structure.name() == "video/x-raw"
            && (features.is_any() || features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY))
    })
}

/// True when a `gldownload` is the only thing keeping `src` and `sink` apart.
///
/// Both pads are asked what they can carry now, with everything already linked
/// around them: the producer's answer is narrowed by whatever is upstream of
/// it, and the consumer's by whatever is downstream, which is what makes a
/// `videoconvert` in front of an encoder answer "system memory" rather than
/// "anything".
pub fn needs_gl_download_to_link(src: &gst::Pad, sink: &gst::Pad) -> bool {
    offers_only_gl_memory(&src.query_caps(None))
        && accepts_system_memory_raw_video(&sink.query_caps(None))
}

/// Link `src` to `sink` through a `gldownload`, for a pair whose direct link was
/// refused with `refusal`.
///
/// Returns `Ok(false)` when this is not that kind of failure and the caller
/// should report the original error, `Ok(true)` when the link now stands, and
/// `Err` when the adaptation was called for but could not be carried out. On
/// `Err` the `gldownload` has been removed again and `src` is unlinked.
pub fn retry_link_with_gl_download(
    src: &gst::Pad,
    sink: &gst::Pad,
    refusal: gst::PadLinkError,
) -> Result<bool, String> {
    // An already-linked pad or a hierarchy mismatch looks the same to the caps
    // test, but a download does not resolve it.
    if refusal != gst::PadLinkError::Noformat || !needs_gl_download_to_link(src, sink) {
        return Ok(false);
    }

    let bin = src
        .parent_element()
        .and_then(|element| element.parent())
        .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        .ok_or_else(|| "source pad's element has no parent bin".to_string())?;

    let name = download_name(sink);
    let gldownload = gst::ElementFactory::make("gldownload")
        .name(&name)
        .build()
        .map_err(|e| format!("gldownload could not be created: {}", e))?;

    bin.add(&gldownload)
        .map_err(|e| format!("gldownload could not be added to {}: {}", bin.name(), e))?;

    if let Err(e) = link_through(src, &gldownload, sink) {
        // Removing the element from the bin unlinks its pads, so `src` is
        // left unlinked rather than feeding an element with no output.
        if let Err(state_error) = gldownload.set_state(gst::State::Null) {
            warn!("{} could not be set to NULL: {}", name, state_error);
        }
        if let Err(remove_error) = bin.remove(&gldownload) {
            warn!(
                "{} could not be removed from {}: {}",
                name,
                bin.name(),
                remove_error
            );
        }
        return Err(e);
    }

    info!(
        "{} produces GL memory that {} cannot take, inserted {} between them",
        src.name(),
        sink.name(),
        name
    );
    Ok(true)
}

/// The element-level counterpart of [`retry_link_with_gl_download`], for a link
/// that names at most one pad on each side and was refused as a whole.
///
/// An element-level refusal does not say why, so the unlinked pads are paired
/// up here: a named side contributes that pad, an unnamed side every unlinked
/// pad it already has. The first pair a `gldownload` would join is linked
/// directly to learn the concrete refusal, which then goes through the pad-level
/// path. Request pads the element has not created yet are not considered.
///
/// Returns as [`retry_link_with_gl_download`] does.
pub fn retry_element_link_with_gl_download(
    src: &gst::Element,
    src_pad_name: Option<&str>,
    sink: &gst::Element,
    sink_pad_name: Option<&str>,
) -> Result<bool, String> {
    let src_pads = candidate_pads(src, src_pad_name, gst::PadDirection::Src);
    let sink_pads = candidate_pads(sink, sink_pad_name, gst::PadDirection::Sink);

    let pair = src_pads.iter().find_map(|src_pad| {
        sink_pads
            .iter()
            .find(|sink_pad| needs_gl_download_to_link(src_pad, sink_pad))
            .map(|sink_pad| (src_pad, sink_pad))
    });
    let Some((src_pad, sink_pad)) = pair else {
        return Ok(false);
    };

    match src_pad.link(sink_pad) {
        Ok(_) => Ok(true),
        Err(refusal) => retry_link_with_gl_download(src_pad, sink_pad, refusal),
    }
}

/// The pad `name` on `element`, or every unlinked pad in `direction` when no
/// name is given.
fn candidate_pads(
    element: &gst::Element,
    name: Option<&str>,
    direction: gst::PadDirection,
) -> Vec<gst::Pad> {
    match name {
        Some(name) => element.static_pad(name).into_iter().collect(),
        None => element
            .pads()
            .into_iter()
            .filter(|pad| pad.direction() == direction && !pad.is_linked())
            .collect(),
    }
}

/// Link `src -> gldownload -> sink` and bring the element to its bin's state.
fn link_through(src: &gst::Pad, gldownload: &gst::Element, sink: &gst::Pad) -> Result<(), String> {
    let download_sink = gldownload
        .static_pad("sink")
        .ok_or_else(|| "gldownload has no sink pad".to_string())?;
    let download_src = gldownload
        .static_pad("src")
        .ok_or_else(|| "gldownload has no src pad".to_string())?;

    src.link(&download_sink)
        .map_err(|e| format!("could not link {} to gldownload: {}", src.name(), e))?;
    download_src
        .link(sink)
        .map_err(|e| format!("could not link gldownload to {}: {}", sink.name(), e))?;

    // State last, so the element negotiates against a complete topology. A
    // no-op while the pipeline is still in NULL.
    gldownload
        .sync_state_with_parent()
        .map_err(|e| format!("gldownload could not reach the pipeline state: {}", e))?;
    Ok(())
}

/// Name the inserted element after the pad it feeds, so two request pads on one
/// consumer do not collide.
fn download_name(sink: &gst::Pad) -> String {
    match sink.parent_element() {
        Some(element) => format!("{}_{}_gldownload", element.name(), sink.name()),
        None => format!("{}_gldownload", sink.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        let _ = gst::init();
        gst::Caps::from_str(s).expect("valid caps")
    }

    #[test]
    fn a_gl_only_producer_is_a_candidate() {
        assert!(offers_only_gl_memory(&caps(
            "video/x-raw(memory:GLMemory), format=RGBA, width=1280, height=720"
        )));
    }

    /// It would have linked on its own, so the download is not what is missing.
    #[test]
    fn a_producer_that_also_offers_system_memory_is_not() {
        assert!(!offers_only_gl_memory(&caps(
            "video/x-raw(memory:GLMemory), format=RGBA; video/x-raw, format=NV12"
        )));
    }

    /// The consumers that advertise these really do take them, and a
    /// `gldownload` could not link to them anyway.
    #[test]
    fn other_gpu_memory_types_are_left_alone() {
        for feature in [
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
            "memory:DMABuf",
        ] {
            assert!(
                !offers_only_gl_memory(&caps(&format!("video/x-raw({}), format=NV12", feature))),
                "{} should not be downloaded",
                feature
            );
        }
    }

    #[test]
    fn encoded_video_and_audio_are_not_candidates() {
        assert!(!offers_only_gl_memory(&caps("video/x-h264")));
        assert!(!offers_only_gl_memory(&caps("audio/x-raw, rate=48000")));
        assert!(!offers_only_gl_memory(&caps("ANY")));
        assert!(!offers_only_gl_memory(&caps("EMPTY")));
    }

    #[test]
    fn a_system_memory_consumer_can_be_fed() {
        assert!(accepts_system_memory_raw_video(&caps(
            "video/x-raw, format=(string){ NV12, I420 }"
        )));
        assert!(accepts_system_memory_raw_video(&caps("video/x-raw(ANY)")));
        assert!(accepts_system_memory_raw_video(&caps("ANY")));
    }

    /// A `gldownload` produces system memory, so a consumer that takes only GPU
    /// memory, of whatever kind, cannot be fed by one.
    #[test]
    fn a_consumer_that_takes_only_gpu_memory_cannot() {
        for feature in [
            "memory:GLMemory",
            "memory:CUDAMemory",
            "memory:NVMM",
            "memory:D3D11Memory",
            "memory:VAMemory",
        ] {
            assert!(
                !accepts_system_memory_raw_video(&caps(&format!(
                    "video/x-raw({}), format=NV12",
                    feature
                ))),
                "a {} consumer should not be fed through a gldownload",
                feature
            );
        }
    }

    #[test]
    fn a_consumer_that_takes_no_raw_video_cannot() {
        assert!(!accepts_system_memory_raw_video(&caps("EMPTY")));
        assert!(!accepts_system_memory_raw_video(&caps("video/x-h264")));
    }
}
