//! WHEP Input - receives streams from external WHEP servers.
//!
//! - `whepclientsrc` (new): uses the signaller interface.
//! - `whepsrc` (stable): simpler implementation with direct properties.
//!
//! Handles dynamic pad creation: every audio stream is decoded into a
//! liveadder mixer that feeds `audio_out`. Video streams take the declared
//! video outputs (`num_video_tracks`) as the video module routes them, and
//! are decoded only into an output that is linked; any other video is
//! discarded undecoded.

pub(crate) mod definition;
mod slot_plan;
mod stream;
mod video;
mod whepclientsrc;
mod whepsrc;

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::ice_preflight;
use std::collections::HashMap;
use strom_types::block::ExternalPads;
use strom_types::PropertyValue;
use tracing::debug;

use whepclientsrc::build_whepclientsrc;
use whepsrc::build_whepsrc;

/// WHEP Input block builder.
pub struct WHEPInputBuilder;

impl BlockBuilder for WHEPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Input block instance: {}", instance_id);
        ice_preflight::require_ice_elements("WHEP Input")?;

        // Get implementation choice (default to stable whepsrc)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whepclientsrc")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whepclientsrc(instance_id, properties, ctx)
        } else {
            build_whepsrc(instance_id, properties, ctx)
        }
    }

    /// `audio_out` always (its internal element is unchanged, so existing
    /// links survive), then one video output per declared video track.
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mut outputs = definition::whep_input_definition().external_pads.outputs;
        outputs.extend(video::external_pads(video::num_video_tracks(properties)));
        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_names(properties: &HashMap<String, PropertyValue>) -> Vec<String> {
        WHEPInputBuilder
            .get_external_pads(properties)
            .expect("WHEP Input has dynamic pads")
            .outputs
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    fn video_tracks(n: u64) -> HashMap<String, PropertyValue> {
        [(video::NUM_VIDEO_TRACKS.to_string(), PropertyValue::UInt(n))].into()
    }

    /// A flow saved before video output existed has no `num_video_tracks`
    /// and must keep exactly the pads it had.
    #[test]
    fn default_has_only_audio_out() {
        assert_eq!(output_names(&HashMap::new()), vec!["audio_out"]);
    }

    #[test]
    fn one_video_track_adds_video_out() {
        assert_eq!(
            output_names(&video_tracks(1)),
            vec!["audio_out", "video_out"]
        );
    }

    #[test]
    fn two_video_tracks_add_numbered_outputs() {
        assert_eq!(
            output_names(&video_tracks(2)),
            vec!["audio_out", "video_out", "video_out_1"]
        );
    }

    /// The property's schema default must agree with the builder's: 0.
    #[test]
    fn definition_defaults_to_no_video() {
        let def = definition::whep_input_definition();
        let prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == video::NUM_VIDEO_TRACKS)
            .expect("num_video_tracks is exposed");
        assert!(matches!(prop.default_value, Some(PropertyValue::UInt(0))));
    }
}
