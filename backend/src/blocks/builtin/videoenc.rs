//! Video encoder block with automatic hardware encoder selection.
//!
//! This block automatically selects the best available video encoder for the chosen codec,
//! with priority given to hardware-accelerated encoders (NVIDIA NVENC, Intel QSV, VA-API, AMD AMF)
//! and fallback to software encoders when hardware is not available.
//!
//! Supported codecs:
//! - H.264 / AVC
//! - H.265 / HEVC
//! - AV1
//! - VP9
//!
//! The block creates a chain: videoconvert -> encoder -> capsfilter
//! - videoconvert: Ensures compatible pixel format for the encoder
//! - encoder: Selected hardware or software encoder
//! - capsfilter: Sets output caps for proper codec negotiation

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, EnumValue, PropertyValue, *};
use tracing::{info, warn};

/// Video Encoder block builder.
pub struct VideoEncBuilder;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Codec {
    H264,
    H265,
    AV1,
    VP9,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum EncoderPreference {
    Auto,
    HardwareOnly,
    SoftwareOnly,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
enum RateControl {
    CBR,
    VBR,
    CQP,
}

impl BlockBuilder for VideoEncBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("🎞️ Building VideoEncoder block instance: {}", instance_id);

        // Parse codec (required)
        let codec = parse_codec(properties)?;

        // Parse encoder preference (optional, default: auto)
        let preference = parse_encoder_preference(properties);

        // Parse allow_software_fallback (optional, default: true)
        let allow_software_fallback = properties
            .get("allow_software_fallback")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // Select best available encoder
        let encoder_name = select_encoder(codec, preference, allow_software_fallback)?;
        info!(
            "🎞️ Selected encoder '{}' for codec {:?} with preference {:?}",
            encoder_name, codec, preference
        );

        // Parse encoding properties
        let bitrate = properties
            .get("bitrate")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as u32),
                PropertyValue::Int(i) if *i > 0 => Some(*i as u32),
                _ => None,
            })
            .unwrap_or(4000);

        let quality_preset = properties
            .get("quality_preset")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("medium");

        let rate_control = parse_rate_control(properties);

        let keyframe_interval = properties
            .get("keyframe_interval")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as u32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as u32),
                _ => None,
            })
            .unwrap_or(60);

        // Create elements
        let convert_id = format!("{}:videoconvert", instance_id);
        let encoder_id = format!("{}:encoder", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoconvert: {}", e)))?;

        let encoder = gst::ElementFactory::make(&encoder_name)
            .name(&encoder_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", encoder_name, e)))?;

        // Set encoder properties
        set_encoder_properties(
            &encoder,
            &encoder_name,
            bitrate,
            quality_preset,
            rate_control,
            keyframe_interval,
        );

        // Create capsfilter with codec-specific caps
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
            "🎞️ VideoEncoder block created (chain: videoconvert -> {} -> capsfilter [{}])",
            encoder_name, caps_str
        );

        // Chain: videoconvert -> encoder -> capsfilter
        let internal_links = vec![
            (
                format!("{}:src", convert_id),
                format!("{}:sink", encoder_id),
            ),
            (
                format!("{}:src", encoder_id),
                format!("{}:sink", capsfilter_id),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (convert_id, videoconvert),
                (encoder_id, encoder),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
        })
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
        .unwrap_or("h264"); // Default to H.264 if not specified

    match codec_str {
        "h264" => Ok(Codec::H264),
        "h265" => Ok(Codec::H265),
        "av1" => Ok(Codec::AV1),
        "vp9" => Ok(Codec::VP9),
        _ => Err(BlockBuildError::InvalidConfiguration(format!(
            "Invalid codec: {}",
            codec_str
        ))),
    }
}

/// Parse encoder preference from properties.
fn parse_encoder_preference(properties: &HashMap<String, PropertyValue>) -> EncoderPreference {
    properties
        .get("encoder_preference")
        .and_then(|v| match v {
            PropertyValue::String(s) => match s.as_str() {
                "hardware" => Some(EncoderPreference::HardwareOnly),
                "software" => Some(EncoderPreference::SoftwareOnly),
                _ => Some(EncoderPreference::Auto),
            },
            _ => None,
        })
        .unwrap_or(EncoderPreference::Auto)
}

