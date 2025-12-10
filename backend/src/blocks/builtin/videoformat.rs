//! Video format block with optional video property conversion.
//!
//! This block provides a simple way to set common video properties:
//! - Resolution (width/height) - enforced by caps
//! - Framerate - enforced by caps (NOTE: videorate temporarily removed, framerate not enforced)
//! - Color format (pixel format) - enforced by caps
//!
//! All properties are optional. The block creates a fixed chain of elements:
//! videoscale -> videoconvert -> capsfilter
//!
//! TEMPORARY: videorate element removed to avoid frame duplication issues.
//!
//! Only the capsfilter caps are set based on which properties are specified.
//! Unspecified properties allow passthrough - elements will not modify those aspects.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{
    block::*, common_video_resolution_enum_values, element::ElementPadRef, EnumValue,
    PropertyValue, *,
};
use tracing::info;

/// Video Format block builder.
pub struct VideoFormatBuilder;

impl BlockBuilder for VideoFormatBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("🎬 Building VideoFormat block instance: {}", instance_id);

        // Parse optional properties
        let resolution = properties.get("resolution").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        });

        let framerate = properties.get("framerate").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            PropertyValue::Int(i) => Some(match *i {
                25 => "25",
                30 => "30",
                50 => "50",
                60 => "60",
                _ => return None,
            }),
            _ => None,
        });

        let format = properties.get("format").and_then(|v| match v {
            PropertyValue::String(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        });

        // Build caps string dynamically with only specified fields
        let mut caps_fields = vec!["video/x-raw".to_string()];

        // Add resolution if specified
        if let Some(res) = resolution {
            // Parse resolution string (e.g., "1920x1080")
            let parts: Vec<&str> = res.split('x').collect();
            if parts.len() == 2 {
                caps_fields.push(format!("width={}", parts[0]));
                caps_fields.push(format!("height={}", parts[1]));
            }
        }

        // Add framerate if specified
        if let Some(fps) = framerate {
            // Convert decimal framerates to proper fractions
            let framerate_fraction = match fps {
                "23.976" => "24000/1001".to_string(),
                "29.97" => "30000/1001".to_string(),
                "59.94" => "60000/1001".to_string(),
                _ => format!("{}/1", fps),
            };
            caps_fields.push(format!("framerate={}", framerate_fraction));
        }

        // Add format if specified
        if let Some(fmt) = format {
            caps_fields.push(format!("format={}", fmt));
        }

        let caps_str = caps_fields.join(",");
        info!("🎬 VideoFormat block caps: {}", caps_str);

        // Always create all elements for consistent external pad references
        // Elements will just pass through if their respective properties aren't set
        let scale_id = format!("{}:videoscale", instance_id);
        let convert_id = format!("{}:videoconvert", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let videoscale = gst::ElementFactory::make("videoscale")
            .name(&scale_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoscale: {}", e)))?;

        // TEMPORARY: videorate removed to avoid frame duplication issues
        // let videorate = gst::ElementFactory::make("videorate")
        //     .name(&rate_id)
        //     .build()
        //     .map_err(|e| BlockBuildError::ElementCreation(format!("videorate: {}", e)))?;

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .name(&convert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoconvert: {}", e)))?;

        // capsfilter with caps (only constraints specified properties)
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!("🎬 VideoFormat block created (chain: videoscale -> videoconvert -> capsfilter) [videorate TEMPORARILY REMOVED]");

        // Chain: videoscale -> videoconvert -> capsfilter (videorate temporarily removed)
        let internal_links = vec![
            (
                ElementPadRef::pad(&scale_id, "src"),
                ElementPadRef::pad(&convert_id, "sink"),
            ),
            (
                ElementPadRef::pad(&convert_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (scale_id, videoscale),
                (convert_id, videoconvert),
                (capsfilter_id, capsfilter),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for VideoFormat block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![videoformat_definition()]
}

/// Get VideoFormat block definition (metadata only).
fn videoformat_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.videoformat".to_string(),
        name: "Video Format".to_string(),
        description: "Optional video format conversion. Set resolution, framerate, and/or pixel format as needed. Unset properties pass through unchanged.".to_string(),
        category: "Video".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "resolution".to_string(),
                label: "Resolution".to_string(),
                description: "Video resolution - creates videoscale element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_resolution_enum_values(true), // include empty "-" option
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "resolution".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "framerate".to_string(),
                label: "Framerate".to_string(),
                description: "Framerate in fps - creates videorate element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "".to_string(), label: Some("-".to_string()) },
                        EnumValue { value: "10".to_string(), label: Some("10 fps".to_string()) },
                        EnumValue { value: "15".to_string(), label: Some("15 fps".to_string()) },
                        EnumValue { value: "23.976".to_string(), label: Some("23.976 fps (24000/1001)".to_string()) },
                        EnumValue { value: "24".to_string(), label: Some("24 fps".to_string()) },
                        EnumValue { value: "25".to_string(), label: Some("25 fps".to_string()) },
                        EnumValue { value: "29.97".to_string(), label: Some("29.97 fps (30000/1001)".to_string()) },
                        EnumValue { value: "30".to_string(), label: Some("30 fps".to_string()) },
                        EnumValue { value: "50".to_string(), label: Some("50 fps".to_string()) },
                        EnumValue { value: "59.94".to_string(), label: Some("59.94 fps (60000/1001)".to_string()) },
                        EnumValue { value: "60".to_string(), label: Some("60 fps".to_string()) },
                        EnumValue { value: "120".to_string(), label: Some("120 fps".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "framerate".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "format".to_string(),
                label: "Pixel Format".to_string(),
                description: "Pixel format/color space - creates videoconvert element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "".to_string(), label: Some("-".to_string()) },
                        EnumValue { value: "I420".to_string(), label: Some("I420 (YUV 4:2:0 planar)".to_string()) },
                        EnumValue { value: "YV12".to_string(), label: Some("YV12 (YUV 4:2:0 planar)".to_string()) },
                        EnumValue { value: "NV12".to_string(), label: Some("NV12 (YUV 4:2:0 semi-planar)".to_string()) },
                        EnumValue { value: "NV21".to_string(), label: Some("NV21 (YUV 4:2:0 semi-planar)".to_string()) },
                        EnumValue { value: "YUY2".to_string(), label: Some("YUY2 (YUV 4:2:2 packed)".to_string()) },
                        EnumValue { value: "UYVY".to_string(), label: Some("UYVY (YUV 4:2:2 packed)".to_string()) },
                        EnumValue { value: "v210".to_string(), label: Some("v210 (10-bit YUV 4:2:2)".to_string()) },
                        EnumValue { value: "RGB".to_string(), label: Some("RGB".to_string()) },
                        EnumValue { value: "BGR".to_string(), label: Some("BGR".to_string()) },
                        EnumValue { value: "RGBA".to_string(), label: Some("RGBA".to_string()) },
                        EnumValue { value: "BGRA".to_string(), label: Some("BGRA".to_string()) },
                        EnumValue { value: "ARGB".to_string(), label: Some("ARGB".to_string()) },
                        EnumValue { value: "ABGR".to_string(), label: Some("ABGR".to_string()) },
                        EnumValue { value: "RGBx".to_string(), label: Some("RGBx".to_string()) },
                        EnumValue { value: "BGRx".to_string(), label: Some("BGRx".to_string()) },
                        EnumValue { value: "xRGB".to_string(), label: Some("xRGB".to_string()) },
                        EnumValue { value: "xBGR".to_string(), label: Some("xBGR".to_string()) },
                        EnumValue { value: "GRAY8".to_string(), label: Some("GRAY8 (8-bit grayscale)".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "format".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "videoscale".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "capsfilter".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎬".to_string()),
            width: Some(1.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}
