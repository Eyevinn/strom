//! Blackmagic DeckLink SDI/HDMI capture and playback block builders.
//!
//! Provides two combined blocks — one input, one output — both selectable
//! between `audio_video`, `video`, or `audio` via the `stream_mode` property:
//!
//! - **DeckLink Input** — always creates `decklinkvideosrc` because the plugin
//!   requires it for `decklinkaudiosrc` to operate on the same device. In
//!   `audio` mode the video src is drained internally to a `fakesink`.
//! - **DeckLink Output** — always creates `decklinkvideosink` for the mirrored
//!   constraint on the playback side. In `audio` mode the videosink is fed
//!   internally by a `videotestsrc` (black, 1920x1080@25 UYVY) so it can reach
//!   PLAYING.
//!
//! No format conversion elements are inserted on either side — peer blocks must
//! accept (input) or deliver (output) the card's native pixel/audio formats.
//!
//! Uses GStreamer's DeckLink plugin (gst-plugins-bad) for hardware integration.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use std::collections::HashMap;
use strom_types::{
    block::{StreamMode, *},
    element::ElementPadRef,
    EnumValue, MediaType, PropertyValue,
};
use tracing::info;

/// DeckLink Input block builder.
///
/// Combined video + audio capture. The `stream_mode` property controls which pads
/// are exposed (`audio_video`, `video`, `audio`). `decklinkvideosrc` is always
/// created internally — required by the DeckLink GStreamer plugin even when only
/// audio is consumed externally.
pub struct DeckLinkInputBuilder;

