//! Windows Media Foundation encoders (`mfh264enc`, `mfh265enc`, `mfvp9enc`)
//! from GStreamer's `mediafoundation` plugin: which features to select, and
//! which properties to set on them.
//!
//! Older Windows systems expose their hardware encoder only as a Media
//! Foundation transform (MFT), with no NVENC, QSV or AMF element. The plugin
//! has no AV1 encoder (H.264, H.265 and VP9 only, through 1.28).

use super::{clamp_to_property_range, set_encoder_property, RateControl};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::warn;

/// The highest device index Media Foundation numbers: up to 12 hardware
/// adapters (the plugin enumerates 12, for AMD), one of them the unnumbered
/// default, plus the software MFT registered after them.
const MF_MAX_DEVICE_INDEX: u32 = 12;

/// Whether `name` is an encoder from GStreamer's `mediafoundation` plugin.
pub(super) fn is_media_foundation(name: &str) -> bool {
    name.starts_with("mf") && name.ends_with("enc")
}

/// The feature names to try, in order, for one candidate of the priority list.
///
/// GStreamer's `mediafoundation` plugin registers the first device it finds
/// for a codec as `mf{codec}enc` and each further one as
/// `mf{codec}device{N}enc`, N counting from 1, one rank lower. Hardware MFTs
/// are registered first, then Microsoft's software MFT, which takes the next
/// free name: `mfh264enc` itself on a host with no hardware MFT. So the
/// numbered variants are tried after the default, in case the default is
/// disabled (rank NONE) or software. Other candidates are tried as named.
pub(super) fn candidate_feature_names(candidate: &str) -> Vec<String> {
    let mut names = vec![candidate.to_string()];
    if is_media_foundation(candidate) && !candidate.contains("device") {
        let codec = &candidate[2..candidate.len() - 3];
        names.extend((1..=MF_MAX_DEVICE_INDEX).map(|i| format!("mf{}device{}enc", codec, i)));
    }
    names
}

/// Whether a feature from the hardware list may be selected. A Media
/// Foundation feature can be Microsoft's software MFT, which the plugin
/// registers under the same naming and rank as the hardware ones; only the
/// klass (`Codec/Encoder/Video/Hardware`) tells them apart. A software MFT is
/// never picked: x264/x265/libvpx are the better software encoders.
pub(super) fn hardware_candidate_accepted(feature_name: &str, is_hardware: bool) -> bool {
    !is_media_foundation(feature_name) || is_hardware
}

/// One property the Media Foundation branch sets.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MfSetting {
    pub(super) name: &'static str,
    pub(super) value: String,
    /// Derived from `bitrate` rather than supplied by the client, so clamped
    /// to the property's range instead of refused (#769).
    pub(super) derived: bool,
}

impl MfSetting {
    fn new(name: &'static str, value: impl ToString) -> Self {
        Self {
            name,
            value: value.to_string(),
            derived: false,
        }
    }

    fn derived(name: &'static str, value: u64) -> Self {
        Self {
            name,
            value: value.to_string(),
            derived: true,
        }
    }
}

/// The Media Foundation `rc-mode` nick for `rate_control`, among the nicks
/// the element offers. The enum differs per codec in GStreamer's
/// `mediafoundation` plugin (1.22 through 1.28): H.264 has `cbr`, `pcvbr`,
/// `uvbr` and `qvbr`; H.265 and VP9 have only `cbr` and `qvbr`.
///
/// - CBR: `cbr`.
/// - VBR: `pcvbr` (peak-constrained, capped by `max-bitrate`), never `uvbr`,
///   whose excursions are unbounded. Where there is no `pcvbr`, `cbr`: the
///   only other mode, `qvbr`, ignores the bitrate, and a user asking for VBR
///   means "roughly the target bitrate".
/// - CQP: `qvbr`, quality-based, the MF counterpart of constant quality.
pub(super) fn mf_rc_mode_nick(
    rate_control: RateControl,
    available: &[&str],
) -> Option<&'static str> {
    let wanted: &[&'static str] = match rate_control {
        RateControl::CBR => &["cbr"],
        RateControl::VBR => &["pcvbr", "cbr"],
        RateControl::CQP => &["qvbr"],
    };
    wanted.iter().copied().find(|nick| available.contains(nick))
}

