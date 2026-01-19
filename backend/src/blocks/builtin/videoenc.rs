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
//! The block creates a chain: videoconvert -> encoder -> parser -> capsfilter
//! - videoconvert: Ensures compatible pixel format for the encoder
//! - encoder: Selected hardware or software encoder
//! - parser: Codec-specific parser (h264parse, h265parse, etc.) for proper stream formatting
//! - capsfilter: Sets output caps for proper codec negotiation

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::video_convert_mode;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, *};
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
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building VideoEncoder block instance: {}", instance_id);

        // Parse codec (required)
        let codec = parse_codec(properties)?;

        // Parse encoder preference (optional, default: auto)
        let preference = parse_encoder_preference(properties);

        // Select best available encoder
        let encoder_name = select_encoder(codec, preference)?;
        info!(
            "Selected encoder '{}' for codec {:?} with preference {:?}",
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
            .unwrap_or("ultrafast");

        let tune = properties
            .get("tune")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("zerolatency");

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
        // Use detected video convert mode (autovideoconvert if GPU interop works, videoconvert otherwise)
        // Note: We always use "videoconvert" as the element ID for consistent external pad references,
        // even when the actual GStreamer element is "autovideoconvert"
        let convert_mode = video_convert_mode();
        let convert_element_name = convert_mode.element_name();
        let convert_id = format!("{}:videoconvert", instance_id);
        let encoder_id = format!("{}:encoder", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let videoconvert = gst::ElementFactory::make(convert_element_name)
            .name(&convert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
            })?;

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
            tune,
            rate_control,
            keyframe_interval,
        );

        // Create parser for the codec (critical for proper MPEG-TS muxing and playback)
        let parser_name = get_parser_name(codec);
        let parser_id = format!("{}:parser", instance_id);
        let parser = gst::ElementFactory::make(parser_name)
            .name(&parser_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", parser_name, e)))?;

        // Configure parser for streaming (insert SPS/PPS headers periodically)
        configure_parser(&parser, codec, keyframe_interval);

        info!("Added {} parser for proper stream formatting", parser_name);

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
            "VideoEncoder block created (chain: {} -> {} -> {} -> capsfilter [{}])",
            convert_element_name, encoder_name, parser_name, caps_str
        );

        // Chain: videoconvert/autovideoconvert -> encoder -> parser -> capsfilter
        let internal_links = vec![
            (
                ElementPadRef::pad(&convert_id, "src"),
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
                (convert_id, videoconvert),
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

/// Get the parser element name for the given codec.
fn get_parser_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264parse",
        Codec::H265 => "h265parse",
        Codec::AV1 => "av1parse",
        Codec::VP9 => "vp9parse",
    }
}