impl BlockBuilder for DeckLinkInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

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
                internal_element_id: "decklinkvideosrc".to_string(),
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
                internal_element_id: "decklinkaudiosrc".to_string(),
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
        info!("Building DeckLink Input block: {}", instance_id);

        let stream_mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        let video_mode = properties
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
            .unwrap_or("auto");

        let video_format = properties
            .get("video_format")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("auto");

        let drop_no_signal_frames = properties
            .get("drop_no_signal_frames")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let profile = properties
            .get("profile")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("default");

        // -1 means "use device-number instead". Higher priority than device-number
        // when set to a real persistent ID.
        let persistent_id = properties
            .get("persistent_id")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i),
                PropertyValue::UInt(u) => Some(*u as i64),
                _ => None,
            })
            .unwrap_or(-1);

        // do-timestamp = true makes the source apply the pipeline clock to each
        // buffer's PTS. Required for absolute timestamping setups (e.g. TAI
        // pipeline clock + EFP/SRT 64-bit absolute timestamps).
        let do_timestamp = properties
            .get("do_timestamp")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let videosrc_id = format!("{}:decklinkvideosrc", instance_id);
        let videosrc = gst::ElementFactory::make("decklinkvideosrc")
            .name(&videosrc_id)
            .property("device-number", device_number)
            .property("persistent-id", persistent_id)
            .property_from_str("mode", video_mode)
            .property_from_str("connection", connection)
            .property_from_str("video-format", video_format)
            .property_from_str("profile", profile)
            .property("drop-no-signal-frames", drop_no_signal_frames)
            .property("do-timestamp", do_timestamp)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosrc: {}", e)))?;

        let mut elements: Vec<(String, gst::Element)> = vec![(videosrc_id.clone(), videosrc)];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        // Audio-only: the video src must still run, drain it to a fakesink so the
        // pipeline can reach PLAYING. Without this, decklinkaudiosrc fails state
        // change with "Audio src needs a video src for its operation".
        if !stream_mode.has_video() {
            let drain_id = format!("{}:video_drain", instance_id);
            let drain = gst::ElementFactory::make("fakesink")
                .name(&drain_id)
                .property("sync", false)
                .property("async", false)
                .property("silent", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("fakesink (video_drain): {}", e))
                })?;
            internal_links.push((
                ElementPadRef::pad(&videosrc_id, "src"),
                ElementPadRef::pad(&drain_id, "sink"),
            ));
            elements.push((drain_id, drain));
        }

        if stream_mode.has_audio() {
            let audio_connection = properties
                .get("audio_connection")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("auto");

            let audio_channels = properties
                .get("audio_channels")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.clone()),
                    PropertyValue::UInt(u) => Some(u.to_string()),
                    PropertyValue::Int(i) if *i > 0 => Some(i.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "2".to_string());

            let audiosrc_id = format!("{}:decklinkaudiosrc", instance_id);
            let audiosrc = gst::ElementFactory::make("decklinkaudiosrc")
                .name(&audiosrc_id)
                .property("device-number", device_number)
                .property("persistent-id", persistent_id)
                .property_from_str("connection", audio_connection)
                .property_from_str("channels", &audio_channels)
                .property("do-timestamp", do_timestamp)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("decklinkaudiosrc: {}", e))
                })?;
            elements.push((audiosrc_id, audiosrc));

            info!(
                "DeckLink Input audio: connection={}, channels={}",
                audio_connection, audio_channels
            );
        }

        info!(
            "DeckLink Input configured: device={}, persistent-id={}, profile={}, stream_mode={}, mode={}, connection={}, video-format={}, drop-no-signal-frames={}, do-timestamp={}",
            device_number, persistent_id, profile, stream_mode.as_str(), video_mode, connection, video_format, drop_no_signal_frames, do_timestamp
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// DeckLink Output block builder.
///
/// Combined video + audio playback. The `stream_mode` property controls which pads
/// are exposed (`audio_video`, `video`, `audio`). `decklinkvideosink` is always
/// created internally — required by the DeckLink GStreamer plugin even when only
/// audio is sent externally. In audio-only mode an internal `videotestsrc` (black,
/// 1920x1080@25 UYVY) drives the videosink so the pipeline can reach PLAYING.
pub struct DeckLinkOutputBuilder;

/// Internal video mode used to drive `decklinkvideosink` when only audio is exposed.
/// Lowest broadcast-quality progressive mode supported by virtually all DeckLink
/// cards; the actual SDI output is irrelevant in this mode (just black frames).
const AUDIO_ONLY_INTERNAL_MODE: &str = "1080p25";
const AUDIO_ONLY_INTERNAL_FORMAT: &str = "8bit-yuv";

impl BlockBuilder for DeckLinkOutputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let mut inputs = Vec::new();
        if mode.has_video() {
            inputs.push(ExternalPad {
                label: if mode.has_audio() {
                    Some("V".to_string())
                } else {
                    None
                },
                name: "video_in".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "decklinkvideosink".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }
        if mode.has_audio() {
            inputs.push(ExternalPad {
                label: if mode.has_video() {
                    Some("A".to_string())
                } else {
                    None
                },
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "decklinkaudiosink".to_string(),
                internal_pad_name: "sink".to_string(),
            });
        }

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
        info!("Building DeckLink Output block: {}", instance_id);

        let stream_mode = properties
            .get("stream_mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(StreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or_default();

        let device_number = properties
            .get("device_number")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) if *i >= 0 => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(0);

        // In audio-only mode the videosink runs in a fixed internal mode regardless
        // of the user's `mode`/`video_format` settings — those only apply when the
        // video pad is actually wired up.
        let (video_mode, video_format) = if stream_mode.has_video() {
            let m = properties
                .get("mode")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("1080p25");
            let f = properties
                .get("video_format")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("8bit-yuv");
            (m, f)
        } else {
            (AUDIO_ONLY_INTERNAL_MODE, AUDIO_ONLY_INTERNAL_FORMAT)
        };

        let videosink_id = format!("{}:decklinkvideosink", instance_id);
        let videosink = gst::ElementFactory::make("decklinkvideosink")
            .name(&videosink_id)
            .property("device-number", device_number)
            .property_from_str("mode", video_mode)
            .property_from_str("video-format", video_format)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosink: {}", e)))?;

        let mut elements: Vec<(String, gst::Element)> = vec![(videosink_id.clone(), videosink)];
        let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

        // Audio-only: feed videosink with black frames so it can reach PLAYING.
        // (decklinkaudiosink requires decklinkvideosink to be running on the same device.)
        if !stream_mode.has_video() {
            let testsrc_id = format!("{}:video_filler", instance_id);
            let capsfilter_id = format!("{}:video_filler_caps", instance_id);

            let testsrc = gst::ElementFactory::make("videotestsrc")
                .name(&testsrc_id)
                .property_from_str("pattern", "black")
                .property("is-live", true)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("videotestsrc (filler): {}", e))
                })?;

            // Caps must match the videosink's hardcoded internal mode.
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "UYVY")
                .field("width", 1920i32)
                .field("height", 1080i32)
                .field("framerate", gst::Fraction::new(25, 1))
                .field("interlace-mode", "progressive")
                .build();
            let capsfilter = gst::ElementFactory::make("capsfilter")
                .name(&capsfilter_id)
                .property("caps", &caps)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("capsfilter (filler): {}", e))
                })?;

            internal_links.push((
                ElementPadRef::pad(&testsrc_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&capsfilter_id, "src"),
                ElementPadRef::pad(&videosink_id, "sink"),
            ));
            elements.push((testsrc_id, testsrc));
            elements.push((capsfilter_id, capsfilter));
        }

        if stream_mode.has_audio() {
            let audiosink_id = format!("{}:decklinkaudiosink", instance_id);
            let audiosink = gst::ElementFactory::make("decklinkaudiosink")
                .name(&audiosink_id)
                .property("device-number", device_number)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("decklinkaudiosink: {}", e))
                })?;
            elements.push((audiosink_id, audiosink));
        }

        info!(
            "DeckLink Output configured: device={}, stream_mode={}, mode={}, video-format={}",
            device_number,
            stream_mode.as_str(),
            video_mode,
            video_format
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for DeckLink blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![decklink_input_definition(), decklink_output_definition()]
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

/// Get DeckLink Input block definition (combined video + audio).
fn decklink_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_input".to_string(),
        name: "DeckLink Input".to_string(),
        description: "Captures video and/or audio from a Blackmagic DeckLink SDI/HDMI card. Outputs the card's native pixel and audio formats — no internal conversion. Use a downstream videoformat/audioformat block to convert if needed.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which media tracks to expose: video only, audio only, or both. (decklinkvideosrc is always created internally — required by the DeckLink plugin even in audio-only mode.)".to_string(),
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
                live: false,
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
                live: false,
            },
            ExposedProperty {
                name: "connection".to_string(),
                label: "Video Connection".to_string(),
                description: "Video input connection type".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "auto".to_string(),
                            label: Some("Auto".to_string()),
                        },
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
                            value: "svideo".to_string(),
                            label: Some("S-Video".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "connection".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "video_format".to_string(),
                label: "Video Format".to_string(),
                description: "Pixel format the card delivers. Native formats only — no conversion is performed. 'auto' lets the plugin pick based on mode (typically 8bit-yuv); 10bit-yuv (v210) is the broadcast-grade choice.".to_string(),
                property_type: PropertyType::Enum {
                    values: decklink_video_format_enum_values(),
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_format".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "drop_no_signal_frames".to_string(),
                label: "Drop No-Signal Frames".to_string(),
                description: "Drop frames the card flags as having no input signal instead of forwarding black/test-pattern downstream. Recommended for ingest so the encoder doesn't ship filler over SRT when the source is unplugged.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "drop_no_signal_frames".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "profile".to_string(),
                label: "Sub-Device Profile".to_string(),
                description: "Sub-device profile for cards that support multiple (Quad 2, Duo 2, 8K Pro). 'default' keeps whatever profile is configured in Desktop Video Setup. Setting a profile from here lets the flow control sub-device layout without requiring a manual GUI change on the host.".to_string(),
                property_type: PropertyType::Enum {
                    values: decklink_profile_enum_values(),
                },
                default_value: Some(PropertyValue::String("default".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "profile".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "persistent_id".to_string(),
                label: "Persistent ID".to_string(),
                description: "DeckLink persistent device ID — stable across reboots, profile changes, and PCIe re-enumeration. Higher priority than 'Device Number' when set to a non-default value. Use -1 (default) to fall back to device-number selection.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(-1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "persistent_id".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "do_timestamp".to_string(),
                label: "Apply Pipeline Clock to Buffers".to_string(),
                description: "When enabled, the source applies the pipeline clock to each buffer's PTS at capture. Required for absolute-timestamp setups (e.g. TAI pipeline clock + EFP/SRT 64-bit absolute timestamps). Default off keeps GStreamer's default behaviour where the source's own stream-time is used.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "do_timestamp".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "audio_connection".to_string(),
                label: "Audio Connection".to_string(),
                description: "Audio input connection type. Only used when audio is exposed.".to_string(),
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
                            value: "aes".to_string(),
                            label: Some("AES/EBU".to_string()),
                        },
                        EnumValue {
                            value: "analog".to_string(),
                            label: Some("Analog".to_string()),
                        },
                        EnumValue {
                            value: "analog-xlr".to_string(),
                            label: Some("Analog (XLR)".to_string()),
                        },
                        EnumValue {
                            value: "analog-rca".to_string(),
                            label: Some("Analog (RCA)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_connection".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "audio_channels".to_string(),
                label: "Audio Channels".to_string(),
                description: "Number of audio channels to capture (48 kHz, S16LE/S32LE — chosen by downstream caps negotiation).".to_string(),
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
                    property_name: "audio_channels".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        // Static fallback — matches the default `audio_video` mode. The actual
        // pads are computed dynamically in `DeckLinkInputBuilder::get_external_pads`
        // based on the `stream_mode` property.
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "decklinkvideosrc".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "decklinkaudiosrc".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
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

/// Get DeckLink Output block definition (combined video + audio).
fn decklink_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.decklink_output".to_string(),
        name: "DeckLink Output".to_string(),
        description: "Outputs video and/or audio to a Blackmagic DeckLink SDI/HDMI card. Accepts the card's native pixel and audio formats only — no internal conversion. In audio-only mode the videosink is internally driven with black frames so it can reach PLAYING.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "stream_mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "Which media tracks to expose: video only, audio only, or both. (decklinkvideosink is always created internally — required by the DeckLink plugin even in audio-only mode.)".to_string(),
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
                live: false,
            },
            ExposedProperty {
                name: "mode".to_string(),
                label: "Video Mode".to_string(),
                description: "Output video mode (resolution and framerate). Ignored in audio-only mode.".to_string(),
                property_type: PropertyType::Enum {
                    values: video_mode_enum_values(),
                },
                default_value: Some(PropertyValue::String("1080p25".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "video_format".to_string(),
                label: "Video Format".to_string(),
                description: "Pixel format the card expects. Native formats only — no conversion is performed; upstream must deliver this exact format. Ignored in audio-only mode.".to_string(),
                property_type: PropertyType::Enum {
                    values: decklink_video_format_enum_values(),
                },
                default_value: Some(PropertyValue::String("8bit-yuv".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_format".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        // Static fallback — matches the default `audio_video` mode. Actual pads
        // are computed dynamically in `DeckLinkOutputBuilder::get_external_pads`.
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "decklinkvideosink".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "decklinkaudiosink".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
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
