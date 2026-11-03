//! Windows Media Foundation encoders from GStreamer's `mediafoundation`
//! plugin: how they are identified, and which properties are set on them.
//!
//! Older Windows systems expose their hardware encoder only as a Media
//! Foundation transform (MFT), with no NVENC, QSV or AMF element. The plugin
//! has encoders for H.264, H.265 and VP9 only (no AV1, through 1.28).
//!
//! Intel QSV and #1163: QSV is demoted on Windows hosts whose Intel driver
//! corrupts the heap during the `qsv` plugin's load-time probe. Intel's MFT
//! comes in the same driver package, but on the affected host the
//! `mediafoundation` family alone crashed 0 of 15 runs (QSV alone: 3 of 15).
//! Strom demotes only what is known to be affected, so the MFTs keep their
//! rank. If MFT crashes are reported, add a demotion then.

use super::{clamp_to_property_range, set_encoder_property, BurstLimits, RateControl};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The name GStreamer's Media Foundation plugin registers under.
pub(super) const MEDIA_FOUNDATION_PLUGIN: &str = "mediafoundation";

/// Priority-list entry that stands for every hardware video encoder of the
/// `mediafoundation` plugin for the codec. Selection resolves it from the
/// registry, because the plugin names its features per device.
pub(super) const MEDIA_FOUNDATION_CANDIDATE: &str = "mediafoundation";

/// Whether a factory's plugin is GStreamer's `mediafoundation` plugin.
pub(super) fn is_media_foundation_plugin(plugin_name: Option<&str>) -> bool {
    plugin_name == Some(MEDIA_FOUNDATION_PLUGIN)
}

/// A typed property value for a Media Foundation encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MfValue {
    /// An integer; `clamp` when it is derived by the block (unit conversion,
    /// burst policy), so it is clamped to the property's range rather than
    /// refused (#769).
    Number {
        value: u64,
        clamp: bool,
    },
    Bool(bool),
    /// An enum nick.
    Nick(&'static str),
}

/// One property the Media Foundation branch sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MfSetting {
    pub(super) name: &'static str,
    pub(super) value: MfValue,
}

impl MfSetting {
    fn number(name: &'static str, value: u64) -> Self {
        Self {
            name,
            value: MfValue::Number {
                value,
                clamp: false,
            },
        }
    }

    fn derived(name: &'static str, value: u64) -> Self {
        Self {
            name,
            value: MfValue::Number { value, clamp: true },
        }
    }
}

/// Convert the block's bitrate (kbps, 1000 bits) to Media Foundation's
/// `bitrate` unit: "Bitrate in kbit/sec", where the plugin multiplies by 1024
/// (`MIN (self->bitrate * 1024, G_MAXUINT - 1)` in `gstmfh264enc.cpp`,
/// `gstmfh265enc.cpp`, `gstmfvp9enc.cpp`). Rounded to nearest.
pub(super) fn mf_kbit_from_kbps(bitrate_kbps: u32) -> u64 {
    (u64::from(bitrate_kbps) * 1000 + 512) / 1024
}

