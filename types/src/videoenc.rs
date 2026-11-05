//! Shared types for the Video Encoder block.

use crate::block::{EnumValue, PropertyCondition};

/// Codec profile constraint on the video encoder's output.
///
/// Values map 1:1 to the GStreamer `profile` caps field for H.264 / H.265,
/// except for the two that name a policy rather than a profile: `Auto`, which
/// resolves per codec, and `None`, which means "no profile field" — let the
/// encoder negotiate freely with downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(non_camel_case_types)]
pub enum Profile {
    /// The codec's widely decodable 8-bit 4:2:0 profile, resolved by the
    /// caller that knows the codec: `high` for H.264, `main` for H.265, and no
    /// profile field for AV1 / VP9.
    ///
    /// Unpinned, an encoder follows whatever pixel format reaches it, so a
    /// 4:2:2, 4:4:4 or 10-bit input yields a profile many delivery targets
    /// refuse.
    #[default]
    Auto,
    /// No profile constraint — encoder negotiates freely with downstream.
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
        Self::Auto,
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
    /// For variants other than `Auto` and `None` this is also the GStreamer
    /// caps profile name.
    pub fn as_property_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
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
    ///
    /// [`Profile::Auto`] has no codec-independent name, so it also yields
    /// `None` here. Resolve it against the codec first.
    pub fn as_caps_str(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::None => None,
            other => Some(other.as_property_str()),
        }
    }

    /// Human-readable UI label for property dropdowns.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::None => "None",
            Self::ConstrainedBaseline => "Constrained Baseline",
            Self::Baseline => "Baseline",
            Self::Main => "Main",
            Self::High => "High",
            Self::High10 => "High 10",
            Self::High422 => "High 4:2:2",
            Self::High444 => "High 4:4:4",
            Self::Main10 => "Main 10",
            Self::Main12 => "Main 12",
            Self::Main422_10 => "Main 4:2:2 10",
            Self::Main422_12 => "Main 4:2:2 12",
            Self::Main444 => "Main 4:4:4",
            Self::Main444_10 => "Main 4:4:4 10",
            Self::Main444_12 => "Main 4:4:4 12",
            Self::MainStillPicture => "Main Still Picture",
        }
    }

    /// Parse a property string. Returns `None` for unknown values so callers
    /// can decide whether to fall back to default and/or log.
    pub fn from_property_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.as_property_str() == s)
    }

    /// The `codec` property values this profile applies to, or `None` for
    /// the two policies (`auto`, `none`) that apply to every codec.
    pub fn codecs(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Auto | Self::None => None,
            Self::Main => Some(&[CODEC_H264, CODEC_H265]),
            Self::ConstrainedBaseline
            | Self::Baseline
            | Self::High
            | Self::High10
            | Self::High422
            | Self::High444 => Some(&[CODEC_H264]),
            Self::Main10
            | Self::Main12
            | Self::Main422_10
            | Self::Main422_12
            | Self::Main444
            | Self::Main444_10
            | Self::Main444_12
            | Self::MainStillPicture => Some(&[CODEC_H265]),
        }
    }

    /// `EnumValue` list for `BlockDefinition` — keeps the API enum surface and
    /// the Rust enum in lock-step. Each named profile is offered only for the
    /// codecs it belongs to (see [`Profile::codecs`]).
    pub fn block_enum_values() -> Vec<EnumValue> {
        Self::ALL
            .iter()
            .map(|p| EnumValue {
                value: p.as_property_str().to_string(),
                label: Some(p.label().to_string()),
                visible_when: p
                    .codecs()
                    .map(|codecs| PropertyCondition::one_of_strings(CODEC_PROPERTY, codecs)),
            })
            .collect()
    }
}

/// H.264 level pinned on the video encoder's output caps.
///
/// An encoder picks the level from the picture size and rate, so a source
/// that changes size mid-stream (an HLS variant switch) also changes the
/// level, which a player or sink that cannot follow a level change refuses.
/// A pinned level stays the same through the change. The default, `None`,
/// pins nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum H264Level {
    /// The lowest of 4.2, 5.1 and 5.2 that covers the first input format.
    Auto,
    L3_1,
    L4_1,
    L4_2,
    L5_1,
    L5_2,
    /// No level field: the encoder picks the level, and changes it with the
    /// input's size and rate.
    #[default]
    None,
}

