//! Media player API types shared between backend and frontend.

use crate::PropertyValue;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// Playback state of a media player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
}

impl std::fmt::Display for PlayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Playing => write!(f, "playing"),
            Self::Paused => write!(f, "paused"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Player control action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayerAction {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

/// Request to control the media player.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PlayerControlRequest {
    pub action: PlayerAction,
}

/// Request to set the playlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[cfg_attr(feature = "validation", derive(garde::Validate))]
pub struct SetPlaylistRequest {
    /// List of file URIs (e.g., "file:///path/to/video.mp4")
    #[cfg_attr(feature = "validation", garde(length(min = 1)))]
    pub files: Vec<String>,
}

/// Request to seek to a position.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SeekRequest {
    /// Position in nanoseconds
    pub position_ns: u64,
}

/// Request to go to a specific file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct GotoRequest {
    /// File index (0-based)
    pub index: usize,
}

/// Response with the current player state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PlayerStateResponse {
    /// Current playback state
    pub state: PlayerState,
    /// Current position in nanoseconds
    pub position_ns: u64,
    /// Total duration in nanoseconds
    pub duration_ns: u64,
    /// Current file index (0-based)
    pub current_file_index: usize,
    /// Total number of files in playlist
    pub total_files: usize,
    /// Current file path/URI
    pub current_file: Option<String>,
    /// Full playlist
    pub playlist: Vec<String>,
    /// Whether playlist loops
    pub loop_playlist: bool,
}

/// Block definition id of the Media Player.
pub const MEDIA_PLAYER_BLOCK_ID: &str = "builtin.media_player";

/// Media Player property that selects the [`PlayerMode`].
pub const MODE_PROPERTY: &str = "mode";
/// Timeline mode: the indexed source (`tams+https://...`, a directory).
pub const SOURCE_PROPERTY: &str = "source";
/// Timeline mode: players with the same group share one timeline.
pub const GROUP_PROPERTY: &str = "group";
/// Timeline mode: seconds of decoded frames kept ahead of the playhead.
pub const DECODED_AHEAD_PROPERTY: &str = "decoded_ahead_s";
/// Timeline mode: output frame rate (empty = the source's rate).
pub const OUTPUT_FRAMERATE_PROPERTY: &str = "output_framerate";
/// Every mode: seconds of coded media fetched ahead.
pub const READ_AHEAD_PROPERTY: &str = "read_ahead_s";
/// TAMS source auth, named as on TAMS Output.
pub const AUTH_MODE_PROPERTY: &str = "auth_mode";
pub const API_TOKEN_PROPERTY: &str = "api_token";
pub const OSC_SERVICE_ID_PROPERTY: &str = "osc_service_id";

/// Prefix of the private group of a timeline player without a `group`: the
/// group is `@<block id>`. A configured group may not start with it.
pub const PRIVATE_GROUP_PREFIX: char = '@';

/// What a Media Player does. Selected by the `mode` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PlayerMode {
    /// Plays files and URLs in order: play, pause, seek, next.
    #[default]
    Playlist,
    /// Clip source for a vision mixer's stinger input, driven by the mixer.
    Stinger,
    /// An indexed source on its own timeline: scrub, slow motion, reverse,
    /// live edge, grouping (replay).
    Timeline,
}

impl PlayerMode {
    pub const ALL: [PlayerMode; 3] = [Self::Playlist, Self::Stinger, Self::Timeline];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playlist => "playlist",
            Self::Stinger => "stinger",
            Self::Timeline => "timeline",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s.trim())
    }

    /// The mode of a block from its properties: `mode` when set, else what
    /// the legacy properties select (see [`migrate_legacy_properties`]).
    /// An unknown `mode` value is an error naming the value.
    pub fn from_properties(properties: &HashMap<String, PropertyValue>) -> Result<Self, String> {
        match properties.get(MODE_PROPERTY) {
            Some(PropertyValue::String(s)) => Self::parse(s).ok_or_else(|| {
                format!(
                    "unknown mode '{}' (expected one of playlist, stinger, timeline)",
                    s
                )
            }),
            Some(other) => Err(format!("mode must be a string, got {:?}", other)),
            None => Ok(legacy_mode(properties).unwrap_or_default()),
        }
    }
}

