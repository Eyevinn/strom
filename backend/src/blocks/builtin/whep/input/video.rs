//! WHEP Input video outputs: one `tee` per declared video slot.
//!
//! The block declares `num_video_tracks` slots at build time. Each slot is a
//! static `tee` the external pad maps to (`video_out`, `video_out_1`, ...), so
//! a consumer can link to it before any WebRTC session exists. Incoming video
//! streams claim the lowest free slot; [`super::stream`] decodes a stream
//! into its slot's tee only when that slot's output is linked, and otherwise
//! discards it undecoded, as streams beyond the declared slots always are.
//! When a stream's source pad goes away (a session that ends and starts again
//! within one run), its slot is released and its decoder removed, so the
//! stream that replaces it gets the same slot and output.

use crate::blocks::builtin::whep::explicit_track_count;
use crate::blocks::{BlockBuildContext, BlockBuildError};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::block::ExternalPad;
use strom_types::{MediaType, PropertyValue};
use tracing::{debug, info};

/// Property holding the number of video outputs. Default 0: a flow saved
/// before video output existed stores no value and keeps its audio-only pads.
pub(super) const NUM_VIDEO_TRACKS: &str = "num_video_tracks";

/// Declared video slots, from `num_video_tracks` (0..=8, default 0).
pub(super) fn num_video_tracks(properties: &HashMap<String, PropertyValue>) -> usize {
    explicit_track_count(properties, NUM_VIDEO_TRACKS).unwrap_or(0)
}

/// Internal element ID of a slot's output tee.
pub(super) fn tee_id(slot: usize) -> String {
    format!("video_out_tee_{}", slot)
}

/// External pad name of a slot. Slot 0 is unsuffixed, like every other
/// block's first output.
fn pad_name(slot: usize) -> String {
    if slot == 0 {
        "video_out".to_string()
    } else {
        format!("video_out_{}", slot)
    }
}

/// External video output pads for `num` slots.
pub(super) fn external_pads(num: usize) -> Vec<ExternalPad> {
    (0..num)
        .map(|slot| ExternalPad {
            label: (num > 1).then(|| format!("V{}", slot)),
            name: pad_name(slot),
            media_type: MediaType::Video,
            internal_element_id: tee_id(slot),
            internal_pad_name: "src_%u".to_string(),
        })
        .collect()
}

/// One declared video output.
struct Slot {
    tee: gst::glib::WeakRef<gst::Element>,
    /// Whether the slot's output is linked to anything. Decided once, by the
    /// element-setup hook, after the flow is linked and before it starts.
    connected: AtomicBool,
    /// Whether a stream holds the slot.
    taken: AtomicBool,
}

/// The block's video slots, shared with the pad-added path.
pub(super) struct VideoSlots {
    slots: Vec<Slot>,
}

/// Where an incoming video stream goes.
pub(super) enum VideoRoute {
    /// Decode into this slot's tee.
    Decode { slot: usize, tee: gst::Element },
    /// Discard without decoding. `slot` is the slot the stream claimed, or
    /// `None` when it arrived after every declared slot was taken.
    Discard { slot: Option<usize> },
}

impl VideoSlots {
    /// Claim the lowest free slot for a new video stream. Streams that arrive
    /// together take slots in arrival order.
    pub(super) fn claim(&self) -> VideoRoute {
        let Some((index, slot)) = self.slots.iter().enumerate().find(|(_, slot)| {
            slot.taken
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }) else {
            return VideoRoute::Discard { slot: None };
        };
        if !slot.connected.load(Ordering::SeqCst) {
            return VideoRoute::Discard { slot: Some(index) };
        }
        match slot.tee.upgrade() {
            Some(tee) => VideoRoute::Decode { slot: index, tee },
            None => VideoRoute::Discard { slot: Some(index) },
        }
    }

    /// Slots over `tees`, each with whether its output counts as linked.
    #[cfg(test)]
    pub(super) fn for_test(tees: &[(&gst::Element, bool)]) -> Self {
        VideoSlots {
            slots: tees
                .iter()
                .map(|(tee, connected)| Slot {
                    tee: tee.downgrade(),
                    connected: AtomicBool::new(*connected),
                    taken: AtomicBool::new(false),
                })
                .collect(),
        }
    }

    /// Free `slot` for the next stream, once the stream that held it is gone.
    pub(super) fn release(&self, slot: usize) {
        if let Some(slot) = self.slots.get(slot) {
            slot.taken.store(false, Ordering::SeqCst);
        }
    }
}

/// Namespaced block elements, as `BlockBuildResult::elements` holds them.
type NamedElements = Vec<(String, gst::Element)>;

/// Whether anything downstream is linked to the tee's request src pads.
fn output_is_connected(tee: &gst::glib::WeakRef<gst::Element>) -> bool {
    tee.upgrade()
        .map(|t| t.src_pads().iter().any(|p| p.is_linked()))
        .unwrap_or(false)
}

