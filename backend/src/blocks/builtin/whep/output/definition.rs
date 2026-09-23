//! WHEP Output block definition.

use strom_types::{block::*, PropertyValue, *};

/// Get WHEP Output block definition (server mode - hosts WHEP endpoint).
pub(crate) fn whep_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_output".to_string(),
        name: "WHEP Output".to_string(),
        description: "Hosts a WHEP server endpoint. Clients can connect via WHEP to receive the WebRTC stream. Access at /api/whep/{endpoint_id}. Set Number of Video/Audio Tracks to 0 to disable that media type.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Number of Video Tracks".to_string(),
                description: "Number of video input tracks (0 disables video on this endpoint).".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Number of Audio Tracks".to_string(),
                description: "Number of audio input tracks (0 disables audio on this endpoint).".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHEP endpoint. Leave empty to auto-generate a UUID. Access at /api/whep/{endpoint_id}".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "endpoint_id".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "ts_offset_ms".to_string(),
                label: "TS Offset (ms)".to_string(),
                description: "Shifts the clock wait at this output's input. A negative value releases buffers to the WebRTC sessions earlier, but each viewer's session still waits out the full pipeline latency, so viewers gain far less than the offset (-30 ms measured about 8 ms). A/V sync is maintained.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ts_offset_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "do_retransmission".to_string(),
                label: "Retransmission (RTX)".to_string(),
                description: "Resend lost packets to viewers on request (NACK-based). Without it, packet loss forces a full keyframe request instead of a cheap resend.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "do_retransmission".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "ice_transport_policy".to_string(),
                label: "ICE Transport Policy".to_string(),
                description: "Which ICE candidates this WHEP playback endpoint may use. Leave on the server default to follow the server-wide setting. Force TURN relay when host and server-reflexive candidates cannot cross the network in between — every candidate then goes through the configured TURN server, which requires one to be configured in the server's ICE servers.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "".to_string(),
                            label: Some("Server default".to_string()),
                        },
                        EnumValue {
                            value: "all".to_string(),
                            label: Some("All (host, srflx, relay)".to_string()),
                        },
                        EnumValue {
                            value: "relay".to_string(),
                            label: Some("Relay only (force TURN)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ice_transport_policy".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        // Note: external_pads here are the static defaults (1 video + 1 audio).
        // The actual pads are determined dynamically by WHEPOutputBuilder::get_external_pads()
        // based on num_audio_tracks / num_video_tracks (and the legacy mode property if present).
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_queue".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_queue".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
