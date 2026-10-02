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

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::{self, video_convert_mode};
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{
    block::*, common_video_framerate_enum_values, common_video_pixel_format_enum_values,
    common_video_resolution_enum_values, element::ElementPadRef, PropertyValue, *,
};
use tracing::info;

/// Video Format block builder.
pub struct VideoFormatBuilder;

impl BlockBuilder for VideoFormatBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building VideoFormat block instance: {}", instance_id);

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
                // Pin PAR to 1:1 so autovideoconvert doesn't compensate with non-square pixels
                caps_fields.push("pixel-aspect-ratio=1/1".to_string());
            }
        }

        // Add framerate if specified (supports both fraction "25/1" and legacy decimal "25" formats)
        if let Some(fps) = framerate {
            let framerate_fraction = if fps.contains('/') {
                // Already in fraction format (e.g. "25/1", "30000/1001")
                fps.to_string()
            } else {
                // Legacy decimal format — convert to fraction
                match fps {
                    "23.976" => "24000/1001".to_string(),
                    "29.97" => "30000/1001".to_string(),
                    "59.94" => "60000/1001".to_string(),
                    _ => format!("{}/1", fps),
                }
            };
            caps_fields.push(format!("framerate={}", framerate_fraction));
        }

        // Add format if specified
        if let Some(fmt) = format {
            caps_fields.push(format!("format={}", fmt));
        }

        let caps_str = caps_fields.join(",");
        info!("VideoFormat block caps: {}", caps_str);

        // Always create all elements for consistent external pad references
        // Elements will just pass through if their respective properties aren't set
        let scale_id = format!("{}:videoscale", instance_id);
        // Use detected video convert mode (autovideoconvert if GPU interop works, videoconvert otherwise)
        // Note: We always use "videoconvert" as the element ID for consistent external pad references,
        // even when the actual GStreamer element is "autovideoconvert"
        let convert_mode = video_convert_mode();
        let convert_element_name = convert_mode.element_name();
        let convert_id = format!("{}:videoconvert", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        let videoscale = gst::ElementFactory::make("videoscale")
            .name(&scale_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoscale: {}", e)))?;
        gpu::configure_video_convert(&videoscale);

        // TEMPORARY: videorate removed to avoid frame duplication issues
        // let videorate = gst::ElementFactory::make("videorate")
        //     .name(&rate_id)
        //     .build()
        //     .map_err(|e| BlockBuildError::ElementCreation(format!("videorate: {}", e)))?;

        let videoconvert = gst::ElementFactory::make(convert_element_name)
            .name(&convert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
            })?;
        gpu::configure_video_convert(&videoconvert);

        // capsfilter with caps (only constraints specified properties)
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!("VideoFormat block created (chain: videoscale -> {} -> capsfilter) [videorate TEMPORARILY REMOVED]", convert_element_name);

        // Chain: videoscale -> videoconvert/autovideoconvert -> capsfilter (videorate temporarily removed)
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
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "framerate".to_string(),
                label: "Framerate".to_string(),
                description: "Framerate in fps - creates videorate element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_framerate_enum_values(true),
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "framerate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "format".to_string(),
                label: "Pixel Format".to_string(),
                description: "Pixel format/color space - creates videoconvert element. Leave empty to pass through.".to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_pixel_format_enum_values(true),
                },
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "format".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                label: None,
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "videoscale".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![ExternalPad {
                label: None,
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

// macOS only: thread counts are only raised there, and the converter under
// the letterbox test is only stromvimageconvert there.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::blocks::BlockBuildContext;
    use gst::prelude::*;

    fn build(pairs: &[(&str, &str)]) -> BlockBuildResult {
        let _ = gst::init();
        // video_convert_mode() panics until this has run.
        crate::gpu::detect_gpu_capabilities();
        let props: HashMap<String, PropertyValue> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), PropertyValue::String(v.to_string())))
            .collect();
        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        VideoFormatBuilder
            .build("vf0", &props, &ctx)
            .expect("VideoFormat block must build")
    }

    /// Both the scaling and the converting element must get the configured
    /// thread count, or a resize to or from 4K runs on one core.
    #[test]
    fn scale_and_convert_are_threaded() {
        let result = build(&[("resolution", "1280x720"), ("format", "I420")]);

        for (id, element) in &result.elements {
            if !element.has_property("n-threads") {
                continue;
            }
            assert_eq!(
                element.property::<u32>("n-threads"),
                crate::gpu::video_convert_threads(),
                "configure_video_convert did not reach '{}'",
                id
            );
        }
    }

    /// A resize with a format change must still letterbox. The converter
    /// after `videoscale` has no borders to add, so if it ever offers to
    /// resize, `videoscale` negotiates passthrough and a portrait source comes
    /// out stretched to the target shape.
    #[test]
    fn a_portrait_source_is_letterboxed_when_the_format_also_changes() {
        use gstreamer_app as gst_app;
        use gstreamer_video::prelude::*;
        use gstreamer_video::VideoInfo;

        for format in ["NV12", "I420"] {
            let result = build(&[("resolution", "1280x720"), ("format", format)]);
            let elements: HashMap<String, gst::Element> = result.elements.into_iter().collect();

            let pipeline = gst::Pipeline::new();
            let in_info = VideoInfo::builder(gstreamer_video::VideoFormat::Rgba, 1080, 1920)
                .fps(gst::Fraction::new(30, 1))
                .build()
                .expect("input info");
            let src = gst_app::AppSrc::builder()
                .caps(&in_info.to_caps().expect("input caps"))
                .format(gst::Format::Time)
                .build();
            let sink = gst_app::AppSink::builder().sync(false).build();
            pipeline
                .add_many([src.upcast_ref::<gst::Element>(), sink.upcast_ref()])
                .expect("add endpoints");
            for element in elements.values() {
                pipeline.add(element).expect("add block element");
            }
            for (from, to) in &result.internal_links {
                elements[&from.element_id]
                    .link_pads(
                        from.pad_name.as_deref(),
                        &elements[&to.element_id],
                        to.pad_name.as_deref(),
                    )
                    .expect("internal link");
            }
            src.link(&elements["vf0:videoscale"]).expect("link input");
            elements["vf0:capsfilter"].link(&sink).expect("link output");

            pipeline.set_state(gst::State::Playing).expect("play");
            let mut white = gst::Buffer::with_size(in_info.size()).expect("allocate");
            white
                .get_mut()
                .unwrap()
                .map_writable()
                .expect("map")
                .as_mut_slice()
                .fill(255);
            src.push_buffer(white).expect("push");
            let sample = sink
                .try_pull_sample(gst::ClockTime::from_seconds(10))
                .expect("the block produced no output");
            let out_info = VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
            let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
                sample.buffer().expect("buffer"),
                &out_info,
            )
            .expect("map output");
            let luma = frame.plane_data(0).expect("luma plane");
            let row = out_info.height() as usize / 2 * frame.plane_stride()[0] as usize;
            let border = (0..out_info.width() as usize)
                .filter(|&x| luma[row + x] < 32)
                .count();
            pipeline.set_state(gst::State::Null).expect("stop");

            assert_eq!((out_info.width(), out_info.height()), (1280, 720));
            // 1080x1920 fitted into 720 lines is 405 pixels wide, leaving 875
            // of the 1280 columns as border.
            assert!(
                (870..=880).contains(&border),
                "format={format}: {border} border pixels in the middle row, expected ~875; \
                 the portrait source was stretched instead of letterboxed"
            );
        }
    }
}
