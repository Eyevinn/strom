//! H.264 level on the Video Encoder's output caps.
//!
//! An encoder picks the level from the picture size and rate. When an
//! adaptive source (HLS, DASH) switches variant, 640x360 at 25 fps (level 3)
//! becomes 1920x1080 at 50 fps (level 4.2). A player or sink that cannot
//! follow a level change (some TVs and hardware decoders) needs the level to
//! stay the same: a level pinned on the capsfilter does, and the size change
//! passes in-band. WHEP Output follows a level change on its own input; it does
//! not need this setting.
//!
//! The default is `none`: no level field, as before the setting existed.
//! A fixed level is pinned when the block is built. `auto` is decided from the
//! first caps that reach the encoder's input, and kept from then on. Every
//! caps event is checked against the pinned level: a format the level does not
//! cover fails the flow with a message naming the level it needs.

use gstreamer as gst;
use gstreamer::prelude::*;

use super::Codec;
use std::collections::HashMap;
use std::sync::OnceLock;
use strom_types::videoenc::H264Level;
use strom_types::PropertyValue;
use tracing::{info, warn};

/// Parse the `level` property. Unknown / missing values fall back to
/// [`H264Level::None`].
pub(super) fn parse_h264_level(properties: &HashMap<String, PropertyValue>) -> H264Level {
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
pub(super) fn applied_h264_level(
    codec: Codec,
    setting: H264Level,
    encoder_name: &str,
) -> H264Level {
    if codec != Codec::H264 || setting == H264Level::None {
        return H264Level::None;
    }
    if !takes_level_from_downstream(encoder_name) {
        log_not_applied(encoder_name, setting);
        return H264Level::None;
    }
    setting
}

/// Encoders verified to take the level from the downstream caps. Others pick
/// their own level, and a pinned one could fail to link, so they are left
/// alone.
fn takes_level_from_downstream(encoder_name: &str) -> bool {
    matches!(encoder_name, "vtenc_h264" | "vtenc_h264_hw" | "x264enc")
}

/// What arrives at the encoder's input: picture size and frame rate
/// (`None` when the caps carry no rate or a variable one, `0/1`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct InputFormat {
    pub width: u32,
    pub height: u32,
    pub fps: Option<(u32, u32)>,
}

impl InputFormat {
    pub fn from_caps(caps: &gst::CapsRef) -> Option<Self> {
        let s = caps.structure(0)?;
        let width = u32::try_from(s.get::<i32>("width").ok()?).ok()?;
        let height = u32::try_from(s.get::<i32>("height").ok()?).ok()?;
        let fps = s.get::<gst::Fraction>("framerate").ok().and_then(|f| {
            let (n, d) = (
                u32::try_from(f.numer()).ok()?,
                u32::try_from(f.denom()).ok()?,
            );
            (n > 0 && d > 0).then_some((n, d))
        });
        Some(Self { width, height, fps })
    }

    /// Macroblocks per frame: ceil(w/16) * ceil(h/16).
    fn frame_macroblocks(self) -> u64 {
        u64::from(self.width.div_ceil(16)) * u64::from(self.height.div_ceil(16))
    }

    /// Macroblocks per second, rounded up.
    fn macroblock_rate(self) -> Option<u64> {
        let (n, d) = self.fps?;
        Some((self.frame_macroblocks() * u64::from(n)).div_ceil(u64::from(d)))
    }

    fn describe(self) -> String {
        match self.fps {
            Some((n, 1)) => format!("{}x{} at {} fps", self.width, self.height, n),
            Some((n, d)) => format!(
                "{}x{} at {:.2} fps",
                self.width,
                self.height,
                f64::from(n) / f64::from(d)
            ),
            None => format!("{}x{} (frame rate unknown)", self.width, self.height),
        }
    }
}

/// Whether a fixed `level` covers `format` (frame size and macroblock rate).
/// An unknown frame rate is checked on frame size only.
pub(super) fn covers(level: H264Level, format: InputFormat) -> bool {
    let Some((max_fs, max_mbps)) = level.limits() else {
        return true;
    };
    format.frame_macroblocks() <= max_fs
        && format.macroblock_rate().is_none_or(|rate| rate <= max_mbps)
}