/// Parse rate control mode from properties.
fn parse_rate_control(properties: &HashMap<String, PropertyValue>) -> RateControl {
    properties
        .get("rate_control")
        .and_then(|v| match v {
            PropertyValue::String(s) => match s.as_str() {
                "cbr" => Some(RateControl::CBR),
                "cqp" => Some(RateControl::CQP),
                _ => Some(RateControl::VBR),
            },
            _ => None,
        })
        .unwrap_or(RateControl::VBR)
}

/// Select the best available encoder for the given codec and preference.
fn select_encoder(
    codec: Codec,
    preference: EncoderPreference,
    allow_software_fallback: bool,
) -> Result<String, BlockBuildError> {
    let registry = gst::Registry::get();

    // Get priority list of encoders to try
    let encoder_list = get_encoder_priority_list(codec, preference);

    // Try each encoder in priority order
    for encoder_name in &encoder_list {
        if registry
            .find_feature(encoder_name, gst::ElementFactory::static_type())
            .is_some()
        {
            info!("✓ Found available encoder: {}", encoder_name);
            return Ok(encoder_name.to_string());
        } else {
            info!("✗ Encoder not available: {}", encoder_name);
        }
    }

    // If we get here, no encoder from the priority list was found
    match preference {
        EncoderPreference::SoftwareOnly => {
            // Software encoders should always be available, this is an error
            Err(BlockBuildError::InvalidConfiguration(format!(
                "No software encoder found for {:?}",
                codec
            )))
        }
        EncoderPreference::HardwareOnly => {
            // Hardware-only mode, no fallback allowed
            Err(BlockBuildError::InvalidConfiguration(format!(
                "No hardware encoder available for {:?}",
                codec
            )))
        }
        EncoderPreference::Auto => {
            if allow_software_fallback {
                // Try software encoders as fallback
                let software_list = get_software_encoder_list(codec);
                for encoder_name in &software_list {
                    if registry
                        .find_feature(encoder_name, gst::ElementFactory::static_type())
                        .is_some()
                    {
                        warn!("⚠️ Using software fallback encoder: {}", encoder_name);
                        return Ok(encoder_name.to_string());
                    }
                }
                Err(BlockBuildError::InvalidConfiguration(format!(
                    "No encoder available for {:?} (tried hardware and software)",
                    codec
                )))
            } else {
                Err(BlockBuildError::InvalidConfiguration(format!(
                    "No hardware encoder available for {:?} and software fallback is disabled",
                    codec
                )))
            }
        }
    }
}

/// Get priority-ordered list of encoders to try for the given codec and preference.
fn get_encoder_priority_list(codec: Codec, preference: EncoderPreference) -> Vec<&'static str> {
    match preference {
        EncoderPreference::SoftwareOnly => get_software_encoder_list(codec),
        EncoderPreference::HardwareOnly | EncoderPreference::Auto => {
            get_hardware_encoder_list(codec)
        }
    }
}

/// Get priority-ordered list of hardware encoders for the given codec.
fn get_hardware_encoder_list(codec: Codec) -> Vec<&'static str> {
    match codec {
        Codec::H264 => vec![
            // NVIDIA (best for NVIDIA GPUs)
            "nvautogpuh264enc", // Auto GPU select mode
            "nvh264enc",        // CUDA mode
            "nvd3d11h264enc",   // Direct3D11 mode (Windows)
            // Intel QSV
            "qsvh264enc",
            // VA-API (Intel/AMD on Linux)
            "vah264enc",
            "vah264lpenc", // Low power variant
            // AMD AMF (Windows)
            "amfh264enc",
        ],
        Codec::H265 => vec![
            // NVIDIA
            "nvautogpuh265enc",
            "nvh265enc",
            "nvd3d11h265enc",
            // Intel QSV
            "qsvh265enc",
            // VA-API
            "vah265enc",
            "vah265lpenc",
            // AMD AMF
            "amfh265enc",
        ],
        Codec::AV1 => vec![
            // NVIDIA
            "nvautogpuav1enc",
            "nvav1enc",
            "nvd3d11av1enc",
            // Intel QSV
            "qsvav1enc",
            // VA-API
            "vaav1enc",
            // AMD AMF
            "amfav1enc",
        ],
        Codec::VP9 => vec![
            // Intel QSV
            "qsvvp9enc",
            // VA-API
            "vavp9enc",
        ],
    }
}