/// Configure parser for streaming (insert codec headers periodically).
fn configure_parser(parser: &gst::Element, codec: Codec, _keyframe_interval: u32) {
    match codec {
        Codec::H264 | Codec::H265 => {
            // For H.264/H.265: Insert SPS/PPS headers frequently for best streaming
            // config-interval: -1 = only at start, 0 = disabled, >0 = every N seconds
            //
            // Best practice for live streaming: Insert at EVERY keyframe (IDR)
            // - Minimal overhead (SPS/PPS are tiny: ~20-50 bytes)
            // - Instant stream join at any keyframe
            // - Better resilience to packet loss
            // - Proper sync with keyframes
            //
            // We set config-interval=1 to insert headers every second, which ensures
            // headers are present at every keyframe for typical GOP sizes (30-120 frames)

            if parser.has_property("config-interval") {
                // Set to 1 second for frequent SPS/PPS insertion
                // This is better than time-based on GOP because:
                // 1. Viewers can join stream instantly at any keyframe
                // 2. No waiting for next config interval
                // 3. Overhead is negligible (~50 bytes per second)
                parser.set_property("config-interval", 1i32);
                info!("Parser configured: config-interval=1s (SPS/PPS at every keyframe for instant stream join)");
            }
        }
        Codec::AV1 | Codec::VP9 => {
            // AV1 and VP9 parsers don't need special configuration for headers
            // They handle sequence headers automatically
        }
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
fn select_encoder(codec: Codec, preference: EncoderPreference) -> Result<String, BlockBuildError> {
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
            // Try software encoders as fallback
            let software_list = get_software_encoder_list(codec);
            for encoder_name in &software_list {
                if registry
                    .find_feature(encoder_name, gst::ElementFactory::static_type())
                    .is_some()
                {
                    warn!("Using software fallback encoder: {}", encoder_name);
                    return Ok(encoder_name.to_string());
                }
            }
            Err(BlockBuildError::InvalidConfiguration(format!(
                "No encoder available for {:?} (tried hardware and software)",
                codec
            )))
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
            // NOTE: nvh264enc is prioritized over nvautogpuh264enc due to keyframe bug in nvautogpu*
            // nvautogpuh264enc has a bug where it doesn't generate keyframes regardless of gop-size settings
            "nvh264enc",        // CUDA mode - WORKS correctly for keyframes
            "nvautogpuh264enc", // Auto GPU select mode - HAS KEYFRAME BUG (doesn't respect gop-size)
            "nvd3d11h264enc",   // Direct3D11 mode (Windows)
            // Intel QSV
            "qsvh264enc",
            // VA-API (Intel/AMD on Linux)
            "vah264enc",
            "vah264lpenc", // Low power variant
            // AMD AMF (Windows)
            "amfh264enc",
            // V4L2 (Raspberry Pi, embedded Linux)
            "v4l2h264enc",
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
            // V4L2 (Raspberry Pi 4+, embedded Linux)
            "v4l2h265enc",
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
    tune: &str,
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
        // x264/x265: tune (enum property) - optimize for specific use case
        encoder.set_property_from_str("tune", tune);
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

        // NVENC: Disable adaptive I-frame insertion to respect gop-size
        if encoder.has_property("i-adapt") {
            encoder.set_property("i-adapt", false);
        }

        // NVENC: Enable strict GOP mode for consistent keyframe intervals
        if encoder.has_property("strict-gop") {
            encoder.set_property("strict-gop", true);
        }

        // NVENC: Disable B-frames for simpler GOP structure (helps with keyframe consistency)
        if encoder.has_property("b-frames") {
            encoder.set_property("b-frames", 0u32);
        }
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
    } else if encoder_name.starts_with("v4l2") {
        // V4L2 encoders (Raspberry Pi, embedded Linux)
        // V4L2 encoders use extra-controls structure for bitrate
        // The bitrate is in bits per second (not kbps)
        let bitrate_bps = bitrate * 1000;

        // Try to set extra-controls with video_bitrate
        // This is the standard way for V4L2 stateful encoders
        if encoder.has_property("extra-controls") {
            // Create a GStreamer structure with the video bitrate
            let controls = gst::Structure::builder("extra-controls")
                .field("video_bitrate", bitrate_bps)
                .build();
            encoder.set_property("extra-controls", &controls);
            info!(
                "V4L2 encoder: set video_bitrate={} bps via extra-controls",
                bitrate_bps
            );
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
        "Set encoder properties: bitrate={} kbps, preset={}, tune={}, rate_control={:?}, gop={}",
        bitrate, quality_preset, tune, rate_control, keyframe_interval
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
        Codec::H264 => "video/x-h264,alignment=au".to_string(),
        Codec::H265 => "video/x-h265,alignment=au".to_string(),
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
                default_value: Some(PropertyValue::String("ultrafast".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "quality_preset".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "tune".to_string(),
                label: "Tune".to_string(),
                description: "Optimize encoder for specific use case (x264/x265 only). Zero latency disables look-ahead for minimal delay.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "zerolatency".to_string(), label: Some("Zero Latency (streaming/real-time)".to_string()) },
                        EnumValue { value: "film".to_string(), label: Some("Film (high quality)".to_string()) },
                        EnumValue { value: "animation".to_string(), label: Some("Animation".to_string()) },
                        EnumValue { value: "grain".to_string(), label: Some("Grain (preserve film grain)".to_string()) },
                        EnumValue { value: "stillimage".to_string(), label: Some("Still Image".to_string()) },
                        EnumValue { value: "fastdecode".to_string(), label: Some("Fast Decode".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("zerolatency".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "tune".to_string(),
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
            icon: Some("🎞".to_string()),
            width: Some(1.5),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests;
