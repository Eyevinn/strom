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
//! via the `audio_channels` property (1/2/4/6/8/16/default, or a surround layout). What you
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
//!   either WASAPI exclusive mode (`wasapi_exclusive_mode=true`) or
//!   the ASIO plugin (`asiosrc`) — once loaded, ASIO devices appear
//!   automatically in the picker just like any other `GstDevice`. For an
//!   ASIO device, `audio_channels` selects inputs 1..N (`input-channels`).
//!
//! In Strom, more than two channels means N parallel channels with no
//! speaker layout, not surround. Unless `audio_channels` names a surround
//! layout (`"6:0x3f"`, the Audio Format block's value form), the block
//! marks 3+ channels as unpositioned (`channel-mask=0`) whatever the
//! source reports, so a consumer never downmixes them as 5.1/7.1.
//!
//! The design is plugin-agnostic: any provider registered with
//! `GstDeviceMonitor` (PipeWire, third-party ASIO, ...) is picked up
//! without changes here.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu::{self, video_convert_mode};
use crate::gst::video_adapt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{
    block::{
        common_video_framerate_enum_values, common_video_resolution_enum_values,
        parse_resolution_string, StreamMode, *,
    },
    element::ElementPadRef,
    parse_audio_channel_config, AudioChannelConfig, MediaType, PropertyValue,
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
            // Both memory types are offered, system memory first so a device
            // that can deliver either keeps producing what it does today. A
            // capsfilter naming only `video/x-raw` means SystemMemory, not
            // "any": a camera that offers GL memory and nothing else (an
            // AVFoundation camera behind avfvideosrc is the common case) could
            // not negotiate this filter at all. No download is asked for here
            // — the linker inserts one where a GL producer meets a consumer
            // that needs system memory.
            let mut structure_builder = gst::Structure::builder("video/x-raw");
            if let Some((w, h)) = resolution {
                structure_builder = structure_builder
                    .field("width", w as i32)
                    .field("height", h as i32);
            }
            if let Some(fr) = framerate {
                structure_builder = structure_builder.field("framerate", fr);
            }
            let structure = structure_builder.build();

            let mut src_caps = gst::Caps::new_empty();
            {
                let caps = src_caps.get_mut().expect("fresh caps are uniquely owned");
                caps.append_structure(structure.clone());
                caps.append_structure_full(
                    structure,
                    Some(gst::CapsFeatures::new([video_adapt::GL_MEMORY_FEATURE])),
                );
            }

            let videosrc_caps = gst::ElementFactory::make("capsfilter")
                .name(&videosrc_caps_id)
                .property("caps", &src_caps)
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
            let channels = read_string(properties, "audio_channels")
                .and_then(|s| parse_audio_channel_config(&s));
            if let Some(c) = channels.filter(|c| c.channels > MAX_AUDIO_CHANNELS) {
                return Err(BlockBuildError::InvalidConfiguration(format!(
                    "audio_channels {} is more than the {} channels GStreamer can convert",
                    c.channels, MAX_AUDIO_CHANNELS
                )));
            }
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

            let is_asio = audiosrc
                .factory()
                .is_some_and(|f| f.name() == ASIO_SOURCE_FACTORY);
            if is_asio {
                // Device default on a device wider than GStreamer's channel
                // limit: take the first MAX_AUDIO_CHANNELS inputs instead of
                // every input.
                let inputs = channels.map(|c| c.channels).or_else(|| {
                    let max = ctx
                        .local_device(&audio_device_id)
                        .and_then(|d| d.caps())
                        .and_then(|caps| max_channels(&caps))?;
                    (max > MAX_AUDIO_CHANNELS).then(|| {
                        warn!(
                            "Local Input: ASIO device has {} inputs, capturing the first {}",
                            max, MAX_AUDIO_CHANNELS
                        );
                        MAX_AUDIO_CHANNELS
                    })
                });
                if let Some(inputs) = inputs {
                    let list = asio_input_channels(inputs);
                    info!(
                        "Local Input: capturing ASIO inputs {} on {}",
                        list, audiosrc_id
                    );
                    audiosrc.set_property("input-channels", list);
                }
            }

            // Pre-convert capsfilter: lets the source negotiate the requested
            // sample rate / channel count directly. Sample-rate conversion
            // and channel mixing still go through audioresample/audioconvert
            // downstream, but only when the device truly can't deliver the
            // requested format.
            let layout = audio_source_layout(rate, channels);
            let audiosrc_caps = gst::ElementFactory::make("capsfilter")
                .name(&audiosrc_caps_id)
                .property("caps", &layout.caps)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audiosrc_caps: {}", e)))?;
            if let Some(pad) = audiosrc_caps.static_pad("src") {
                install_layout_probe(&pad, layout.mask);
            }

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

/// Factory name of GStreamer's ASIO capture element (gst-plugins-bad, Windows).
const ASIO_SOURCE_FACTORY: &str = "asiosrc";

/// Widest channel count GStreamer's channel mixer takes. `audioconvert`
/// segfaults above it on GStreamer 1.24 (`gst_audio_channel_mixer_new`
/// asserts `in_channels <= 64`), so the block never negotiates more.
const MAX_AUDIO_CHANNELS: u32 = 64;

/// Highest channel count a device's caps offer.
fn max_channels(caps: &gst::Caps) -> Option<u32> {
    caps.iter()
        .filter_map(|s| {
            s.get::<gst::IntRange<i32>>("channels")
                .map(|r| r.max())
                .or_else(|_| s.get::<i32>("channels"))
                .ok()
        })
        .max()
        .and_then(|c| u32::try_from(c).ok())
}

/// `input-channels` value that captures the first `channels` ASIO inputs.
fn asio_input_channels(channels: u32) -> String {
    (0..channels)
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The channel layout the block delivers, and the caps for the capsfilter
/// right after the audio source.
///
/// Strom treats 3+ channels as N parallel channels with no speaker layout
/// (`channel-mask=0`) unless `audio_channels` names a surround layout
/// (`"6:0x3f"`). The filter offers the wanted layout first, with a fallback
/// for sources that only offer another one; [`install_layout_probe`] then
/// relabels what they deliver. Offering a mask is also what lets `asiosrc`
/// negotiate: it advertises `channels=N` with no mask, which GStreamer only
/// accepts for 1-2 channels.
struct AudioSourceLayout {
    caps: gst::Caps,
    /// Mask that 3+ channels leave the filter with.
    mask: u64,
}

fn audio_source_layout(rate: Option<i32>, config: Option<AudioChannelConfig>) -> AudioSourceLayout {
    let structure = |channels: Option<gst::glib::SendValue>, mask: Option<u64>| {
        let mut s = gst::Structure::builder("audio/x-raw");
        if let Some(r) = rate {
            s = s.field("rate", r);
        }
        if let Some(c) = channels {
            s = s.field("channels", c);
        }
        if let Some(m) = mask {
            s = s.field("channel-mask", gst::Bitmask::new(m));
        }
        s.build()
    };

    let mask = config.and_then(|c| c.channel_mask).unwrap_or(0);
    let mut caps = gst::Caps::new_empty();
    let caps_mut = caps.get_mut().expect("new caps are writable");
    match config {
        Some(c) if c.channels > 2 || mask != 0 => {
            let channels = c.channels as i32;
            caps_mut.append_structure(structure(Some(channels.to_send_value()), Some(mask)));
            caps_mut.append_structure(structure(Some(channels.to_send_value()), None));
        }
        Some(c) => {
            caps_mut.append_structure(structure(Some((c.channels as i32).to_send_value()), None))
        }
        // Device default: the count is unknown until the source opens.
        None => {
            caps_mut.append_structure(structure(
                Some(gst::IntRange::new(1, 2).to_send_value()),
                None,
            ));
            let max = MAX_AUDIO_CHANNELS as i32;
            caps_mut.append_structure(structure(
                Some(gst::IntRange::new(3, max).to_send_value()),
                Some(0),
            ));
            caps_mut.append_structure(structure(
                Some(gst::IntRange::new(1, max).to_send_value()),
                None,
            ));
        }
    }
    AudioSourceLayout { caps, mask }
}

/// `caps` relabelled to `mask` when they carry 3+ channels and exactly one
/// side is unpositioned, or `None` when they need no change.
///
/// `audioconvert` will not turn a positioned layout into an unpositioned
/// one of the same width, or back, so the block rewrites the caps event
/// instead: the samples stay as they are, only the layout label changes.
/// A source with a different positioned layout keeps it.
fn relabel_caps(caps: &gst::CapsRef, mask: u64) -> Option<gst::Caps> {
    let s = caps.structure(0)?;
    let channels = s.get::<i32>("channels").ok()?;
    let current = s.get::<gst::Bitmask>("channel-mask").ok()?.0;
    if channels <= 2 || current == mask || (current != 0 && mask != 0) {
        return None;
    }
    let mut caps = caps.to_owned();
    caps.get_mut()?
        .structure_mut(0)?
        .set("channel-mask", gst::Bitmask::new(mask));
    Some(caps)
}

/// Relabel 3+ channels to `mask` on `pad`'s caps events (see
/// [`relabel_caps`]). An event probe: it fires once per caps change, not
/// per buffer.
fn install_layout_probe(pad: &gst::Pad, mask: u64) {
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        if let Some(caps) = relabel_caps(caps_event.caps(), mask) {
            info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(&caps)));
        }
        gst::PadProbeReturn::Ok
    });
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
                description: "Number of channels requested from the audio source. Empty = device default. Pro audio cards / ASIO / PipeWire / CoreAudio can expose 4/6/8/16-channel multichannel streams in one go; Plain counts of 3 or more are parallel channels with no speaker layout (channel-mask 0x0); the Surround choices carry a speaker layout. On an ASIO device this captures inputs 1..N, and empty captures every input; many WASAPI drivers instead split a multichannel card into separate stereo devices (1/2, 3/4, ...) — in that case pick the right device and keep this at 2.".to_string(),
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
                            label: Some("4 (multichannel)".to_string()),
                        },
                        EnumValue {
                            value: "6".to_string(),
                            label: Some("6 (multichannel)".to_string()),
                        },
                        EnumValue {
                            value: "8".to_string(),
                            label: Some("8 (multichannel)".to_string()),
                        },
                        EnumValue {
                            value: "16".to_string(),
                            label: Some("16 (multichannel)".to_string()),
                        },
                        EnumValue {
                            value: "4:0x33".to_string(),
                            label: Some("4 - Quad Surround".to_string()),
                        },
                        EnumValue {
                            value: "6:0x3f".to_string(),
                            label: Some("6 - 5.1 Surround".to_string()),
                        },
                        EnumValue {
                            value: "8:0xc3f".to_string(),
                            label: Some("8 - 7.1 Surround".to_string()),
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

    // --- read_string / read_bool ---

    fn props_with(value: &Option<PropertyValue>) -> HashMap<String, PropertyValue> {
        let mut p = HashMap::new();
        if let Some(v) = value {
            p.insert("k".to_string(), v.clone());
        }
        p
    }

    #[test]
    fn read_string_and_bool() {
        let string_cases: &[(Option<PropertyValue>, Option<&str>)] = &[
            (None, None),
            (Some(prop_string("")), None),
            (Some(prop_string("hello")), Some("hello")),
            (Some(PropertyValue::Bool(true)), None),
        ];
        for (value, expected) in string_cases {
            assert_eq!(
                read_string(&props_with(value), "k").as_deref(),
                *expected,
                "read_string({:?})",
                value
            );
        }

        // No coercion: PropertyValue::String("true") is not Bool(true).
        let bool_cases: &[(Option<PropertyValue>, bool)] = &[
            (None, false),
            (Some(PropertyValue::Bool(true)), true),
            (Some(PropertyValue::Bool(false)), false),
            (Some(prop_string("true")), false),
        ];
        for (value, expected) in bool_cases {
            assert_eq!(
                read_bool(&props_with(value), "k"),
                *expected,
                "read_bool({:?})",
                value
            );
        }
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

    // --- ASIO multichannel ---

    /// Caps as `asiosrc` advertises them once its channels are configured:
    /// a fixed channel count and no `channel-mask`.
    fn asio_source_caps(channels: i32) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::builder("audio/x-raw")
            .field("layout", "interleaved")
            .field("format", "F32LE")
            .field("rate", 48000)
            .field("channels", channels)
            .build()
    }

    /// Negotiate `source` against `filter` the way a base source does
    /// (intersect with the downstream caps, then fixate), and return the
    /// fixated caps.
    fn negotiate(source: &gst::Caps, filter: &gst::Caps) -> gst::Caps {
        let mut caps = source.intersect_with_mode(filter, gst::CapsIntersectMode::First);
        assert!(!caps.is_empty(), "{} does not intersect {}", source, filter);
        caps.fixate();
        caps
    }

    /// Push one buffer with `caps` into `audioconvert` and report whether
    /// the caps were accepted.
    fn audioconvert_accepts(caps: &gst::Caps) -> Result<(), String> {
        let s = caps.structure(0).unwrap();
        let channels = s.get::<i32>("channels").unwrap() as usize;

        let pipeline = gst::Pipeline::new();
        let src = gstreamer_app::AppSrc::builder()
            .caps(caps)
            .format(gst::Format::Time)
            .build();
        let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline
            .add_many([src.upcast_ref(), &convert, &sink])
            .unwrap();
        gst::Element::link_many([src.upcast_ref(), &convert, &sink]).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; 480 * channels * 4]);
        buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
        let _ = src.push_buffer(buffer);
        let _ = src.end_of_stream();

        let bus = pipeline.bus().unwrap();
        let result = match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(e) => Err(format!("{} ({:?})", e.error(), e.debug())),
                _ => unreachable!(),
            },
            None => Err("no EOS or error within 5 s".to_string()),
        };
        pipeline.set_state(gst::State::Null).unwrap();
        result
    }

    #[test]
    fn asio_without_channel_mask_fails_above_two_channels() {
        // Negative control: the failure from issue #1040. If this starts
        // passing, GStreamer no longer needs the mask and the ASIO caps
        // below can go.
        assert!(audioconvert_accepts(&asio_source_caps(2)).is_ok());
        assert!(audioconvert_accepts(&asio_source_caps(3)).is_err());
    }

    #[test]
    fn asio_multichannel_negotiates_through_the_block_filter() {
        for channels in [3, 4, 16] {
            let source = asio_source_caps(channels);
            let filter = audio_source_layout(None, channel_config(&channels.to_string())).caps;
            let caps = negotiate(&source, &filter);
            audioconvert_accepts(&caps)
                .unwrap_or_else(|e| panic!("{} channels, caps {}: {}", channels, caps, e));
        }
    }

    #[test]
    fn asio_device_default_negotiates_any_channel_count() {
        // audio_channels empty: asiosrc opens every input on the device.
        for channels in [1, 2, 3, 64] {
            let source = asio_source_caps(channels);
            let filter = audio_source_layout(Some(48000), None).caps;
            let caps = negotiate(&source, &filter);
            let positioned = caps.structure(0).unwrap().has_field("channel-mask");
            assert_eq!(positioned, channels > 2, "{} channels: {}", channels, caps);
            audioconvert_accepts(&caps)
                .unwrap_or_else(|e| panic!("{} channels, caps {}: {}", channels, caps, e));
        }
    }

    #[test]
    fn device_default_refuses_more_channels_than_audioconvert_takes() {
        // 65+ channels segfault audioconvert on GStreamer 1.24: they must
        // fail negotiation at the filter instead of reaching it.
        gst::init().unwrap();
        let filter = audio_source_layout(None, None).caps;
        assert!(asio_source_caps(65).intersect(&filter).is_empty());
        assert!(asio_source_caps(128).intersect(&filter).is_empty());
        let positioned_wide: gst::Caps = "audio/x-raw,channels=65".parse().unwrap();
        assert!(positioned_wide.intersect(&filter).is_empty());
    }

    #[test]
    fn max_channels_reads_a_device_range() {
        gst::init().unwrap();
        let caps: gst::Caps = "audio/x-raw,channels=[1,128]".parse().unwrap();
        assert_eq!(max_channels(&caps), Some(128));
        let caps: gst::Caps = "audio/x-raw,channels=10".parse().unwrap();
        assert_eq!(max_channels(&caps), Some(10));
    }

    fn channel_config(audio_channels: &str) -> Option<AudioChannelConfig> {
        parse_audio_channel_config(audio_channels)
    }

    #[test]
    fn asio_surround_choice_negotiates_its_layout() {
        let source = asio_source_caps(6);
        let filter = audio_source_layout(None, channel_config("6:0x3f")).caps;
        let caps = negotiate(&source, &filter);
        assert_eq!(channel_mask(&caps), Some(0x3f), "{}", caps);
        audioconvert_accepts(&caps).unwrap_or_else(|e| panic!("caps {}: {}", caps, e));
    }

    /// Run `audiotestsrc` constrained to `source_caps` through the block's
    /// audio chain for `audio_channels` (source capsfilter with its probe,
    /// then `audioconvert`), and return the caps that reach the sink.
    fn caps_after_block_chain(source_caps: &str, audio_channels: &str) -> gst::Caps {
        gst::init().unwrap();
        let layout = audio_source_layout(None, channel_config(audio_channels));
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("num-buffers", 3)
            .build()
            .unwrap();
        let source_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", source_caps.parse::<gst::Caps>().unwrap())
            .build()
            .unwrap();
        let block_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &layout.caps)
            .build()
            .unwrap();
        install_layout_probe(&block_filter.static_pad("src").unwrap(), layout.mask);
        let convert = gst::ElementFactory::make("audioconvert").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        let chain = [&src, &source_filter, &block_filter, &convert, &sink];
        pipeline.add_many(chain).unwrap();
        gst::Element::link_many(chain).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("no EOS or error within 5 s");
        let caps = sink.static_pad("sink").unwrap().current_caps();
        pipeline.set_state(gst::State::Null).unwrap();
        if let gst::MessageView::Error(e) = msg.view() {
            panic!(
                "{} with audio_channels {:?}: {} ({:?})",
                source_caps,
                audio_channels,
                e.error(),
                e.debug()
            );
        }
        caps.expect("sink has caps")
    }

    fn channel_mask(caps: &gst::Caps) -> Option<u64> {
        caps.structure(0)
            .unwrap()
            .get::<gst::Bitmask>("channel-mask")
            .ok()
            .map(|m| m.0)
    }

    const SOURCE_5_1: &str = "audio/x-raw,channels=6,channel-mask=(bitmask)0x3f";
    const SOURCE_6_UNPOSITIONED: &str = "audio/x-raw,channels=6,channel-mask=(bitmask)0x0";

    #[test]
    fn multichannel_choice_unpositions_any_source() {
        // A source that only offers a 5.1 layout (wasapisrc, pulsesrc), and
        // one that offers no layout.
        for source in [SOURCE_5_1, SOURCE_6_UNPOSITIONED] {
            for audio_channels in ["", "6", "6:0x0"] {
                let caps = caps_after_block_chain(source, audio_channels);
                assert_eq!(
                    channel_mask(&caps),
                    Some(0),
                    "{} / {:?}: {}",
                    source,
                    audio_channels,
                    caps
                );
                assert_eq!(caps.structure(0).unwrap().get::<i32>("channels"), Ok(6));
            }
        }
    }

    #[test]
    fn surround_choice_delivers_its_layout() {
        for source in [SOURCE_5_1, SOURCE_6_UNPOSITIONED] {
            let caps = caps_after_block_chain(source, "6:0x3f");
            assert_eq!(channel_mask(&caps), Some(0x3f), "{}: {}", source, caps);
        }
    }

    #[test]
    fn stereo_sources_keep_their_layout() {
        for audio_channels in ["", "2"] {
            let caps = caps_after_block_chain("audio/x-raw,channels=2", audio_channels);
            assert_ne!(
                channel_mask(&caps),
                Some(0),
                "{:?}: {}",
                audio_channels,
                caps
            );
        }
    }

    #[test]
    fn asio_input_channels_lists_the_first_inputs() {
        assert_eq!(asio_input_channels(1), "0");
        assert_eq!(asio_input_channels(4), "0,1,2,3");
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
        // The external-pad `internal_element_id` strings have to match the
        // element IDs `build()` produces (after the instance-id prefix),
        // otherwise external links resolve to nothing and the flow won't run.
        // No device is selected, so build() falls back to autovideosrc and
        // autoaudiosrc without opening hardware.
        gst::init().expect("GStreamer init");
        crate::gpu::detect_gpu_capabilities();
        let mut p = HashMap::new();
        p.insert("stream_mode".to_string(), prop_string("audio_video"));
        let ctx = BlockBuildContext::new(Vec::new(), "all".to_string());
        let result = LocalInputBuilder
            .build("dev", &p, &ctx)
            .unwrap_or_else(|e| panic!("build failed: {:?}", e));
        let pads = LocalInputBuilder.get_external_pads(&p).expect("pads");

        assert_eq!(pads.outputs.len(), 2);
        for pad in &pads.outputs {
            let full_id = format!("dev:{}", pad.internal_element_id);
            let (_, element) = result
                .elements
                .iter()
                .find(|(id, _)| *id == full_id)
                .unwrap_or_else(|| {
                    panic!(
                        "{:?} pad points at {}, which build() did not create",
                        pad.media_type, full_id
                    )
                });
            assert!(
                element.static_pad(&pad.internal_pad_name).is_some(),
                "{} has no {} pad",
                full_id,
                pad.internal_pad_name
            );
        }
    }
}