/// Legacy property names, before `mode` existed. Read only by
/// [`migrate_legacy_properties`].
pub mod legacy {
    /// `true` selected stinger mode (released, may be in saved flows).
    pub const STINGER_MODE: &str = crate::stinger::STINGER_MODE_PROPERTY;
    /// Non-empty selected the replay (timeline) mode and named its group.
    pub const REPLAY_GROUP: &str = "replay_group";
    pub const REPLAY_SOURCE: &str = "replay_source";
    pub const REPLAY_CACHE_S: &str = "replay_cache_s";
    pub const REPLAY_PREFETCH_S: &str = "replay_prefetch_s";
    pub const REPLAY_AUTH_MODE: &str = "replay_auth_mode";
    pub const REPLAY_API_TOKEN: &str = "replay_api_token";
    pub const REPLAY_OSC_SERVICE_ID: &str = "replay_osc_service_id";
    pub const REPLAY_OUTPUT_FRAMERATE: &str = "replay_output_framerate";
}

fn non_empty_str<'a>(properties: &'a HashMap<String, PropertyValue>, key: &str) -> Option<&'a str> {
    match properties.get(key) {
        Some(PropertyValue::String(s)) if !s.trim().is_empty() => Some(s.trim()),
        _ => None,
    }
}

/// The mode the legacy properties select, as the block did before `mode`:
/// a non-empty `replay_group` wins over `stinger_mode`.
fn legacy_mode(properties: &HashMap<String, PropertyValue>) -> Option<PlayerMode> {
    if non_empty_str(properties, legacy::REPLAY_GROUP).is_some() {
        Some(PlayerMode::Timeline)
    } else if matches!(
        properties.get(legacy::STINGER_MODE),
        Some(PropertyValue::Bool(true))
    ) {
        Some(PlayerMode::Stinger)
    } else {
        None
    }
}