/// Get list of software encoders for the given codec.
fn get_software_encoder_list(codec: Codec) -> Vec<&'static str> {
    match codec {
        Codec::H264 => vec!["x264enc"],
        Codec::H265 => vec!["x265enc"],
        Codec::AV1 => vec![
            "svtav1enc", // SVT-AV1 (high quality, good performance)
            "av1enc",    // libaom (reference encoder, slower)
        ],
        Codec::VP9 => vec!["vp9enc"],
    }
}

/// Set encoder properties based on the encoder type.
fn set_encoder_properties(
    encoder: &gst::Element,
    encoder_name: &str,
    bitrate: u32,
    quality_preset: &str,
    rate_control: RateControl,
    keyframe_interval: u32,
) {
    // Bitrate mapping (different encoders use different property names and units)
    if encoder_name.starts_with("x264") || encoder_name.starts_with("x265") {
        // x264/x265: bitrate in kbps
        encoder.set_property("bitrate", bitrate);
        // x264/x265: speed-preset (enum property) - use set_property_from_str for enum
        let preset_nick = map_quality_preset_x264(quality_preset);
        encoder.set_property_from_str("speed-preset", preset_nick);
    } else if encoder_name.starts_with("nv") {
        // NVENC encoders: bitrate in kbps
        encoder.set_property("bitrate", bitrate);

        // NVENC: preset (enum property) - different naming for nvautogpu* vs regular nv*
        // nvautogpu* uses p1-p7 (newer), regular nv* uses default/hp/hq (older)
        let preset_nick = if encoder_name.starts_with("nvautogpu") {
            map_quality_preset_nvenc_new(quality_preset) // p1-p7 style
        } else {
            map_quality_preset_nvenc_old(quality_preset) // default/hp/hq style
        };
        encoder.set_property_from_str("preset", preset_nick);

        // Rate control property name differs between nvautogpu* and regular nv* encoders
        let rc_property = if encoder_name.starts_with("nvautogpu") {
            "rate-control" // nvautogpu* variants use this
        } else {
            "rc-mode" // regular nv* encoders use this
        };

        // Rate control (enum property)
        let rc_nick = match rate_control {
            RateControl::CQP => "cqp",
            RateControl::VBR => "vbr",
            RateControl::CBR => "cbr",
        };
        encoder.set_property_from_str(rc_property, rc_nick);
    } else if encoder_name.starts_with("qsv") {
        // Intel QSV: bitrate in kbps
        encoder.set_property("bitrate", bitrate);
        // QSV: target-usage for quality/speed tradeoff (1=best quality, 7=fastest)
        let target_usage = map_quality_preset_qsv(quality_preset);
        encoder.set_property("target-usage", target_usage);
    } else if encoder_name.starts_with("va") {
        // VA-API: bitrate in kbps
        encoder.set_property("bitrate", bitrate);
    } else if encoder_name.starts_with("amf") {
        // AMD AMF: bitrate in kbps
        encoder.set_property("bitrate", bitrate);
        // AMF: usage for quality preset (try setting, may not be available on all versions)
        let usage = map_quality_preset_amf(quality_preset);
        // AMF usage is a string enum, attempt to set it (may fail gracefully)
        if encoder.has_property("usage") {
            // Note: AMF usage might be a string enum - this may fail, but won't crash
            encoder.set_property("usage", usage);
        }
    } else if encoder_name == "svtav1enc" {
        // SVT-AV1: target-bitrate in kbps
        encoder.set_property("target-bitrate", bitrate);
        // SVT-AV1: preset (0=slowest/best, 13=fastest)
        let preset = map_quality_preset_svtav1(quality_preset);
        encoder.set_property("preset", preset);
    } else if encoder_name == "av1enc" {
        // libaom AV1: target-bitrate in kbps
        encoder.set_property("target-bitrate", bitrate);
        // libaom: cpu-used (0=slowest, 8=fastest)
        let cpu_used = map_quality_preset_av1enc(quality_preset);
        encoder.set_property("cpu-used", cpu_used);
    } else if encoder_name == "vp9enc" {
        // libvpx VP9: target-bitrate in kbps (expects i32, not u32!)
        let bitrate_i32 = bitrate as i32;
        encoder.set_property("target-bitrate", bitrate_i32);
        // VP9: cpu-used (0=slowest, 5=fastest for realtime)
        let cpu_used = map_quality_preset_vp9enc(quality_preset);
        encoder.set_property("cpu-used", cpu_used);
    }

    // Keyframe interval (GOP size)
    if keyframe_interval > 0 {
        // Set GOP size - different encoders use different property names and types
        // x264: key-int-max (guint/u32)
        // x265: key-int-max (gint/i32) - yes, they're different!
        // NVENC/others: gop-size (gint/i32)
        if encoder.has_property("key-int-max") {
            // x264 expects u32, x265 expects i32
            if encoder_name.contains("x264") {
                encoder.set_property("key-int-max", keyframe_interval);
            } else {
                let gop_size = keyframe_interval as i32;
                encoder.set_property("key-int-max", gop_size);
            }
        } else if encoder.has_property("gop-size") {
            // Most other encoders expect i32
            let gop_size = keyframe_interval as i32;
            encoder.set_property("gop-size", gop_size);
        } else if encoder.has_property("keyint-max") {
            let gop_size = keyframe_interval as i32;
            encoder.set_property("keyint-max", gop_size);
        }
    }

    info!(
        "🎞️ Set encoder properties: bitrate={} kbps, preset={}, rate_control={:?}, gop={}",
        bitrate, quality_preset, rate_control, keyframe_interval
    );
}

