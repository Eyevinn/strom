//! Local Input block — cross-platform local video/audio source capture via
//! the OS's native media APIs.
//!
//! Wraps platform-appropriate capture sources (v4l2src/ksvideosrc/mfvideosrc/
//! avfvideosrc for video; pulsesrc/wasapisrc/osxaudiosrc for audio) behind a
//! single block. Works for anything the OS exposes as a `Video/Source` or
//! `Audio/Source` GStreamer device — built-in cameras, USB capture cards,
//! HDMI/SDI grabbers, professional audio interfaces, virtual sources from
//! other software, etc. When a specific `video_device`/`audio_device` is
//! selected (via `/api/discovery/devices?category=video_source|audio_source`),
//! the source element is created from the corresponding `GstDevice` using
//! `Device::create_element()` — that bakes in the device-path/identifier
//! property the platform plugin needs. When no device id is set the block
//! falls back to `autovideosrc`/`autoaudiosrc`, which picks the OS default.
//!
//! All captured streams are normalised through `videoconvert` + a
//! `video/x-raw` capsfilter (and `audioconvert` + `audioresample` + an
//! `audio/x-raw` capsfilter) so downstream blocks see a stable raw format
//! regardless of what the source actually delivers.
//!
//! `stream_mode` chooses which pads are exposed (`audio_video`, `video`,
//! `audio`) the same way the DeckLink and WHEP blocks do.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::video_convert_mode;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::{StreamMode, *},
    element::ElementPadRef,
    MediaType, PropertyValue,
};
use tracing::info;

/// Local Input block builder.
pub struct LocalInputBuilder;