/// The Media Foundation `rc-mode` nick for `rate_control`, among the nicks
/// the element's enum offers. The enum differs per codec (1.22 through 1.28):
/// H.264 has `cbr`, `pcvbr`, `uvbr` and `qvbr`; H.265 and VP9 have only `cbr`
/// and `qvbr`.
///
/// The enum is static: it says what the plugin knows, not what this MFT
/// supports. The plugin hands the mode to the MFT only when the stream starts
/// and logs a GStreamer warning if the MFT refuses it, after which the MFT
/// keeps its own default; reading the property back returns the value set,
/// accepted or not. So there is no way to check at build time, and the mapping
/// prefers the mode with bounded output that MFTs are most likely to accept:
/// - CBR: `cbr`.
/// - VBR: `cbr` too. `pcvbr` (H.264 only) would be bounded, but if the MFT
///   refuses it nothing bounds the output; `uvbr` is unbounded by design, and
///   `qvbr` ignores the bitrate.
/// - CQP: `qvbr`, quality-based VBR: the nearest MF mode, not a fixed QP.
pub(super) fn mf_rc_mode_nick(
    rate_control: RateControl,
    available: &[&str],
) -> Option<&'static str> {
    let wanted = match rate_control {
        RateControl::CBR | RateControl::VBR => "cbr",
        RateControl::CQP => "qvbr",
    };
    available.contains(&wanted).then_some(wanted)
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
/// - `bitrate`: see [`mf_kbit_from_kbps`].
/// - `rc-mode`: see [`mf_rc_mode_nick`].
/// - `vbv-buffer-size`: bytes (H.264/H.265 only), from [`BurstLimits`]; not
///   when CQP is requested, whatever mode the element resolves to.
/// - `quality-vs-speed`: see [`map_quality_preset_mf`].
/// - `low-latency`: from the `zerolatency` tune, as VideoToolbox `realtime`.
/// - `bframes`: 0 (also the plugin's default; set explicitly): B-frames
///   reorder PTS, which breaks RTP timestamps (H.264/H.265 only).
///
/// `max-bitrate` is not set: the plugin applies it only in `pcvbr`, which is
/// not used. `gop-size` is set by the block's shared keyframe-interval step.
pub(super) fn mf_encoder_settings(
    has_property: impl Fn(&str) -> bool,
    rc_mode_nicks: &[&str],
    bitrate_kbps: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
) -> Vec<MfSetting> {
    let mut settings = Vec::new();
    if has_property("bitrate") {
        settings.push(MfSetting::derived(
            "bitrate",
            mf_kbit_from_kbps(bitrate_kbps),
        ));
    }
    if has_property("rc-mode") {
        if let Some(nick) = mf_rc_mode_nick(rate_control, rc_mode_nicks) {
            settings.push(MfSetting {
                name: "rc-mode",
                value: MfValue::Nick(nick),
            });
        }
    }
    if rate_control != RateControl::CQP && has_property("vbv-buffer-size") {
        let limits = BurstLimits::for_bitrate_kbps(bitrate_kbps);
        settings.push(MfSetting::derived("vbv-buffer-size", limits.vbv_bits / 8));
    }
    if has_property("quality-vs-speed") {
        settings.push(MfSetting::number(
            "quality-vs-speed",
            u64::from(map_quality_preset_mf(quality_preset)),
        ));
    }
    if has_property("low-latency") {
        settings.push(MfSetting {
            name: "low-latency",
            value: MfValue::Bool(tune == "zerolatency"),
        });
    }
    if has_property("bframes") {
        settings.push(MfSetting::number("bframes", 0));
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
    bitrate_kbps: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
) -> Result<(), BlockBuildError> {
    let nicks = rc_mode_nicks(encoder);
    let nicks: Vec<&str> = nicks.iter().map(String::as_str).collect();

    let settings = mf_encoder_settings(
        |name| encoder.has_property(name),
        &nicks,
        bitrate_kbps,
        quality_preset,
        tune,
        rate_control,
    );
    let rc_set = settings.iter().find_map(|s| match s {
        MfSetting {
            name: "rc-mode",
            value: MfValue::Nick(nick),
        } => Some(*nick),
        _ => None,
    });
    match (rate_control, rc_set) {
        (_, None) => warn!(
            "{}: no rc-mode for rate_control={:?} (offers: {:?}); the MFT default applies",
            encoder_name, rate_control, nicks
        ),
        (RateControl::VBR, Some(nick)) => warn!(
            "{}: rate_control=VBR requested, rc-mode={} set; whether this MFT supports a bounded VBR mode cannot be checked before the stream starts",
            encoder_name, nick
        ),
        (RateControl::CQP, Some(nick)) => warn!(
            "{}: rate_control=CQP requested, rc-mode={} (quality-based VBR) set; Media Foundation has no constant-QP mode",
            encoder_name, nick
        ),
        (RateControl::CBR, Some(nick)) => {
            info!("{}: rc-mode={}", encoder_name, nick)
        }
    }

    for setting in settings {
        let text = match setting.value {
            MfValue::Number { value, clamp } => {
                let value = if clamp {
                    clamp_to_property_range(encoder, setting.name, value)
                } else {
                    value
                };
                value.to_string()
            }
            MfValue::Bool(value) => value.to_string(),
            MfValue::Nick(nick) => nick.to_string(),
        };
        set_encoder_property(encoder, setting.name, &text)?;
    }
    Ok(())
}
