//! Stinger transitions: a clip played over the program while the source
//! changes beneath it.
//!
//! Three variants, after the conventions of ATEM, vMix and OBS:
//! - **Classic**: a graphic with an alpha channel that covers the frame at a
//!   cut point, where the program cuts or mixes beneath it.
//! - **Track matte**: the clip carries a matte beside or below the graphic
//!   (the OBS layout). Black in the matte shows the old source, white the new
//!   one, grey blends; the switch follows the clip frame by frame.
//! - **Mask only**: the whole clip is a matte, with no graphic: an animated
//!   wipe.
//!
//! The matte variants need the GPU mixer; the software mixer plays them as a
//! classic stinger.

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Block property on a Media Player that makes it a stinger clip source,
/// and on an HTML Input that makes its page a stinger.
pub const STINGER_MODE_PROPERTY: &str = "stinger_mode";

/// HTML Input property: how long a stinger page covers the program, from
/// the trigger. Required for a page stinger.
pub const WEB_STINGER_DURATION_PROPERTY: &str = "stinger_duration_ms";

/// HTML Input property: how far into a stinger page the program changes.
/// 0 takes the middle of the duration.
pub const WEB_STINGER_CUT_POINT_PROPERTY: &str = "stinger_cut_point_ms";

/// HTML Input property: how long the program mixes at the cut point
/// (0 = cut).
pub const WEB_STINGER_MIX_PROPERTY: &str = "stinger_mix_ms";

/// Block property on a Media Player holding per-clip stinger settings, as a
/// JSON object keyed by playlist entry. Managed through the stinger API.
pub const STINGER_CLIPS_PROPERTY: &str = "stinger_clips";

/// Vision mixer property that adds the stinger input.
pub const ENABLE_STINGER_PROPERTY: &str = "enable_stinger";

/// Vision mixer property: how long after a take the clip's first frame goes
/// on air. The clip is released ahead of its frames, so this must cover the
/// time a take takes to start the clip.
pub const STINGER_PREROLL_PROPERTY: &str = "stinger_preroll_ms";

/// Default take-to-air delay. A parked clip's first frame reaches the mixer
/// within milliseconds of a take, so this covers the control path (the
/// request, programming the mixer, starting the clip) with room to spare:
/// takes measured clean down to 50 ms.
pub const DEFAULT_STINGER_PREROLL_MS: u64 = 80;

/// Bounds for [`STINGER_PREROLL_PROPERTY`].
pub const MIN_STINGER_PREROLL_MS: u64 = 40;
pub const MAX_STINGER_PREROLL_MS: u64 = 2_000;

/// External pad name of the vision mixer's stinger input.
pub const STINGER_INPUT_PAD: &str = "stinger_in";

/// How a stinger clip lays out its graphic and matte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StingerLayout {
    /// Detect from the clip: its shape (two standard frames side by side or
    /// stacked) and whether it carries alpha.
    #[default]
    Auto,
    /// The whole frame is the graphic, keyed by its alpha channel.
    Classic,
    /// Graphic on the left half, matte on the right half (OBS "horizontal").
    SideBySide,
    /// Graphic on the top half, matte on the bottom half (OBS "vertical").
    Stacked,
    /// The whole frame is a matte; there is no graphic.
    MaskOnly,
}

impl StingerLayout {
    /// The variant a layout plays as, before any downgrade.
    pub fn variant(self) -> Option<StingerVariant> {
        match self {
            StingerLayout::Auto => None,
            StingerLayout::Classic => Some(StingerVariant::Classic),
            StingerLayout::SideBySide | StingerLayout::Stacked => Some(StingerVariant::TrackMatte),
            StingerLayout::MaskOnly => Some(StingerVariant::MaskOnly),
        }
    }
}

/// How a stinger switches the program beneath the clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StingerVariant {
    /// Cut or mix at a cut point while the graphic covers the frame.
    Classic,
    /// The clip's matte decides, pixel by pixel, where the new source shows.
    TrackMatte,
    /// Like track matte, with no graphic on top.
    MaskOnly,
}

impl StingerVariant {
    /// Whether this variant composites a matte, which needs the GPU mixer.
    pub fn uses_matte(self) -> bool {
        !matches!(self, StingerVariant::Classic)
    }
}

/// What a classic stinger does to the program at its cut point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StingerBeneath {
    /// Switch on the cut point's frame.
    #[default]
    Cut,
    /// Mix from the old source to the new one, starting at the cut point.
    Mix,
}