impl BlockBuilder for LocalInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = read_stream_mode(properties);

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
                internal_element_id: "videocapsfilter".to_string(),
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
                internal_element_id: "audiocapsfilter".to_string(),
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
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Local Input block: {}", instance_id);

        let stream_mode = read_stream_mode(properties);

        let mut elements: Vec<(String, gst::Element)> = Vec::new();
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        if stream_mode.has_video() {
            let video_device_id = read_string(properties, "video_device").unwrap_or_default();
            let videosrc_id = format!("{}:videosrc", instance_id);
            let videoconvert_id = format!("{}:videoconvert", instance_id);
            let videocaps_id = format!("{}:videocapsfilter", instance_id);

            let videosrc = make_source(
                ctx,
                MediaType::Video,
                &video_device_id,
                &videosrc_id,
                "autovideosrc",
            )?;

            let convert_element_name = video_convert_mode().element_name();
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
                .map_err(|e| BlockBuildError::ElementCreation(format!("videocapsfilter: {}", e)))?;

            internal_links.push((
                ElementPadRef::pad(&videosrc_id, "src"),
                ElementPadRef::pad(&videoconvert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&videoconvert_id, "src"),
                ElementPadRef::pad(&videocaps_id, "sink"),
            ));

            elements.push((videosrc_id, videosrc));
            elements.push((videoconvert_id, videoconvert));
            elements.push((videocaps_id, videocaps));
        }

        if stream_mode.has_audio() {
            let audio_device_id = read_string(properties, "audio_device").unwrap_or_default();
            let audiosrc_id = format!("{}:audiosrc", instance_id);
            let audioconvert_id = format!("{}:audioconvert", instance_id);
            let audioresample_id = format!("{}:audioresample", instance_id);
            let audiocaps_id = format!("{}:audiocapsfilter", instance_id);

            let audiosrc = make_source(
                ctx,
                MediaType::Audio,
                &audio_device_id,
                &audiosrc_id,
                "autoaudiosrc",
            )?;

            let audioconvert = gst::ElementFactory::make("audioconvert")
                .name(&audioconvert_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

            let audioresample = gst::ElementFactory::make("audioresample")
                .name(&audioresample_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

            let audio_caps = gst::Caps::builder("audio/x-raw").build();
            let audiocaps = gst::ElementFactory::make("capsfilter")
                .name(&audiocaps_id)
                .property("caps", &audio_caps)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audiocapsfilter: {}", e)))?;

            internal_links.push((
                ElementPadRef::pad(&audiosrc_id, "src"),
                ElementPadRef::pad(&audioconvert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&audioconvert_id, "src"),
                ElementPadRef::pad(&audioresample_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&audioresample_id, "src"),
                ElementPadRef::pad(&audiocaps_id, "sink"),
            ));

            elements.push((audiosrc_id, audiosrc));
            elements.push((audioconvert_id, audioconvert));
            elements.push((audioresample_id, audioresample));
            elements.push((audiocaps_id, audiocaps));
        }

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Build a source element for the given media type.
///
/// If `device_id` is non-empty, look up the live `gst::Device` from the
/// long-running `DeviceDiscovery` (shared via `BlockBuildContext`) and call
/// `Device::create_element()` — that bakes in the platform-specific
/// device-path property automatically.
///
/// If `device_id` is empty, create `fallback_element_name` (typically
/// `autovideosrc`/`autoaudiosrc`) so the block still works without an
/// explicit selection.
///
/// **Why not spin up a transient `gst::DeviceMonitor` here?** That was the
/// original implementation, and it crashed inside `gst_device_provider_stop`
/// on macOS (SIGSEGV with pointer-authentication failure) when the AVFoundation
/// / CoreAudio providers were torn down right after enumeration. Reusing the
/// app-lifetime monitor sidesteps the buggy stop path entirely.
fn make_source(
    ctx: &BlockBuildContext,
    media: MediaType,
    device_id: &str,
    element_id: &str,
    fallback_element_name: &str,
) -> Result<gst::Element, BlockBuildError> {
    if device_id.is_empty() {
        return gst::ElementFactory::make(fallback_element_name)
            .name(element_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", fallback_element_name, e))
            });
    }

    let device_kind = match media {
        MediaType::Video => "Video/Source",
        MediaType::Audio => "Audio/Source",
        _ => {
            return Err(BlockBuildError::InvalidConfiguration(format!(
                "Unsupported media type for Local Input source: {:?}",
                media
            )));
        }
    };

    let device = ctx.local_device(device_id).ok_or_else(|| {
        BlockBuildError::InvalidConfiguration(format!(
            "{} device '{}' not found — refresh /api/discovery/devices and pick a current id",
            device_kind, device_id
        ))
    })?;

    let element = device.create_element(Some(element_id)).map_err(|e| {
        BlockBuildError::ElementCreation(format!(
            "create_element for {} device '{}' ({}): {}",
            device_kind,
            device.display_name(),
            device_id,
            e
        ))
    })?;

    info!(
        "Local Input: bound {} pad to device '{}' (id={}, factory={})",
        device_kind,
        device.display_name(),
        device_id,
        element
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );

    Ok(element)
}

fn read_stream_mode(properties: &HashMap<String, PropertyValue>) -> StreamMode {
    properties
        .get("stream_mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(StreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or_default()
}

fn read_string(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    properties.get(key).and_then(|v| match v {
        PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    })
}

/// Get metadata for the Local Input block (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![local_input_definition()]
}

fn local_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.local_input".to_string(),
        name: "Local Input".to_string(),
        description: "Captures from local video/audio sources exposed by the OS's native media APIs (v4l2 on Linux, AVFoundation on macOS, Media Foundation/WASAPI on Windows). Works for any GStreamer Video/Source or Audio/Source device — built-in cameras, USB capture cards, HDMI/SDI grabbers, professional audio interfaces, virtual sources, etc. Pick devices from /api/discovery/devices?category=video_source and ?category=audio_source — leave empty to use the OS default (autovideosrc/autoaudiosrc). Output is normalized to raw video/audio via videoconvert + audioconvert/audioresample.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which media tracks to expose: video only, audio only, or both. Audio and video can come from independent devices.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video only".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "stream_mode".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "video_device".to_string(),
                label: "Video Source".to_string(),
                description: "Local Video/Source device to capture from (built-in cameras, USB capture cards, HDMI/SDI grabbers, virtual sources, ...). Picked from the live device list — leave empty to use the OS default (autovideosrc).".to_string(),
                property_type: PropertyType::VideoDevice,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_device".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "audio_device".to_string(),
                label: "Audio Source".to_string(),
                description: "Local Audio/Source device to capture from (built-in inputs, professional audio interfaces, USB grabbers, virtual sources, ...). Picked from the live device list — leave empty to use the OS default (autoaudiosrc).".to_string(),
                property_type: PropertyType::AudioDevice,
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_device".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audiocapsfilter".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎥".to_string()),
            width: Some(2.0),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}
