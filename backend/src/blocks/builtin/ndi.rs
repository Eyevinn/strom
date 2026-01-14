//! NDI (Network Device Interface) input and output blocks.
//!
//! Provides NDI input and output blocks with configurable modes for audio/video handling.
//! Uses the gst-plugin-ndi GStreamer plugin for NDI integration.
//! Requires the NDI SDK from NewTek/Vizrt to be installed.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::video_convert_mode;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, *};
use tracing::info;

// NDI Input defaults
const NDI_INPUT_DEFAULT_TIMEOUT_MS: u32 = 5000;
const NDI_INPUT_DEFAULT_CONNECT_TIMEOUT_MS: u32 = 10000;

/// NDI bandwidth modes
fn bandwidth_enum_values() -> Vec<EnumValue> {
    vec![
        EnumValue {
            value: "100".to_string(),
            label: Some("Highest".to_string()),
        },
        EnumValue {
            value: "10".to_string(),
            label: Some("Audio Only".to_string()),
        },
        EnumValue {
            value: "-10".to_string(),
            label: Some("Metadata Only".to_string()),
        },
    ]
}

/// NDI mode (combined, video only, or audio only)
fn mode_enum_values() -> Vec<EnumValue> {
    vec![
        EnumValue {
            value: "combined".to_string(),
            label: Some("Audio + Video".to_string()),
        },
        EnumValue {
            value: "video".to_string(),
            label: Some("Video Only".to_string()),
        },
        EnumValue {
            value: "audio".to_string(),
            label: Some("Audio Only".to_string()),
        },
    ]
}

/// NDI Input block builder.
pub struct NDIInputBuilder;

