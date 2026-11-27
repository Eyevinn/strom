//! Block definition (metadata) for the media player block.

use super::bridge::Decoder;
use strom_types::block::*;
use strom_types::mediaplayer::{
    PlayerMode, CLIP_IN_PROPERTY, CLIP_OUT_PROPERTY, CLIP_SPEED_PROPERTY, DECODED_AHEAD_PROPERTY,
    GROUP_PROPERTY, MODE_PROPERTY, OUTPUT_FRAMERATE_PROPERTY, PACING_OFFLINE, PACING_PROPERTY,
    PACING_REALTIME, READ_AHEAD_PROPERTY, SOURCE_PROPERTY,
};
use strom_types::mediaplayer::{
    FOLLOW_LIVE_PROPERTY, JOG_GRID_PROPERTY, LIVE_MARGIN_PROPERTY, OFFSET_MS_PROPERTY,
    START_MODE_PROPERTY,
};
use strom_types::{MediaType, PropertyValue};

/// Default of `decoded_ahead_s`.
pub const DEFAULT_DECODED_AHEAD_S: u64 = 3;

/// Show and apply a property only in these modes.
fn in_modes(modes: &[PlayerMode]) -> Option<PropertyCondition> {
    let values: Vec<&str> = modes.iter().map(|m| m.as_str()).collect();
    Some(PropertyCondition::one_of_strings(MODE_PROPERTY, &values))
}

fn block_mapping(name: &str) -> PropertyMapping {
    PropertyMapping {
        element_id: "_block".to_string(),
        property_name: name.to_string(),
        transform: None,
    }
}

/// Get metadata for Media Player blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![media_player_definition()]
}

