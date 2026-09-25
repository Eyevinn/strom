//! HTML input block — a web page as a video (and audio) source.
//!
//! Chromium renders the page offscreen through `cefsrc`, which hands the
//! pipeline BGRA frames with the page's audio riding along as buffer metadata.
//! `cefdemux` splits that audio back out, so it is only built when the block
//! actually exposes an audio pad.
//!
//! ```text
//! cefsrc -> capsfilter -> cefdemux -> video -> videoconvert -> video_output -> [video_out]
//!                                  -> audio -> audioconvert -> audioresample -> audio_output -> [audio_out]
//! ```
//!
//! Video only skips `cefdemux` entirely:
//!
//! ```text
//! cefsrc -> capsfilter -> videoconvert -> video_output -> [video_out]
//! ```
//!
//! The capsfilter is what decides the render size: `cefsrc` sizes its browser
//! from the caps negotiated downstream, so width, height and framerate are
//! properties of the block rather than of anything later in the graph.
//!
//! # Why this exists as a block
//!
//! HTML sources have been raw `cefsrc`/`cefdemux` elements in imported
//! pipelines. That works, but it leaves the operator setting caps by hand, and
//! it leaves Strom with no name for "this HTML source" — which is what the
//! DevTools endpoint needs in order to hand out a link for one page rather
//! than for every page in the instance.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::StreamMode, block::*, element::ElementPadRef, PropertyValue, *};
use tracing::info;

/// The page shown by a block nobody has configured yet.
const DEFAULT_URL: &str = "https://example.com";
const DEFAULT_WIDTH: u64 = 1920;
const DEFAULT_HEIGHT: u64 = 1080;
const DEFAULT_FRAMERATE: u64 = 30;

/// This block's definition id.
pub const BLOCK_ID: &str = "builtin.html_input";

/// The property that decides whether a link may be minted for this block.
///
/// It maps to `_block` because nothing in the pipeline reads it: the API reads
/// it from the stored block when an operator asks for a link. Storing it is
/// therefore the whole write, which is why it can change on a running flow.
pub const REMOTE_CONTROL_PROPERTY: &str = "remote_control";

/// HTML input block builder.
pub struct HtmlInputBuilder;

