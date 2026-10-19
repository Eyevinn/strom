//! WHIP Output - sends media to an external WHIP server.
//!
//! - `whipsink` (legacy, default): webrtcbin-based, takes RTP. The block
//!   encodes audio to Opus and payloads it, and payloads encoded video.
//! - `whipclientsink` (new): webrtcsink-based, uses the signaller interface and
//!   payloads (and for raw audio, encodes) internally.
//!
//! Audio arrives raw and is encoded inside the block. Video must arrive encoded
//! (H.264): codec, profile and bitrate are the operator's choice, made in a
//! `builtin.videoenc` block in front of this one. Raw video and other codecs
//! fail the flow with an error that says so; see [`video`].
//!
//! # Request pads only for linked inputs
//!
//! Both sinks negotiate one session over every request pad they have, and wait
//! for each pad's caps before they do. A declared track that nothing feeds would
//! hold the whole session back, audio included. So the sink pads are requested
//! in a `register_element_setup` hook, the window after construction has linked
//! every block and before the pipeline leaves NULL, and only for the inputs that
//! are linked by then. See [`input_is_connected`] for the one kind of
//! link that window cannot see.

pub(crate) mod definition;
mod video;
mod whipclientsink;
mod whipsink;

use crate::blocks::builtin::inputs::{explicit_track_count, input_is_connected};
use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, MediaType, PropertyValue};
use tracing::{debug, error, info, warn};

use whipclientsink::build_whipclientsink;
use whipsink::build_whipsink;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

impl BlockBuilder for WHIPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Output block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHIP Output")?;

        // Get implementation choice (default to stable whipsink)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whipclientsink")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whipclientsink(instance_id, properties, ctx)
        } else {
            build_whipsink(instance_id, properties, ctx)
        }
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
        let mut inputs = Vec::new();

        for slot in 0..num_video_tracks {
            inputs.push(ExternalPad {
                label: (num_audio_tracks > 0 || num_video_tracks > 1).then(|| format!("V{}", slot)),
                name: slot_name("video_in", slot),
                media_type: MediaType::Video,
                internal_element_id: slot_name(video::INPUT_ELEMENT, slot),
                internal_pad_name: "sink".to_string(),
            });
        }
        for slot in 0..num_audio_tracks {
            inputs.push(ExternalPad {
                label: (num_video_tracks > 0 || num_audio_tracks > 1).then(|| format!("A{}", slot)),
                name: slot_name("audio_in", slot),
                media_type: MediaType::Audio,
                internal_element_id: slot_name(AUDIO_INPUT_ELEMENT, slot),
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }
}

/// The first element of each audio track's chain, which its `audio_in` pad feeds.
const AUDIO_INPUT_ELEMENT: &str = "audioconvert";

/// Resolve `(num_audio_tracks, num_video_tracks)` from the block properties.
///
/// Defaults are one audio track and no video: the block was audio only before
/// it took video, and saved flows do not store properties at their default
/// value, so these defaults are what keeps every existing flow unchanged.
pub(super) fn resolve_track_counts(properties: &HashMap<String, PropertyValue>) -> (usize, usize) {
    (
        explicit_track_count(properties, "num_audio_tracks").unwrap_or(1),
        explicit_track_count(properties, "num_video_tracks").unwrap_or(0),
    )
}

/// `base` for slot 0, `base_N` for the others.
///
/// Slot 0 keeps the unsuffixed name, so the `audio_in` pad and the element IDs
/// of a flow saved before the block had track counts still resolve.
pub(super) fn slot_name(base: &str, slot: usize) -> String {
    if slot == 0 {
        base.to_string()
    } else {
        format!("{}_{}", base, slot)
    }
}

/// The ID of an element inside this block instance.
pub(super) fn element_id(instance_id: &str, base: &str, slot: usize) -> String {
    format!("{}:{}", instance_id, slot_name(base, slot))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackKind {
    Audio,
    Video,
}

impl TrackKind {
    fn as_str(self) -> &'static str {
        match self {
            TrackKind::Audio => "audio",
            TrackKind::Video => "video",
        }
    }
}

/// One input track, as the setup hook needs it.
pub(super) struct Track {
    pub kind: TrackKind,
    pub slot: usize,
    /// The element the track's external pad feeds. Linked means connected.
    pub input: gst::glib::WeakRef<gst::Element>,
    /// The element whose src pad goes to the sink's request pad.
    pub tail: gst::glib::WeakRef<gst::Element>,
    /// `whipsink` only: the RTP payloader. Every stream in the session gets its
    /// own payload type.
    pub payloader: Option<gst::glib::WeakRef<gst::Element>>,
}

/// Which sink the block feeds, which decides its request pad names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sink {
    /// `sink_%u`, numbered across every stream.
    WhipSink,
    /// `audio_%u` and `video_%u`, numbered per media type.
    WhipClientSink,
}

/// The payload type of the n-th stream on `whipsink`. The first is 96, the
/// payloaders' default, so an audio-only flow sends what it always did.
const FIRST_DYNAMIC_PT: u32 = 96;