/// Get Media Player block definition (metadata only).
pub fn media_player_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.media_player".to_string(),
        name: "Media Player".to_string(),
        description: "Plays files and URLs (http(s) including HLS and DASH, rtsp, srt, udp, ...) from a playlist, feeds a stinger, or plays an indexed source on a timeline (replay).".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: [
            vec![
            ExposedProperty {
                name: MODE_PROPERTY.to_string(),
                label: "Mode".to_string(),
                description: "Playlist = files and URLs in order. Stinger = clip source for a Vision Mixer's stinger input. Timeline = an indexed source to scrub, jog and play at any speed (replay)."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: PlayerMode::Playlist.as_str().to_string(),
                            label: Some("Playlist".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: PlayerMode::Stinger.as_str().to_string(),
                            label: Some("Stinger clip source".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: PlayerMode::Timeline.as_str().to_string(),
                            label: Some("Timeline (replay)".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(
                    PlayerMode::Playlist.as_str().to_string(),
                )),
                mapping: block_mapping(MODE_PROPERTY),
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: SOURCE_PROPERTY.to_string(),
                label: "Source".to_string(),
                description: "Indexed source: a TAMS flow as tams+https://<gateway>/flows/<flow_id> (or tams+http://), or a directory with index.json (absolute or relative to media path)."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(SOURCE_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: GROUP_PROPERTY.to_string(),
                label: "Group".to_string(),
                description: "Players with the same group share one timeline: one playhead, speed and range. Empty = a group of its own."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(GROUP_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: PACING_PROPERTY.to_string(),
                label: "Pacing".to_string(),
                description: "Realtime: output on the flow clock, steered by the replay API. Offline: play the clip (Clip In to Clip Out) once, as fast as downstream takes it, then end the stream: export to a file with an encoder, a muxer and a sink with sync=false."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: PACING_REALTIME.to_string(),
                            label: Some("Realtime".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: PACING_OFFLINE.to_string(),
                            label: Some("Offline (export a clip)".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(PACING_REALTIME.to_string())),
                mapping: block_mapping(PACING_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: CLIP_IN_PROPERTY.to_string(),
                label: "Clip In".to_string(),
                description: "Offline: the clip starts with the frame that covers this instant, UTC (2026-11-24T13:21:56.080Z) or TAI (<seconds>:<nanoseconds>)."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(CLIP_IN_PROPERTY),
                live: false,
                persist: None,
                // Conditions are flat: shown in timeline mode, used offline.
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: CLIP_OUT_PROPERTY.to_string(),
                label: "Clip Out".to_string(),
                description: "Offline: end of the clip (exclusive), UTC or TAI.".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(CLIP_OUT_PROPERTY),
                live: false,
                persist: None,
                // Conditions are flat: shown in timeline mode, used offline.
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: CLIP_SPEED_PROPERTY.to_string(),
                label: "Clip Speed".to_string(),
                description: "Offline: constant forward speed of the clip, e.g. 0.5 for slow motion (each 50p source frame twice on a 50p output). Max 8."
                    .to_string(),
                property_type: PropertyType::Float,
                default_value: Some(PropertyValue::Float(1.0)),
                mapping: block_mapping(CLIP_SPEED_PROPERTY),
                live: false,
                persist: None,
                // Conditions are flat: shown in timeline mode, used offline.
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: OUTPUT_FRAMERATE_PROPERTY.to_string(),
                label: "Output Framerate".to_string(),
                description: "Frame rate of the output. Empty = the source's own rate. Set it to the rate of what follows (e.g. the vision mixer) for slow motion from high-frame-rate sources: a 100p source at 50 % on a 50p output shows every source frame."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_framerate_enum_values(true),
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(OUTPUT_FRAMERATE_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: READ_AHEAD_PROPERTY.to_string(),
                label: "Read-ahead (s)".to_string(),
                description: "Seconds of coded media fetched ahead of the playhead, in memory. \
                              Playlist and Stinger: http(s)/HLS/DASH sources, 0 = GStreamer default. \
                              Timeline: half as much is kept behind, minimum 2. Max 120."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(
                    super::bridge::NetworkBuffer::DEFAULT_READ_AHEAD_S,
                )),
                mapping: block_mapping(READ_AHEAD_PROPERTY),
                live: false,
                persist: None,
                // Every mode reads it: a stinger player's clips can be http(s).
                visible_when: None,
            },
            ExposedProperty {
                name: DECODED_AHEAD_PROPERTY.to_string(),
                label: "Decoded Ahead (s)".to_string(),
                description: "Seconds of decoded frames kept ahead of the playhead, in the direction it moves. Minimum 1."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_DECODED_AHEAD_S)),
                mapping: block_mapping(DECODED_AHEAD_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: LIVE_MARGIN_PROPERTY.to_string(),
                label: "Live Margin (ms)".to_string(),
                description: "Replay group: how far behind the live edge the group plays when live. Empty = the live edge's measured step + 400 ms, at least 800 and at most 3000. The group takes the largest value of its players."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: block_mapping(LIVE_MARGIN_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: FOLLOW_LIVE_PROPERTY.to_string(),
                label: "Follow Live".to_string(),
                description: "Replay group: forward play without a cue that reaches the live point goes live by itself. Off = review mode: play stops on the last frame every angle has. Off on any player turns it off for the group."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: block_mapping(FOLLOW_LIVE_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: JOG_GRID_PROPERTY.to_string(),
                label: "Jog Grid".to_string(),
                description: "Replay group: the frames jog steps. Source = the finest source rate (every source frame in slow motion), Output = the output rate. Source on any player wins."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: ["source", "output"]
                        .iter()
                        .map(|v| EnumValue {
                            value: v.to_string(),
                            label: None,
                            visible_when: None,
                        })
                        .collect(),
                },
                default_value: Some(PropertyValue::String("source".to_string())),
                mapping: block_mapping(JOG_GRID_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: START_MODE_PROPERTY.to_string(),
                label: "Start Mode".to_string(),
                description: "Replay group: how the group starts. Paused (or First) = paused at the start of the media; Live = live as soon as every angle is ready. The group's first player decides."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: ["paused", "first", "live"]
                        .iter()
                        .map(|v| EnumValue {
                            value: v.to_string(),
                            label: None,
                            visible_when: None,
                        })
                        .collect(),
                },
                default_value: Some(PropertyValue::String("paused".to_string())),
                mapping: block_mapping(START_MODE_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ExposedProperty {
                name: OFFSET_MS_PROPERTY.to_string(),
                label: "Offset (ms)".to_string(),
                description: "Trim: moves this angle's media on the group's time axis, positive later. For cameras without a common time, or to adjust one. The times in the store do not change."
                    .to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: block_mapping(OFFSET_MS_PROPERTY),
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Timeline]),
            },
            ],
            // TAMS sources authenticate as TAMS Output does.
            crate::tams::auth::auth_properties(in_modes(&[PlayerMode::Timeline])),
            vec![
            ExposedProperty {
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode to raw video/audio (true) or pass through encoded streams (false). Passthrough is more efficient for transcoding."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist]),
            },
            ExposedProperty {
                name: "decoder".to_string(),
                label: "Decoder".to_string(),
                description: "With Decode on: which GStreamer decoder plays the source. \
                              decodebin3 handles HLS and DASH quality switches without a \
                              hiccup and buffers on its own; classic is the decoder this \
                              block used before, kept as a fallback."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: Decoder::CLASSIC.to_string(),
                            label: Some("Classic (uridecodebin, fallback)".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: Decoder::DECODEBIN3.to_string(),
                            label: Some("decodebin3 (uridecodebin3)".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(Decoder::DECODEBIN3.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decoder".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist, PlayerMode::Stinger]),
            },
            ExposedProperty {
                name: "sync".to_string(),
                label: "Sync".to_string(),
                description: "Pace playback at real-time rate. Disable for fastest-possible throughput."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sync".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist]),
            },
            ExposedProperty {
                name: "playout_delay_ms".to_string(),
                label: "Playout Delay (ms)".to_string(),
                description: "With Sync on, how long each buffer is held before it plays. \
                              Rides out network and decoder hiccups on streams; a stall \
                              longer than this skips ahead and refills the delay."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(
                    super::timing::DEFAULT_PLAYOUT_DELAY_MS,
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "playout_delay_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist]),
            },
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Video Tracks".to_string(),
                description: "How many video tracks of a file get an output (video_out, \
                              video_out_1, ...), in the order the file lists them. Tracks \
                              beyond these are discarded. 0 discards all video."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist, PlayerMode::Stinger]),
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Audio Tracks".to_string(),
                description: "How many audio tracks of a file get an output (audio_out, \
                              audio_out_1, ...), in the order the file lists them - a second \
                              language or commentary track, say. Tracks beyond these, and \
                              subtitles, are discarded. 0 discards all audio."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist, PlayerMode::Stinger]),
            },
            ExposedProperty {
                name: "loop_playlist".to_string(),
                label: "Loop Playlist".to_string(),
                description: "Loop back to the first file when reaching the end of the playlist"
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "loop_playlist".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist]),
            },
            ExposedProperty {
                name: "position_update_interval".to_string(),
                label: "Position Update Interval (ms)".to_string(),
                description: "How often to broadcast position updates (lower = more responsive)"
                    .to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(200)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "position_update_interval".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist, PlayerMode::Stinger]),
            },
            ],
        ]
        .concat(),
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: None,
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_out".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: None,
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_out".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None,
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use strom_types::block::effective_properties;

    /// The properties a block in `mode` is built with when every exposed
    /// property is set: what `visible_properties` leaves of a saved flow.
    fn applied_in(mode: PlayerMode) -> BTreeSet<String> {
        let definition = media_player_definition();
        let mut properties: HashMap<String, PropertyValue> = definition
            .exposed_properties
            .iter()
            .filter_map(|p| Some((p.name.clone(), p.default_value.clone()?)))
            .collect();
        properties.insert(
            MODE_PROPERTY.to_string(),
            PropertyValue::String(mode.as_str().to_string()),
        );
        let (effective, _) = effective_properties(&definition.exposed_properties, &properties);
        effective.into_keys().collect()
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn playlist_mode_applies_the_playlist_properties() {
        assert_eq!(
            applied_in(PlayerMode::Playlist),
            names(&[
                "mode",
                "read_ahead_s",
                "decode",
                "decoder",
                "sync",
                "playout_delay_ms",
                "num_video_tracks",
                "num_audio_tracks",
                "loop_playlist",
                "position_update_interval",
            ])
        );
    }

    #[test]
    fn stinger_mode_applies_what_a_stinger_player_reads() {
        // Decode, sync, playout delay and looping are overridden in stinger
        // mode. Read-ahead is honoured, as it was before `mode`.
        assert_eq!(
            applied_in(PlayerMode::Stinger),
            names(&[
                "mode",
                "read_ahead_s",
                "decoder",
                "num_video_tracks",
                "num_audio_tracks",
                "position_update_interval",
            ])
        );
    }

    #[test]
    fn timeline_mode_applies_the_timeline_properties() {
        assert_eq!(
            applied_in(PlayerMode::Timeline),
            names(&[
                "mode",
                "source",
                "group",
                "pacing",
                "clip_in",
                "clip_out",
                "clip_speed",
                "output_framerate",
                "read_ahead_s",
                "decoded_ahead_s",
                "live_margin_ms",
                "follow_live",
                "jog_grid",
                "start_mode",
                "offset_ms",
                "auth_mode",
                "api_token",
                "osc_service_id",
            ])
        );
    }

    #[test]
    fn a_saved_stinger_read_ahead_is_kept() {
        let definition = media_player_definition();
        let properties: HashMap<String, PropertyValue> = [
            (
                MODE_PROPERTY.to_string(),
                PropertyValue::String("stinger".to_string()),
            ),
            (READ_AHEAD_PROPERTY.to_string(), PropertyValue::UInt(0)),
        ]
        .into();
        let (effective, ignored) =
            effective_properties(&definition.exposed_properties, &properties);
        assert!(ignored.is_empty(), "{:?}", ignored);
        let buffer = super::super::bridge::NetworkBuffer::from_properties(&effective);
        assert_eq!(
            buffer.read_ahead_s, 0,
            "the saved 0 must win over the default"
        );
    }
}