/// Map quality preset to x264/x265 speed-preset enum nick (string value for enum lookup).
fn map_quality_preset_x264(quality_preset: &str) -> &str {
    match quality_preset {
        "ultrafast" => "ultrafast",
        "fast" => "fast",
        "slow" => "slow",
        "veryslow" => "veryslow",
        _ => "medium", // default
    }
}

/// Map quality preset to x264/x265 speed-preset enum value (for testing).
/// Values: 0=none, 1=ultrafast, 2=superfast, 3=veryfast, 4=faster, 5=fast,
///         6=medium, 7=slow, 8=slower, 9=veryslow, 10=placebo
#[cfg(test)]
fn map_quality_preset_x264_enum(quality_preset: &str) -> i32 {
    match quality_preset {
        "ultrafast" => 1,
        "fast" => 5,
        "slow" => 7,
        "veryslow" => 9,
        _ => 6, // medium (default)
    }
}

/// Map quality preset to NVENC preset enum nick (p1-p7 style for nvautogpu* encoders).
fn map_quality_preset_nvenc_new(quality_preset: &str) -> &str {
    match quality_preset {
        "ultrafast" => "p1", // fastest
        "fast" => "p3",      // fast
        "slow" => "p6",      // slower
        "veryslow" => "p7",  // slowest
        _ => "p4",           // medium (default)
    }
}

/// Map quality preset to NVENC preset enum nick (old style for regular nv* encoders).
fn map_quality_preset_nvenc_old(quality_preset: &str) -> &str {
    match quality_preset {
        "ultrafast" => "hp",            // high performance (fastest)
        "fast" => "low-latency-hp",     // low latency high performance
        "slow" => "hq",                 // high quality
        "veryslow" => "low-latency-hq", // low latency high quality
        _ => "default",                 // default (medium)
    }
}

/// Map quality preset to NVENC preset enum value (for testing).
/// Values: 8=p1 (fastest), 9=p2, 10=p3, 11=p4 (medium), 12=p5, 13=p6, 14=p7 (slowest)
#[cfg(test)]
fn map_quality_preset_nvenc_enum(quality_preset: &str) -> i32 {
    match quality_preset {
        "ultrafast" => 8, // p1 - fastest
        "fast" => 10,     // p3 - fast
        "slow" => 13,     // p6 - slower
        "veryslow" => 14, // p7 - slowest
        _ => 11,          // p4 - medium (default)
    }
}

/// Map quality preset to Intel QSV target-usage (1=best quality, 7=fastest).
fn map_quality_preset_qsv(quality_preset: &str) -> u32 {
    match quality_preset {
        "ultrafast" => 7,
        "fast" => 5,
        "slow" => 2,
        "veryslow" => 1,
        _ => 4, // medium
    }
}

/// Map quality preset to AMD AMF usage.
fn map_quality_preset_amf(quality_preset: &str) -> &str {
    match quality_preset {
        "ultrafast" => "lowlatency",
        "fast" => "lowlatency",
        "slow" => "quality",
        "veryslow" => "quality",
        _ => "transcoding", // balanced
    }
}

/// Map quality preset to SVT-AV1 preset (0=best, 13=fastest).
fn map_quality_preset_svtav1(quality_preset: &str) -> u32 {
    match quality_preset {
        "ultrafast" => 12,
        "fast" => 10,
        "slow" => 4,
        "veryslow" => 0,
        _ => 8, // medium
    }
}

