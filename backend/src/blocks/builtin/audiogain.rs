//! Simple serial audio gain block with volume, mute, and polarity inversion.
//!
//! A lightweight inline audio processing block for minor adjustments.
//! All properties can be changed at runtime without restarting the flow.
//!
//! The gain property is stored in dB and converted to linear scale when
//! applied to the GStreamer volume element (both at build time and runtime).
//! The invert property is stored as a bool and converted to audioamplify
//! amplification (1.0 = normal, -1.0 = inverted).
//!
//! Chain: volume → audioamplify

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::info;

/// Convert dB to linear scale.
fn db_to_linear(db: f64) -> f64 {
    if db <= -60.0 {
        0.0
    } else {
        10.0_f64.powf(db / 20.0)
    }
}

/// Translate audiogain properties for runtime updates.
///
/// Called by the pipeline property update path. Returns translated
/// (property_name, value) pairs, or empty vec if no translation needed.
///
/// Handles two conversions:
/// - gain_volume element: dB → linear for the "volume" property
/// - gain_invert element: bool → amplification (1.0 or -1.0)
pub fn translate_property(
    element_id: &str,
    prop_name: &str,
    value: &PropertyValue,
) -> Vec<(String, PropertyValue)> {
    // Gain: dB → linear on the gain_volume element
    if prop_name == "volume" && element_id.ends_with(":gain_volume") {
        if let PropertyValue::Float(db) = value {
            let linear = db_to_linear(*db);
            return vec![("volume".to_string(), PropertyValue::Float(linear))];
        }
    }

    // Invert: bool → amplification on the gain_invert element
    if prop_name == "amplification" && element_id.ends_with(":gain_invert") {
        if let PropertyValue::Bool(inverted) = value {
            let amp: f64 = if *inverted { -1.0 } else { 1.0 };
            return vec![("amplification".to_string(), PropertyValue::Float(amp))];
        }
    }

    vec![]
}

/// Audio Gain block builder.
pub struct AudioGainBuilder;

impl BlockBuilder for AudioGainBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building AudioGain block instance: {}", instance_id);

        // Gain is stored as dB, convert to linear for GStreamer
        let gain_db = properties
            .get("gain")
            .and_then(|v| match v {
                PropertyValue::Float(f) => Some(*f),
                _ => None,
            })
            .unwrap_or(0.0);
        let gain_linear = db_to_linear(gain_db);

        let mute = properties
            .get("mute")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        // Invert is stored as bool, convert to amplification for audioamplify
        let inverted = properties
            .get("invert")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);
        let amplification: f32 = if inverted { -1.0 } else { 1.0 };

        info!(
            "AudioGain properties: gain_db={}, gain_linear={}, mute={}, inverted={}",
            gain_db, gain_linear, mute, inverted
        );

        // Create volume element (named "gain_volume" to distinguish from mixer volume elements)
        let volume_id = format!("{}:gain_volume", instance_id);
        let volume_elem = gst::ElementFactory::make("volume")
            .name(&volume_id)
            .property("volume", gain_linear)
            .property("mute", mute)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("volume: {}", e)))?;

        // Create audioamplify element for polarity inversion
        let invert_id = format!("{}:gain_invert", instance_id);
        let invert_elem = gst::ElementFactory::make("audioamplify")
            .name(&invert_id)
            .property("amplification", amplification)
            .property_from_str("clipping-method", "none")
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioamplify: {}", e)))?;

        info!("AudioGain block created (chain: volume -> audioamplify)");

        // Chain: volume -> audioamplify
        let internal_links = vec![(
            ElementPadRef::pad(&volume_id, "src"),
            ElementPadRef::pad(&invert_id, "sink"),
        )];

        Ok(BlockBuildResult {
            elements: vec![(volume_id, volume_elem), (invert_id, invert_elem)],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for AudioGain block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audiogain_definition()]
}

/// Get AudioGain block definition (metadata only).
fn audiogain_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.audiogain".to_string(),
        name: "Audio Gain".to_string(),
        description:
            "Simple audio gain with volume, mute, and polarity inversion. All properties can be changed at runtime."
                .to_string(),
        category: "Audio".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "gain".to_string(),
                label: "Gain (dB)".to_string(),
                description: "Audio gain in dB. 0 dB = unity, -60 dB = silence, +20 dB = max."
                    .to_string(),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(0.0)),
                mapping: PropertyMapping {
                    element_id: "gain_volume".to_string(),
                    property_name: "volume".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "mute".to_string(),
                label: "Mute".to_string(),
                description: "Mute audio output.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "gain_volume".to_string(),
                    property_name: "mute".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "invert".to_string(),
                label: "Polarity Invert".to_string(),
                description: "Invert audio polarity (180° phase flip).".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "gain_invert".to_string(),
                    property_name: "amplification".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "gain_volume".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "gain_invert".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("\u{E228}".to_string()),
            width: Some(1.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}