/// Per-clip stinger settings, stored on the clip's Media Player.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerClipSettings {
    /// Clip layout. `auto` detects it.
    #[serde(default)]
    pub layout: StingerLayout,
    /// Classic: how far into the clip the program changes. `None` takes it
    /// from the clip: the frame where the graphic covers most, or where the
    /// matte crosses half way.
    #[serde(default)]
    pub cut_point_ms: Option<u64>,
    /// Classic: cut or mix at the cut point.
    #[serde(default)]
    pub beneath: StingerBeneath,
    /// Classic: length of the mix beneath, when `beneath` is `mix`.
    #[serde(default)]
    pub mix_ms: u64,
    /// The graphic's colour is premultiplied by its alpha (After Effects'
    /// "Premultiplied (Matted)" export).
    #[serde(default)]
    pub premultiplied: bool,
    /// Swap black and white in the matte.
    #[serde(default)]
    pub invert_matte: bool,
}

/// What analysing a clip found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerClipInfo {
    /// Decoded frame size of the whole clip.
    pub width: u32,
    pub height: u32,
    /// Frame rate as a fraction.
    pub framerate_num: i32,
    pub framerate_den: i32,
    /// Number of decoded frames.
    pub frames: u32,
    /// Length of the clip.
    pub duration_ms: u64,
    /// Whether the decoded frames carry an alpha channel.
    pub has_alpha: bool,
    /// Layout read from the clip's shape and alpha. Never `auto`.
    pub detected_layout: StingerLayout,
    /// Where the graphic covers the largest part of the frame, and how much
    /// (0..1). Taken from the graphic's alpha.
    pub cover_peak_ms: Option<u64>,
    pub cover_peak: f32,
    /// Where the matte first shows more of the new source than the old.
    pub matte_midpoint_ms: Option<u64>,
    /// Where the matte starts and finishes moving: the first frame with any
    /// of the new source, and the first with all of it.
    pub matte_start_ms: Option<u64>,
    pub matte_end_ms: Option<u64>,
    /// How long the analysis took.
    pub analysis_ms: u64,
}

/// One playlist entry of the stinger source, as the mixer sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerClip {
    /// Playlist index on the stinger source.
    pub index: usize,
    /// Playlist entry (path or URI).
    pub file: String,
    /// Settings as stored.
    pub settings: StingerClipSettings,
    /// Analysis, once it has run.
    pub info: Option<StingerClipInfo>,
    /// How this clip plays on this mixer, after detection and any downgrade.
    pub variant: Option<StingerVariant>,
    /// Set when the variant the clip asks for cannot run here.
    pub downgraded_from: Option<StingerVariant>,
    /// Effective cut point for a classic take.
    pub cut_point_ms: Option<u64>,
}

/// A finished take, with what was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerTakeReport {
    /// The take this reports on, as returned by the take request.
    pub take_id: u64,
    pub index: usize,
    pub file: String,
    pub variant: StingerVariant,
    pub downgraded_from: Option<StingerVariant>,
    pub from_input: usize,
    pub to_input: usize,
    /// Time from the take request to the clip's first frame on air.
    pub take_to_air_ms: f64,
    /// Time spent loading the clip, when the take had to cue it first.
    pub cue_ms: Option<u64>,
    /// Clip length.
    pub duration_ms: u64,
    /// Classic: where the program changed, into the clip.
    pub cut_point_ms: Option<u64>,
    /// Clip frames that should have reached the mixer.
    pub frames_expected: u32,
    /// Clip frames that did reach it.
    pub frames_arrived: u32,
    /// Clip frames, on the graphic or the matte pad, that reached the mixer
    /// after their time on air had begun.
    pub frames_late: u32,
    /// Smallest lead a clip frame had on its time on air; negative is late.
    pub worst_margin_ms: Option<f64>,
}

/// What feeds a vision mixer's stinger input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StingerSourceKind {
    /// A Media Player whose playlist is the clip library.
    Clips,
    /// An HTML Input whose page plays the stinger on a trigger. Its settings
    /// are properties of the block, and it is the only entry in `clips`.
    Page,
}