fn stream_mode(properties: &HashMap<String, PropertyValue>) -> StreamMode {
    properties
        .get("stream_mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(StreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or(StreamMode::Video)
}

fn uint_property(properties: &HashMap<String, PropertyValue>, name: &str, fallback: u64) -> u64 {
    properties
        .get(name)
        .and_then(|v| match v {
            PropertyValue::UInt(u) => Some(*u),
            PropertyValue::Int(i) if *i > 0 => Some(*i as u64),
            _ => None,
        })
        .filter(|n| *n > 0)
        .unwrap_or(fallback)
}

impl BlockBuilder for HtmlInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = stream_mode(properties);
        let mut outputs = Vec::new();

        if mode.has_video() {
            outputs.push(ExternalPad {
                label: if mode.has_audio() {
                    Some("V".to_string())
                } else {
                    None
                },
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_output".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }
        if mode.has_audio() {
            outputs.push(ExternalPad {
                label: if mode.has_video() {
                    Some("A".to_string())
                } else {
                    None
                },
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audio_output".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }

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
        let mode = stream_mode(properties);
        let url = properties
            .get("url")
            .and_then(|v| match v {
                PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        let width = uint_property(properties, "width", DEFAULT_WIDTH);
        let height = uint_property(properties, "height", DEFAULT_HEIGHT);
        let framerate = uint_property(properties, "framerate", DEFAULT_FRAMERATE);

        info!(
            "Building HTML Input block instance: {} ({}x{}@{} mode={})",
            instance_id,
            width,
            height,
            framerate,
            mode.as_str()
        );

        let make = |factory: &str| -> Result<gst::Element, BlockBuildError> {
            gst::ElementFactory::make(factory).build().map_err(|e| {
                // cefsrc and cefdemux are the two that are realistically
                // absent: they come from gstcefsrc, which only the strom-full
                // image carries. Say so rather than leaving the operator with
                // a bare element-creation failure.
                if matches!(factory, "cefsrc" | "cefdemux") {
                    BlockBuildError::MissingPlugin(format!(
                        "HTML sources need the gstcefsrc plugin for `{}`, which ships in the \
                         strom-full image ({})",
                        factory, e
                    ))
                } else {
                    BlockBuildError::ElementCreation(format!(
                        "Failed to create {} for the HTML input block: {}",
                        factory, e
                    ))
                }
            })
        };

        let cefsrc = make("cefsrc")?;
        cefsrc.set_property("url", &url);

        // cefsrc renders at whatever size is negotiated downstream, so this
        // capsfilter is the page's viewport. BGRA is what cefsrc produces and
        // what cefdemux accepts; converting happens after the split.
        let capsfilter = make("capsfilter")?;
        capsfilter.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "BGRA")
                .field("width", width as i32)
                .field("height", height as i32)
                .field("framerate", gst::Fraction::new(framerate as i32, 1))
                .build(),
        );

        let mut elements: Vec<(String, gst::Element)> = vec![
            (format!("{}:cefsrc", instance_id), cefsrc),
            (format!("{}:capsfilter", instance_id), capsfilter),
        ];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = vec![(
            ElementPadRef::pad(format!("{}:cefsrc", instance_id), "src"),
            ElementPadRef::pad(format!("{}:capsfilter", instance_id), "sink"),
        )];

        // The audio only exists on the far side of cefdemux, so a video-only
        // block has no reason to build it.
        let video_source = if mode.has_audio() {
            let cefdemux = make("cefdemux")?;
            elements.push((format!("{}:cefdemux", instance_id), cefdemux));
            internal_links.push((
                ElementPadRef::pad(format!("{}:capsfilter", instance_id), "src"),
                ElementPadRef::pad(format!("{}:cefdemux", instance_id), "sink"),
            ));

            let audioconvert = make("audioconvert")?;
            let audioresample = make("audioresample")?;
            let audio_output = make("identity")?;
            elements.push((format!("{}:audioconvert", instance_id), audioconvert));
            elements.push((format!("{}:audioresample", instance_id), audioresample));
            elements.push((format!("{}:audio_output", instance_id), audio_output));
            internal_links.push((
                ElementPadRef::pad(format!("{}:cefdemux", instance_id), "audio"),
                ElementPadRef::pad(format!("{}:audioconvert", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:audioconvert", instance_id), "src"),
                ElementPadRef::pad(format!("{}:audioresample", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:audioresample", instance_id), "src"),
                ElementPadRef::pad(format!("{}:audio_output", instance_id), "sink"),
            ));

            (format!("{}:cefdemux", instance_id), "video")
        } else {
            (format!("{}:capsfilter", instance_id), "src")
        };

        if mode.has_video() {
            let videoconvert = make("videoconvert")?;
            let video_output = make("identity")?;
            elements.push((format!("{}:videoconvert", instance_id), videoconvert));
            elements.push((format!("{}:video_output", instance_id), video_output));
            internal_links.push((
                ElementPadRef::pad(video_source.0.clone(), video_source.1),
                ElementPadRef::pad(format!("{}:videoconvert", instance_id), "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(format!("{}:videoconvert", instance_id), "src"),
                ElementPadRef::pad(format!("{}:video_output", instance_id), "sink"),
            ));
        } else {
            // Audio-only still renders the page — cefdemux needs the video to
            // take the audio out of, so it has to go somewhere.
            let fakesink = make("fakesink")?;
            fakesink.set_property("sync", false);
            fakesink.set_property("async", false);
            elements.push((format!("{}:video_sink", instance_id), fakesink));
            internal_links.push((
                ElementPadRef::pad(video_source.0.clone(), video_source.1),
                ElementPadRef::pad(format!("{}:video_sink", instance_id), "sink"),
            ));
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for HTML input blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![html_input_definition()]
}

fn html_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: BLOCK_ID.to_string(),
        name: "HTML Input".to_string(),
        description: "Renders a web page as a video source through Chromium, with the page's \
                      audio as an optional second output. Needs the gstcefsrc plugin, which \
                      ships in the strom-full image."
            .to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "url".to_string(),
                label: "URL".to_string(),
                description: "Page to render (http://, https://, file:// or data:)".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_URL.to_string())),
                mapping: PropertyMapping {
                    element_id: "cefsrc".to_string(),
                    property_name: "url".to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
            ExposedProperty {
                name: "width".to_string(),
                label: "Width".to_string(),
                description: "Viewport width in pixels. The page is rendered at this size."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_WIDTH)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "width".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "height".to_string(),
                label: "Height".to_string(),
                description: "Viewport height in pixels. The page is rendered at this size."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_HEIGHT)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "height".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "framerate".to_string(),
                label: "Framerate".to_string(),
                description: "Frames per second the page is rendered at. Chromium caps this at 60."
                    .to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_FRAMERATE)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "framerate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which outputs the block exposes. Pages are usually silent, so \
                              video only is the default."
                    .to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video".to_string()),
                        },
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stream_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: REMOTE_CONTROL_PROPERTY.to_string(),
                label: "Remote Control".to_string(),
                description: "Allow an operator to be handed a link that drives this page \
                              remotely - to log in, clear a consent dialog, click a tab. The \
                              instance also needs a CEF debug port configured. A session \
                              opened this way reaches every HTML source in the instance, not \
                              only this one."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: REMOTE_CONTROL_PROPERTY.to_string(),
                    transform: None,
                },
                live: true,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: None,
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "video_output".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, PropertyValue)]) -> HashMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn video_only_is_the_default_shape() {
        let pads = HtmlInputBuilder.get_external_pads(&props(&[])).unwrap();
        assert!(pads.inputs.is_empty());
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "video_out");
        assert_eq!(pads.outputs[0].media_type, MediaType::Video);
        // A lone output needs no label to tell it apart from a sibling.
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn audio_video_exposes_both_pads_labelled() {
        let pads = HtmlInputBuilder
            .get_external_pads(&props(&[(
                "stream_mode",
                PropertyValue::String("audio_video".to_string()),
            )]))
            .unwrap();
        let names: Vec<_> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["video_out", "audio_out"]);
        assert_eq!(pads.outputs[0].label.as_deref(), Some("V"));
        assert_eq!(pads.outputs[1].label.as_deref(), Some("A"));
    }

    #[test]
    fn audio_only_exposes_audio_alone() {
        let pads = HtmlInputBuilder
            .get_external_pads(&props(&[(
                "stream_mode",
                PropertyValue::String("audio".to_string()),
            )]))
            .unwrap();
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "audio_out");
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn sizes_fall_back_rather_than_negotiating_zero() {
        // A zero or negative viewport cannot be negotiated, and a block that
        // fails to start says less than one that renders at the default.
        let p = props(&[
            ("width", PropertyValue::UInt(0)),
            ("height", PropertyValue::Int(-1)),
            ("framerate", PropertyValue::String("thirty".to_string())),
        ]);
        assert_eq!(uint_property(&p, "width", DEFAULT_WIDTH), DEFAULT_WIDTH);
        assert_eq!(uint_property(&p, "height", DEFAULT_HEIGHT), DEFAULT_HEIGHT);
        assert_eq!(
            uint_property(&p, "framerate", DEFAULT_FRAMERATE),
            DEFAULT_FRAMERATE
        );
    }

    #[test]
    fn an_int_viewport_is_accepted_as_written() {
        let p = props(&[("width", PropertyValue::Int(1280))]);
        assert_eq!(uint_property(&p, "width", DEFAULT_WIDTH), 1280);
    }
}