/// Map quality preset to libaom AV1 cpu-used (0=slowest, 8=fastest).
fn map_quality_preset_av1enc(quality_preset: &str) -> i32 {
    match quality_preset {
        "ultrafast" => 8,
        "fast" => 6,
        "slow" => 2,
        "veryslow" => 0,
        _ => 4, // medium
    }
}

/// Map quality preset to VP9 cpu-used (0=slowest, 5=fastest for realtime).
fn map_quality_preset_vp9enc(quality_preset: &str) -> i32 {
    match quality_preset {
        "ultrafast" => 5,
        "fast" => 4,
        "slow" => 1,
        "veryslow" => 0,
        _ => 3, // medium
    }
}

/// Get codec-specific caps string for capsfilter.
fn get_codec_caps_string(codec: Codec) -> String {
    match codec {
        Codec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au".to_string(),
        Codec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au".to_string(),
        Codec::AV1 => "video/x-av1".to_string(),
        Codec::VP9 => "video/x-vp9".to_string(),
    }
}

/// Get metadata for VideoEncoder block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![videoenc_definition()]
}

/// Get VideoEncoder block definition (metadata only).
fn videoenc_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.videoenc".to_string(),
        name: "Video Encoder".to_string(),
        description: "Video encoder with automatic hardware acceleration selection. Supports H.264, H.265, AV1, and VP9 with automatic selection of NVIDIA NVENC, Intel QSV, VA-API, AMD AMF, or software encoders.".to_string(),
        category: "Video".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "codec".to_string(),
                label: "Codec".to_string(),
                description: "Video codec to encode to".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "h264".to_string(), label: Some("H.264 / AVC".to_string()) },
                        EnumValue { value: "h265".to_string(), label: Some("H.265 / HEVC".to_string()) },
                        EnumValue { value: "av1".to_string(), label: Some("AV1".to_string()) },
                        EnumValue { value: "vp9".to_string(), label: Some("VP9".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("h264".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "codec".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "encoder_preference".to_string(),
                label: "Encoder Preference".to_string(),
                description: "Prefer hardware or software encoding".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "auto".to_string(), label: Some("Auto (Hardware first, then software)".to_string()) },
                        EnumValue { value: "hardware".to_string(), label: Some("Hardware Only".to_string()) },
                        EnumValue { value: "software".to_string(), label: Some("Software Only".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "encoder_preference".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "bitrate".to_string(),
                label: "Bitrate (kbps)".to_string(),
                description: "Target bitrate in kilobits per second (100-100000 kbps)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(4000)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bitrate".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "quality_preset".to_string(),
                label: "Quality Preset".to_string(),
                description: "Encoding quality/speed tradeoff. Slower presets provide better quality at same bitrate.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "ultrafast".to_string(), label: Some("Ultra Fast".to_string()) },
                        EnumValue { value: "fast".to_string(), label: Some("Fast".to_string()) },
                        EnumValue { value: "medium".to_string(), label: Some("Medium".to_string()) },
                        EnumValue { value: "slow".to_string(), label: Some("Slow".to_string()) },
                        EnumValue { value: "veryslow".to_string(), label: Some("Very Slow".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("medium".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "quality_preset".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "rate_control".to_string(),
                label: "Rate Control".to_string(),
                description: "Rate control mode for encoding".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "vbr".to_string(), label: Some("VBR (Variable Bitrate)".to_string()) },
                        EnumValue { value: "cbr".to_string(), label: Some("CBR (Constant Bitrate)".to_string()) },
                        EnumValue { value: "cqp".to_string(), label: Some("CQP (Constant Quality)".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("vbr".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "rate_control".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "keyframe_interval".to_string(),
                label: "Keyframe Interval".to_string(),
                description: "GOP size (keyframe interval) in frames. 0 = automatic. Typical: 60 frames = 2 seconds at 30fps. Range: 0-600.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(60)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "keyframe_interval".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "allow_software_fallback".to_string(),
                label: "Allow Software Fallback".to_string(),
                description: "Allow fallback to software encoding if no hardware encoder is available (only relevant for Auto/Hardware modes)".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "allow_software_fallback".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "videoconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                name: "encoded_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎞️".to_string()),
            color: Some("#FF5722".to_string()), // Deep Orange for video encoding
            width: Some(1.5),
            height: Some(2.5),
        }),
    }
}

#[cfg(test)]
mod tests;
