//! Block definition (metadata) for the media player block.

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
        description: "Plays video and audio files with playlist support. Connect video_out and audio_out to Inter Output blocks for streaming.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode to raw video/audio (true) or pass through encoded streams (false). Passthrough is more efficient for transcoding."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
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
            },
            ExposedProperty {
                name: "stinger_source".to_string(),
                label: "Stinger Clip Source".to_string(),
                description: "Declare this player as a stinger clip source. Its clip is held on its first frame so a stinger fires without decode latency, and looping is disabled so it plays once per trigger. Leave off for graphics on a keyed input that should keep playing."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stinger_source".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "stinger_cut_point_ms".to_string(),
                label: "Stinger Cut Point (ms)".to_string(),
                description: "How far into the clip the program source changes. Set it to the moment the clip fully covers the frame. 0 uses the halfway point."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stinger_cut_point_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "stinger_under_transition".to_string(),
                label: "Stinger Beneath".to_string(),
                description: "Transition running under the clip while it covers the frame. A cut suits a clip that covers completely; a clip that does not is a reason to mix or wipe instead."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "cut".to_string(),
                            label: Some("Cut".to_string()),
                        },
                        EnumValue {
                            value: "fade".to_string(),
                            label: Some("Mix".to_string()),
                        },
                        EnumValue {
                            value: "dip_to_black".to_string(),
                            label: Some("Dip to Black".to_string()),
                        },
                        EnumValue {
                            value: "wipe_left".to_string(),
                            label: Some("Wipe Left".to_string()),
                        },
                        EnumValue {
                            value: "wipe_right".to_string(),
                            label: Some("Wipe Right".to_string()),
                        },
                        EnumValue {
                            value: "wipe_up".to_string(),
                            label: Some("Wipe Up".to_string()),
                        },
                        EnumValue {
                            value: "wipe_down".to_string(),
                            label: Some("Wipe Down".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("cut".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stinger_under_transition".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "stinger_under_duration_ms".to_string(),
                label: "Stinger Beneath Duration (ms)".to_string(),
                description: "How long the transition beneath takes. Ignored for a cut, and shortened if it would outlast the clip."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stinger_under_duration_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
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
