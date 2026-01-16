//! Blackmagic DeckLink SDI/HDMI capture and playback block builders.
//!
//! Provides separate video and audio input/output blocks for Blackmagic DeckLink cards.
//! Uses GStreamer's DeckLink plugin (gst-plugins-bad) for hardware integration.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::video_convert_mode;
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, *};
use tracing::info;

/// DeckLink Video Input block builder.
pub struct DeckLinkVideoInputBuilder;

impl BlockBuilder for DeckLinkVideoInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("📹 Building DeckLink Video Input block: {}", instance_id);

        // Parse properties
        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let connection = properties
            .get("connection")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("sdi");

        // Create elements with namespaced IDs
        // Use detected video convert mode (autovideoconvert if GPU interop works, videoconvert otherwise)
        // Note: We always use "videoconvert" as the element ID for consistent external pad references,
        // even when the actual GStreamer element is "autovideoconvert"
        let convert_mode = video_convert_mode();
        let convert_element_name = convert_mode.element_name();
        let videosrc_id = format!("{}:decklinkvideosrc", instance_id);
        let videoconvert_id = format!("{}:videoconvert", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let videosrc = gst::ElementFactory::make("decklinkvideosrc")
            .name(&videosrc_id)
            .property("device-number", device_number)
            .property_from_str("mode", mode)
            .property_from_str("connection", connection)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosrc: {}", e)))?;

        let videoconvert = gst::ElementFactory::make(convert_element_name)
            .name(&videoconvert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
            })?;

        // Capsfilter with generic video/x-raw caps (no specific format restriction)
        let caps = gst::Caps::builder("video/x-raw").build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!(
            "📹 DeckLink Video Input configured: device={}, mode={}, connection={}",
            device_number, mode, connection
        );

        // Chain: decklinkvideosrc -> videoconvert -> capsfilter
        let internal_links = vec![
            (
                ElementPadRef::pad(&videosrc_id, "src"),
                ElementPadRef::pad(&videoconvert_id, "sink"),
            ),
            (
                ElementPadRef::pad(&videoconvert_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (videosrc_id, videosrc),
                (videoconvert_id, videoconvert),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// DeckLink Audio Input block builder.
pub struct DeckLinkAudioInputBuilder;

impl BlockBuilder for DeckLinkAudioInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building DeckLink Audio Input block: {}", instance_id);

        // Parse properties
        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        let connection = properties
            .get("connection")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let channels = properties
            .get("channels")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(u.to_string()),
                PropertyValue::Int(i) if *i > 0 => Some(i.to_string()),
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "2".to_string());

        // Create elements with namespaced IDs
        let audiosrc_id = format!("{}:decklinkaudiosrc", instance_id);
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let audiosrc = gst::ElementFactory::make("decklinkaudiosrc")
            .name(&audiosrc_id)
            .property("device-number", device_number)
            .property_from_str("connection", connection)
            .property_from_str("channels", &channels)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkaudiosrc: {}", e)))?;

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        // Capsfilter with generic audio/x-raw caps
        let caps = gst::Caps::builder("audio/x-raw").build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!(
            "🎵 DeckLink Audio Input configured: device={}, connection={}, channels={}",
            device_number, connection, channels
        );

        // Chain: decklinkaudiosrc -> audioconvert -> audioresample -> capsfilter
        let internal_links = vec![
            (
                ElementPadRef::pad(&audiosrc_id, "src"),
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
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (audiosrc_id, audiosrc),
                (audioconvert_id, audioconvert),
                (audioresample_id, audioresample),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// DeckLink Video Output block builder.
pub struct DeckLinkVideoOutputBuilder;

impl BlockBuilder for DeckLinkVideoOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("📹 Building DeckLink Video Output block: {}", instance_id);

        // Parse properties
        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        // Create elements with namespaced IDs
        // Use detected video convert mode (autovideoconvert if GPU interop works, videoconvert otherwise)
        // Note: We always use "videoconvert" as the element ID for consistent external pad references,
        // even when the actual GStreamer element is "autovideoconvert"
        let convert_mode = video_convert_mode();
        let convert_element_name = convert_mode.element_name();
        let videoconvert_id = format!("{}:videoconvert", instance_id);
        let videosink_id = format!("{}:decklinkvideosink", instance_id);

        let videoconvert = gst::ElementFactory::make(convert_element_name)
            .name(&videoconvert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
            })?;

        let videosink = gst::ElementFactory::make("decklinkvideosink")
            .name(&videosink_id)
            .property("device-number", device_number)
            .property_from_str("mode", mode)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosink: {}", e)))?;

        info!(
            "📹 DeckLink Video Output configured: device={}, mode={}",
            device_number, mode
        );

        // Chain: videoconvert -> decklinkvideosink
        let internal_links = vec![(
            ElementPadRef::pad(&videoconvert_id, "src"),
            ElementPadRef::pad(&videosink_id, "sink"),
        )];

        Ok(BlockBuildResult {
            elements: vec![(videoconvert_id, videoconvert), (videosink_id, videosink)],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// DeckLink Audio Output block builder.
pub struct DeckLinkAudioOutputBuilder;

impl BlockBuilder for DeckLinkAudioOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building DeckLink Audio Output block: {}", instance_id);

        // Parse properties
        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        // Create elements with namespaced IDs
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);
        let audiosink_id = format!("{}:decklinkaudiosink", instance_id);

        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        let audiosink = gst::ElementFactory::make("decklinkaudiosink")
            .name(&audiosink_id)
            .property("device-number", device_number)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkaudiosink: {}", e)))?;

        info!(
            "🎵 DeckLink Audio Output configured: device={}",
            device_number
        );

        // Chain: audioconvert -> audioresample -> decklinkaudiosink
        let internal_links = vec![
            (
                ElementPadRef::pad(&audioconvert_id, "src"),
                ElementPadRef::pad(&audioresample_id, "sink"),
            ),
            (
                ElementPadRef::pad(&audioresample_id, "src"),
                ElementPadRef::pad(&audiosink_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (audioconvert_id, audioconvert),
                (audioresample_id, audioresample),
                (audiosink_id, audiosink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for DeckLink blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![
        decklink_video_input_definition(),
        decklink_audio_input_definition(),
        decklink_video_output_definition(),
        decklink_audio_output_definition(),
    ]
}

/// Common video mode enum values for DeckLink
fn video_mode_enum_values() -> Vec<EnumValue> {
    vec![
        EnumValue {
            value: "auto".to_string(),
            label: Some("Auto".to_string()),
        },
        // HD modes
        EnumValue {
            value: "1080p2398".to_string(),
            label: Some("1080p 23.98".to_string()),
        },
        EnumValue {
            value: "1080p24".to_string(),
            label: Some("1080p 24".to_string()),
        },
        EnumValue {
            value: "1080p25".to_string(),
            label: Some("1080p 25".to_string()),
        },
        EnumValue {
            value: "1080p2997".to_string(),
            label: Some("1080p 29.97".to_string()),
        },
        EnumValue {
            value: "1080p30".to_string(),
            label: Some("1080p 30".to_string()),
        },
        EnumValue {
            value: "1080p50".to_string(),
            label: Some("1080p 50".to_string()),
        },
        EnumValue {
            value: "1080p5994".to_string(),
            label: Some("1080p 59.94".to_string()),
        },
        EnumValue {
            value: "1080p60".to_string(),
            label: Some("1080p 60".to_string()),
        },
        EnumValue {
            value: "1080i50".to_string(),
            label: Some("1080i 50".to_string()),
        },
        EnumValue {
            value: "1080i5994".to_string(),
            label: Some("1080i 59.94".to_string()),
        },
        EnumValue {
            value: "1080i60".to_string(),
            label: Some("1080i 60".to_string()),
        },
        EnumValue {
            value: "720p50".to_string(),
            label: Some("720p 50".to_string()),
        },
        EnumValue {
            value: "720p5994".to_string(),
            label: Some("720p 59.94".to_string()),
        },
        EnumValue {
            value: "720p60".to_string(),
            label: Some("720p 60".to_string()),
        },
        // UHD modes
        EnumValue {
            value: "2160p2398".to_string(),
            label: Some("4K 23.98".to_string()),
        },
        EnumValue {
            value: "2160p24".to_string(),
            label: Some("4K 24".to_string()),
        },
        EnumValue {
            value: "2160p25".to_string(),
            label: Some("4K 25".to_string()),
        },
        EnumValue {
            value: "2160p2997".to_string(),
            label: Some("4K 29.97".to_string()),
        },
        EnumValue {
            value: "2160p30".to_string(),
            label: Some("4K 30".to_string()),
        },
        EnumValue {
            value: "2160p50".to_string(),
            label: Some("4K 50".to_string()),
        },
        EnumValue {
            value: "2160p5994".to_string(),
            label: Some("4K 59.94".to_string()),
        },
        EnumValue {
            value: "2160p60".to_string(),
            label: Some("4K 60".to_string()),
        },
    ]
}

/// Get DeckLink Video Input block definition.
fn decklink_video_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_video_input".to_string(),
        name: "DeckLink Video Input".to_string(),
        description: "Captures video from Blackmagic DeckLink SDI/HDMI card using decklinkvideosrc. Supports various video modes and connections.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "device_number".to_string(),
                label: "Device Number".to_string(),
                description: "DeckLink device number (0-based index for multi-card systems)"
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "device_number".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Video Mode".to_string(),
                description: "Video mode (resolution and framerate)".to_string(),
                property_type: PropertyType::Enum {
                    values: video_mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "connection".to_string(),
                label: "Connection Type".to_string(),
                description: "Input connection type".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "sdi".to_string(),
                            label: Some("SDI".to_string()),
                        },
                        EnumValue {
                            value: "hdmi".to_string(),
                            label: Some("HDMI".to_string()),
                        },
                        EnumValue {
                            value: "optical-sdi".to_string(),
                            label: Some("Optical SDI".to_string()),
                        },
                        EnumValue {
                            value: "component".to_string(),
                            label: Some("Component".to_string()),
                        },
                        EnumValue {
                            value: "composite".to_string(),
                            label: Some("Composite".to_string()),
                        },
                        EnumValue {
                            value: "s-video".to_string(),
                            label: Some("S-Video".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("sdi".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "connection".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📹".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get DeckLink Audio Input block definition.
fn decklink_audio_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_audio_input".to_string(),
        name: "DeckLink Audio Input".to_string(),
        description: "Captures audio from Blackmagic DeckLink SDI/HDMI card using decklinkaudiosrc. Supports embedded audio from SDI/HDMI.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "device_number".to_string(),
                label: "Device Number".to_string(),
                description: "DeckLink device number (0-based index for multi-card systems)"
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "device_number".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "connection".to_string(),
                label: "Connection Type".to_string(),
                description: "Audio input connection type".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "auto".to_string(),
                            label: Some("Auto".to_string()),
                        },
                        EnumValue {
                            value: "embedded".to_string(),
                            label: Some("Embedded (SDI/HDMI)".to_string()),
                        },
                        EnumValue {
                            value: "aesebu".to_string(),
                            label: Some("AES/EBU".to_string()),
                        },
                        EnumValue {
                            value: "analog".to_string(),
                            label: Some("Analog".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "connection".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "channels".to_string(),
                label: "Channels".to_string(),
                description: "Number of audio channels".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "2".to_string(),
                            label: Some("2 (Stereo)".to_string()),
                        },
                        EnumValue {
                            value: "8".to_string(),
                            label: Some("8".to_string()),
                        },
                        EnumValue {
                            value: "16".to_string(),
                            label: Some("16".to_string()),
                        },
                        EnumValue {
                            value: "max".to_string(),
                            label: Some("Max (auto-detect)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("2".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "channels".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎵".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get DeckLink Video Output block definition.
fn decklink_video_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_video_output".to_string(),
        name: "DeckLink Video Output".to_string(),
        description: "Outputs video to Blackmagic DeckLink SDI/HDMI card using decklinkvideosink."
            .to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "device_number".to_string(),
                label: "Device Number".to_string(),
                description: "DeckLink device number (0-based index for multi-card systems)"
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "device_number".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Video Mode".to_string(),
                description: "Video mode (resolution and framerate)".to_string(),
                property_type: PropertyType::Enum {
                    values: video_mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "videoconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📺".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get DeckLink Audio Output block definition.
fn decklink_audio_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_audio_output".to_string(),
        name: "DeckLink Audio Output".to_string(),
        description: "Outputs audio to Blackmagic DeckLink SDI/HDMI card using decklinkaudiosink. Audio is embedded in SDI/HDMI output.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![ExposedProperty {
            name: "device_number".to_string(),
            label: "Device Number".to_string(),
            description: "DeckLink device number (0-based index for multi-card systems)"
                .to_string(),
            property_type: PropertyType::UInt,
            default_value: Some(PropertyValue::UInt(0)),
            mapping: PropertyMapping {
                element_id: "_block".to_string(),
                property_name: "device_number".to_string(),
                transform: None,
            },
        }],
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
            icon: Some("🔊".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