/// Build the output tee for each declared video slot and register the hook
/// that records which of them the flow links.
///
/// Returns the namespaced elements to add to the block and the shared slot
/// state for the pad-added path.
pub(super) fn build_video_slots(
    instance_id: &str,
    num: usize,
    ctx: &BlockBuildContext,
) -> Result<(NamedElements, Arc<VideoSlots>), BlockBuildError> {
    let mut elements = Vec::with_capacity(num);
    let mut slots = Vec::with_capacity(num);
    for slot in 0..num {
        let id = format!("{}:{}", instance_id, tee_id(slot));
        // allow-not-linked: a flow may link the pad to nothing yet, and a
        // consumer can come and go without stopping the stream.
        let tee = gst::ElementFactory::make("tee")
            .name(&id)
            .property("allow-not-linked", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", tee_id(slot), e)))?;
        slots.push(Slot {
            tee: tee.downgrade(),
            connected: AtomicBool::new(false),
            taken: AtomicBool::new(false),
        });
        elements.push((id, tee));
    }
    let video = Arc::new(VideoSlots { slots });

    if num > 0 {
        // Runs after the flow is linked and before the pipeline leaves NULL,
        // so connectivity is known before any WebRTC stream can arrive.
        let video_for_setup = Arc::clone(&video);
        let instance = instance_id.to_string();
        ctx.register_element_setup(Box::new(move |_flow_id, _events| {
            for (index, slot) in video_for_setup.slots.iter().enumerate() {
                let connected = output_is_connected(&slot.tee);
                slot.connected.store(connected, Ordering::SeqCst);
                if connected {
                    info!(
                        "WHEP Input {}: {} is linked, its video will be decoded",
                        instance,
                        pad_name(index)
                    );
                } else {
                    debug!(
                        "WHEP Input {}: {} is not linked, its video will be discarded undecoded",
                        instance,
                        pad_name(index)
                    );
                }
            }
        }));
    }

    Ok((elements, video))
}

/// Configure keyframe recovery on an element the video `decodebin` plugs.
///
/// The decodebin sits in the same pipeline as the WebRTC source, so an
/// upstream force-key-unit event from it reaches `webrtcbin`, whose RTP
/// session turns it into a PLI to the sender. Without a request a sender
/// sends no further keyframe after packet loss, and the picture stays broken
/// until the session ends.
/// - Depayloaders ask on packet loss (`request-keyframe`).
/// - Decoders ask when they output a corrupted frame, at most once a second.
///   They keep decoding meanwhile (`corrupt-output` only, not
///   `discard-input`), so loss shows as a smeared picture that repairs itself,
///   not a freeze.
pub(super) fn configure_keyframe_recovery(element: &gst::Element) {
    if element.has_property("request-keyframe") {
        element.set_property("request-keyframe", true);
    }
    if element.has_property("automatic-request-sync-points") {
        element.set_property("automatic-request-sync-points", true);
        if element.has_property("automatic-request-sync-point-flags") {
            element.set_property_from_str("automatic-request-sync-point-flags", "corrupt-output");
        }
        if element.has_property("min-force-key-unit-interval") {
            element.set_property("min-force-key-unit-interval", 1_000_000_000u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_tracks_default_to_none() {
        assert_eq!(num_video_tracks(&HashMap::new()), 0);
    }

    #[test]
    fn video_tracks_are_clamped() {
        let props: HashMap<String, PropertyValue> =
            [(NUM_VIDEO_TRACKS.to_string(), PropertyValue::UInt(50))].into();
        assert_eq!(num_video_tracks(&props), 8);
    }

    #[test]
    fn first_slot_is_unsuffixed() {
        let pads = external_pads(2);
        assert_eq!(pads[0].name, "video_out");
        assert_eq!(pads[0].internal_element_id, "video_out_tee_0");
        assert_eq!(pads[1].name, "video_out_1");
        assert_eq!(pads[1].internal_element_id, "video_out_tee_1");
    }

    fn slots(connected: &[bool]) -> (Vec<gst::Element>, VideoSlots) {
        gst::init().unwrap();
        let tees: Vec<gst::Element> = connected
            .iter()
            .map(|_| gst::ElementFactory::make("tee").build().unwrap())
            .collect();
        let pairs: Vec<(&gst::Element, bool)> =
            tees.iter().zip(connected.iter().copied()).collect();
        let video = VideoSlots::for_test(&pairs);
        (tees, video)
    }

    /// Streams take slots in arrival order; an unlinked slot still consumes
    /// its stream, so the next stream goes to the next slot, not this one.
    #[test]
    fn streams_claim_slots_in_arrival_order() {
        let (_tees, video) = slots(&[false, true]);
        assert!(matches!(
            video.claim(),
            VideoRoute::Discard { slot: Some(0) }
        ));
        assert!(matches!(video.claim(), VideoRoute::Decode { slot: 1, .. }));
        assert!(matches!(video.claim(), VideoRoute::Discard { slot: None }));
    }

    /// A stream that ends frees its slot, and the stream that replaces it
    /// takes the same slot, not one past the end.
    #[test]
    fn a_released_slot_is_claimed_again() {
        let (_tees, video) = slots(&[true, true]);
        assert!(matches!(video.claim(), VideoRoute::Decode { slot: 0, .. }));
        assert!(matches!(video.claim(), VideoRoute::Decode { slot: 1, .. }));
        video.release(0);
        assert!(matches!(video.claim(), VideoRoute::Decode { slot: 0, .. }));
        assert!(matches!(video.claim(), VideoRoute::Discard { slot: None }));
    }
}
