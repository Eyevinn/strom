//! RTP statistics collection from GStreamer jitterbuffer elements.

use gstreamer as gst;
use gstreamer::prelude::*;
use strom_types::RtpJitterbufferStats;
use tracing::{debug, warn};

/// Collect RTP jitterbuffer statistics from an rtpjitterbuffer element.
///
/// The rtpjitterbuffer element exposes a "stats" property containing:
/// - num-pushed, num-lost, num-late, num-duplicates
/// - avg-jitter
/// - rtx-count, rtx-success-count, rtx-per-packet, rtx-rtt
pub fn collect_rtp_jitterbuffer_stats(element: &gst::Element) -> Option<RtpJitterbufferStats> {
    let factory = element.factory()?;
    let factory_name = factory.name();

    if factory_name != "rtpjitterbuffer" {
        warn!("Expected rtpjitterbuffer element, got {}", factory_name);
        return None;
    }

    // Get the stats property - it's a GstStructure
    let stats: gst::Structure = element.property("stats");

    debug!("RTP jitterbuffer stats structure: {:?}", stats);

    // Extract values from the structure
    let num_pushed = stats.get::<u64>("num-pushed").unwrap_or(0);
    let num_lost = stats.get::<u64>("num-lost").unwrap_or(0);
    let num_late = stats.get::<u64>("num-late").unwrap_or(0);
    let num_duplicates = stats.get::<u64>("num-duplicates").unwrap_or(0);
    let avg_jitter = stats.get::<u64>("avg-jitter").unwrap_or(0);
    let rtx_count = stats.get::<u64>("rtx-count").unwrap_or(0);
    let rtx_success_count = stats.get::<u64>("rtx-success-count").unwrap_or(0);
    let rtx_per_packet = stats.get::<f64>("rtx-per-packet").unwrap_or(0.0);
    let rtx_rtt = stats.get::<u64>("rtx-rtt").unwrap_or(0);
    let latency_ms = element.property::<u32>("latency") as u64;

    Some(RtpJitterbufferStats {
        num_pushed,
        num_lost,
        num_late,
        num_duplicates,
        avg_jitter_ns: avg_jitter,
        rtx_count,
        rtx_success_count,
        rtx_per_packet,
        rtx_rtt_ns: rtx_rtt,
        latency_ms,
    })
}

/// The medium a caps describes: the RTP `media` field where there is one, and
/// otherwise the first half of a media type such as `audio/x-opus`.
pub fn media_kind_from_caps(caps: &gst::Caps) -> Option<String> {
    let structure = caps.structure(0)?;
    if let Ok(media) = structure.get::<String>("media") {
        return Some(media);
    }
    match structure.name().split('/').next() {
        Some(kind @ ("audio" | "video")) => Some(kind.to_string()),
        _ => None,
    }
}

/// Which medium a jitterbuffer is carrying.
///
/// A session has one jitterbuffer per medium and they are named by creation
/// order, so the name alone cannot tell an operator which seat's audio is
/// arriving late. The caps on the buffer's own pads cannot either: inside
/// `webrtcbin` the RTP caps reaching it are stripped to an SSRC. So follow the
/// stream downstream until caps appear that name a medium, which in practice is
/// the depayloader's output.
pub fn jitterbuffer_media_kind(element: &gst::Element) -> Option<String> {
    let mut pad = first_linked_src_pad(element)?;
    for _ in 0..MEDIA_KIND_MAX_HOPS {
        if let Some(kind) = pad.current_caps().as_ref().and_then(media_kind_from_caps) {
            return Some(kind);
        }
        let peer = pad.peer()?;
        if let Some(kind) = peer.current_caps().as_ref().and_then(media_kind_from_caps) {
            return Some(kind);
        }
        pad = first_linked_src_pad(&peer.parent_element()?)?;
    }
    None
}

/// How far downstream of a jitterbuffer to look for caps that name a medium.
/// Enough to clear `rtpptdemux`, the bin boundary and the depayloader.
const MEDIA_KIND_MAX_HOPS: usize = 8;

/// The element's first source pad that is linked to something.
///
/// Demuxers expose their source pads only once a stream appears, so the pad
/// cannot be looked up by name.
fn first_linked_src_pad(element: &gst::Element) -> Option<gst::Pad> {
    element
        .iterate_src_pads()
        .into_iter()
        .flatten()
        .find(|pad| pad.is_linked())
}

