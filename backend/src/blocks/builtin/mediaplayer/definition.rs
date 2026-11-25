//! Block definition (metadata) for the media player block.

use super::bridge::Decoder;
use strom_types::block::*;
use strom_types::mediaplayer::{
    PlayerMode, DECODED_AHEAD_PROPERTY, GROUP_PROPERTY, MODE_PROPERTY, OUTPUT_FRAMERATE_PROPERTY,
    SOURCE_PROPERTY,
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
                name: "read_ahead_s".to_string(),
                label: "Read-ahead (s)".to_string(),
                description: "Seconds of coded media fetched ahead of the playhead, in memory. \
                              Playlist: http(s)/HLS/DASH sources, 0 = GStreamer default. \
                              Timeline: half as much is kept behind, minimum 2. Max 120."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(
                    super::bridge::NetworkBuffer::DEFAULT_READ_AHEAD_S,
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "read_ahead_s".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: in_modes(&[PlayerMode::Playlist, PlayerMode::Timeline]),
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
