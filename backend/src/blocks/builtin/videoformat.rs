//! Video format block with optional video property conversion.
//!
//! This block provides a simple way to set common video properties:
//! - Resolution (width/height) - enforced by caps
//! - Framerate - enforced by caps (NOTE: videorate temporarily removed, framerate not enforced)
//! - Color format (pixel format) - enforced by caps
//!
//! All properties are optional. The block creates a fixed chain of elements:
//! videoconvertscale -> capsfilter
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

        // `videoconvertscale` converts and scales in a single walk of the frame.
        //
        // The element ID is "videoscale": the block's external input pad
        // resolves through that name (see `external_pads` below), and saved
        // flows resolve their links through it at build time, so renaming it
        // breaks every saved flow that links into this block. The ID names the
        // role, not the factory.
        let convert_id = format!("{}:videoscale", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);

        // videoconvertscale on the CPU; autovideoconvert scales as well as converts.
        let convert_element_name = video_convert_mode().convert_scale_element_name();

        // TEMPORARY: videorate removed to avoid frame duplication issues
        // let videorate = gst::ElementFactory::make("videorate")
        //     .name(&rate_id)
        //     .build()
        //     .map_err(|e| BlockBuildError::ElementCreation(format!("videorate: {}", e)))?;

        let convert_scale = gst::ElementFactory::make(convert_element_name)
            .name(&convert_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
            })?;
        // Raises `n-threads` off the stock default of 1, for both the
        // converting and the scaling half.
        gpu::configure_video_convert(&convert_scale);

        // capsfilter with caps (only constraints specified properties)
        let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
            BlockBuildError::InvalidConfiguration(format!("Invalid caps: {}", caps_str))
        })?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        info!(
            "VideoFormat block created (chain: {} -> capsfilter) [videorate TEMPORARILY REMOVED]",
            convert_element_name
        );

        // Chain: videoconvertscale/autovideoconvert -> capsfilter (videorate temporarily removed)
        let internal_links = vec![(
            ElementPadRef::pad(&convert_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        )];

        Ok(BlockBuildResult {
            elements: vec![(convert_id, convert_scale), (capsfilter_id, capsfilter)],
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
                description: "Video resolution - applied by the videoconvertscale element. Leave empty to pass through.".to_string(),
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
                description: "Pixel format/color space - applied by the videoconvertscale element. Leave empty to pass through.".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::BlockBuildContext;
    use gst::prelude::*;

    fn init_gst() {
        let _ = gst::init();
        // video_convert_mode() panics until this has run.
        crate::gpu::detect_gpu_capabilities();
    }

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), PropertyValue::String(v.to_string())))
            .collect()
    }

    fn build(pairs: &[(&str, &str)]) -> BlockBuildResult {
        init_gst();
        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        VideoFormatBuilder
            .build("vf0", &props(pairs), &ctx)
            .expect("VideoFormat block must build")
    }

    /// Guards external pad stability. Saved flows store links as
    /// `block_id:external_pad_name` and resolve them through the block
    /// definition's `internal_element_id` at build time, so a definition that
    /// names an element the builder does not produce silently breaks every
    /// saved flow linking into this block.
    ///
    /// Renaming the conversion element without updating `external_pads` (or
    /// vice versa) fails here.
    #[test]
    fn external_pads_resolve_to_built_elements() {
        let result = build(&[("resolution", "1280x720"), ("format", "I420")]);
        let built: Vec<&str> = result.elements.iter().map(|(id, _)| id.as_str()).collect();

        let definition = videoformat_definition();
        let pads = definition
            .external_pads
            .inputs
            .iter()
            .chain(definition.external_pads.outputs.iter());

        for pad in pads {
            let resolved = format!("vf0:{}", pad.internal_element_id);
            assert!(
                built.contains(&resolved.as_str()),
                "external pad '{}' resolves to element '{}', which the builder does not create. \
                 Built elements: {:?}",
                pad.name,
                resolved,
                built
            );
        }
    }

    /// The block builds exactly one conversion element plus the capsfilter.
    /// Splitting the conversion back into separate scale and convert elements
    /// fails this test.
    #[test]
    fn builds_one_conversion_element_not_two() {
        let result = build(&[("resolution", "1280x720"), ("format", "I420")]);

        assert_eq!(
            result.elements.len(),
            2,
            "expected one conversion element plus the capsfilter, got {:?}",
            result
                .elements
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            result.internal_links.len(),
            1,
            "a two-element chain has exactly one internal link"
        );

        let factory = result.elements[0]
            .1
            .factory()
            .expect("built element has a factory")
            .name()
            .to_string();
        assert!(
            factory == "videoconvertscale" || factory == "autovideoconvert",
            "conversion element must convert and scale in one pass, got '{}'",
            factory
        );
    }

    /// The single element must actually do both jobs: reject 1080p RGBA in,
    /// 720p I420 out unless one element performed both the scale and the
    /// colorspace conversion.
    #[test]
    fn one_element_both_scales_and_converts() {
        let result = build(&[("resolution", "1280x720"), ("format", "I420")]);

        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 2i32)
            .property("is-live", false)
            .build()
            .expect("videotestsrc is in gst-plugins-base");
        let in_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                "video/x-raw,width=1920,height=1080,format=RGBA,framerate=30/1"
                    .parse::<gst::Caps>()
                    .unwrap(),
            )
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .unwrap();

        let convert = &result.elements[0].1;
        let capsfilter = &result.elements[1].1;
        pipeline
            .add_many([&src, &in_caps, convert, capsfilter, &sink])
            .unwrap();
        gst::Element::link_many([&src, &in_caps, convert, capsfilter, &sink])
            .expect("the block's single element must link 1080p RGBA to 720p I420");

        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let msg = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );

        let out_caps = capsfilter
            .static_pad("src")
            .unwrap()
            .current_caps()
            .expect("output caps must be negotiated");
        pipeline.set_state(gst::State::Null).unwrap();

        match msg {
            Some(m) if m.type_() == gst::MessageType::Error => {
                panic!("pipeline error: {:?}", m)
            }
            None => panic!("pipeline did not reach EOS"),
            _ => {}
        }

        let s = out_caps.structure(0).unwrap();
        assert_eq!(s.get::<i32>("width").unwrap(), 1280);
        assert_eq!(s.get::<i32>("height").unwrap(), 720);
        assert_eq!(s.get::<String>("format").unwrap(), "I420");
    }

    /// The block leaves `method` unset and takes the element default. If the
    /// two elements' defaults diverge, scaled output changes and the block
    /// must pin `method` explicitly.
    #[test]
    fn scaling_method_default_matches_videoscale() {
        init_gst();

        let scale = gst::ElementFactory::make("videoscale").build().unwrap();
        let convert_scale = gst::ElementFactory::make("videoconvertscale")
            .build()
            .unwrap();

        let scale_method = scale.property_value("method");
        let convert_scale_method = convert_scale.property_value("method");
        assert_eq!(
            format!("{:?}", scale_method),
            format!("{:?}", convert_scale_method),
            "videoscale and videoconvertscale disagree on the default scaling method; \
             the block must pin `method` or the output change must be called out"
        );
    }

    /// `configure_video_convert` must reach the conversion element, so that
    /// both the converting and the scaling half are threaded.
    #[cfg(target_os = "macos")]
    #[test]
    fn conversion_element_is_threaded() {
        let result = build(&[("resolution", "1280x720"), ("format", "I420")]);
        let convert = &result.elements[0].1;

        assert!(
            convert.has_property("n-threads"),
            "the merged element must expose n-threads for configure_video_convert to reach"
        );
        assert_eq!(
            convert.property::<u32>("n-threads"),
            crate::gpu::video_convert_threads(),
            "configure_video_convert did not reach the merged element"
        );
        assert!(
            convert.property::<u32>("n-threads") > 1,
            "n-threads left at the stock default of 1"
        );
    }
}
