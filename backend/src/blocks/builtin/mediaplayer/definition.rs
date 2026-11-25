//! Block definition (metadata) for the media player block.

use super::bridge::Decoder;
use strom_types::block::*;
use strom_types::{MediaType, PropertyValue};

/// Get metadata for Media Player blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![media_player_definition()]
}

/// Get Media Player block definition (metadata only).
pub fn media_player_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.media_player".to_string(),
        name: "Media Player".to_string(),
        description: "Plays files and URLs (http(s) including HLS and DASH, rtsp, srt, udp, ...) from a playlist.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "replay_group".to_string(),
                label: "Replay Group".to_string(),
                description: "Experimental. Set to enable replay mode: players with the same group share one jog/speed cursor."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_group".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_source".to_string(),
                label: "Replay Source".to_string(),
                description: "Replay mode: where the segments are. A TAMS flow as tams+https://<gateway>/flows/<flow_id> (or tams+http://), or a directory with index.json and MPEG-TS segments (absolute or relative to media path)."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_source".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_cache_s".to_string(),
                label: "Replay Cache Ahead (s)".to_string(),
                description: "Replay mode: seconds of decoded frames kept ahead of the cursor.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(3)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_cache_s".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_prefetch_s".to_string(),
                label: "Replay Prefetch (s)".to_string(),
                description: "Replay mode: seconds of coded segments fetched ahead of the cursor (half as much behind).".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(10)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_prefetch_s".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_auth_mode".to_string(),
                label: "Replay Authentication".to_string(),
                description: "Replay mode, TAMS source: Static API Token = a bearer token (or none). OSC PAT/SAT = Service Access Tokens minted from the OSC Personal Access Token configured on this Strom instance.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "static".to_string(),
                            label: Some("Static API Token".to_string()),
                            visible_when: None,
                        },
                        EnumValue {
                            value: "osc".to_string(),
                            label: Some("OSC PAT/SAT".to_string()),
                            visible_when: None,
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("static".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_auth_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_api_token".to_string(),
                label: "Replay API Token".to_string(),
                description: "Replay mode, TAMS source: static bearer token for the gateway. Empty = STROM_TAMS_API_TOKEN, or no token.".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_api_token".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_osc_service_id".to_string(),
                label: "Replay OSC Service ID".to_string(),
                description: "Replay mode, OSC PAT/SAT: the OSC service type the SAT is scoped to. Empty = derived from the gateway URL.".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_osc_service_id".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
            ExposedProperty {
                name: "replay_output_framerate".to_string(),
                label: "Replay Output Framerate".to_string(),
                description: "Replay mode: frame rate of the output. Empty = the source's own rate. Set it to the rate of what follows (e.g. the vision mixer) for slow motion from high-frame-rate sources: a 100p source at 50 % on a 50p output shows every source frame."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_framerate_enum_values(true),
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "replay_output_framerate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
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
                visible_when: None,
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
                visible_when: None,
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
                visible_when: None,
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
                visible_when: None,
            },
            ExposedProperty {
                name: "read_ahead_s".to_string(),
                label: "Read-ahead (s)".to_string(),
                description: "Seconds of http(s)/HLS/DASH media fetched ahead of playback, \
                              in memory. Raise for high-bitrate files on an uneven network. \
                              0 = GStreamer default, max 120."
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
                visible_when: None,
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
                visible_when: None,
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
                visible_when: None,
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
                visible_when: None,
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
                visible_when: None,
            },
            ExposedProperty {
                name: strom_types::stinger::STINGER_MODE_PROPERTY.to_string(),
                label: "Stinger Clip Source".to_string(),
                description: "Feed a Vision Mixer's stinger input: the playlist is the stinger library. The player decodes, parks the cued clip on its first frame and plays it once per take; looping, sync and playout delay are overridden. Wire video_out to the mixer's stinger input; audio_out carries the clip's sound."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: strom_types::stinger::STINGER_MODE_PROPERTY.to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
                visible_when: None,
            },
        ],
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