/// Stinger state of one vision mixer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerState {
    /// The block wired to the stinger input.
    pub source_block_id: Option<String>,
    /// What kind of block that is.
    #[serde(default)]
    pub source_kind: Option<StingerSourceKind>,
    /// Why stingers cannot run, when they cannot.
    pub problem: Option<String>,
    /// Whether the matte variants run here (GPU mixer).
    pub matte_supported: bool,
    /// Take-to-air delay.
    pub preroll_ms: u64,
    pub clips: Vec<StingerClip>,
    /// The clip a take plays when it names none.
    pub cued_index: Option<usize>,
    /// The cued clip is loaded and parked on its first frame.
    pub ready: bool,
    /// How long the last cue took.
    pub last_cue_ms: Option<u64>,
    /// A take is on air.
    pub running: bool,
    pub last_take: Option<StingerTakeReport>,
}

/// Cue a stinger clip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerCueRequest {
    /// Playlist index on the stinger source.
    pub index: usize,
    /// When set, the playlist entry at `index` must be this file; a library
    /// changed by someone else since the client read it fails with 409.
    #[serde(default)]
    pub file: Option<String>,
}

/// Add a clip to the stinger library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerAddClipRequest {
    /// Path relative to the media directory (as the media API names files),
    /// an absolute path, or a URI.
    pub file: String,
    /// Settings to store with it. Defaults detect everything.
    #[serde(default)]
    pub settings: Option<StingerClipSettings>,
}

/// Guards a request that names a clip by index: when `file` is given, the
/// playlist entry at that index must be this file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StingerFileGuard {
    #[serde(default)]
    pub file: Option<String>,
}

/// Take a stinger from PGM to PVW.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerTakeRequest {
    /// Playlist index to play. Defaults to the cued clip.
    #[serde(default)]
    pub index: Option<usize>,
    /// When set, the clip played must be this file (the entry at `index`,
    /// or the cued one); otherwise the take fails with 409.
    #[serde(default)]
    pub file: Option<String>,
}

/// Result of a take request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerTakeResponse {
    /// Identifies this take in `StingerStarted`, `StingerCompleted` and
    /// `StingerFailed`.
    pub take_id: u64,
    pub index: usize,
    /// The clip played.
    pub file: String,
    pub variant: StingerVariant,
    pub downgraded_from: Option<StingerVariant>,
    /// Time from the request to the clip's first frame on air.
    pub take_to_air_ms: f64,
    pub duration_ms: u64,
}

/// Example stinger clips written to the media directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct StingerExamplesResponse {
    /// Paths relative to the media directory, one per variant.
    pub files: Vec<String>,
    /// Whether they were added to the stinger source's playlist.
    pub added_to_playlist: bool,
}

/// Parse the per-clip settings property. Unknown or malformed entries are
/// dropped rather than failing the whole map.
pub fn parse_clip_settings(json: &str) -> std::collections::HashMap<String, StingerClipSettings> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(json) else {
        return Default::default();
    };
    map.into_iter()
        .filter_map(|(k, v)| serde_json::from_value(v).ok().map(|s| (k, s)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_auto_and_cut() {
        let s: StingerClipSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, StingerClipSettings::default());
        assert_eq!(s.layout, StingerLayout::Auto);
        assert_eq!(s.beneath, StingerBeneath::Cut);
    }

    #[test]
    fn clip_settings_map_keeps_good_entries() {
        let map = parse_clip_settings(
            r#"{"a.mkv": {"layout": "side_by_side", "invert_matte": true},
                "b.mkv": {"layout": "nonsense"},
                "c.mkv": {"cut_point_ms": 700, "beneath": "mix", "mix_ms": 200}}"#,
        );
        assert_eq!(map.len(), 2);
        assert_eq!(map["a.mkv"].layout, StingerLayout::SideBySide);
        assert!(map["a.mkv"].invert_matte);
        assert_eq!(map["c.mkv"].cut_point_ms, Some(700));
        assert_eq!(map["c.mkv"].beneath, StingerBeneath::Mix);
        assert!(parse_clip_settings("not json").is_empty());
    }

    #[test]
    fn layouts_map_to_variants() {
        assert_eq!(StingerLayout::Auto.variant(), None);
        assert_eq!(
            StingerLayout::Classic.variant(),
            Some(StingerVariant::Classic)
        );
        assert_eq!(
            StingerLayout::Stacked.variant(),
            Some(StingerVariant::TrackMatte)
        );
        assert!(StingerVariant::MaskOnly.uses_matte());
        assert!(!StingerVariant::Classic.uses_matte());
    }
}