impl BlockBuilder for NDIInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "combined".to_string());

        let outputs = match mode.as_str() {
            "combined" => vec![
                ExternalPad {
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audiocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
            "video" => vec![ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
            "audio" => vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
            _ => vec![], // Invalid mode, no pads
        };

        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building NDI Input block: {}", instance_id);

        // Parse properties
        let ndi_name = properties
            .get("ndi_name")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        let url_address = properties
            .get("url_address")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();

        let bandwidth = properties
            .get("bandwidth")
            .and_then(|v| match v {
                PropertyValue::String(s) => s.parse::<i32>().ok(),
                PropertyValue::Int(i) => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(100); // Default: highest

        let timeout_ms = properties
            .get("timeout_ms")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u32),
                PropertyValue::UInt(u) => Some(*u as u32),
                _ => None,
            })
            .unwrap_or(NDI_INPUT_DEFAULT_TIMEOUT_MS);

        let connect_timeout_ms = properties
            .get("connect_timeout_ms")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u32),
                PropertyValue::UInt(u) => Some(*u as u32),
                _ => None,
            })
            .unwrap_or(NDI_INPUT_DEFAULT_CONNECT_TIMEOUT_MS);

        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "combined".to_string());

        // Create elements with namespaced IDs
        let ndisrc_id = format!("{}:ndisrc", instance_id);
        let demux_id = format!("{}:ndisrcdemux", instance_id);

        let mut ndisrc_builder = gst::ElementFactory::make("ndisrc")
            .name(&ndisrc_id)
            .property("bandwidth", bandwidth)
            .property("timeout", timeout_ms)
            .property("connect-timeout", connect_timeout_ms);

        // Set ndi-name or url-address (one must be provided)
        if !ndi_name.is_empty() {
            ndisrc_builder = ndisrc_builder.property("ndi-name", &ndi_name);
        }
        if !url_address.is_empty() {
            ndisrc_builder = ndisrc_builder.property("url-address", &url_address);
        }

        let ndisrc = ndisrc_builder
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("ndisrc: {}", e)))?;

        let demux = gst::ElementFactory::make("ndisrcdemux")
            .name(&demux_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("ndisrcdemux: {}", e)))?;

        let mut elements = vec![(ndisrc_id.clone(), ndisrc), (demux_id.clone(), demux)];

        let mut internal_links = vec![(
            ElementPadRef::pad(&ndisrc_id, "src"),
            ElementPadRef::pad(&demux_id, "sink"),
        )];

        // Build pipeline based on mode
        match mode.as_str() {
            "combined" => {
                // Video path - use detected video convert mode
                let convert_mode = video_convert_mode();
                let convert_element_name = convert_mode.element_name();
                let videoconvert_id = format!("{}:videoconvert", instance_id);
                let videocaps_id = format!("{}:videocapsfilter", instance_id);

                let videoconvert = gst::ElementFactory::make(convert_element_name)
                    .name(&videoconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
                    })?;

                let video_caps = gst::Caps::builder("video/x-raw").build();
                let videocaps = gst::ElementFactory::make("capsfilter")
                    .name(&videocaps_id)
                    .property("caps", &video_caps)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("videocapsfilter: {}", e))
                    })?;

                // Audio path
                let audioconvert_id = format!("{}:audioconvert", instance_id);
                let audioresample_id = format!("{}:audioresample", instance_id);
                let audiocaps_id = format!("{}:audiocapsfilter", instance_id);

                let audioconvert = gst::ElementFactory::make("audioconvert")
                    .name(&audioconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioconvert: {}", e))
                    })?;

                let audioresample = gst::ElementFactory::make("audioresample")
                    .name(&audioresample_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioresample: {}", e))
                    })?;

                let audio_caps = gst::Caps::builder("audio/x-raw").build();
                let audiocaps = gst::ElementFactory::make("capsfilter")
                    .name(&audiocaps_id)
                    .property("caps", &audio_caps)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audiocapsfilter: {}", e))
                    })?;

                elements.extend(vec![
                    (videoconvert_id.clone(), videoconvert),
                    (videocaps_id.clone(), videocaps),
                    (audioconvert_id.clone(), audioconvert),
                    (audioresample_id.clone(), audioresample),
                    (audiocaps_id.clone(), audiocaps),
                ]);

                internal_links.extend(vec![
                    (
                        ElementPadRef::pad(&demux_id, "video"),
                        ElementPadRef::pad(&videoconvert_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&videoconvert_id, "src"),
                        ElementPadRef::pad(&videocaps_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&demux_id, "audio"),
                        ElementPadRef::pad(&audioconvert_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioconvert_id, "src"),
                        ElementPadRef::pad(&audioresample_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioresample_id, "src"),
                        ElementPadRef::pad(&audiocaps_id, "sink"),
                    ),
                ]);

                info!(
                    "NDI Input (combined) configured: ndi_name={}, bandwidth={}",
                    ndi_name, bandwidth
                );
            }
            "video" => {
                // Use detected video convert mode
                let convert_mode = video_convert_mode();
                let convert_element_name = convert_mode.element_name();
                let videoconvert_id = format!("{}:videoconvert", instance_id);
                let capsfilter_id = format!("{}:capsfilter", instance_id);

                let videoconvert = gst::ElementFactory::make(convert_element_name)
                    .name(&videoconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
                    })?;

                let caps = gst::Caps::builder("video/x-raw").build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .name(&capsfilter_id)
                    .property("caps", &caps)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

                elements.extend(vec![
                    (videoconvert_id.clone(), videoconvert),
                    (capsfilter_id.clone(), capsfilter),
                ]);

                internal_links.extend(vec![
                    (
                        ElementPadRef::pad(&demux_id, "video"),
                        ElementPadRef::pad(&videoconvert_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&videoconvert_id, "src"),
                        ElementPadRef::pad(&capsfilter_id, "sink"),
                    ),
                ]);

                info!(
                    "NDI Input (video) configured: ndi_name={}, bandwidth={}",
                    ndi_name, bandwidth
                );
            }
            "audio" => {
                let audioconvert_id = format!("{}:audioconvert", instance_id);
                let audioresample_id = format!("{}:audioresample", instance_id);
                let capsfilter_id = format!("{}:capsfilter", instance_id);

                let audioconvert = gst::ElementFactory::make("audioconvert")
                    .name(&audioconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioconvert: {}", e))
                    })?;

                let audioresample = gst::ElementFactory::make("audioresample")
                    .name(&audioresample_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioresample: {}", e))
                    })?;

                let caps = gst::Caps::builder("audio/x-raw").build();
                let capsfilter = gst::ElementFactory::make("capsfilter")
                    .name(&capsfilter_id)
                    .property("caps", &caps)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

                elements.extend(vec![
                    (audioconvert_id.clone(), audioconvert),
                    (audioresample_id.clone(), audioresample),
                    (capsfilter_id.clone(), capsfilter),
                ]);

                internal_links.extend(vec![
                    (
                        ElementPadRef::pad(&demux_id, "audio"),
                        ElementPadRef::pad(&audioconvert_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioconvert_id, "src"),
                        ElementPadRef::pad(&audioresample_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioresample_id, "src"),
                        ElementPadRef::pad(&capsfilter_id, "sink"),
                    ),
                ]);

                info!(
                    "NDI Input (audio) configured: ndi_name={}, bandwidth={}",
                    ndi_name, bandwidth
                );
            }
            _ => {
                return Err(BlockBuildError::ElementCreation(format!(
                    "Invalid mode: {}",
                    mode
                )))
            }
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// NDI Output block builder.
pub struct NDIOutputBuilder;

