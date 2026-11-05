//! Video encoder block with automatic hardware encoder selection.
//!
//! This block automatically selects the best available video encoder for the chosen codec,
//! with priority given to hardware-accelerated encoders (NVIDIA NVENC, Intel QSV, VA-API,
//! Apple VideoToolbox, AMD AMF, Windows Media Foundation, V4L2) and fallback to software
//! encoders when hardware is not available.
//!
//! Media Foundation (`mfh264enc`, `mfh265enc`, `mfvp9enc`) comes after the vendor-native
//! encoders, which expose more control. Older Windows systems expose their hardware encoder
//! only as a Media Foundation transform; only hardware ones are selected (see `mediafoundation`).
//!
//! Supported codecs:
//! - H.264 / AVC
//! - H.265 / HEVC
//! - AV1
//! - VP9
//!
//! The block creates a chain: input -> videoconvert -> [rawcaps] -> encoder -> parser -> capsfilter
//! - input: `identity` whose sink pad adapts the memory that arrives to what the
//!   encoder takes, from the negotiated caps (see `gst::download_front`)
//! - videoconvert: Ensures compatible pixel format for the encoder. Plain
//!   `videoconvert`, never `autovideoconvert`: it passes GPU memory the encoder
//!   takes straight through, and converts system memory on the CPU
//! - rawcaps (VP9/AV1 unless profile is "none"): limits system-memory input to
//!   8-bit 4:2:0, so the encoder produces its base profile
//! - encoder: Selected hardware or software encoder
//! - parser: Codec-specific parser (h264parse, h265parse, etc.) for proper stream formatting
//! - capsfilter: Sets output caps for proper codec negotiation, including the
//!   H.264 level (see `level`)

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::download_front;
use crate::gst::pipeline::properties::set_property_checked;
use crate::gst::video_adapt::VIDEO_CONVERT_FACTORY;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::*,
    element::ElementPadRef,
    videoenc::{H264Level, Profile, CODEC_H264, CODEC_H265, CODEC_PROPERTY},
    EnumValue, PropertyValue, *,
};
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

        // The profile the capsfilter pins; the encoder must be able to produce it
        let profile = parse_profile(properties);

        // The H.264 level the capsfilter pins (see `level`)
        let h264_level = parse_h264_level(properties);

        // Select best available encoder
        let encoder_name = select_encoder(codec, preference, profile)?;
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

        // Create elements.
        //
        // The converter is plain `videoconvert`, never `autovideoconvert`.
        // `autovideoconvert` turned CUDA memory from
        // `nvh264dec` into GL RGBA that `nvh264enc` never negotiates from
        // (#1104). `videoconvert` passes GPU memory through untouched when the
        // encoder lists it, and converts system memory; the input front puts
        // a download in front of it for GPU memory the encoder does not take.
        let input_id = format!("{}:input", instance_id);
        let convert_id = format!("{}:videoconvert", instance_id);
        let rawcaps_id = format!("{}:rawcaps", instance_id);
        let encoder_id = format!("{}:encoder", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        // What arrives depends on the producer, often a decoder `decodebin`
        // picks at runtime, so the memory is adapted from the negotiated caps.
        let front = download_front::convert_front(
            &input_id,
            &convert_id,
            &format!("Video Encoder ({})", encoder_name),
        )?;

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
        )?;

        // VP9 and AV1 encoders do not pick their input format from a profile
        // pinned downstream, so the 4:2:0 constraint goes in front of them.
        let rawcaps = match base_profile_input_caps(codec, profile, &encoder) {
            Some(caps) => {
                info!("Constraining {} input to {}", encoder_name, caps);
                Some(
                    gst::ElementFactory::make("capsfilter")
                        .name(&rawcaps_id)
                        .property("caps", &caps)
                        .build()
                        .map_err(|e| {
                            BlockBuildError::ElementCreation(format!("capsfilter: {}", e))
                        })?,
                )
            }
            None => None,
        };

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
        let mut caps_str = get_codec_caps_string(codec, profile);
        let base_caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;
        let level = applied_h264_level(codec, h264_level, &encoder_name);
        if let Some(pinned) = level.as_caps_str() {
            caps_str.push_str(&format!(",level=(string){}", pinned));
        }
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        if level != H264Level::None {
            level::install_probe(&encoder, &capsfilter, base_caps, level);
        }

        info!(
            "VideoEncoder block created (chain: input -> {} -> {} -> {} -> capsfilter [{}])",
            VIDEO_CONVERT_FACTORY, encoder_name, parser_name, caps_str
        );

        // Chain: input -> videoconvert -> [rawcaps] -> encoder -> parser -> capsfilter
        let mut internal_links = vec![front.link()];
        if rawcaps.is_some() {
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&rawcaps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&rawcaps_id, "src"),
                ElementPadRef::pad(&encoder_id, "sink"),
            ));
        } else {
            internal_links.push((
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&encoder_id, "sink"),
            ));
        }
        internal_links.extend([
            (
                ElementPadRef::pad(&encoder_id, "src"),
                ElementPadRef::pad(&parser_id, "sink"),
            ),
            (
                ElementPadRef::pad(&parser_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
        ]);

        let mut elements = vec![(input_id, front.front), (convert_id, front.convert)];
        if let Some(rawcaps) = rawcaps {
            elements.push((rawcaps_id, rawcaps));
        }
        elements.extend([
            (encoder_id, encoder),
            (parser_id, parser),
            (capsfilter_id, capsfilter),
        ]);

        Ok(BlockBuildResult {
            elements,
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
        .get(CODEC_PROPERTY)
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(CODEC_H264); // Default to H.264 if not specified

    match codec_str {
        CODEC_H264 => Ok(Codec::H264),
        CODEC_H265 => Ok(Codec::H265),
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

/// Select the best available encoder for the given codec, preference and
/// profile. An encoder whose source template cannot produce the profile the
/// block pins on its capsfilter is skipped (see `selection::choose_encoder`).
fn select_encoder(
    codec: Codec,
    preference: EncoderPreference,
    profile: Profile,
) -> Result<String, BlockBuildError> {
    let wanted = EncoderOutput {
        media_type: codec_media_type(codec),
        profile: pinned_profile(codec, profile),
    };
    // Get priority list of encoders to try
    let encoder_list = get_encoder_priority_list(codec, preference);
    let factories = registry_snapshot(&encoder_list);
    if let Some(encoder_name) = choose_encoder(&encoder_list, &wanted, &factories) {
        return Ok(encoder_name);
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
            let software_list = named(&get_software_encoder_list(codec));
            let factories = registry_snapshot(&software_list);
            if let Some(encoder_name) = choose_encoder(&software_list, &wanted, &factories) {
                warn!("Using software fallback encoder: {}", encoder_name);
                return Ok(encoder_name);
            }
            Err(BlockBuildError::InvalidConfiguration(format!(
                "No encoder available for {:?} (tried hardware and software)",
                codec
            )))
        }
    }
}

/// Get priority-ordered list of encoders to try for the given codec and preference.
fn get_encoder_priority_list(codec: Codec, preference: EncoderPreference) -> Vec<Candidate> {
    match preference {
        EncoderPreference::SoftwareOnly => named(&get_software_encoder_list(codec)),
        EncoderPreference::HardwareOnly | EncoderPreference::Auto => {
            get_hardware_encoder_list(codec)
        }
    }
}

/// Get priority-ordered list of hardware encoders for the given codec.
fn get_hardware_encoder_list(codec: Codec) -> Vec<Candidate> {
    use Candidate::{Named, Plugin};
    match codec {
        Codec::H264 => vec![
            // NVIDIA (best for NVIDIA GPUs)
            // NOTE: nvh264enc is prioritized over nvautogpuh264enc due to keyframe bug in nvautogpu*
            // nvautogpuh264enc has a bug where it doesn't generate keyframes regardless of gop-size settings
            Named("nvh264enc"),        // CUDA mode - WORKS correctly for keyframes
            Named("nvautogpuh264enc"), // Auto GPU select mode - HAS KEYFRAME BUG (doesn't respect gop-size)
            Named("nvd3d11h264enc"),   // Direct3D11 mode (Windows)
            // Intel QSV
            Named("qsvh264enc"),
            // VA-API (Intel/AMD on Linux)
            Named("vah264enc"),
            Named("vah264lpenc"), // Low power variant
            // Apple VideoToolbox (macOS)
            Named("vtenc_h264_hw"), // Hardware-only variant
            Named("vtenc_h264"),    // May use software fallback
            // AMD AMF (Windows)
            Named("amfh264enc"),
            // Windows Media Foundation hardware MFTs (any vendor; older Windows
            // systems with no NVENC/QSV/AMF element)
            Plugin(MEDIA_FOUNDATION_PLUGIN),
            // V4L2 (Raspberry Pi, embedded Linux)
            Named("v4l2h264enc"),
        ],
        Codec::H265 => vec![
            // NVIDIA
            Named("nvautogpuh265enc"),
            Named("nvh265enc"),
            Named("nvd3d11h265enc"),
            // Intel QSV
            Named("qsvh265enc"),
            // VA-API
            Named("vah265enc"),
            Named("vah265lpenc"),
            // Apple VideoToolbox (macOS)
            Named("vtenc_h265_hw"), // Hardware-only variant
            Named("vtenc_h265"),    // May use software fallback
            // AMD AMF
            Named("amfh265enc"),
            // Windows Media Foundation hardware MFTs
            Plugin(MEDIA_FOUNDATION_PLUGIN),
            // V4L2 (Raspberry Pi 4+, embedded Linux)
            Named("v4l2h265enc"),
        ],
        Codec::AV1 => vec![
            // NVIDIA
            Named("nvautogpuav1enc"),
            Named("nvav1enc"),
            Named("nvd3d11av1enc"),
            // Intel QSV
            Named("qsvav1enc"),
            // VA-API
            Named("vaav1enc"),
            // AMD AMF
            Named("amfav1enc"),
            // No Media Foundation entry: GStreamer's `mediafoundation` plugin
            // has no AV1 encoder (H.264, H.265 and VP9 only, through 1.28).
        ],
        Codec::VP9 => vec![
            // Intel QSV
            Named("qsvvp9enc"),
            // VA-API
            Named("vavp9enc"),
            // Windows Media Foundation hardware MFTs
            Plugin(MEDIA_FOUNDATION_PLUGIN),
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

/// Apply one encoder property from its text form, checked against the spec.
///
/// The bare `set_property_from_str` this replaces panics rather than fails on a
/// value the property's `GParamSpec` would have to clamp, and `bitrate`, `tune`
/// and `keyframe_interval` come from the flow definition — so that panic was
/// reachable straight from a request body (#769).
fn set_encoder_property(
    encoder: &gst::Element,
    property_name: &str,
    value: &str,
) -> Result<(), BlockBuildError> {
    set_property_checked(
        encoder,
        property_name,
        &PropertyValue::String(value.to_string()),
    )
    .map_err(|reason| BlockBuildError::InvalidProperty(format!("{}: {}", property_name, reason)))
}

/// Clamp a value the block derived from `bitrate` to its target's range.
///
/// A cap such as `max-bitrate`, or a kbps-to-bits/s conversion, can fall
/// outside the range of the property it is written to even when the client's
/// `bitrate` was in range. Refusing the flow over a number the operator never
/// supplied is wrong, so derived values are clamped here; the client's own
/// values still go through `set_encoder_property` and are refused (#769).
fn clamp_to_property_range(encoder: &gst::Element, property_name: &str, value: u64) -> u64 {
    let Some(pspec) = encoder.find_property(property_name) else {
        return value;
    };
    let (min, max) = if let Some(p) = pspec.downcast_ref::<gst::glib::ParamSpecUInt>() {
        (u64::from(p.minimum()), u64::from(p.maximum()))
    } else if let Some(p) = pspec.downcast_ref::<gst::glib::ParamSpecInt>() {
        (p.minimum().max(0) as u64, p.maximum().max(0) as u64)
    } else if let Some(p) = pspec.downcast_ref::<gst::glib::ParamSpecUInt64>() {
        (p.minimum(), p.maximum())
    } else if let Some(p) = pspec.downcast_ref::<gst::glib::ParamSpecInt64>() {
        (p.minimum().max(0) as u64, p.maximum().max(0) as u64)
    } else {
        return value;
    };
    value.clamp(min, max.max(min))
}

/// Set encoder properties based on the encoder type.
///
/// Values go in through their text form, which lets GStreamer's own deserializer
/// handle the type conversion; `set_encoder_property` adds the range and
/// writability check the raw setter signals by aborting the thread.
fn set_encoder_properties(
    encoder: &gst::Element,
    encoder_name: &str,
    bitrate: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
    keyframe_interval: u32,
) -> Result<(), BlockBuildError> {
    let bitrate_str = bitrate.to_string();
    let keyframe_str = keyframe_interval.to_string();

    // Bitrate mapping (different encoders use different property names and units)
    let plugin_name = encoder.factory().and_then(|f| f.plugin_name());
    if is_media_foundation_plugin(plugin_name.as_deref()) {
        // Identified by plugin, not name: the plugin names its features per
        // device, `mf{codec}enc` and `mf{codec}device{N}enc`.
        set_mf_encoder_properties(
            encoder,
            encoder_name,
            bitrate,
            quality_preset,
            tune,
            rate_control,
        )?;
    } else if encoder_name.starts_with("x264") || encoder_name.starts_with("x265") {
        // x264/x265: bitrate in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;
        // x264/x265: speed-preset
        let preset_nick = map_quality_preset_x264(quality_preset);
        set_encoder_property(encoder, "speed-preset", preset_nick)?;
        // x264/x265: tune - optimize for specific use case
        set_encoder_property(encoder, "tune", tune)?;
        // VBV buffer capacity in ms. Set explicitly instead of relying on the
        // upstream default (600 ms) so single frames cannot spike far above
        // the target bitrate — large frame bursts overflow shallow buffers on
        // constrained viewer paths (observed as bursty packet loss on WebRTC).
        if encoder.has_property("vbv-buf-capacity") {
            set_encoder_property(
                encoder,
                "vbv-buf-capacity",
                &BURST_VBV_WINDOW_MS.to_string(),
            )?;
        }
    } else if encoder_name.starts_with("nv") {
        // NVENC encoders: bitrate in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;

        // NVENC: preset - different naming for nvautogpu* vs regular nv*
        // nvautogpu* uses p1-p7 (newer), regular nv* uses default/hp/hq (older)
        let preset_nick = if encoder_name.starts_with("nvautogpu") {
            map_quality_preset_nvenc_new(quality_preset)
        } else {
            map_quality_preset_nvenc_old(quality_preset)
        };
        if encoder.has_property("preset") {
            set_encoder_property(encoder, "preset", preset_nick)?;
        }

        // Rate control is `rate-control` on some nv* encoders and `rc-mode` on
        // others. Use whichever the element exposes: a name prefix covers only
        // nvautogpu*, and the nvd3d11*/nvav1enc encoders we also select would
        // otherwise fail the build on a property they lack (#769).
        let rc_property = ["rate-control", "rc-mode"]
            .into_iter()
            .find(|name| encoder.has_property(name));
        let rc_nick = match rate_control {
            RateControl::CQP => "cqp",
            RateControl::VBR => "vbr",
            RateControl::CBR => "cbr",
        };
        if let Some(rc_property) = rc_property {
            set_encoder_property(encoder, rc_property, rc_nick)?;
        } else {
            warn!(
                "{} has neither rate-control nor rc-mode; rate_control={} is not applied",
                encoder_name, rc_nick
            );
        }

        // NVENC defaults leave VBR excursions unconstrained: max-bitrate is
        // unset and vbv-buffer-size=0 ("NVENC default"), so single frames can
        // spike to ~10x the average frame size. Those frames leave the NIC as
        // line-rate packet bursts that overflow shallow buffers on constrained
        // viewer paths (observed as bursty packet loss on WebRTC outputs).
        // - max-bitrate (kbit/s): cap VBR at the peak (ignored in CBR mode)
        // - vbv-buffer-size (kbits): one VBV window of target bitrate,
        //   bounding how large any single frame can get
        let limits = BurstLimits::for_bitrate_kbps(bitrate);
        if encoder.has_property("max-bitrate") {
            let max_bitrate =
                clamp_to_property_range(encoder, "max-bitrate", limits.peak_bits_per_sec / 1000);
            set_encoder_property(encoder, "max-bitrate", &max_bitrate.to_string())?;
        }
        if encoder.has_property("vbv-buffer-size") {
            let vbv_size =
                clamp_to_property_range(encoder, "vbv-buffer-size", limits.vbv_bits / 1000);
            set_encoder_property(encoder, "vbv-buffer-size", &vbv_size.to_string())?;
        }

        // NVENC: Disable adaptive I-frame insertion to respect gop-size
        if encoder.has_property("i-adapt") {
            set_encoder_property(encoder, "i-adapt", "false")?;
        }

        // NVENC: Enable strict GOP mode for consistent keyframe intervals
        if encoder.has_property("strict-gop") {
            set_encoder_property(encoder, "strict-gop", "true")?;
        }

        // NVENC: Disable B-frames for simpler GOP structure (helps with keyframe consistency)
        if encoder.has_property("b-frames") {
            set_encoder_property(encoder, "b-frames", "0")?;
        }
    } else if encoder_name.starts_with("qsv") {
        // Intel QSV: bitrate in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;
        // QSV: target-usage for quality/speed tradeoff (1=best quality, 7=fastest)
        let target_usage = map_quality_preset_qsv(quality_preset);
        set_encoder_property(encoder, "target-usage", &target_usage.to_string())?;
    } else if encoder_name.starts_with("va") {
        // VA-API: bitrate in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;
    } else if encoder_name.starts_with("amf") {
        // AMD AMF: bitrate in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;
        // AMF: usage for quality preset
        let usage = map_quality_preset_amf(quality_preset);
        if encoder.has_property("usage") {
            set_encoder_property(encoder, "usage", usage)?;
        }
    } else if encoder_name.starts_with("vtenc") {
        // Apple VideoToolbox (macOS): bitrate property is in kbps
        set_encoder_property(encoder, "bitrate", &bitrate_str)?;
        // VideoToolbox: realtime mode disables frame buffering / lookahead.
        // That is a *latency* property, orthogonal to compression quality.
        // Strom is a live mixer/streamer (WebRTC/SRT), so drive it from the
        // low-latency tune (the same signal x264/x265 use), not the quality
        // preset — otherwise picking medium/slow would silently add encoder
        // latency in a live context.
        if encoder.has_property("realtime") {
            let realtime = tune == "zerolatency";
            set_encoder_property(encoder, "realtime", if realtime { "true" } else { "false" })?;
        }
        // VideoToolbox rate control. Measured on Apple Silicon hardware
        // (GStreamer 1.28, 720p30, 5 Mbit/s target, noise content):
        //
        // - ABR does NOT follow the bitrate: vtenc unconditionally sets
        //   kVTCompressionPropertyKey_Quality (property default 0.5) after
        //   AverageBitRate, and the hardware then encodes at constant quality
        //   and ignores the bitrate target entirely (~95 Mbit/s measured
        //   against a 5 Mbit/s target; a direct VTCompressionSession with the
        //   same settings minus Quality tracks the target fine). Upstream
        //   GStreamer bug.
        // - CBR uses a different VT key (ConstantBitRate) that IS honored in
        //   steady state.
        //
        // So map VBR to CBR too: a user asking for VBR means "roughly the
        // target bitrate", never "unbounded constant quality". CQP maps to
        // ABR, whose constant-quality behavior is exactly what CQP requests
        // (the encoder's `quality` property is the dial). Note: scene changes
        // still produce a multi-frame overshoot (~2-4 MB at 720p30) that no
        // exposed VT property bounds, in CBR as well — for strictly capped
        // bursts (e.g. WebRTC on constrained paths) use a software encoder
        // (x264 with VBV) via encoder_preference=software.
        if encoder.has_property("rate-control") {
            let rc = match rate_control {
                RateControl::CBR | RateControl::VBR => "cbr",
                RateControl::CQP => "abr",
            };
            set_encoder_property(encoder, "rate-control", rc)?;
        }
        // data-rate-limits (the VT counterpart of a VBV cap, 1.2x target over
        // a 0.5 s window). vtenc skips it in CBR mode, and on Apple Silicon
        // hardware it is accepted (noErr) but measured to have no effect even
        // in ABR. Still set it: it costs nothing where ignored and other
        // VideoToolbox backends may honor it on the ABR (CQP) path.
        if bitrate > 0 && encoder.has_property("data-rate-limits") {
            let max_kbps = bitrate.saturating_mul(12) / 10;
            set_encoder_property(encoder, "data-rate-limits", &format!("{},0.5", max_kbps))?;
        }
        // VideoToolbox: cap the wall-clock gap between keyframes in addition
        // to the frame-based max-keyframe-interval set below. WebRTC clients
        // need a keyframe to start decoding when they join, so an unbounded
        // wall-clock gap hurts join latency if the source runs slower than
        // expected. keyframe_interval is in frames; the block's convention
        // (see property docs) is "60 frames = 2 s at 30 fps", so derive the
        // ns cap at the same nominal 30 fps. Above 30 fps the frame cap fires
        // first (tighter); below it this duration holds the line.
        if keyframe_interval > 0 && encoder.has_property("max-keyframe-interval-duration") {
            let duration_ns = u64::from(keyframe_interval) * 1_000_000_000 / 30;
            set_encoder_property(
                encoder,
                "max-keyframe-interval-duration",
                &duration_ns.to_string(),
            )?;
        }
        // VideoToolbox defaults to allow-frame-reordering=true, which emits
        // B-frames. B-frames cause non-monotonic PTS in decode order —
        // rtph264pay uses PTS for RTP timestamps, so the RTP stream becomes
        // invalid and WebRTC clients (Chrome desktop especially) fail to
        // decode. Disable unconditionally: VT HW produces good quality at
        // streaming bitrates without B-frames.
        if encoder.has_property("allow-frame-reordering") {
            encoder.set_property("allow-frame-reordering", false);
        }
    } else if encoder_name.starts_with("v4l2") {
        // V4L2 encoders (Raspberry Pi, embedded Linux)
        // V4L2 encoders use extra-controls structure for bitrate (bits per second)
        let bitrate_bps = bitrate.saturating_mul(1000);
        if encoder.has_property("extra-controls") {
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
        set_encoder_property(encoder, "target-bitrate", &bitrate_str)?;
        // SVT-AV1: preset (0=slowest/best, 13=fastest)
        let preset = map_quality_preset_svtav1(quality_preset);
        set_encoder_property(encoder, "preset", &preset.to_string())?;
        // The keyframe interval is `intra-period-length`, the frames between
        // two keyframes: 59 gives one every 60 frames. Left at auto, a viewer
        // that joins or loses a frame waits seconds for a picture.
        if let Some(period) = svtav1_intra_period(keyframe_interval) {
            set_encoder_property(encoder, "intra-period-length", &period.to_string())?;
        }
        // SVT-AV1's random-access defaults hold frames back for look-ahead,
        // temporal filtering and a 5-level hierarchy: 1.7 s on average at
        // 720p25 (2.3 s worst). The live settings bring that to ~0.2 s.
        // Low-delay prediction (`pred-struct=1`, `rtc=1`) would go lower, but
        // svtav1enc stalls after its first frames in that mode.
        if tune == "zerolatency" {
            set_encoder_property(encoder, "parameters-string", SVTAV1_LIVE_PARAMETERS)?;
        }
    } else if encoder_name == "av1enc" {
        // libaom AV1: target-bitrate in kbps
        set_encoder_property(encoder, "target-bitrate", &bitrate_str)?;
        // libaom: cpu-used (0=slowest, 8=fastest)
        let cpu_used = map_quality_preset_av1enc(quality_preset);
        set_encoder_property(encoder, "cpu-used", &cpu_used.to_string())?;
        // libaom's keyframe interval is `keyframe-max-dist`.
        if keyframe_interval > 0 && encoder.has_property("keyframe-max-dist") {
            set_encoder_property(encoder, "keyframe-max-dist", &keyframe_str)?;
        }
        // libaom defaults to the good-quality usage with 19 frames of
        // look-ahead. Live: the realtime usage, no lag, row threading.
        if tune == "zerolatency" {
            for (property, value) in AV1ENC_LIVE_PROPERTIES {
                if encoder.has_property(property) {
                    set_encoder_property(encoder, property, value)?;
                }
            }
        }
    } else if encoder_name == "vp9enc" {
        // libvpx VP9: target-bitrate is in BITS/SEC, not kbps —
        // verified via `gst-inspect-1.0 vp9enc` on GStreamer 1.28:
        //   target-bitrate : Target bitrate (in bits/sec) ... Default: 256000
        // The block exposes bitrate in kbps so multiply by 1000.
        let bitrate_bps =
            clamp_to_property_range(encoder, "target-bitrate", u64::from(bitrate) * 1000);
        set_encoder_property(encoder, "target-bitrate", &bitrate_bps.to_string())?;
        // VP9: cpu-used (0=slowest, 5=fastest for realtime)
        let cpu_used = map_quality_preset_vp9enc(quality_preset);
        set_encoder_property(encoder, "cpu-used", &cpu_used.to_string())?;

        // libvpx VP9 needs explicit realtime knobs — defaults are tuned for
        // off-line "best quality" and burn entire CPUs on M1 / multi-core:
        //  - deadline default = 1_000_000 µs ("best quality"). At 30 fps the
        //    encoder gets up to 1 s per frame and uses it all → ~10–100×
        //    slower than the realtime mode (deadline=1).
        //  - lag-in-frames default = 25 → look-ahead adds ~833 ms latency
        //    on 30 fps plus extra CPU for the look-ahead analysis.
        //  - row-mt default = false → only frame-level parallelism even
        //    with threads=8. Enabling row-mt gives ~2-3× speedup on
        //    multi-core CPUs.
        // For non-realtime presets we leave the defaults alone.
        if matches!(quality_preset, "ultrafast" | "fast") {
            encoder.set_property("deadline", 1i64);
            encoder.set_property("lag-in-frames", 0i32);
            if encoder.has_property("row-mt") {
                encoder.set_property("row-mt", true);
            }
        }
    }

    // Keyframe interval (GOP size) - try different property names
    if keyframe_interval > 0 {
        if let Some(property) = keyframe_interval_property(|name| encoder.has_property(name)) {
            set_encoder_property(encoder, property, &keyframe_str)?;
        }
    }

    // Rate-limit force-keyunit requests (GstVideoEncoder base-class property,
    // present on all encoders; default 0 = honor every request). WebRTC
    // viewers send a PLI on picture loss and webrtcsink converts each one
    // into a force-keyunit event. With no floor, a burst-induced loss spiral
    // (big frame -> loss -> PLI -> outsized IDR -> more loss -> more PLIs)
    // makes the encoder emit back-to-back keyframes and multiplies the burst
    // it started from. A 1 s floor caps PLI-driven IDRs at one per second:
    // new viewers and genuine recoveries still get their keyframe quickly,
    // but a PLI storm can no longer drive the output to many times the
    // target bitrate.
    if encoder.has_property("min-force-key-unit-interval") {
        encoder.set_property("min-force-key-unit-interval", 1_000_000_000u64);
    }

    info!(
        "Set encoder properties: bitrate={} kbps, preset={}, tune={}, rate_control={:?}, gop={}",
        bitrate, quality_preset, tune, rate_control, keyframe_interval
    );

    Ok(())
}

/// Peak bitrate allowed in VBR, as a percentage of the target.
const BURST_PEAK_PERCENT: u64 = 120;

/// VBV (HRD) window: how much of the target bitrate one burst may take.
const BURST_VBV_WINDOW_MS: u64 = 500;

/// The block's burst policy in explicit units, for encoders whose defaults
/// leave single-frame excursions unbounded. Frames far above the average
/// leave the NIC as line-rate packet bursts that overflow shallow buffers on
/// constrained viewer paths (observed as bursty packet loss on WebRTC). Each
/// encoder branch converts these to its own property units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BurstLimits {
    /// VBR peak cap, bits per second (1000-based, as the block's `bitrate`).
    peak_bits_per_sec: u64,
    /// VBV buffer, bits: one [`BURST_VBV_WINDOW_MS`] window of target bitrate.
    vbv_bits: u64,
}

impl BurstLimits {
    fn for_bitrate_kbps(bitrate_kbps: u32) -> Self {
        let bits_per_sec = u64::from(bitrate_kbps) * 1000;
        Self {
            peak_bits_per_sec: bits_per_sec * BURST_PEAK_PERCENT / 100,
            vbv_bits: bits_per_sec * BURST_VBV_WINDOW_MS / 1000,
        }
    }
}

/// The encoded media type `codec` produces, as it appears in an encoder's
/// source pad template.
fn codec_media_type(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/x-h264",
        Codec::H265 => "video/x-h265",
        Codec::AV1 => "video/x-av1",
        Codec::VP9 => "video/x-vp9",
    }
}

/// The property that takes the keyframe interval in frames, by the names
/// encoders use for it: `key-int-max` (x264/x265, VA-API), `gop-size` (NVENC,
/// QSV, AMF, Media Foundation), `keyint-max`, and `max-keyframe-interval`
/// (Apple VideoToolbox).
fn keyframe_interval_property(has_property: impl Fn(&str) -> bool) -> Option<&'static str> {
    [
        "key-int-max",
        "gop-size",
        "keyint-max",
        "max-keyframe-interval",
    ]
    .into_iter()
    .find(|name| has_property(name))
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

/// svtav1enc `parameters-string` for live output (`tune=zerolatency`): no
/// look-ahead beyond what the encoder needs, no temporal filtering, the
/// shallowest prediction hierarchy.
const SVTAV1_LIVE_PARAMETERS: &str = "lookahead=0:enable-tf=0:hierarchical-levels=1";

/// libaom `av1enc` properties for live output (`tune=zerolatency`).
const AV1ENC_LIVE_PROPERTIES: &[(&str, &str)] = &[
    ("usage-profile", "realtime"),
    ("lag-in-frames", "0"),
    ("row-mt", "true"),
];

/// svtav1enc `intra-period-length` for a keyframe every `keyframe_interval`
/// frames, or `None` for the encoder's choice (0).
fn svtav1_intra_period(keyframe_interval: u32) -> Option<u32> {
    (keyframe_interval > 0).then(|| keyframe_interval - 1)
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

/// Get codec-specific caps string for capsfilter, given a profile selection.
///
/// For H.264/H.265: pins `profile=<name>` on the capsfilter. [`Profile::Auto`]
/// resolves to the codec's 8-bit 4:2:0 profile; [`Profile::None`] adds no
/// profile field and lets the encoder negotiate freely with downstream.
/// For AV1/VP9: caps only contain the codec media type; the profile is kept
/// by constraining the encoder's input instead (see [`base_profile_input_caps`]).
///
/// The pinned field is what keeps the output 4:2:0. It propagates back up
/// through the parser to the encoder, which picks an input format that can
/// produce the profile; without it the encoder follows whatever pixel format
/// arrives and a 4:2:2, 4:4:4 or 10-bit input escalates the output profile.
fn get_codec_caps_string(codec: Codec, profile: Profile) -> String {
    let media_type = codec_media_type(codec);
    if matches!(codec, Codec::AV1 | Codec::VP9) {
        return media_type.to_string();
    }
    match pinned_profile(codec, profile) {
        None => format!("{},alignment=au", media_type),
        Some(p) => format!("{},alignment=au,profile={}", media_type, p),
    }
}

/// The `profile` caps value the capsfilter pins for `codec`, if any.
/// [`Profile::Auto`] resolves to the codec's 8-bit 4:2:0 profile; AV1 and
/// VP9 pin none.
fn pinned_profile(codec: Codec, profile: Profile) -> Option<&'static str> {
    let auto = match codec {
        // High rather than Main: same 8-bit 4:2:0, better compression, and
        // every H.264 target that takes Main takes High.
        Codec::H264 => Profile::High,
        Codec::H265 => Profile::Main,
        Codec::AV1 | Codec::VP9 => return None,
    };
    let effective = if profile == Profile::Auto {
        auto
    } else {
        profile
    };
    effective.as_caps_str()
}

/// 8-bit 4:2:0 raw formats: the input that gives VP9 profile 0 and AV1 Main.
const BASE_PROFILE_FORMATS: [&str; 3] = ["I420", "YV12", "NV12"];

/// Input caps that keep a VP9 or AV1 encoder on its 8-bit 4:2:0 base profile
/// (VP9 profile 0, AV1 Main), or `None` when no constraint applies.
///
/// H.264/H.265 get this from the profile pinned after the encoder (see
/// [`get_codec_caps_string`]); `vp9enc` instead fails with "Invalid vpx
/// profile" on a pinned profile, and follows whatever format arrives, so a
/// 4:4:4 input gives VP9 profile 1 or AV1 High (#1176).
///
/// The caps come from the encoder's sink template. System-memory structures
/// keep only [`BASE_PROFILE_FORMATS`], since the 4:2:0 formats each encoder
/// takes differ (`vp9enc` takes `I420`, not `NV12`). GPU-memory structures are
/// kept as they are, so `videoconvert` still passes GPU memory through (#1104).
/// [`Profile::None`] keeps free negotiation.
fn base_profile_input_caps(
    codec: Codec,
    profile: Profile,
    encoder: &gst::Element,
) -> Option<gst::Caps> {
    if !matches!(codec, Codec::VP9 | Codec::AV1) || profile == Profile::None {
        return None;
    }
    let template = encoder.pad_template("sink")?.caps().clone();
    let caps = constrain_to_base_profile(&template);
    if caps.is_none() {
        warn!(
            "{} takes no 8-bit 4:2:0 format in system memory; its output profile follows the input",
            encoder
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default()
        );
    }
    caps
}

/// `template` with its system-memory raw video limited to
/// [`BASE_PROFILE_FORMATS`] and everything else kept, or `None` when no
/// system-memory structure takes one of them.
fn constrain_to_base_profile(template: &gst::CapsRef) -> Option<gst::Caps> {
    let mut caps = gst::Caps::new_empty();
    let mut constrained = false;
    for (structure, features) in template.iter_with_features() {
        let system_memory =
            features.is_any() || features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);
        if structure.name() != "video/x-raw" || !system_memory {
            caps.get_mut()
                .unwrap()
                .append_structure_full(structure.to_owned(), Some(features.to_owned()));
            continue;
        }
        let accepted: Vec<&str> = BASE_PROFILE_FORMATS
            .into_iter()
            .filter(|format| match structure.value("format") {
                Ok(value) => format.to_value().intersect(value).is_some(),
                Err(_) => true,
            })
            .collect();
        if accepted.is_empty() {
            continue;
        }
        let mut structure = structure.to_owned();
        structure.set("format", gst::List::new(accepted));
        caps.get_mut()
            .unwrap()
            .append_structure_full(structure, Some(features.to_owned()));
        constrained = true;
    }
    constrained.then_some(caps)
}

/// Parse the `profile` property. Unknown / missing values fall back to the
/// enum's `Default` ([`Profile::Auto`]).
fn parse_profile(properties: &HashMap<String, PropertyValue>) -> Profile {
    properties
        .get("profile")
        .and_then(|v| match v {
            PropertyValue::String(s) => {
                let parsed = Profile::from_property_str(s);
                if parsed.is_none() {
                    warn!(
                        "videoenc: invalid profile value '{}', falling back to default",
                        s
                    );
                }
                parsed
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// Parse the `level` property. Unknown / missing values fall back to
/// [`H264Level::None`].
fn parse_h264_level(properties: &HashMap<String, PropertyValue>) -> H264Level {
    properties
        .get("level")
        .and_then(|v| match v {
            PropertyValue::String(s) => {
                let parsed = H264Level::from_property_str(s);
                if parsed.is_none() {
                    warn!(
                        "videoenc: invalid level value '{}', falling back to default",
                        s
                    );
                }
                parsed
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The level setting that applies to `encoder_name`: the setting itself for
/// H.264 on an encoder that takes the level from downstream caps, otherwise
/// [`H264Level::None`] (no level field, as before the setting existed).
fn applied_h264_level(codec: Codec, setting: H264Level, encoder_name: &str) -> H264Level {
    if codec != Codec::H264 || setting == H264Level::None {
        return H264Level::None;
    }
    if !level::takes_level_from_downstream(encoder_name) {
        level::log_not_applied(encoder_name, setting);
        return H264Level::None;
    }
    setting
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
        description: "Video encoder with automatic hardware acceleration selection. Supports H.264, H.265, AV1, and VP9 with automatic selection of NVIDIA NVENC, Intel QSV, VA-API, Apple VideoToolbox, AMD AMF, Windows Media Foundation, V4L2, or software encoders.".to_string(),
        category: "Video".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: CODEC_PROPERTY.to_string(),
                label: "Codec".to_string(),
                description: "Video codec to encode to".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: CODEC_H264.to_string(), label: Some("H.264 / AVC".to_string()), visible_when: None },
                        EnumValue { value: CODEC_H265.to_string(), label: Some("H.265 / HEVC".to_string()), visible_when: None },
                        EnumValue { value: "av1".to_string(), label: Some("AV1".to_string()), visible_when: None },
                        EnumValue { value: "vp9".to_string(), label: Some("VP9".to_string()), visible_when: None },
                    ],
                },
                default_value: Some(PropertyValue::String(CODEC_H264.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: CODEC_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "profile".to_string(),
                label: "Profile".to_string(),
                description: "Codec profile of the encoded output. Auto: High for H.264, Main for H.265, 8-bit 4:2:0 for VP9 and AV1, which every common player decodes. None: the encoder follows the input format.".to_string(),
                property_type: PropertyType::Enum {
                    values: Profile::block_enum_values(),
                },
                default_value: Some(PropertyValue::String(
                    Profile::default().as_property_str().to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "profile".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "level".to_string(),
                label: "Level".to_string(),
                description: "H.264 level of the encoded output. None (default): the encoder picks the level and changes it with the input. A fixed level, or Auto (the lowest of 4.2, 5.1 and 5.2 that fits the first input), keeps it the same when the input changes size mid-stream, for players that cannot follow a level change. Applies to VideoToolbox and x264 only.".to_string(),
                property_type: PropertyType::Enum {
                    values: H264Level::block_enum_values(),
                },
                default_value: Some(PropertyValue::String(
                    H264Level::default().as_property_str().to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "level".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: Some(PropertyCondition::one_of_strings(CODEC_PROPERTY, &[CODEC_H264])),
            },
            ExposedProperty {
                name: "encoder_preference".to_string(),
                label: "Encoder Preference".to_string(),
                description: "Prefer hardware or software encoding".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "auto".to_string(), label: Some("Auto (Hardware first, then software)".to_string()), visible_when: None },
                        EnumValue { value: "hardware".to_string(), label: Some("Hardware Only".to_string()), visible_when: None },
                        EnumValue { value: "software".to_string(), label: Some("Software Only".to_string()), visible_when: None },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "encoder_preference".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
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
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "quality_preset".to_string(),
                label: "Quality Preset".to_string(),
                description: "Encoding quality/speed tradeoff. Slower presets provide better quality at same bitrate.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "ultrafast".to_string(), label: Some("Ultra Fast".to_string()), visible_when: None },
                        EnumValue { value: "fast".to_string(), label: Some("Fast".to_string()), visible_when: None },
                        EnumValue { value: "medium".to_string(), label: Some("Medium".to_string()), visible_when: None },
                        EnumValue { value: "slow".to_string(), label: Some("Slow".to_string()), visible_when: None },
                        EnumValue { value: "veryslow".to_string(), label: Some("Very Slow".to_string()), visible_when: None },
                    ],
                },
                default_value: Some(PropertyValue::String("ultrafast".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "quality_preset".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "tune".to_string(),
                label: "Tune".to_string(),
                description: "Optimize encoder for specific use case (x264/x265; Zero Latency also sets the software AV1 encoders to live settings). Zero latency disables look-ahead for minimal delay.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "zerolatency".to_string(), label: Some("Zero Latency (streaming/real-time)".to_string()), visible_when: None },
                        EnumValue { value: "film".to_string(), label: Some("Film (high quality)".to_string()), visible_when: None },
                        EnumValue { value: "animation".to_string(), label: Some("Animation".to_string()), visible_when: None },
                        EnumValue { value: "grain".to_string(), label: Some("Grain (preserve film grain)".to_string()), visible_when: None },
                        EnumValue { value: "stillimage".to_string(), label: Some("Still Image".to_string()), visible_when: None },
                        EnumValue { value: "fastdecode".to_string(), label: Some("Fast Decode".to_string()), visible_when: None },
                    ],
                },
                default_value: Some(PropertyValue::String("zerolatency".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "tune".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "rate_control".to_string(),
                label: "Rate Control".to_string(),
                description: "Rate control mode for encoding".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "vbr".to_string(), label: Some("VBR (Variable Bitrate)".to_string()), visible_when: None },
                        EnumValue { value: "cbr".to_string(), label: Some("CBR (Constant Bitrate)".to_string()), visible_when: None },
                        EnumValue { value: "cqp".to_string(), label: Some("CQP (Constant Quality)".to_string()), visible_when: None },
                    ],
                },
                default_value: Some(PropertyValue::String("vbr".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "rate_control".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
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
                live: false,
                persist: None,
                visible_when: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "input".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
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

mod level;
mod mediafoundation;
mod selection;
use mediafoundation::{
    is_media_foundation_plugin, set_mf_encoder_properties, MEDIA_FOUNDATION_PLUGIN,
};
use selection::{choose_encoder, named, registry_snapshot, Candidate, EncoderOutput};

#[cfg(test)]
mod tests;
