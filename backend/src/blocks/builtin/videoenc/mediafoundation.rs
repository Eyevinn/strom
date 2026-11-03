//! Windows Media Foundation encoders from GStreamer's `mediafoundation`
//! plugin: how they are identified, and which properties are set on them.
//!
//! Older Windows systems expose their hardware encoder only as a Media
//! Foundation transform (MFT), with no NVENC, QSV or AMF element. The plugin
//! has encoders for H.264, H.265 and VP9 only (no AV1, through 1.28).
//!
//! Policy: the MFTs are not demoted along with Intel QSV (#1163). Strom
//! demotes only what is known to be affected; if MFT crashes are reported,
//! add a demotion then.

use super::{clamp_to_property_range, set_encoder_property, BurstLimits, RateControl};
use crate::blocks::BlockBuildError;
use gstreamer as gst;
use gstreamer::prelude::*;
use tracing::{info, warn};

/// The name GStreamer's Media Foundation plugin registers under. The
/// priority lists name it as a `Candidate::Plugin`: the plugin names its
/// features per device, so they are resolved from the registry.
pub(super) const MEDIA_FOUNDATION_PLUGIN: &str = "mediafoundation";

/// `qp` on `mfh264enc`/`mfh265enc` (16-51, "QP applied when rc-mode is
/// qvbr"): the plugin's own default. No block property sets a quality
/// target, so CQP uses this fixed mid-high quality.
pub(super) const MF_CQP_QP: u64 = 24;

/// `quality` on `mfh264enc` where the MFT has no `qp` (1-100, "Quality
/// applied when rc-mode is qvbr"): the plugin's own default, for the same
/// reason as [`MF_CQP_QP`].
pub(super) const MF_CQP_QUALITY: u64 = 70;

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
/// - VBR: [`MF_VBR_RC_MODE`] where the element offers it, else `cbr`.
/// - CQP: `qvbr`, quality-based VBR: the nearest MF mode, not a fixed QP
///   (see `qp`/`quality` in [`mf_encoder_settings`]).
#[cfg(test)]
pub(super) fn mf_rc_mode_nick(
    rate_control: RateControl,
    available: &[&str],
) -> Option<&'static str> {
    mf_rc_mode_nick_with(rate_control, available, MF_VBR_RC_MODE)
}

/// The `rc-mode` VBR maps to: `pcvbr`, peak-constrained VBR capped by
/// `max-bitrate` (see [`mf_max_bitrate`]). H.264 only; H.265 and VP9 have no
/// `pcvbr` and fall back to `cbr`. `uvbr` is unbounded by design and `qvbr`
/// ignores the bitrate. Whether an MFT accepts `pcvbr` cannot be checked
/// before the stream starts; an Intel Quick Sync H.264 MFT on Windows did,
/// with no warning, and kept to the cap. `cbr` here is the fallback if an
/// MFT turns out to refuse it.
pub(super) const MF_VBR_RC_MODE: &str = "pcvbr";

/// [`mf_rc_mode_nick`] with the VBR mode given, so both choices are tested.
pub(super) fn mf_rc_mode_nick_with(
    rate_control: RateControl,
    available: &[&str],
    vbr_mode: &'static str,
) -> Option<&'static str> {
    let wanted: &[&'static str] = match rate_control {
        RateControl::CBR => &["cbr"],
        RateControl::VBR => &[vbr_mode, "cbr"],
        RateControl::CQP => &["qvbr"],
    };
    wanted.iter().copied().find(|nick| available.contains(nick))
}