impl BlockBuilder for NDIOutputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "combined".to_string());

        let inputs = match mode.as_str() {
            "combined" => vec![
                ExternalPad {
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videoconvert".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audioconvert".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            "video" => vec![ExternalPad {
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "videoconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            "audio" => vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            _ => vec![], // Invalid mode, no pads
        };

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building NDI Output block: {}", instance_id);

        // Parse properties
        let ndi_name = properties
            .get("ndi_name")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "Strom NDI".to_string());

        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "combined".to_string());

        let mut elements = vec![];
        let mut internal_links = vec![];

        // Build pipeline based on mode
        match mode.as_str() {
            "combined" => {
                // Video path - use detected video convert mode
                let convert_mode = video_convert_mode();
                let convert_element_name = convert_mode.element_name();
                let videoconvert_id = format!("{}:videoconvert", instance_id);
                let videoconvert = gst::ElementFactory::make(convert_element_name)
                    .name(&videoconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
                    })?;

                // Audio path
                let audioconvert_id = format!("{}:audioconvert", instance_id);
                let audioresample_id = format!("{}:audioresample", instance_id);

                let audioconvert = gst::ElementFactory::make("audioconvert")
                    .name(&audioconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioconvert: {}", e))
                    })?;

                let audioresample = gst::ElementFactory::make("audioresample")
                    .name(&audioresample_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioresample: {}", e))
                    })?;

                // Combiner and sink
                let combiner_id = format!("{}:ndisinkcombiner", instance_id);
                let ndisink_id = format!("{}:ndisink", instance_id);

                let combiner = gst::ElementFactory::make("ndisinkcombiner")
                    .name(&combiner_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("ndisinkcombiner: {}", e))
                    })?;

                // Request the audio pad (it's an "on request" pad)
                let audio_pad = combiner.request_pad_simple("audio").ok_or_else(|| {
                    BlockBuildError::ElementCreation(
                        "Failed to request audio pad on ndisinkcombiner".to_string(),
                    )
                })?;
                let audio_pad_name = audio_pad.name().to_string();

                let ndisink = gst::ElementFactory::make("ndisink")
                    .name(&ndisink_id)
                    .property("ndi-name", &ndi_name)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("ndisink: {}", e)))?;

                elements.extend(vec![
                    (videoconvert_id.clone(), videoconvert),
                    (audioconvert_id.clone(), audioconvert),
                    (audioresample_id.clone(), audioresample),
                    (combiner_id.clone(), combiner),
                    (ndisink_id.clone(), ndisink),
                ]);

                internal_links.extend(vec![
                    (
                        ElementPadRef::pad(&videoconvert_id, "src"),
                        ElementPadRef::pad(&combiner_id, "video"),
                    ),
                    (
                        ElementPadRef::pad(&audioconvert_id, "src"),
                        ElementPadRef::pad(&audioresample_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioresample_id, "src"),
                        ElementPadRef::pad(&combiner_id, &audio_pad_name),
                    ),
                    (
                        ElementPadRef::pad(&combiner_id, "src"),
                        ElementPadRef::pad(&ndisink_id, "sink"),
                    ),
                ]);

                info!("NDI Output (combined) configured: ndi_name={}", ndi_name);
            }
            "video" => {
                // Use detected video convert mode
                let convert_mode = video_convert_mode();
                let convert_element_name = convert_mode.element_name();
                let videoconvert_id = format!("{}:videoconvert", instance_id);
                let ndisink_id = format!("{}:ndisink", instance_id);

                let videoconvert = gst::ElementFactory::make(convert_element_name)
                    .name(&videoconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
                    })?;

                let ndisink = gst::ElementFactory::make("ndisink")
                    .name(&ndisink_id)
                    .property("ndi-name", &ndi_name)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("ndisink: {}", e)))?;

                elements.extend(vec![
                    (videoconvert_id.clone(), videoconvert),
                    (ndisink_id.clone(), ndisink),
                ]);

                internal_links.push((
                    ElementPadRef::pad(&videoconvert_id, "src"),
                    ElementPadRef::pad(&ndisink_id, "sink"),
                ));

                info!("NDI Output (video) configured: ndi_name={}", ndi_name);
            }
            "audio" => {
                let audioconvert_id = format!("{}:audioconvert", instance_id);
                let audioresample_id = format!("{}:audioresample", instance_id);
                let ndisink_id = format!("{}:ndisink", instance_id);

                let audioconvert = gst::ElementFactory::make("audioconvert")
                    .name(&audioconvert_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioconvert: {}", e))
                    })?;

                let audioresample = gst::ElementFactory::make("audioresample")
                    .name(&audioresample_id)
                    .build()
                    .map_err(|e| {
                        BlockBuildError::ElementCreation(format!("audioresample: {}", e))
                    })?;

                let ndisink = gst::ElementFactory::make("ndisink")
                    .name(&ndisink_id)
                    .property("ndi-name", &ndi_name)
                    .build()
                    .map_err(|e| BlockBuildError::ElementCreation(format!("ndisink: {}", e)))?;

                elements.extend(vec![
                    (audioconvert_id.clone(), audioconvert),
                    (audioresample_id.clone(), audioresample),
                    (ndisink_id.clone(), ndisink),
                ]);

                internal_links.extend(vec![
                    (
                        ElementPadRef::pad(&audioconvert_id, "src"),
                        ElementPadRef::pad(&audioresample_id, "sink"),
                    ),
                    (
                        ElementPadRef::pad(&audioresample_id, "src"),
                        ElementPadRef::pad(&ndisink_id, "sink"),
                    ),
                ]);

                info!("NDI Output (audio) configured: ndi_name={}", ndi_name);
            }
            _ => {
                return Err(BlockBuildError::ElementCreation(format!(
                    "Invalid mode: {}",
                    mode
                )))
            }
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Check if NDI GStreamer plugins are available
fn is_ndi_available() -> bool {
    use gst::prelude::*;

    // Check if the NDI plugin is registered
    let registry = gst::Registry::get();

    // Check for all required NDI elements
    let has_ndisrc = registry
        .find_feature("ndisrc", gst::ElementFactory::static_type())
        .is_some();
    let has_ndisink = registry
        .find_feature("ndisink", gst::ElementFactory::static_type())
        .is_some();
    let has_ndisrcdemux = registry
        .find_feature("ndisrcdemux", gst::ElementFactory::static_type())
        .is_some();
    let has_ndisinkcombiner = registry
        .find_feature("ndisinkcombiner", gst::ElementFactory::static_type())
        .is_some();

    has_ndisrc && has_ndisink && has_ndisrcdemux && has_ndisinkcombiner
}

