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
//!
//! # Cross-platform notes (verified on macOS; Linux/Windows untested as of 2026-05)
//!
//! The design is platform-agnostic but there are a few known follow-ups
//! before we can declare parity with macOS:
//!
//! - **MJPEG webcams on Linux v4l2:** the pre-convert capsfilter is
//!   `video/x-raw` only. USB webcams that deliver `image/jpeg` natively
//!   above 720p will fail caps negotiation. Fix is to widen the source caps
//!   to `video/x-raw | image/jpeg` and put a `jpegdec` (or `decodebin`)
//!   between the source and `videoconvert` when JPEG is negotiated.
//!
//! - **Device id stability (best-effort):**
//!   [`crate::discovery::device::DeviceDiscovery::device_id_for`] prefers
//!   `device.path` / `object.path` / `api.v4l2.path` / `device.serial`
//!   before falling back to `display_name`, which covers most desktop
//!   providers (Pulse / WASAPI / v4l2 / PipeWire). On providers that
//!   expose *none* of those keys the id remains derived from
//!   `display_name`, so identical-name duplicates can still collide
//!   there — uncommon in practice but worth knowing.
//!
//! - **`Device::create_element(Some(name))` on Windows:** some MF/KS
//!   providers historically dislike having a name passed at create time.
//!   If issues appear, fall back to `create_element(None)` followed by
//!   `set_property("name", ...)`.
//!
//! - **Bare-ALSA Linux** (no Pulse/PipeWire) won't enumerate audio sources
//!   via the standard `Audio/Source` filter. Niche on desktop; relevant for
//!   embedded / headless servers.
//!
//! # Multichannel audio
//!
//! The block can request any channel count supported by the chosen device
//! via the `audio_channels` property (1/2/4/6/8/16/default). What you
//! actually get depends on how the OS exposes the device:
//!
//! - **macOS CoreAudio:** native multichannel works directly; aggregate
//!   devices created in *Audio MIDI Setup* show up as one device with the
//!   total channel count.
//! - **Linux ALSA / PipeWire:** native multichannel works. Pulse may
//!   downmix to stereo depending on the device profile. Pure JACK servers
//!   (jackd) are *not* enumerated by `GstDeviceMonitor` — the stock
//!   `gst-plugin-good` jack module only registers `jackaudiosrc`/sink
//!   elements, no device provider. Modern Linux setups typically run
//!   PipeWire with `pw-jack` for JACK-style routing, which *does*
//!   register a device provider and thus shows up in the picker.
//! - **Windows WASAPI:** most pro audio drivers expose a multichannel card
//!   as separate stereo *devices* (1/2, 3/4, 5/6, ...). Pick the pair you
//!   want and keep `audio_channels=2`. For true multichannel you need
//!   either WASAPI exclusive mode (`wasapi_exclusive_mode=true`) or a
//!   third-party ASIO GStreamer plugin (`gstasio` / equivalent) — once
//!   loaded, ASIO devices appear automatically in the picker just like
//!   any other `GstDevice`.
//!
//! The design is plugin-agnostic: any provider registered with
//! `GstDeviceMonitor` (PipeWire, third-party ASIO, ...) is picked up
//! without changes here.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::{self, video_convert_mode};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::{
        common_video_framerate_enum_values, common_video_resolution_enum_values,
        parse_resolution_string, StreamMode, *,
    },
    element::ElementPadRef,
    MediaType, PropertyValue,
};
use tracing::{info, warn};

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
            let resolution = read_string(properties, "video_resolution")
                .and_then(|s| parse_resolution_string(&s));
            let framerate =
                read_string(properties, "video_framerate").and_then(|s| parse_fraction_string(&s));

            let videosrc_id = format!("{}:videosrc", instance_id);
            let videosrc_caps_id = format!("{}:videosrc_caps", instance_id);
            let videoconvert_id = format!("{}:videoconvert", instance_id);
            let videocaps_id = format!("{}:videocapsfilter", instance_id);

            let videosrc = make_source(
                ctx,
                MediaType::Video,
                &video_device_id,
                &videosrc_id,
                "autovideosrc",
            )?;

            // Pre-convert capsfilter: lets the source negotiate the requested
            // resolution / framerate at capture-time (AVFoundation, v4l2 and
            // Media Foundation all pick the closest matching mode) rather
            // than always running at the device's default mode and paying a
            // software downscale in videoconvert.
            let mut src_caps_builder = gst::Caps::builder("video/x-raw");
            if let Some((w, h)) = resolution {
                src_caps_builder = src_caps_builder
                    .field("width", w as i32)
                    .field("height", h as i32);
            }
            if let Some(fr) = framerate {
                src_caps_builder = src_caps_builder.field("framerate", fr);
            }
            let videosrc_caps = gst::ElementFactory::make("capsfilter")
                .name(&videosrc_caps_id)
                .property("caps", src_caps_builder.build())
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("videosrc_caps: {}", e)))?;

            let convert_element_name = video_convert_mode().element_name();
            let videoconvert = gst::ElementFactory::make(convert_element_name)
                .name(&videoconvert_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("{}: {}", convert_element_name, e))
                })?;
            gpu::configure_video_convert(&videoconvert);

            let video_caps = gst::Caps::builder("video/x-raw").build();
            let videocaps = gst::ElementFactory::make("capsfilter")
                .name(&videocaps_id)
                .property("caps", &video_caps)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("videocapsfilter: {}", e)))?;

            internal_links.push((
                ElementPadRef::pad(&videosrc_id, "src"),
                ElementPadRef::pad(&videosrc_caps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&videosrc_caps_id, "src"),
                ElementPadRef::pad(&videoconvert_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&videoconvert_id, "src"),
                ElementPadRef::pad(&videocaps_id, "sink"),
            ));

            elements.push((videosrc_id, videosrc));
            elements.push((videosrc_caps_id, videosrc_caps));
            elements.push((videoconvert_id, videoconvert));
            elements.push((videocaps_id, videocaps));
        }

        if stream_mode.has_audio() {
            let audio_device_id = read_string(properties, "audio_device").unwrap_or_default();
            let rate = read_string(properties, "audio_rate").and_then(|s| s.parse::<i32>().ok());
            let channels =
                read_string(properties, "audio_channels").and_then(|s| s.parse::<i32>().ok());
            let wasapi_exclusive = read_bool(properties, "wasapi_exclusive_mode");

            let audiosrc_id = format!("{}:audiosrc", instance_id);
            let audiosrc_caps_id = format!("{}:audiosrc_caps", instance_id);
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

            // Best-effort: ask Windows WASAPI to claim the device in
            // exclusive mode. wasapisrc (legacy) exposes `exclusive`;
            // wasapi2src (modern) exposes `low-latency` which behaves
            // similarly for our purposes (bypasses the system mixer).
            // No-op on other platforms / element types.
            if wasapi_exclusive {
                if audiosrc.has_property("exclusive") {
                    audiosrc.set_property("exclusive", true);
                    info!(
                        "Local Input: enabled WASAPI exclusive mode on {}",
                        audiosrc_id
                    );
                } else if audiosrc.has_property("low-latency") {
                    audiosrc.set_property("low-latency", true);
                    info!(
                        "Local Input: enabled wasapi2src low-latency on {}",
                        audiosrc_id
                    );
                }
            }

            // Pre-convert capsfilter: lets the source negotiate the requested
            // sample rate / channel count directly. Sample-rate conversion
            // and channel mixing still go through audioresample/audioconvert
            // downstream, but only when the device truly can't deliver the
            // requested format.
            let mut audio_src_caps = gst::Caps::builder("audio/x-raw");
            if let Some(r) = rate {
                audio_src_caps = audio_src_caps.field("rate", r);
            }
            if let Some(c) = channels {
                audio_src_caps = audio_src_caps.field("channels", c);
            }
            let audiosrc_caps = gst::ElementFactory::make("capsfilter")
                .name(&audiosrc_caps_id)
                .property("caps", audio_src_caps.build())
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audiosrc_caps: {}", e)))?;

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
                ElementPadRef::pad(&audiosrc_caps_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&audiosrc_caps_id, "src"),
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
            elements.push((audiosrc_caps_id, audiosrc_caps));
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
    // StreamMode::parse falls back to Video on unknown strings (legacy
    // WHEP behaviour). Local Input has no historical "video-only" default
    // — for us audio_video is the natural fallback, so handle unknown
    // values explicitly with a warning instead of silently downgrading
    // to video-only.
    match properties.get("stream_mode") {
        Some(PropertyValue::String(s)) => match s.as_str() {
            "audio_video" => StreamMode::AudioVideo,
            "video" => StreamMode::Video,
            "audio" => StreamMode::Audio,
            other => {
                warn!(
                    "Local Input: unknown stream_mode '{}' — defaulting to audio_video",
                    other
                );
                StreamMode::AudioVideo
            }
        },
        _ => StreamMode::default(),
    }
}