/// Find rtpjitterbuffer elements within an sdpdemux element.
///
/// sdpdemux creates rtpbin which in turn creates rtpjitterbuffer elements.
/// We need to traverse the bin hierarchy to find them.
pub fn find_jitterbuffers_in_bin(bin: &gst::Bin) -> Vec<gst::Element> {
    let mut jitterbuffers = Vec::new();

    // Iterate through all elements in the bin
    for element in bin.iterate_elements().into_iter().flatten() {
        if let Some(factory) = element.factory() {
            let factory_name = factory.name();
            if factory_name == "rtpjitterbuffer" {
                jitterbuffers.push(element.clone());
            }
        }

        // If this element is also a bin, recurse into it
        if let Ok(sub_bin) = element.clone().dynamic_cast::<gst::Bin>() {
            jitterbuffers.extend(find_jitterbuffers_in_bin(&sub_bin));
        }
    }

    jitterbuffers
}

/// Collect all RTP jitterbuffer stats from a bin (like sdpdemux).
pub fn collect_all_jitterbuffer_stats(bin: &gst::Bin) -> Vec<(String, RtpJitterbufferStats)> {
    let jitterbuffers = find_jitterbuffers_in_bin(bin);

    jitterbuffers
        .into_iter()
        .filter_map(|jb| {
            let name = jb.name().to_string();
            collect_rtp_jitterbuffer_stats(&jb).map(|stats| (name, stats))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_jitterbuffers_empty_bin() {
        gst::init().unwrap();
        let bin = gst::Bin::new();
        let jitterbuffers = find_jitterbuffers_in_bin(&bin);
        assert!(jitterbuffers.is_empty());
    }

    /// The measured jitter is only actionable next to the buffer it is measured
    /// against, so the configured size travels with the stats.
    #[test]
    fn stats_carry_the_configured_buffer_size() {
        gst::init().unwrap();
        let jb = gst::ElementFactory::make("rtpjitterbuffer")
            .property("latency", 150u32)
            .build()
            .expect("rtpjitterbuffer is in gst-plugins-good");

        let stats = collect_rtp_jitterbuffer_stats(&jb).expect("stats for an rtpjitterbuffer");
        assert_eq!(stats.latency_ms, 150);
    }

    #[test]
    fn stats_are_refused_for_a_non_jitterbuffer() {
        gst::init().unwrap();
        let fakesrc = gst::ElementFactory::make("fakesrc").build().unwrap();
        assert!(collect_rtp_jitterbuffer_stats(&fakesrc).is_none());
    }

    /// The walk is the part that is not obvious: inside `webrtcbin` the medium
    /// is only visible some hops downstream of the buffer itself.
    #[test]
    fn media_kind_is_found_downstream_of_the_element() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc").build().unwrap();
        let queue = gst::ElementFactory::make("queue").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add_many([&src, &queue, &sink]).unwrap();
        gst::Element::link_many([&src, &queue, &sink]).unwrap();

        pipeline.set_state(gst::State::Paused).unwrap();
        let _ = pipeline.state(gst::ClockTime::from_seconds(5));

        // Two hops from `src`, so the caps are not on its own pad.
        assert_eq!(jitterbuffer_media_kind(&src).as_deref(), Some("audio"));

        pipeline.set_state(gst::State::Null).unwrap();
    }

    #[test]
    fn media_kind_is_none_for_an_unlinked_element() {
        gst::init().unwrap();
        let jb = gst::ElementFactory::make("rtpjitterbuffer")
            .build()
            .unwrap();
        assert_eq!(jitterbuffer_media_kind(&jb), None);
    }

    #[test]
    fn media_kind_comes_from_the_rtp_caps() {
        gst::init().unwrap();
        let caps = gst::Caps::builder("application/x-rtp")
            .field("media", "audio")
            .build();
        assert_eq!(media_kind_from_caps(&caps).as_deref(), Some("audio"));

        let bare = gst::Caps::builder("application/x-rtp").build();
        assert_eq!(media_kind_from_caps(&bare), None);

        // Past the depayloader the medium is the media type itself.
        let decoded = gst::Caps::builder("audio/x-opus").build();
        assert_eq!(media_kind_from_caps(&decoded).as_deref(), Some("audio"));
    }
}
