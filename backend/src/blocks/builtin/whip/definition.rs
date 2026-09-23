//! WHIP Output and WHIP Input block definitions.

use strom_types::{block::*, PropertyValue, *};

// ============================================================================
// Block Definitions
// ============================================================================

/// Get metadata for WHIP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whip_output_definition(), whip_input_definition()]
}

/// WHIP Output block definition.
fn whip_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_output".to_string(),
        name: "WHIP Output".to_string(),
        description: "Sends audio via WebRTC WHIP protocol. Default uses stable whipsink element.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whipsink (stable) or whipclientsink (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whipsink".to_string(),
                            label: Some("whipsink (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whipclientsink".to_string(),
                            label: Some("whipclientsink (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whipsink".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "whip_endpoint".to_string(),
                label: "WHIP Endpoint".to_string(),
                description: "WHIP server endpoint URL (e.g., https://example.com/whip/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whip_endpoint".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "auth_token".to_string(),
                label: "Auth Token".to_string(),
                description: "Bearer token for authentication (optional)".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "auth_token".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "opus_complexity".to_string(),
                label: "Opus Complexity".to_string(),
                description: "Opus encoder complexity (0-10). Lower values use less CPU. 5 is recommended for real-time.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_OPUS_COMPLEXITY as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "opus_complexity".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "opus_bitrate".to_string(),
                label: "Opus Bitrate".to_string(),
                description: "Opus encoder bitrate in bps (4000-650000)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_OPUS_BITRATE as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "opus_bitrate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "ice_transport_policy".to_string(),
                label: "ICE Transport Policy".to_string(),
                description: "Which ICE candidates this WHIP publisher may use. Leave on the server default to follow the server-wide setting. Force TURN relay when host and server-reflexive candidates cannot cross the network in between — every candidate then goes through the configured TURN server, which requires one to be configured in the server's ICE servers.".to_string(),
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
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// WHIP Input block definition (server mode - hosts WHIP endpoint).
fn whip_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_input".to_string(),
        name: "WHIP Input".to_string(),
        description: "Hosts a WHIP server endpoint. Clients (browsers, OBS, encoders) connect via WHIP to send media. Access ingest page at /player/whip-ingest".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "What media to accept: audio + video, audio only, or video only".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio Only".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video Only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHIP endpoint. Leave empty to auto-generate. Ingest at /whip/{endpoint_id}".to_string(),
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
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode incoming RTP to raw audio/video. When disabled, outputs RTP (application/x-rtp).".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "jitterbuffer_latency_ms".to_string(),
                label: "Jitterbuffer Latency (ms)".to_string(),
                description: "How long to buffer incoming RTP before releasing/dropping it. Too low can cause an initial video keyframe's packet burst to be dropped locally on connect, stalling video entirely even though packets arrived fine. Increase if video never starts.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(400)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "jitterbuffer_latency_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "do_retransmission".to_string(),
                label: "Retransmission (RTX)".to_string(),
                description: "Request retransmission of lost packets from the publisher (NACK-based). Without it, any packet loss forces a full keyframe request instead of a cheap resend.".to_string(),
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
                name: "drop_on_latency".to_string(),
                label: "Drop On Latency".to_string(),
                description: "Drop queued packets that exceed the jitterbuffer latency instead of holding them. On by default: it works around a jitterbuffer bug that otherwise stalls the stream for the length of a mute gap. Turn it off when a downstream WebRTC endpoint has its own adaptive buffer and should decide what is too late.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "drop_on_latency".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_video_bitrate".to_string(),
                label: "Max Video Bitrate (kbps)".to_string(),
                description: "Maximum video bitrate hint sent to the browser via SDP. The browser's encoder will ramp up to this value.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(
                    strom_types::whip::DEFAULT_MAX_VIDEO_BITRATE_KBPS as i64,
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_video_bitrate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "max_sessions".to_string(),
                label: "Max Sessions".to_string(),
                description: "Maximum number of simultaneous WHIP client connections. Each session gets its own independent output.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "max_sessions".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "ice_transport_policy".to_string(),
                label: "ICE Transport Policy".to_string(),
                description: "Which ICE candidates this WHIP ingest endpoint may use. Leave on the server default to follow the server-wide setting. Force TURN relay when host and server-reflexive candidates cannot cross the network in between — every candidate then goes through the configured TURN server, which requires one to be configured in the server's ICE servers.".to_string(),
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
        // Note: external_pads here are the static defaults for audio_video mode with max_sessions=1.
        // Actual pads are determined dynamically by WHIPInputBuilder::get_external_pads().
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_out_tee_0".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_out_tee_0".to_string(),
                    internal_pad_name: "src_%u".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📹".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