/// `max-bitrate` for `pcvbr`, in the plugin's 1024-bit kbit/s
/// (`MIN (self->max_bitrate * 1024, G_MAXUINT - 1)`): the [`BurstLimits`]
/// peak, rounded.
pub(super) fn mf_max_bitrate(bitrate_kbps: u32) -> u64 {
    (BurstLimits::for_bitrate_kbps(bitrate_kbps).peak_bits_per_sec + 512) / 1024
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
/// - `bitrate`: see [`mf_kbit_from_kbps`]. Set in CQP too: `qvbr` ignores
///   it, but the plugin writes it to the output media type
///   (`MF_MT_AVG_BITRATE`) unconditionally, so leaving it out would only put
///   the plugin's 2048 default there instead of the block's value.
/// - `rc-mode`: see [`mf_rc_mode_nick`].
/// - `qp` / `quality`: only for CQP (`qvbr`), [`MF_CQP_QP`] or, where the MFT
///   has no `qp`, [`MF_CQP_QUALITY`]. `mfvp9enc` has neither.
/// - `vbv-buffer-size`: bytes (H.264/H.265 only), from [`BurstLimits`]; not
///   when CQP is requested, whatever mode the element resolves to.
/// - `quality-vs-speed`: see [`map_quality_preset_mf`].
/// - `low-latency`: true only for `tune=zerolatency`, as VideoToolbox
///   `realtime`. Every other tune (`film`, `animation`, `grain`, ...) leaves
///   it false, and the MFT may then buffer frames for lookahead and
///   reordering, adding encoder latency (how much depends on the vendor's
///   MFT). The block's default tune is `zerolatency`.
/// - `bframes`: 0 (also the plugin's default; set explicitly): B-frames
///   reorder PTS, which breaks RTP timestamps (H.264/H.265 only).
///
/// - `max-bitrate`: only with `pcvbr`, the only mode the plugin applies it
///   in; see [`mf_max_bitrate`] and [`MF_VBR_RC_MODE`].
///
/// `gop-size` is set by the block's shared keyframe-interval step.
pub(super) fn mf_encoder_settings(
    has_property: impl Fn(&str) -> bool,
    rc_mode_nicks: &[&str],
    bitrate_kbps: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
) -> Vec<MfSetting> {
    mf_encoder_settings_with(
        has_property,
        rc_mode_nicks,
        bitrate_kbps,
        quality_preset,
        tune,
        rate_control,
        MF_VBR_RC_MODE,
    )
}

/// [`mf_encoder_settings`] with the VBR mode given, so the `pcvbr` path is
/// tested while it is not the default.
pub(super) fn mf_encoder_settings_with(
    has_property: impl Fn(&str) -> bool,
    rc_mode_nicks: &[&str],
    bitrate_kbps: u32,
    quality_preset: &str,
    tune: &str,
    rate_control: RateControl,
    vbr_mode: &'static str,
) -> Vec<MfSetting> {
    let mut settings = Vec::new();
    if has_property("bitrate") {
        settings.push(MfSetting::derived(
            "bitrate",
            mf_kbit_from_kbps(bitrate_kbps),
        ));
    }
    let rc_nick = if has_property("rc-mode") {
        mf_rc_mode_nick_with(rate_control, rc_mode_nicks, vbr_mode)
    } else {
        None
    };
    if let Some(nick) = rc_nick {
        settings.push(MfSetting {
            name: "rc-mode",
            value: MfValue::Nick(nick),
        });
    }
    if rc_nick == Some("pcvbr") && has_property("max-bitrate") {
        settings.push(MfSetting::derived(
            "max-bitrate",
            mf_max_bitrate(bitrate_kbps),
        ));
    }
    if rate_control == RateControl::CQP {
        // The plugin installs `quality` only where the MFT has no `qp`.
        if has_property("qp") {
            settings.push(MfSetting::number("qp", MF_CQP_QP));
        } else if has_property("quality") {
            settings.push(MfSetting::number("quality", MF_CQP_QUALITY));
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
        (RateControl::VBR, Some(nick)) if nick == MF_VBR_RC_MODE => {
            info!("{}: rate_control=VBR, rc-mode={}", encoder_name, nick)
        }
        // INFO, not WARN: VBR is the block default and the operator cannot act on it.
        (RateControl::VBR, Some(nick)) => info!(
            "{}: rate_control=VBR requested, rc-mode={} set: this codec has no {}",
            encoder_name, nick, MF_VBR_RC_MODE
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