/// Request a sink pad for every linked track and link the track into it, from
/// the setup hook. See the module docs for why it waits until then.
pub(super) fn register_sink_pad_requests(
    ctx: &BlockBuildContext,
    instance_id: &str,
    sink_element: &gst::Element,
    sink: Sink,
    tracks: Vec<Track>,
) {
    let sink_weak = sink_element.downgrade();
    let instance_id = instance_id.to_string();
    ctx.register_element_setup(Box::new(move |_flow_id, _events| {
        let Some(sink_element) = sink_weak.upgrade() else {
            return;
        };
        let mut linked = 0usize;
        let mut linked_audio = 0usize;
        let mut linked_video = 0usize;

        for track in &tracks {
            if !input_is_connected(&track.input) {
                info!(
                    "WHIP Output {}: {} track {} is not connected, requesting no sink pad for it",
                    instance_id,
                    track.kind.as_str(),
                    track.slot
                );
                continue;
            }
            let pad_name = match (sink, track.kind) {
                (Sink::WhipSink, _) => format!("sink_{}", linked),
                (Sink::WhipClientSink, TrackKind::Audio) => format!("audio_{}", linked_audio),
                (Sink::WhipClientSink, TrackKind::Video) => format!("video_{}", linked_video),
            };
            if let Some(payloader) = track.payloader.as_ref().and_then(|p| p.upgrade()) {
                payloader.set_property("pt", FIRST_DYNAMIC_PT + linked as u32);
            }
            let Some(sink_pad) = sink_element.request_pad_simple(&pad_name) else {
                error!(
                    "WHIP Output {}: {} refused a '{}' pad, {} track {} will not be sent",
                    instance_id,
                    sink_element.name(),
                    pad_name,
                    track.kind.as_str(),
                    track.slot
                );
                continue;
            };
            let Some(src_pad) = track.tail.upgrade().and_then(|t| t.static_pad("src")) else {
                sink_element.release_request_pad(&sink_pad);
                continue;
            };
            if let Err(e) = src_pad.link(&sink_pad) {
                error!(
                    "WHIP Output {}: could not link {} track {} to {}: {:?}",
                    instance_id,
                    track.kind.as_str(),
                    track.slot,
                    pad_name,
                    e
                );
                sink_element.release_request_pad(&sink_pad);
                continue;
            }
            debug!(
                "WHIP Output {}: {} track {} linked to {}",
                instance_id,
                track.kind.as_str(),
                track.slot,
                pad_name
            );
            linked += 1;
            match track.kind {
                TrackKind::Audio => linked_audio += 1,
                TrackKind::Video => linked_video += 1,
            }
        }

        if sink == Sink::WhipClientSink && linked_video == 0 {
            // No video stream: offer no video codecs.
            sink_element.set_property("video-caps", gst::Caps::new_empty());
        }
        if linked == 0 {
            warn!(
                "WHIP Output {}: no input is connected, so there is nothing to publish",
                instance_id
            );
        } else {
            info!(
                "WHIP Output {}: publishing {} audio and {} video stream(s)",
                instance_id, linked_audio, linked_video
            );
        }
    }));
}

/// Make `factory` named `id`, or say which element could not be made.
pub(super) fn make(factory: &str, id: &str) -> Result<gst::Element, BlockBuildError> {
    gst::ElementFactory::make(factory)
        .name(id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(audio: Option<u64>, video: Option<u64>) -> HashMap<String, PropertyValue> {
        let mut p = HashMap::new();
        if let Some(a) = audio {
            p.insert("num_audio_tracks".to_string(), PropertyValue::UInt(a));
        }
        if let Some(v) = video {
            p.insert("num_video_tracks".to_string(), PropertyValue::UInt(v));
        }
        p
    }

    fn names(pads: &ExternalPads, media: MediaType) -> Vec<String> {
        pads.inputs
            .iter()
            .filter(|p| p.media_type == media)
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn defaults_keep_the_block_audio_only_with_the_existing_pad() {
        let pads = WHIPOutputBuilder
            .get_external_pads(&props(None, None))
            .expect("pads");
        assert_eq!(names(&pads, MediaType::Audio), vec!["audio_in"]);
        assert!(names(&pads, MediaType::Video).is_empty());
        // The pad saved flows link to keeps its internal element too.
        assert_eq!(pads.inputs[0].internal_element_id, "audioconvert");
        assert_eq!(pads.inputs[0].label, None);
    }

    #[test]
    fn video_tracks_add_video_pads_and_keep_audio_in() {
        let pads = WHIPOutputBuilder
            .get_external_pads(&props(None, Some(2)))
            .expect("pads");
        assert_eq!(names(&pads, MediaType::Audio), vec!["audio_in"]);
        assert_eq!(
            names(&pads, MediaType::Video),
            vec!["video_in", "video_in_1"]
        );
    }

    #[test]
    fn more_audio_tracks_suffix_from_slot_one() {
        let pads = WHIPOutputBuilder
            .get_external_pads(&props(Some(3), Some(0)))
            .expect("pads");
        assert_eq!(
            names(&pads, MediaType::Audio),
            vec!["audio_in", "audio_in_1", "audio_in_2"]
        );
        let ids: Vec<&str> = pads
            .inputs
            .iter()
            .map(|p| p.internal_element_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["audioconvert", "audioconvert_1", "audioconvert_2"]
        );
    }

    #[test]
    fn the_static_definition_matches_the_default_pads() {
        let definition = definition::whip_output_definition();
        let computed = WHIPOutputBuilder
            .get_external_pads(&HashMap::new())
            .expect("pads");
        assert_eq!(definition.external_pads.inputs.len(), computed.inputs.len());
        for (a, b) in definition
            .external_pads
            .inputs
            .iter()
            .zip(computed.inputs.iter())
        {
            assert_eq!(a.name, b.name);
            assert_eq!(a.internal_element_id, b.internal_element_id);
        }
    }
}
