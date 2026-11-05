//! Settings for the software AV1 encoders: SVT-AV1 (`svtav1enc`) and libaom
//! (`av1enc`).

use super::set_encoder_property;
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;

/// Set bitrate, speed, keyframe interval and live settings on `svtav1enc` or
/// `av1enc`. Other encoders are left alone.
pub(super) fn set_av1_properties(
    encoder: &gst::Element,
    encoder_name: &str,
    bitrate: u32,
    quality_preset: &str,
    tune: &str,
    keyframe_interval: u32,
) -> Result<(), BlockBuildError> {
    let bitrate_str = bitrate.to_string();
    if encoder_name == "svtav1enc" {
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
            set_encoder_property(encoder, "keyframe-max-dist", &keyframe_interval.to_string())?;
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
    }
    Ok(())
}

/// svtav1enc `parameters-string` for live output (`tune=zerolatency`): no
/// look-ahead beyond what the encoder needs, no temporal filtering, the
/// shallowest prediction hierarchy.
pub(super) const SVTAV1_LIVE_PARAMETERS: &str = "lookahead=0:enable-tf=0:hierarchical-levels=1";

/// libaom `av1enc` properties for live output (`tune=zerolatency`).
const AV1ENC_LIVE_PROPERTIES: &[(&str, &str)] = &[
    ("usage-profile", "realtime"),
    ("lag-in-frames", "0"),
    ("row-mt", "true"),
];

/// svtav1enc `intra-period-length` for a keyframe every `keyframe_interval`
/// frames, or `None` for the encoder's choice (0).
pub(super) fn svtav1_intra_period(keyframe_interval: u32) -> Option<u32> {
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
