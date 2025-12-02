//! WHIP (WebRTC-HTTP Ingestion Protocol) output block builder.
//!
//! Uses GStreamer's whipclientsink for WebRTC streaming via WHIP signalling.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::debug;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

impl BlockBuilder for WHIPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Output block instance: {}", instance_id);

        // Get required WHIP endpoint
        let whip_endpoint = properties
            .get("whip_endpoint")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidProperty("whip_endpoint property required".to_string())
            })?;

        // Get optional auth token
        let auth_token = properties.get("auth_token").and_then(|v| {
            if let PropertyValue::String(s) = v {
                if s.is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            } else {
                None
            }
        });

        // Get STUN server (optional)
        let stun_server = properties
            .get("stun_server")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.clone())
                    }
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "stun://stun.l.google.com:19302".to_string());

        // Create namespaced element IDs
        let whipclientsink_id = format!("{}:whipclientsink", instance_id);

        // For audio input, we need audioconvert and audioresample before the sink
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);

        // Create audio processing elements
        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        // Create whipclientsink element
        let whipclientsink = gst::ElementFactory::make("whipclientsink")
            .name(&whipclientsink_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("whipclientsink: {}", e)))?;

        // Set signaller properties using the child proxy interface
        // whipclientsink exposes signaller properties via signaller::property-name
        whipclientsink.set_property("stun-server", &stun_server);

        // Disable video codecs by setting video-caps to empty
        whipclientsink.set_property("video-caps", gst::Caps::new_empty());

        // Access the signaller child and set its properties
        let signaller = whipclientsink.property::<gst::glib::Object>("signaller");

        signaller.set_property("whip-endpoint", &whip_endpoint);

        if let Some(token) = &auth_token {
            signaller.set_property("auth-token", token);
        }

        debug!(
            "WHIP Output configured: endpoint={}, stun={}",
            whip_endpoint, stun_server
        );

        // Define internal links
        // Note: whipclientsink uses request pads (audio_%u, video_%u)
        // The first audio pad requested will be audio_0
        let internal_links = vec![
            (
                ElementPadRef::pad(&audioconvert_id, "src"),
                ElementPadRef::pad(&audioresample_id, "sink"),
            ),
            (
                ElementPadRef::pad(&audioresample_id, "src"),
                ElementPadRef::pad(&whipclientsink_id, "audio_0"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (audioconvert_id, audioconvert),
                (audioresample_id, audioresample),
                (whipclientsink_id, whipclientsink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for WHIP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whip_output_definition()]
}

/// Get WHIP Output block definition (metadata only).
fn whip_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_output".to_string(),
        name: "WHIP Output".to_string(),
        description: "Sends audio/video via WebRTC WHIP protocol. Uses whipclientsink for signalling and media transport.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
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
            },
            ExposedProperty {
                name: "stun_server".to_string(),
                label: "STUN Server".to_string(),
                description: "STUN server URL for NAT traversal".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(
                    "stun://stun.l.google.com:19302".to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stun_server".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
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
            color: Some("#9C27B0".to_string()), // Purple for WebRTC
            width: Some(2.5),
            height: Some(1.5),
        }),
    }
}