/// The level `auto` pins for the first input format: the lowest of 4.2, 5.1
/// and 5.2 that covers it, or `None` when even 5.2 does not.
pub(super) fn auto_level(format: InputFormat) -> Option<H264Level> {
    H264Level::AUTO_CANDIDATES
        .iter()
        .copied()
        .find(|l| covers(*l, format))
}

/// `Err` with the operator-facing message when `level` does not cover
/// `format`. `auto_from` names the first format when `level` came from auto.
pub(super) fn check(
    level: H264Level,
    format: InputFormat,
    auto_from: Option<InputFormat>,
) -> Result<(), String> {
    if covers(level, format) {
        return Ok(());
    }
    let (max_fs, max_mbps) = level.limits().unwrap_or_default();
    let mut message = format!(
        "Video Encoder: input {} is {} macroblocks per frame",
        format.describe(),
        format.frame_macroblocks()
    );
    if let Some(rate) = format.macroblock_rate() {
        message.push_str(&format!(" and {} per second", rate));
    }
    message.push_str(&format!(
        "; H.264 level {} allows {} and {}.",
        level.as_property_str(),
        max_fs,
        max_mbps
    ));
    if let Some(first) = auto_from {
        message.push_str(&format!(
            " Level auto chose {} from the first input ({}) and keeps it.",
            level.as_property_str(),
            first.describe()
        ));
    }
    match H264Level::FIXED.iter().find(|l| covers(**l, format)) {
        Some(needed) => message.push_str(&format!(
            " It needs level {} or higher: set the Level property to {}.",
            needed.as_property_str(),
            needed.as_property_str()
        )),
        None => message.push_str(" It needs a level above 5.2: set the Level property to none."),
    }
    Err(message)
}

/// Install the caps probe on the encoder's input that pins `auto` and checks
/// every input format against the pinned level. `base_caps` are the
/// capsfilter's caps without a level.
///
/// EVENT_DOWNSTREAM only: it fires per caps event, not per buffer. The
/// capsfilter is held weakly (see CLAUDE.md on references in closures).
pub(super) fn install_probe(
    encoder: &gst::Element,
    capsfilter: &gst::Element,
    base_caps: gst::Caps,
    setting: H264Level,
) {
    let Some(sink) = encoder.static_pad("sink") else {
        warn!("videoenc: encoder has no sink pad, H.264 level not checked");
        return;
    };
    let capsfilter = capsfilter.downgrade();
    // (pinned level, first format when auto chose it)
    let pinned: OnceLock<(H264Level, Option<InputFormat>)> = OnceLock::new();
    if setting != H264Level::Auto {
        let _ = pinned.set((setting, None));
    }
    sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(format) = InputFormat::from_caps(caps_event.caps()) else {
            return gst::PadProbeReturn::Ok;
        };
        let (level, auto_from) = *pinned.get_or_init(|| {
            let Some(level) = auto_level(format) else {
                warn!(
                    "videoenc: H.264 level auto found no level up to 5.2 for {}, level left unpinned",
                    format.describe()
                );
                return (H264Level::None, Some(format));
            };
            if let Some(capsfilter) = capsfilter.upgrade() {
                let mut caps = base_caps.clone();
                if let Some(s) = caps.make_mut().structure_mut(0) {
                    s.set("level", level.as_property_str());
                }
                info!(
                    "videoenc: H.264 level auto pins {} for {}",
                    level.as_property_str(),
                    format.describe()
                );
                capsfilter.set_property("caps", &caps);
            }
            (level, Some(format))
        });
        if level == H264Level::None {
            // Auto found no level that covers the first input: unpinned.
            return gst::PadProbeReturn::Ok;
        }
        match check(level, format, auto_from) {
            Ok(()) => gst::PadProbeReturn::Ok,
            Err(message) => {
                if let Some(encoder) = pad.parent_element() {
                    gst::element_error!(encoder, gst::CoreError::Negotiation, ("{}", message));
                }
                gst::PadProbeReturn::Drop
            }
        }
    });
}

/// Log, once per block build, that the level setting is not applied to an
/// encoder that picks its own level.
fn log_not_applied(encoder_name: &str, setting: H264Level) {
    info!(
        "videoenc: H.264 level setting '{}' is not applied to {}: only vtenc_h264, vtenc_h264_hw and x264enc are known to take the level from downstream caps",
        setting.as_property_str(),
        encoder_name
    );
}

#[cfg(test)]
mod tests;
