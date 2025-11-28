//! Audio format block with optional audio property conversion.
//!
//! This block provides a simple way to set common audio properties:
//! - Sample rate - enforced by caps
//! - Channels - enforced by caps (with channel-mask support)
//! - PCM format (bit depth/encoding) - enforced by caps
//!
//! Channel configurations support both:
//! - Positioned channels (surround sound): 4.0 Quad, 5.1, 7.1 with proper channel masks
//! - Unpositioned channels (multi-channel): independent mono channels with channel-mask=0x0
//!
//! All properties are optional. The block always creates a fixed chain of elements:
//! audioresample -> audioconvert -> capsfilter
//!
//! Only the capsfilter caps are set based on which properties are specified.
//! Unspecified properties allow passthrough - elements will not modify those aspects.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue, *};
use tracing::info;

/// Audio Format block builder.
pub struct AudioFormatBuilder;

impl BlockBuilder for AudioFormatBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("🎵 Building AudioFormat block instance: {}", instance_id);

        // Parse optional properties
        let sample_rate = properties.get("sample_rate").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            PropertyValue::Int(i) => Some(match *i {
                8000 => "8000",
                11025 => "11025",
                16000 => "16000",
                22050 => "22050",
                32000 => "32000",
                44100 => "44100",
                48000 => "48000",
                88200 => "88200",
                96000 => "96000",
                176400 => "176400",
                192000 => "192000",
                _ => return None,
            }),
            _ => None,
        });

        // Parse channel configuration: either "N" or "N:0xMASK"
        // N = number of channels, MASK = optional channel mask
        let channels = properties.get("channels").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        });

        let format = properties.get("format").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        });

        // Build caps string dynamically with only specified fields
        let mut caps_fields = vec!["audio/x-raw".to_string()];

        // Add sample rate if specified
        if let Some(rate) = sample_rate {
            caps_fields.push(format!("rate={}", rate));
        }

        // Add channels if specified
        // Format: "N" for channels only, or "N:0xMASK" for channels with channel-mask
        if let Some(ch_config) = channels {
            if let Some((ch, mask)) = ch_config.split_once(':') {
                // Format: "N:0xMASK" - includes channel mask
                caps_fields.push(format!("channels={}", ch));
                caps_fields.push(format!("channel-mask=(bitmask){}", mask));
            } else {
                // Format: "N" - just channel count (for mono/stereo)
                caps_fields.push(format!("channels={}", ch_config));
            }
        }

        // Add format if specified
        if let Some(fmt) = format {
            caps_fields.push(format!("format={}", fmt));
        }

        let caps_str = caps_fields.join(",");
        info!("🎵 AudioFormat block caps: {}", caps_str);

        // Always create all elements for consistent external pad references
        // Elements will just pass through if their respective properties aren't set
        let resample_id = format!("{}:audioresample", instance_id);
        let convert_id = format!("{}:audioconvert", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&resample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        // capsfilter with caps (only constraints specified properties)
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!("🎵 AudioFormat block created (chain: audioresample -> audioconvert -> capsfilter)");

        // Chain: audioresample -> audioconvert -> capsfilter
        let internal_links = vec![
            (
                format!("{}:src", resample_id),
                format!("{}:sink", convert_id),
            ),
            (
                format!("{}:src", convert_id),
                format!("{}:sink", capsfilter_id),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (resample_id, audioresample),
                (convert_id, audioconvert),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
        })
    }
}

/// Get metadata for AudioFormat block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audioformat_definition()]
}