fn read_string(properties: &HashMap<String, PropertyValue>, key: &str) -> Option<String> {
    properties.get(key).and_then(|v| match v {
        PropertyValue::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    })
}

fn read_bool(properties: &HashMap<String, PropertyValue>, key: &str) -> bool {
    properties
        .get(key)
        .map(|v| matches!(v, PropertyValue::Bool(true)))
        .unwrap_or(false)
}

/// Parse a `"N/D"` framerate string (e.g. `"30/1"`, `"30000/1001"`) into a
/// `gst::Fraction`. Matches the format produced by
/// `common_video_framerate_enum_values()` so the dropdown values plug
/// straight into a capsfilter.
fn parse_fraction_string(s: &str) -> Option<gst::Fraction> {
    let (num, den) = s.split_once('/')?;
    let num: i32 = num.parse().ok()?;
    let den: i32 = den.parse().ok()?;
    if den == 0 {
        return None;
    }
    Some(gst::Fraction::new(num, den))
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
                persist: None,
            },
            ExposedProperty {
                name: "video_device".to_string(),
                label: "Video Source".to_string(),
                description: "Local Video/Source device to capture from (built-in cameras, USB capture cards, HDMI/SDI grabbers, virtual sources, ...). Picked from the live device list — leave empty to use the OS default (autovideosrc).".to_string(),
                property_type: PropertyType::Device {
                    category: strom_types::discovery::DeviceCategory::VideoSource,
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_device".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_device".to_string(),
                label: "Audio Source".to_string(),
                description: "Local Audio/Source device to capture from (built-in inputs, professional audio interfaces, USB grabbers, virtual sources, ...). Picked from the live device list — leave empty to use the OS default (autoaudiosrc).".to_string(),
                property_type: PropertyType::Device {
                    category: strom_types::discovery::DeviceCategory::AudioSource,
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_device".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "video_resolution".to_string(),
                label: "Video Resolution".to_string(),
                description: "Resolution to request from the video source. The platform plugin (AVFoundation/v4l2/Media Foundation) picks the closest matching capture mode. Empty leaves it to the device default — often the highest mode the camera supports, which can be expensive.".to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_resolution_enum_values(true),
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_resolution".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "video_framerate".to_string(),
                label: "Video Framerate".to_string(),
                description: "Framerate to request from the video source. Plugin picks the closest mode. Empty = device default.".to_string(),
                property_type: PropertyType::Enum {
                    values: common_video_framerate_enum_values(true),
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "video_framerate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_rate".to_string(),
                label: "Audio Sample Rate".to_string(),
                description: "Sample rate (Hz) requested from the audio source. audioresample handles conversion if the device can't deliver it natively. Empty = device default.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: String::new(),
                            label: Some("Default (device)".to_string()),
                        },
                        EnumValue {
                            value: "192000".to_string(),
                            label: Some("192 kHz".to_string()),
                        },
                        EnumValue {
                            value: "96000".to_string(),
                            label: Some("96 kHz".to_string()),
                        },
                        EnumValue {
                            value: "88200".to_string(),
                            label: Some("88.2 kHz".to_string()),
                        },
                        EnumValue {
                            value: "48000".to_string(),
                            label: Some("48 kHz".to_string()),
                        },
                        EnumValue {
                            value: "44100".to_string(),
                            label: Some("44.1 kHz".to_string()),
                        },
                        EnumValue {
                            value: "32000".to_string(),
                            label: Some("32 kHz".to_string()),
                        },
                        EnumValue {
                            value: "16000".to_string(),
                            label: Some("16 kHz".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_rate".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "audio_channels".to_string(),
                label: "Audio Channels".to_string(),
                description: "Number of channels requested from the audio source. Empty = device default. Pro audio cards / ASIO / PipeWire / CoreAudio can expose 4/6/8/16-channel multichannel streams in one go; many WASAPI drivers instead split a multichannel card into separate stereo devices (1/2, 3/4, ...) — in that case pick the right device and keep this at 2.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: String::new(),
                            label: Some("Default (device)".to_string()),
                        },
                        EnumValue {
                            value: "1".to_string(),
                            label: Some("1 (Mono)".to_string()),
                        },
                        EnumValue {
                            value: "2".to_string(),
                            label: Some("2 (Stereo)".to_string()),
                        },
                        EnumValue {
                            value: "4".to_string(),
                            label: Some("4 (Quad)".to_string()),
                        },
                        EnumValue {
                            value: "6".to_string(),
                            label: Some("6 (5.1)".to_string()),
                        },
                        EnumValue {
                            value: "8".to_string(),
                            label: Some("8 (7.1)".to_string()),
                        },
                        EnumValue {
                            value: "16".to_string(),
                            label: Some("16".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String(String::new())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "audio_channels".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "wasapi_exclusive_mode".to_string(),
                label: "WASAPI Exclusive Mode".to_string(),
                description: "Windows only. Ask WASAPI to claim the audio device in exclusive mode, bypassing the system mixer. Required for accessing the full hardware-native channel layout on pro audio cards via wasapi/wasapi2src. No-op on macOS / Linux / non-WASAPI sources. Default off.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "wasapi_exclusive_mode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prop_string(s: &str) -> PropertyValue {
        PropertyValue::String(s.to_string())
    }

    // --- read_stream_mode ---

    #[test]
    fn stream_mode_audio_video() {
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string("audio_video"));
        assert_eq!(read_stream_mode(&p), StreamMode::AudioVideo);
    }

    #[test]
    fn stream_mode_video_only() {
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string("video"));
        assert_eq!(read_stream_mode(&p), StreamMode::Video);
    }

    #[test]
    fn stream_mode_audio_only() {
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string("audio"));
        assert_eq!(read_stream_mode(&p), StreamMode::Audio);
    }

    #[test]
    fn stream_mode_unknown_falls_back_to_audio_video() {
        // Regression test: legacy StreamMode::parse falls back to Video,
        // but for Local Input we want AudioVideo so the block keeps
        // working with both pads. Verifies the local fallback override.
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string("garbled"));
        assert_eq!(read_stream_mode(&p), StreamMode::AudioVideo);
    }

    #[test]
    fn stream_mode_missing_uses_default() {
        let p = HashMap::new();
        assert_eq!(read_stream_mode(&p), StreamMode::default());
    }

    // --- read_string ---

    #[test]
    fn read_string_returns_none_for_empty() {
        let mut p = HashMap::new();
        p.insert("k".to_string(), prop_string(""));
        assert_eq!(read_string(&p, "k"), None);
    }

    #[test]
    fn read_string_returns_value() {
        let mut p = HashMap::new();
        p.insert("k".to_string(), prop_string("hello"));
        assert_eq!(read_string(&p, "k"), Some("hello".to_string()));
    }

    #[test]
    fn read_string_returns_none_for_missing() {
        let p = HashMap::new();
        assert_eq!(read_string(&p, "k"), None);
    }

    // --- read_bool ---

    #[test]
    fn read_bool_true() {
        let mut p = HashMap::new();
        p.insert("k".to_string(), PropertyValue::Bool(true));
        assert!(read_bool(&p, "k"));
    }

    #[test]
    fn read_bool_false() {
        let mut p = HashMap::new();
        p.insert("k".to_string(), PropertyValue::Bool(false));
        assert!(!read_bool(&p, "k"));
    }

    #[test]
    fn read_bool_missing_defaults_false() {
        let p = HashMap::new();
        assert!(!read_bool(&p, "k"));
    }

    #[test]
    fn read_bool_non_bool_value_defaults_false() {
        let mut p = HashMap::new();
        p.insert("k".to_string(), prop_string("true"));
        // PropertyValue::String("true") is NOT PropertyValue::Bool(true) —
        // we don't coerce, callers should send the right type.
        assert!(!read_bool(&p, "k"));
    }

    // --- parse_fraction_string ---

    #[test]
    fn fraction_simple() {
        assert_eq!(
            parse_fraction_string("30/1"),
            Some(gst::Fraction::new(30, 1))
        );
    }

    #[test]
    fn fraction_drop_frame() {
        assert_eq!(
            parse_fraction_string("30000/1001"),
            Some(gst::Fraction::new(30000, 1001))
        );
    }

    #[test]
    fn fraction_zero_denominator_rejected() {
        assert_eq!(parse_fraction_string("30/0"), None);
    }

    #[test]
    fn fraction_garbage_rejected() {
        assert_eq!(parse_fraction_string(""), None);
        assert_eq!(parse_fraction_string("abc"), None);
        assert_eq!(parse_fraction_string("30"), None);
        assert_eq!(parse_fraction_string("30/"), None);
        assert_eq!(parse_fraction_string("/1"), None);
    }

    // --- get_external_pads ---

    fn pads_for_mode(mode: &str) -> ExternalPads {
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string(mode));
        LocalInputBuilder.get_external_pads(&p).expect("pads")
    }

    #[test]
    fn pads_audio_video_has_both_outputs() {
        let pads = pads_for_mode("audio_video");
        assert!(pads.inputs.is_empty(), "Local Input has no input pads");
        assert_eq!(pads.outputs.len(), 2);
        let names: Vec<&str> = pads.outputs.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"video_out"));
        assert!(names.contains(&"audio_out"));
        // Both pads carry a V/A label when audio+video are both present.
        for pad in &pads.outputs {
            assert!(pad.label.is_some(), "AV mode should label both pads");
        }
    }

    #[test]
    fn pads_video_only_has_one_output_no_label() {
        let pads = pads_for_mode("video");
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "video_out");
        assert_eq!(pads.outputs[0].media_type, MediaType::Video);
        // Single-mode: no need for a V/A label.
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn pads_audio_only_has_one_output_no_label() {
        let pads = pads_for_mode("audio");
        assert_eq!(pads.outputs.len(), 1);
        assert_eq!(pads.outputs[0].name, "audio_out");
        assert_eq!(pads.outputs[0].media_type, MediaType::Audio);
        assert!(pads.outputs[0].label.is_none());
    }

    #[test]
    fn pads_internal_element_ids_match_build_chain() {
        // The external-pad `internal_element_id` strings have to match
        // the actual element IDs `build()` produces (after the
        // instance-id prefix), otherwise external links resolve to
        // nothing and the flow won't run.
        let pads = pads_for_mode("audio_video");
        for pad in &pads.outputs {
            match pad.media_type {
                MediaType::Video => assert_eq!(pad.internal_element_id, "videocapsfilter"),
                MediaType::Audio => assert_eq!(pad.internal_element_id, "audiocapsfilter"),
                _ => panic!("unexpected media type {:?}", pad.media_type),
            }
            assert_eq!(pad.internal_pad_name, "src");
        }
    }
}
