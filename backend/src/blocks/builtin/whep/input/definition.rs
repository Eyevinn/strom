//! WHEP Input block definition.

use strom_types::{block::*, PropertyValue, *};

/// Get WHEP Input block definition (metadata only).
pub(crate) fn whep_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_input".to_string(),
        name: "WHEP Input".to_string(),
        description: "Receives audio/video via WebRTC WHEP protocol. Default uses stable whepsrc element.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whepsrc (stable) or whepclientsrc (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whepsrc".to_string(),
                            label: Some("whepsrc (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whepclientsrc".to_string(),
                            label: Some("whepclientsrc (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whepsrc".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "whep_endpoint".to_string(),
                label: "WHEP Endpoint".to_string(),
                description: "WHEP server endpoint URL (e.g., https://example.com/whep/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whep_endpoint".to_string(),
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
                name: "mixer_latency_ms".to_string(),
                label: "Mixer Latency (ms)".to_string(),
                description: "Latency of the audio mixer in milliseconds (default 30ms, lower = less delay but may cause glitches)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(30)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mixer_latency_ms".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "jitterbuffer_latency_ms".to_string(),
                label: "Jitterbuffer Latency (ms)".to_string(),
                description: "WebRTC jitterbuffer latency in milliseconds (default 200ms). Lower values reduce delay but increase sensitivity to network jitter. For LAN use, 40-80ms is recommended.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_JITTERBUFFER_LATENCY_MS)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "jitterbuffer_latency_ms".to_string(),
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
                name: "ice_transport_policy".to_string(),
                label: "ICE Transport Policy".to_string(),
                description: "Which ICE candidates this WHEP subscriber may use. Leave on the server default to follow the server-wide setting. Force TURN relay when host and server-reflexive candidates cannot cross the network in between — every candidate then goes through the configured TURN server, which requires one to be configured in the server's ICE servers.".to_string(),
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
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "output_audioresample".to_string(),
                internal_pad_name: "src".to_string(),
            }],
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