/// Get AudioFormat block definition (metadata only).
fn audioformat_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.audioformat".to_string(),
        name: "Audio Format".to_string(),
        description: "Optional audio format conversion. Set sample rate, channels (with channel-mask support for surround/multi-channel), and/or PCM format as needed. Unset properties pass through unchanged.".to_string(),
        category: "Audio".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "sample_rate".to_string(),
                label: "Sample Rate".to_string(),
                description: "Audio sample rate - creates audioresample element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "".to_string(), label: Some("-".to_string()) },
                        EnumValue { value: "8000".to_string(), label: Some("8 kHz - Telephony".to_string()) },
                        EnumValue { value: "11025".to_string(), label: Some("11.025 kHz - Low Quality".to_string()) },
                        EnumValue { value: "16000".to_string(), label: Some("16 kHz - Wideband".to_string()) },
                        EnumValue { value: "22050".to_string(), label: Some("22.05 kHz - Medium Quality".to_string()) },
                        EnumValue { value: "32000".to_string(), label: Some("32 kHz - Miniature Disc".to_string()) },
                        EnumValue { value: "44100".to_string(), label: Some("44.1 kHz - CD Quality".to_string()) },
                        EnumValue { value: "48000".to_string(), label: Some("48 kHz - Professional".to_string()) },
                        EnumValue { value: "88200".to_string(), label: Some("88.2 kHz - High-Res 2x CD".to_string()) },
                        EnumValue { value: "96000".to_string(), label: Some("96 kHz - High-Res Professional".to_string()) },
                        EnumValue { value: "176400".to_string(), label: Some("176.4 kHz - Very High-Res 4x CD".to_string()) },
                        EnumValue { value: "192000".to_string(), label: Some("192 kHz - Very High-Res Professional".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sample_rate".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "channels".to_string(),
                label: "Channels".to_string(),
                description: "Audio channels with positioning. Surround options (4.0, 5.1, 7.1) use positioned channels. Multi-channel options use unpositioned independent mono channels (channel-mask=0x0). Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "".to_string(), label: Some("-".to_string()) },
                        EnumValue { value: "1".to_string(), label: Some("1 - Mono".to_string()) },
                        EnumValue { value: "2".to_string(), label: Some("2 - Stereo".to_string()) },
                        // Unpositioned multi-channel (independent mono channels)
                        EnumValue { value: "4:0x0".to_string(), label: Some("4 - Multi-channel".to_string()) },
                        EnumValue { value: "8:0x0".to_string(), label: Some("8 - Multi-channel".to_string()) },
                        EnumValue { value: "16:0x0".to_string(), label: Some("16 - Multi-channel".to_string()) },
                        EnumValue { value: "32:0x0".to_string(), label: Some("32 - Multi-channel".to_string()) },
                        EnumValue { value: "64:0x0".to_string(), label: Some("64 - Multi-channel".to_string()) },
                        // Positioned surround sound configurations
                        EnumValue { value: "4:0x33".to_string(), label: Some("4 - Quad Surround".to_string()) },
                        EnumValue { value: "6:0x3f".to_string(), label: Some("6 - 5.1 Surround".to_string()) },
                        EnumValue { value: "8:0xc3f".to_string(), label: Some("8 - 7.1 Surround".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "channels".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "format".to_string(),
                label: "Format".to_string(),
                description: "PCM format/bit depth - creates audioconvert element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "".to_string(), label: Some("-".to_string()) },
                        EnumValue { value: "S8".to_string(), label: Some("Signed 8-bit".to_string()) },
                        EnumValue { value: "U8".to_string(), label: Some("Unsigned 8-bit".to_string()) },
                        EnumValue { value: "S16LE".to_string(), label: Some("Signed 16-bit LE (most common)".to_string()) },
                        EnumValue { value: "S16BE".to_string(), label: Some("Signed 16-bit BE".to_string()) },
                        EnumValue { value: "S24LE".to_string(), label: Some("Signed 24-bit LE (professional)".to_string()) },
                        EnumValue { value: "S24BE".to_string(), label: Some("Signed 24-bit BE".to_string()) },
                        EnumValue { value: "S32LE".to_string(), label: Some("Signed 32-bit LE (high-end)".to_string()) },
                        EnumValue { value: "S32BE".to_string(), label: Some("Signed 32-bit BE".to_string()) },
                        EnumValue { value: "F32LE".to_string(), label: Some("32-bit Float LE".to_string()) },
                        EnumValue { value: "F32BE".to_string(), label: Some("32-bit Float BE".to_string()) },
                        EnumValue { value: "F64LE".to_string(), label: Some("64-bit Float LE".to_string()) },
                        EnumValue { value: "F64BE".to_string(), label: Some("64-bit Float BE".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "format".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioresample".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎵".to_string()),
            color: Some("#9C27B0".to_string()), // Purple for audio
            width: Some(1.5),
            height: Some(2.0),
        }),
    }
}