impl H264Level {
    /// All variants in canonical display order.
    pub const ALL: &'static [H264Level] = &[
        Self::Auto,
        Self::L3_1,
        Self::L4_1,
        Self::L4_2,
        Self::L5_1,
        Self::L5_2,
        Self::None,
    ];

    /// The fixed levels `Auto` chooses from, lowest first. Never below 4.2:
    /// 1080p60 needs 4.2, and some TVs take nothing between 4.2 and 5.1.
    pub const AUTO_CANDIDATES: &'static [H264Level] = &[Self::L4_2, Self::L5_1, Self::L5_2];

    /// The fixed levels, lowest first.
    pub const FIXED: &'static [H264Level] =
        &[Self::L3_1, Self::L4_1, Self::L4_2, Self::L5_1, Self::L5_2];

    /// Property-value string used in the block's `properties` map and over
    /// the API. For the fixed levels this is also the GStreamer caps `level`.
    pub fn as_property_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::L3_1 => "3.1",
            Self::L4_1 => "4.1",
            Self::L4_2 => "4.2",
            Self::L5_1 => "5.1",
            Self::L5_2 => "5.2",
            Self::None => "none",
        }
    }

    /// GStreamer caps `level` string, or `None` for `Auto` and `None`.
    pub fn as_caps_str(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::None => None,
            other => Some(other.as_property_str()),
        }
    }

    /// Limits from ITU-T H.264 Table A-1: maximum frame size (MaxFS, in
    /// macroblocks) and maximum macroblock processing rate (MaxMBPS, in
    /// macroblocks per second). `None` for `Auto` and `None`.
    pub fn limits(self) -> Option<(u64, u64)> {
        match self {
            Self::L3_1 => Some((3_600, 108_000)),
            Self::L4_1 => Some((8_192, 245_760)),
            Self::L4_2 => Some((8_704, 522_240)),
            Self::L5_1 => Some((36_864, 983_040)),
            Self::L5_2 => Some((36_864, 2_073_600)),
            Self::Auto | Self::None => None,
        }
    }

    /// Human-readable UI label for property dropdowns.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (4.2, 5.1 or 5.2 from the first input)",
            Self::L3_1 => "3.1 (up to 720p30)",
            Self::L4_1 => "4.1 (up to 1080p30)",
            Self::L4_2 => "4.2 (up to 1080p60)",
            Self::L5_1 => "5.1 (up to 2160p30)",
            Self::L5_2 => "5.2 (up to 2160p60)",
            Self::None => "None (default: encoder picks, may change with the input)",
        }
    }

    /// Parse a property string. Returns `None` for unknown values.
    pub fn from_property_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.as_property_str() == s)
    }

    /// `EnumValue` list for `BlockDefinition`.
    pub fn block_enum_values() -> Vec<EnumValue> {
        Self::ALL
            .iter()
            .map(|l| EnumValue {
                value: l.as_property_str().to_string(),
                label: Some(l.label().to_string()),
                visible_when: None,
            })
            .collect()
    }
}

/// Block property that picks the codec.
pub const CODEC_PROPERTY: &str = "codec";
/// `codec` property value for H.264.
pub const CODEC_H264: &str = "h264";
/// `codec` property value for H.265.
pub const CODEC_H265: &str = "h265";
/// `codec` property value for AV1.
pub const CODEC_AV1: &str = "av1";
/// `codec` property value for VP9.
pub const CODEC_VP9: &str = "vp9";
/// `codec` property value for VP8.
pub const CODEC_VP8: &str = "vp8";

/// The codecs the `profile` property applies to. VP8 has no profile choice:
/// its one input format (8-bit 4:2:0) is all the encoders take, so the
/// property is hidden for it.
pub const CODECS_WITH_PROFILE: &[&str] = &[CODEC_H264, CODEC_H265, CODEC_AV1, CODEC_VP9];

/// The codecs the `profile` property does not apply to. Every codec the
/// Video Encoder block offers is in exactly one of this list and
/// [`CODECS_WITH_PROFILE`]; the block's tests check it, so a new codec has to
/// choose.
pub const CODECS_WITHOUT_PROFILE: &[&str] = &[CODEC_VP8];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_level_round_trips_and_defaults_to_none() {
        assert_eq!(H264Level::default(), H264Level::None);
        for level in H264Level::ALL {
            assert_eq!(
                H264Level::from_property_str(level.as_property_str()),
                Some(*level)
            );
        }
        assert_eq!(H264Level::from_property_str("6.2"), None);
        let values: Vec<String> = H264Level::block_enum_values()
            .into_iter()
            .map(|v| v.value)
            .collect();
        assert_eq!(values, ["auto", "3.1", "4.1", "4.2", "5.1", "5.2", "none"]);
    }

    #[test]
    fn h264_level_limits_follow_table_a1() {
        // ITU-T H.264 Table A-1 (MaxFS, MaxMBPS).
        assert_eq!(H264Level::L3_1.limits(), Some((3_600, 108_000)));
        assert_eq!(H264Level::L4_1.limits(), Some((8_192, 245_760)));
        assert_eq!(H264Level::L4_2.limits(), Some((8_704, 522_240)));
        assert_eq!(H264Level::L5_1.limits(), Some((36_864, 983_040)));
        assert_eq!(H264Level::L5_2.limits(), Some((36_864, 2_073_600)));
        assert_eq!(H264Level::Auto.limits(), None);
        assert_eq!(H264Level::None.limits(), None);
        assert_eq!(H264Level::Auto.as_caps_str(), None);
        assert_eq!(H264Level::L4_2.as_caps_str(), Some("4.2"));
    }

    #[test]
    fn every_named_profile_names_its_codec_and_policies_apply_to_all() {
        for p in Profile::ALL {
            let codecs = p.codecs();
            match p {
                Profile::Auto | Profile::None => assert!(codecs.is_none(), "{p:?}"),
                _ => assert!(!codecs.unwrap().is_empty(), "{p:?}"),
            }
        }
        assert_eq!(Profile::High.codecs(), Some(&[CODEC_H264][..]));
        assert_eq!(Profile::Main10.codecs(), Some(&[CODEC_H265][..]));
        assert_eq!(Profile::Main.codecs(), Some(&[CODEC_H264, CODEC_H265][..]));
    }

    #[test]
    fn every_named_profile_belongs_to_a_codec_with_a_profile() {
        assert!(!CODECS_WITH_PROFILE.contains(&CODEC_VP8));
        for codec in CODECS_WITHOUT_PROFILE {
            assert!(
                !CODECS_WITH_PROFILE.contains(codec),
                "{codec} is in both lists"
            );
        }
        for p in Profile::ALL {
            for codec in p.codecs().unwrap_or_default() {
                assert!(CODECS_WITH_PROFILE.contains(codec), "{p:?} names {codec}");
            }
        }
    }
}
