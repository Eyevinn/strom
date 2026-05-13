//! Shared types for the Video Encoder block.

use crate::block::EnumValue;

/// Codec profile constraint on the video encoder's output.
///
/// Values map 1:1 to the GStreamer `profile` caps field for H.264 / H.265
/// (except `None`, which means "no profile field" — let the encoder negotiate
/// freely with downstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(non_camel_case_types)]
pub enum Profile {
    /// No profile constraint — encoder negotiates freely with downstream.
    #[default]
    None,
    ConstrainedBaseline,
    Baseline,
    Main,
    High,
    High10,
    High422,
    High444,
    Main10,
    Main12,
    Main422_10,
    Main422_12,
    Main444,
    Main444_10,
    Main444_12,
    MainStillPicture,
}

impl Profile {
    /// All variants in canonical display order.
    pub const ALL: &'static [Profile] = &[
        Self::None,
        Self::ConstrainedBaseline,
        Self::Baseline,
        Self::Main,
        Self::High,
        Self::High10,
        Self::High422,
        Self::High444,
        Self::Main10,
        Self::Main12,
        Self::Main422_10,
        Self::Main422_12,
        Self::Main444,
        Self::Main444_10,
        Self::Main444_12,
        Self::MainStillPicture,
    ];

    /// Property-value string used in the block's `properties` map and over the API.
    /// For non-`None` variants this is also the GStreamer caps profile name.
    pub fn as_property_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ConstrainedBaseline => "constrained-baseline",
            Self::Baseline => "baseline",
            Self::Main => "main",
            Self::High => "high",
            Self::High10 => "high-10",
            Self::High422 => "high-4:2:2",
            Self::High444 => "high-4:4:4",
            Self::Main10 => "main-10",
            Self::Main12 => "main-12",
            Self::Main422_10 => "main-422-10",
            Self::Main422_12 => "main-422-12",
            Self::Main444 => "main-444",
            Self::Main444_10 => "main-444-10",
            Self::Main444_12 => "main-444-12",
            Self::MainStillPicture => "main-still-picture",
        }
    }

    /// GStreamer caps profile string, or `None` if no `profile=` field should
    /// be set on the capsfilter.
    pub fn as_caps_str(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            other => Some(other.as_property_str()),
        }
    }

    /// Human-readable UI label for property dropdowns.
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None (no constraint)",
            Self::ConstrainedBaseline => "Constrained Baseline (H.264)",
            Self::Baseline => "Baseline (H.264)",
            Self::Main => "Main (H.264 / H.265)",
            Self::High => "High (H.264)",
            Self::High10 => "High 10-bit (H.264)",
            Self::High422 => "High 4:2:2 (H.264, 8/10-bit)",
            Self::High444 => "High 4:4:4 (H.264)",
            Self::Main10 => "Main 10 (H.265, 10-bit 4:2:0)",
            Self::Main12 => "Main 12 (H.265, 12-bit 4:2:0)",
            Self::Main422_10 => "Main 4:2:2 10 (H.265)",
            Self::Main422_12 => "Main 4:2:2 12 (H.265)",
            Self::Main444 => "Main 4:4:4 (H.265)",
            Self::Main444_10 => "Main 4:4:4 10 (H.265)",
            Self::Main444_12 => "Main 4:4:4 12 (H.265)",
            Self::MainStillPicture => "Main Still Picture (H.265)",
        }
    }

    /// Parse a property string. Returns `None` for unknown values so callers
    /// can decide whether to fall back to default and/or log.
    pub fn from_property_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.as_property_str() == s)
    }

    /// `EnumValue` list for `BlockDefinition` — keeps the API enum surface and
    /// the Rust enum in lock-step.
    pub fn block_enum_values() -> Vec<EnumValue> {
        Self::ALL
            .iter()
            .map(|p| EnumValue {
                value: p.as_property_str().to_string(),
                label: Some(p.label().to_string()),
            })
            .collect()
    }
}