/// Rewrite a Media Player's legacy properties to the current names, so a
/// flow saved before `mode` keeps doing what it did:
///
/// - `replay_group` non-empty: `mode=timeline`, `group`;
/// - `stinger_mode=true`: `mode=stinger` (a `replay_group` wins, as before);
/// - `replay_source`, `replay_cache_s`, `replay_auth_mode`,
///   `replay_api_token`, `replay_osc_service_id`, `replay_output_framerate`:
///   `source`, `decoded_ahead_s`, `auth_mode`, `api_token`,
///   `osc_service_id`, `output_framerate`;
/// - `replay_prefetch_s`: `read_ahead_s`, the one read-ahead of both modes.
///   A replay block ignored `read_ahead_s`, so its old value is replaced.
///
/// A current name already set wins over its legacy name. Legacy keys are
/// removed. Idempotent. Returns one line per change, for the log; empty when
/// nothing changed.
pub fn migrate_legacy_properties(properties: &mut HashMap<String, PropertyValue>) -> Vec<String> {
    let mut changes = Vec::new();
    let has_mode = properties.contains_key(MODE_PROPERTY);
    let derived = if has_mode {
        None
    } else {
        legacy_mode(properties)
    };

    if let Some(mode) = derived {
        properties.insert(
            MODE_PROPERTY.to_string(),
            PropertyValue::String(mode.as_str().to_string()),
        );
        changes.push(format!("mode={} (from legacy properties)", mode.as_str()));
        if mode == PlayerMode::Timeline
            && matches!(
                properties.get(legacy::STINGER_MODE),
                Some(PropertyValue::Bool(true))
            )
        {
            changes.push(
                "stinger_mode=true dropped: replay_group selected the timeline mode".to_string(),
            );
        }
    } else if has_mode && legacy_mode(properties).is_some() {
        changes.push("legacy mode properties ignored: mode is set".to_string());
    }

    // A block that the legacy properties put in timeline mode read its
    // read-ahead from replay_prefetch_s (default 10 s, the same as
    // read_ahead_s) and ignored read_ahead_s.
    if derived == Some(PlayerMode::Timeline) {
        match properties.remove(legacy::REPLAY_PREFETCH_S) {
            Some(v) => {
                properties.insert(READ_AHEAD_PROPERTY.to_string(), v);
                changes.push(format!(
                    "{} -> {}",
                    legacy::REPLAY_PREFETCH_S,
                    READ_AHEAD_PROPERTY
                ));
            }
            None => {
                if properties.remove(READ_AHEAD_PROPERTY).is_some() {
                    changes.push(format!(
                        "{} dropped: the replay mode did not use it",
                        READ_AHEAD_PROPERTY
                    ));
                }
            }
        }
    }

    // `replay_group` is the group only when it selected the mode; an empty
    // one meant no replay.
    if let Some(v) = properties.remove(legacy::REPLAY_GROUP) {
        let set = matches!(&v, PropertyValue::String(s) if !s.trim().is_empty());
        if set && !properties.contains_key(GROUP_PROPERTY) {
            properties.insert(GROUP_PROPERTY.to_string(), v);
            changes.push(format!("{} -> {}", legacy::REPLAY_GROUP, GROUP_PROPERTY));
        } else {
            changes.push(format!("{} dropped", legacy::REPLAY_GROUP));
        }
    }

    for (old, new) in [
        (legacy::REPLAY_SOURCE, SOURCE_PROPERTY),
        (legacy::REPLAY_CACHE_S, DECODED_AHEAD_PROPERTY),
        (legacy::REPLAY_AUTH_MODE, AUTH_MODE_PROPERTY),
        (legacy::REPLAY_API_TOKEN, API_TOKEN_PROPERTY),
        (legacy::REPLAY_OSC_SERVICE_ID, OSC_SERVICE_ID_PROPERTY),
        (legacy::REPLAY_OUTPUT_FRAMERATE, OUTPUT_FRAMERATE_PROPERTY),
    ] {
        if let Some(v) = properties.remove(old) {
            if properties.contains_key(new) {
                changes.push(format!("{} dropped: {} is set", old, new));
            } else {
                properties.insert(new.to_string(), v);
                changes.push(format!("{} -> {}", old, new));
            }
        }
    }

    // Left only when the block was not a legacy timeline (handled above):
    // the playlist and stinger modes never read it.
    if properties.remove(legacy::REPLAY_PREFETCH_S).is_some() {
        changes.push(format!(
            "{} dropped: only the replay mode used it",
            legacy::REPLAY_PREFETCH_S
        ));
    }

    if properties.remove(legacy::STINGER_MODE).is_some() && derived != Some(PlayerMode::Timeline) {
        changes.push(format!("{} removed", legacy::STINGER_MODE));
    }

    changes
}

