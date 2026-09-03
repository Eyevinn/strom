//! Audio encoder block. Codecs: AAC (default), Opus, MP3, AC-3.
//!
//! Chain: audioconvert -> audioresample -> capsfilter -> encoder -> parser -> capsfilter
//!
//! The rate capsfilter pins sample rate and channels when configured, the parser gives
//! downstream muxers framed data, and the final capsfilter pins the output codec.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, *};
use tracing::{info, warn};

/// Audio Encoder block builder.
pub struct AudioEncBuilder;

/// Default target bitrate in kbps.
const DEFAULT_BITRATE_KBPS: u32 = 128;

/// Default output sample rate. WebRTC ingest is 48 kHz and Opus accepts only a fixed
/// set of rates, so 48000 keeps every codec negotiable.
const DEFAULT_SAMPLE_RATE: &str = "48000";

/// Sample rates Opus can encode. Anything else has to be resampled first.
const OPUS_SAMPLE_RATES: [i32; 5] = [8000, 12000, 16000, 24000, 48000];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Codec {
    Aac,
    Opus,
    Mp3,
    Ac3,
}

impl BlockBuilder for AudioEncBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building AudioEncoder block instance: {}", instance_id);

        let codec = parse_codec(properties)?;
        let bitrate_kbps = parse_bitrate(properties);
        let sample_rate = parse_sample_rate(properties)?;
        let channels = parse_channels(properties)?;

        // Catch this here rather than as a not-negotiated error on a running flow.
        if codec == Codec::Opus {
            if let Some(rate) = sample_rate {
                if !OPUS_SAMPLE_RATES.contains(&rate) {
                    return Err(BlockBuildError::InvalidConfiguration(format!(
                        "Opus cannot encode at {} Hz — supported rates are {:?}",
                        rate, OPUS_SAMPLE_RATES
                    )));
                }
            }
        }

        let encoder_name = select_encoder(codec)?;
        info!(
            "Selected encoder '{}' for codec {:?} at {} kbps",
            encoder_name, codec, bitrate_kbps
        );

        let convert_id = format!("{}:audioconvert", instance_id);
        let resample_id = format!("{}:audioresample", instance_id);
        let rate_caps_id = format!("{}:rate_caps", instance_id);
        let encoder_id = format!("{}:encoder", instance_id);
        let parser_id = format!("{}:parser", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&resample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        let rate_caps = gst::ElementFactory::make("capsfilter")
            .name(&rate_caps_id)
            .property("caps", build_raw_caps(sample_rate, channels))
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        let encoder = gst::ElementFactory::make(&encoder_name)
            .name(&encoder_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", encoder_name, e)))?;

        set_encoder_bitrate(&encoder, &encoder_name, bitrate_kbps);

        let parser_name = get_parser_name(codec);
        let parser = gst::ElementFactory::make(parser_name)
            .name(&parser_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", parser_name, e)))?;

        let caps_str = get_codec_caps_string(codec);
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!(
            "AudioEncoder block created (chain: audioconvert -> audioresample -> capsfilter -> {} -> {} -> capsfilter [{}])",
            encoder_name, parser_name, caps_str
        );

        let internal_links = vec![
            (
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&resample_id, "sink"),
            ),
            (
                ElementPadRef::pad(&resample_id, "src"),
                ElementPadRef::pad(&rate_caps_id, "sink"),
            ),
            (
                ElementPadRef::pad(&rate_caps_id, "src"),
                ElementPadRef::pad(&encoder_id, "sink"),
            ),
            (
                ElementPadRef::pad(&encoder_id, "src"),
                ElementPadRef::pad(&parser_id, "sink"),
            ),
            (
                ElementPadRef::pad(&parser_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (convert_id, audioconvert),
                (resample_id, audioresample),
                (rate_caps_id, rate_caps),
                (encoder_id, encoder),
                (parser_id, parser),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Build the raw-audio caps that pin sample rate and channel count.
///
/// Fields the caller left as "source" are omitted, so upstream keeps whatever it had.
fn build_raw_caps(sample_rate: Option<i32>, channels: Option<i32>) -> gst::Caps {
    let mut builder = gst::Caps::builder("audio/x-raw");
    if let Some(rate) = sample_rate {
        builder = builder.field("rate", rate);
    }
    if let Some(ch) = channels {
        builder = builder.field("channels", ch);
    }
    builder.build()
}

/// Encoders to try for the given codec, best first.
fn get_encoder_priority_list(codec: Codec) -> Vec<&'static str> {
    match codec {
        // fdkaacenc has the best quality but is not always packaged.
        Codec::Aac => vec!["fdkaacenc", "avenc_aac", "voaacenc", "faac"],
        Codec::Opus => vec!["opusenc"],
        Codec::Mp3 => vec!["lamemp3enc"],
        Codec::Ac3 => vec!["avenc_ac3"],
    }
}

/// Pick the first available encoder for the codec.
fn select_encoder(codec: Codec) -> Result<String, BlockBuildError> {
    let candidates = get_encoder_priority_list(codec);

    for encoder_name in &candidates {
        if let Some(factory) = gst::ElementFactory::find(encoder_name) {
            // Respect GST_PLUGIN_FEATURE_RANK=element:0, which is how an element
            // gets taken out of play without uninstalling it.
            if factory.rank() == gst::Rank::NONE {
                info!("Audio encoder disabled (rank=0): {}", encoder_name);
                continue;
            }
            return Ok((*encoder_name).to_string());
        }
        info!("Audio encoder not available: {}", encoder_name);
    }

    Err(BlockBuildError::InvalidConfiguration(format!(
        "No encoder available for {:?} (tried {})",
        codec,
        candidates.join(", ")
    )))
}

/// Apply the target bitrate, accounting for each encoder's units.
///
/// Uses `set_property_from_str` throughout so a property whose type differs between
/// encoders cannot panic the build.
fn set_encoder_bitrate(encoder: &gst::Element, encoder_name: &str, bitrate_kbps: u32) {
    if encoder_name == "lamemp3enc" {
        // lamemp3enc takes kbps, and only honours it when target is "bitrate"
        // (it optimises for quality otherwise).
        if encoder.has_property("target") {
            encoder.set_property_from_str("target", "bitrate");
        }
        encoder.set_property_from_str("bitrate", &bitrate_kbps.to_string());
        return;
    }

    // Everything else takes bits per second.
    if encoder.has_property("bitrate") {
        encoder.set_property_from_str("bitrate", &(bitrate_kbps * 1000).to_string());
    } else {
        warn!(
            "Audio encoder {} has no bitrate property, using its default",
            encoder_name
        );
    }
}

/// Parser element for the codec, so downstream muxers get properly framed data.
fn get_parser_name(codec: Codec) -> &'static str {
    match codec {
        Codec::Aac => "aacparse",
        Codec::Opus => "opusparse",
        Codec::Mp3 => "mpegaudioparse",
        Codec::Ac3 => "ac3parse",
    }
}

/// Output caps for the codec.
///
/// `stream-format` is deliberately left unpinned for AAC: aacparse converts between
/// raw and adts, and pinning either one here would break the muxers that want the other.
fn get_codec_caps_string(codec: Codec) -> &'static str {
    match codec {
        Codec::Aac => "audio/mpeg,mpegversion=4",
        Codec::Opus => "audio/x-opus",
        Codec::Mp3 => "audio/mpeg,mpegversion=1,layer=3",
        Codec::Ac3 => "audio/x-ac3",
    }
}

/// Parse codec from properties.
fn parse_codec(properties: &HashMap<String, PropertyValue>) -> Result<Codec, BlockBuildError> {
    let codec_str = properties
        .get("codec")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("aac");

    match codec_str {
        "aac" => Ok(Codec::Aac),
        "opus" => Ok(Codec::Opus),
        "mp3" => Ok(Codec::Mp3),
        "ac3" => Ok(Codec::Ac3),
        _ => Err(BlockBuildError::InvalidConfiguration(format!(
            "Invalid codec: {}",
            codec_str
        ))),
    }
}

/// Parse target bitrate in kbps.
fn parse_bitrate(properties: &HashMap<String, PropertyValue>) -> u32 {
    properties
        .get("bitrate")
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u as u32),
            PropertyValue::Int(i) if *i > 0 => Some(*i as u32),
            _ => None,
        })
        .unwrap_or(DEFAULT_BITRATE_KBPS)
}