/// Map quality preset to Media Foundation `quality-vs-speed` (0-100; the
/// plugin documents [0, 33] as low, [34, 66] as medium and [67, 100] as high
/// complexity). Higher is slower and better.
pub(super) fn map_quality_preset_mf(quality_preset: &str) -> u32 {
    match quality_preset {
        "ultrafast" => 0,
        "fast" => 33,
        "slow" => 67,
        "veryslow" => 100,
        _ => 50, // medium (the plugin's default)
    }
}

/// The properties to set on a Media Foundation encoder.
///
/// Names, types and units are from GStreamer's `sys/mediafoundation`
/// (identical in 1.22 and 1.28). The plugin installs most properties only
/// when the MFT reports support for the matching codec API, so the set
/// differs by vendor, driver and codec, and every property is checked first.
/// - `bitrate`: kbit/s.
/// - `rc-mode`: see [`mf_rc_mode_nick`].
/// - `max-bitrate`: kbit/s, applied by the plugin only in `pcvbr`; 1.2x the
///   target, as on NVENC.
/// - `vbv-buffer-size`: bytes (H.264/H.265 only); 0.5 s of target bitrate,
///   bounding single-frame bursts as on NVENC and x264. Not in `qvbr`.
/// - `quality-vs-speed`: see [`map_quality_preset_mf`].
/// - `low-latency`: from the `zerolatency` tune, as VideoToolbox `realtime`.
/// - `bframes`: 0 (also the plugin's default; set explicitly): B-frames
///   reorder PTS, which breaks RTP timestamps (H.264/H.265 only).
///
/// `gop-size` is set by the block's shared keyframe-interval step.
pub(super) fn mf_encoder_settings(
    has_property: impl Fn(&str) -> bool,
    rc_mode_nicks: &[&str],
    bitrate: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
) -> Vec<MfSetting> {
    let mut settings = Vec::new();
    if has_property("bitrate") {
        settings.push(MfSetting::new("bitrate", bitrate));
    }
    let rc_nick = if has_property("rc-mode") {
        mf_rc_mode_nick(rate_control, rc_mode_nicks)
    } else {
        None
    };
    if let Some(nick) = rc_nick {
        settings.push(MfSetting::new("rc-mode", nick));
    }
    if rc_nick == Some("pcvbr") && has_property("max-bitrate") {
        settings.push(MfSetting::derived(
            "max-bitrate",
            u64::from(bitrate) * 12 / 10,
        ));
    }
    if rc_nick != Some("qvbr") && has_property("vbv-buffer-size") {
        // kbit/s to bytes per half second
        settings.push(MfSetting::derived(
            "vbv-buffer-size",
            u64::from(bitrate) * 1000 / 16,
        ));
    }
    if has_property("quality-vs-speed") {
        settings.push(MfSetting::new(
            "quality-vs-speed",
            map_quality_preset_mf(quality_preset),
        ));
    }
    if has_property("low-latency") {
        settings.push(MfSetting::new("low-latency", tune == "zerolatency"));
    }
    if has_property("bframes") {
        settings.push(MfSetting::new("bframes", 0));
    }
    settings
}

/// The nicks of `encoder`'s `rc-mode` enum, empty when it has none.
fn rc_mode_nicks(encoder: &gst::Element) -> Vec<String> {
    encoder
        .find_property("rc-mode")
        .and_then(|pspec| {
            pspec
                .downcast_ref::<gst::glib::ParamSpecEnum>()
                .map(|pspec| {
                    pspec
                        .enum_class()
                        .values()
                        .iter()
                        .map(|v| v.nick().to_string())
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// Apply [`mf_encoder_settings`] to a Media Foundation encoder.
pub(super) fn set_mf_encoder_properties(
    encoder: &gst::Element,
    encoder_name: &str,
    bitrate: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
) -> Result<(), BlockBuildError> {
    let nicks = rc_mode_nicks(encoder);
    let nicks: Vec<&str> = nicks.iter().map(String::as_str).collect();

    let settings = mf_encoder_settings(
        |name| encoder.has_property(name),
        &nicks,
        bitrate,
        quality_preset,
        tune,
        rate_control,
    );
    if !settings.iter().any(|s| s.name == "rc-mode") {
        warn!(
            "{} offers no rc-mode for rate_control={:?} (has: {:?}); the MFT default applies",
            encoder_name, rate_control, nicks
        );
    }
    for setting in settings {
        if setting.derived {
            let value = setting.value.parse::<u64>().unwrap_or_default();
            let value = clamp_to_property_range(encoder, setting.name, value);
            set_encoder_property(encoder, setting.name, &value.to_string())?;
        } else {
            set_encoder_property(encoder, setting.name, &setting.value)?;
        }
    }
    Ok(())
}