/// The timeline group a player belongs to: its `group`, or a private group
/// of one (`@<block id>`) when `group` is empty.
pub fn timeline_group(properties: &HashMap<String, PropertyValue>, block_id: &str) -> String {
    match non_empty_str(properties, GROUP_PROPERTY) {
        Some(g) => g.to_string(),
        None => format!("{}{}", PRIVATE_GROUP_PREFIX, block_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> PropertyValue {
        PropertyValue::String(v.to_string())
    }

    /// PropertyValue has no PartialEq; compare through JSON.
    fn json(p: &HashMap<String, PropertyValue>) -> serde_json::Value {
        serde_json::to_value(p).unwrap()
    }

    #[test]
    fn mode_defaults_to_playlist() {
        assert_eq!(
            PlayerMode::from_properties(&HashMap::new()),
            Ok(PlayerMode::Playlist)
        );
    }

    #[test]
    fn mode_parses_every_value_and_rejects_unknown() {
        for m in PlayerMode::ALL {
            let p = props(&[(MODE_PROPERTY, s(m.as_str()))]);
            assert_eq!(PlayerMode::from_properties(&p), Ok(m));
        }
        let err = PlayerMode::from_properties(&props(&[(MODE_PROPERTY, s("replay"))])).unwrap_err();
        assert!(err.contains("replay"), "{err}");
        assert!(
            PlayerMode::from_properties(&props(&[(MODE_PROPERTY, PropertyValue::Bool(true))]))
                .is_err()
        );
    }

    #[test]
    fn mode_wire_format_matches_property_values() {
        for m in PlayerMode::ALL {
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
    }

    #[test]
    fn legacy_properties_select_the_mode_without_migration() {
        let replay = props(&[(legacy::REPLAY_GROUP, s("r1"))]);
        assert_eq!(
            PlayerMode::from_properties(&replay),
            Ok(PlayerMode::Timeline)
        );
        let stinger = props(&[(legacy::STINGER_MODE, PropertyValue::Bool(true))]);
        assert_eq!(
            PlayerMode::from_properties(&stinger),
            Ok(PlayerMode::Stinger)
        );
        // As before: an empty replay_group is no replay.
        let empty = props(&[(legacy::REPLAY_GROUP, s(" "))]);
        assert_eq!(
            PlayerMode::from_properties(&empty),
            Ok(PlayerMode::Playlist)
        );
        // An explicit mode wins over legacy selectors.
        let both = props(&[
            (MODE_PROPERTY, s("playlist")),
            (legacy::STINGER_MODE, PropertyValue::Bool(true)),
        ]);
        assert_eq!(PlayerMode::from_properties(&both), Ok(PlayerMode::Playlist));
    }

    #[test]
    fn migrates_a_legacy_replay_block() {
        let mut p = props(&[
            (legacy::REPLAY_GROUP, s("r1")),
            (legacy::REPLAY_SOURCE, s("tams+http://gw/flows/abc")),
            (legacy::REPLAY_CACHE_S, PropertyValue::UInt(3)),
            (legacy::REPLAY_PREFETCH_S, PropertyValue::UInt(20)),
            (legacy::REPLAY_AUTH_MODE, s("osc")),
            (legacy::REPLAY_API_TOKEN, s("t")),
            (legacy::REPLAY_OSC_SERVICE_ID, s("svc")),
            (legacy::REPLAY_OUTPUT_FRAMERATE, s("50/1")),
            (READ_AHEAD_PROPERTY, PropertyValue::UInt(60)),
        ]);
        let changes = migrate_legacy_properties(&mut p);
        assert!(!changes.is_empty());
        let want = props(&[
            (MODE_PROPERTY, s("timeline")),
            (GROUP_PROPERTY, s("r1")),
            (SOURCE_PROPERTY, s("tams+http://gw/flows/abc")),
            (DECODED_AHEAD_PROPERTY, PropertyValue::UInt(3)),
            // replay_prefetch_s, not the ignored read_ahead_s
            (READ_AHEAD_PROPERTY, PropertyValue::UInt(20)),
            (AUTH_MODE_PROPERTY, s("osc")),
            (API_TOKEN_PROPERTY, s("t")),
            (OSC_SERVICE_ID_PROPERTY, s("svc")),
            (OUTPUT_FRAMERATE_PROPERTY, s("50/1")),
        ]);
        assert_eq!(json(&p), json(&want));
        // Idempotent.
        assert!(migrate_legacy_properties(&mut p).is_empty());
        assert_eq!(json(&p), json(&want));
    }

    #[test]
    fn legacy_replay_drops_the_read_ahead_it_ignored() {
        let mut p = props(&[
            (legacy::REPLAY_GROUP, s("r1")),
            (legacy::REPLAY_SOURCE, s("/dir")),
            (READ_AHEAD_PROPERTY, PropertyValue::UInt(60)),
        ]);
        migrate_legacy_properties(&mut p);
        assert!(!p.contains_key(READ_AHEAD_PROPERTY));
    }

    #[test]
    fn migrates_stinger_mode() {
        let mut p = props(&[
            (legacy::STINGER_MODE, PropertyValue::Bool(true)),
            ("decode", PropertyValue::Bool(false)),
        ]);
        migrate_legacy_properties(&mut p);
        assert_eq!(
            json(&p),
            json(&props(&[
                (MODE_PROPERTY, s("stinger")),
                ("decode", PropertyValue::Bool(false)),
            ]))
        );
        let mut off = props(&[(legacy::STINGER_MODE, PropertyValue::Bool(false))]);
        assert!(!migrate_legacy_properties(&mut off).is_empty());
        assert!(off.is_empty(), "a playlist block needs no mode: {off:?}");
    }

    #[test]
    fn replay_group_wins_over_stinger_mode_as_before() {
        let mut p = props(&[
            (legacy::STINGER_MODE, PropertyValue::Bool(true)),
            (legacy::REPLAY_GROUP, s("g")),
        ]);
        migrate_legacy_properties(&mut p);
        assert_eq!(PlayerMode::from_properties(&p), Ok(PlayerMode::Timeline));
        assert!(!p.contains_key(legacy::STINGER_MODE));
    }

    #[test]
    fn current_names_win_over_legacy_names() {
        let mut p = props(&[
            (MODE_PROPERTY, s("timeline")),
            (SOURCE_PROPERTY, s("new")),
            (legacy::REPLAY_SOURCE, s("old")),
            (legacy::REPLAY_GROUP, s("old-group")),
            (READ_AHEAD_PROPERTY, PropertyValue::UInt(5)),
            (legacy::REPLAY_PREFETCH_S, PropertyValue::UInt(30)),
        ]);
        migrate_legacy_properties(&mut p);
        assert_eq!(
            json(&p),
            json(&props(&[
                (MODE_PROPERTY, s("timeline")),
                (SOURCE_PROPERTY, s("new")),
                // replay_group fills an unset group even when mode is set.
                (GROUP_PROPERTY, s("old-group")),
                (READ_AHEAD_PROPERTY, PropertyValue::UInt(5)),
            ]))
        );
    }

    #[test]
    fn replay_properties_of_a_playlist_block_do_not_change_its_read_ahead() {
        let mut p = props(&[
            (legacy::REPLAY_GROUP, s("")),
            (legacy::REPLAY_PREFETCH_S, PropertyValue::UInt(30)),
            (legacy::REPLAY_SOURCE, s("/dir")),
        ]);
        migrate_legacy_properties(&mut p);
        assert_eq!(json(&p), json(&props(&[(SOURCE_PROPERTY, s("/dir"))])));
        assert_eq!(PlayerMode::from_properties(&p), Ok(PlayerMode::Playlist));
    }

    #[test]
    fn empty_group_is_a_private_group() {
        assert_eq!(timeline_group(&HashMap::new(), "cam1"), "@cam1");
        let p = props(&[(GROUP_PROPERTY, s(" r1 "))]);
        assert_eq!(timeline_group(&p, "cam1"), "r1");
    }

    #[test]
    fn player_control_request_wire_format() {
        for (action, name) in [
            (PlayerAction::Play, "play"),
            (PlayerAction::Pause, "pause"),
            (PlayerAction::Stop, "stop"),
            (PlayerAction::Next, "next"),
            (PlayerAction::Previous, "previous"),
        ] {
            let json = serde_json::to_string(&PlayerControlRequest { action }).unwrap();
            assert_eq!(json, format!(r#"{{"action":"{name}"}}"#));
        }
    }

    #[test]
    fn goto_request_wire_format() {
        let json = serde_json::to_string(&GotoRequest { index: 3 }).unwrap();
        assert_eq!(json, r#"{"index":3}"#);
    }
}