/// Parse sample rate. `None` means "leave it to upstream".
fn parse_sample_rate(
    properties: &HashMap<String, PropertyValue>,
) -> Result<Option<i32>, BlockBuildError> {
    parse_source_or_number(properties, "sample_rate", DEFAULT_SAMPLE_RATE)
}

/// Parse channel count. `None` means "leave it to upstream".
fn parse_channels(
    properties: &HashMap<String, PropertyValue>,
) -> Result<Option<i32>, BlockBuildError> {
    parse_source_or_number(properties, "channels", "source")
}

/// Shared parsing for the enums whose values are either "source" or a number.
fn parse_source_or_number(
    properties: &HashMap<String, PropertyValue>,
    name: &str,
    default: &str,
) -> Result<Option<i32>, BlockBuildError> {
    let value = properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(default);

    if value == "source" {
        return Ok(None);
    }

    value
        .parse::<i32>()
        .map(Some)
        .map_err(|_| BlockBuildError::InvalidConfiguration(format!("Invalid {}: {}", name, value)))
}

/// Get all audio encoder block definitions.
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![audioenc_definition()]
}

fn audioenc_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.audioenc".to_string(),
        name: "Audio Encoder".to_string(),
        description: "Encodes raw audio to AAC, Opus, MP3, or AC-3. Place it before any block that requires pre-encoded audio, such as the Recorder.".to_string(),
        category: "Audio".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "codec".to_string(),
                label: "Codec".to_string(),
                description: "Audio codec to encode to. AAC is the safe choice for MP4 recordings.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "aac".to_string(), label: Some("AAC".to_string()) },
                        EnumValue { value: "opus".to_string(), label: Some("Opus".to_string()) },
                        EnumValue { value: "mp3".to_string(), label: Some("MP3".to_string()) },
                        EnumValue { value: "ac3".to_string(), label: Some("AC-3".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("aac".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "codec".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "bitrate".to_string(),
                label: "Bitrate (kbps)".to_string(),
                description: "Target bitrate in kilobits per second. 128 is transparent enough for speech and music at 48 kHz stereo.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_BITRATE_KBPS as u64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bitrate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "sample_rate".to_string(),
                label: "Sample Rate".to_string(),
                description: "Output sample rate. \"Match source\" passes upstream's rate through, which Opus rejects unless it is one of 8/12/16/24/48 kHz.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "48000".to_string(), label: Some("48 kHz".to_string()) },
                        EnumValue { value: "44100".to_string(), label: Some("44.1 kHz".to_string()) },
                        EnumValue { value: "32000".to_string(), label: Some("32 kHz".to_string()) },
                        EnumValue { value: "source".to_string(), label: Some("Match source".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String(DEFAULT_SAMPLE_RATE.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sample_rate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "channels".to_string(),
                label: "Channels".to_string(),
                description: "Output channel count. \"Match source\" passes upstream's layout through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "source".to_string(), label: Some("Match source".to_string()) },
                        EnumValue { value: "1".to_string(), label: Some("Mono".to_string()) },
                        EnumValue { value: "2".to_string(), label: Some("Stereo".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("source".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
                name: "encoded_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎚".to_string()),
            width: Some(1.5),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests;