/// Get metadata for NDI blocks (for UI/API).
/// Only returns blocks if NDI plugins are available.
/// Note: Block builders are always registered so existing flows remain valid.
pub fn get_blocks() -> Vec<BlockDefinition> {
    if !is_ndi_available() {
        info!("NDI GStreamer plugins not available - hiding NDI blocks from palette");
        return vec![];
    }

    info!("NDI GStreamer plugins detected - enabling NDI blocks");
    vec![ndi_input_definition(), ndi_output_definition()]
}

/// Get NDI Input block definition.
fn ndi_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.ndi_input".to_string(),
        name: "NDI Input".to_string(),
        description: "Receives audio and/or video from an NDI source over the network. Requires NDI SDK.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "ndi_name".to_string(),
                label: "NDI Source Name".to_string(),
                description: "NDI source name (e.g., 'HOSTNAME (Source Name)'). Use NDI discovery to find sources.".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ndi_name".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "url_address".to_string(),
                label: "URL Address".to_string(),
                description: "Alternative to NDI name: direct URL/address:port of the sender".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "url_address".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Mode".to_string(),
                description: "Receive combined audio+video, video only, or audio only".to_string(),
                property_type: PropertyType::Enum {
                    values: mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("combined".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "bandwidth".to_string(),
                label: "Bandwidth".to_string(),
                description: "Bandwidth mode for receiving".to_string(),
                property_type: PropertyType::Enum {
                    values: bandwidth_enum_values(),
                },
                default_value: Some(PropertyValue::String("100".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bandwidth".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "timeout_ms".to_string(),
                label: "Receive Timeout (ms)".to_string(),
                description: "Receive timeout in milliseconds".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(NDI_INPUT_DEFAULT_TIMEOUT_MS as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "timeout_ms".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "connect_timeout_ms".to_string(),
                label: "Connect Timeout (ms)".to_string(),
                description: "Connection timeout in milliseconds".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(
                    NDI_INPUT_DEFAULT_CONNECT_TIMEOUT_MS as i64
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "connect_timeout_ms".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audiocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get NDI Output block definition.
fn ndi_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.ndi_output".to_string(),
        name: "NDI Output".to_string(),
        description:
            "Sends audio and/or video to an NDI destination over the network. Requires NDI SDK."
                .to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "ndi_name".to_string(),
                label: "NDI Stream Name".to_string(),
                description:
                    "The name this NDI stream will be published as (will be prefixed with hostname)"
                        .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("Strom NDI".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ndi_name".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Mode".to_string(),
                description: "Send combined audio+video, video only, or audio only".to_string(),
                property_type: PropertyType::Enum {
                    values: mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("combined".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videoconvert".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audioconvert".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📤".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
