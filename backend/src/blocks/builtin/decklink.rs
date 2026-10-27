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
//! DeckLink Input emits the card's native formats. DeckLink Output adapts its
//! video input to the configured mode: an input front downloads GPU memory
//! (`gst::download_front`), then `videorate`, `videoscale` and `videoconvert`
//! change the framerate, scale and convert the pixel format to what
//! `decklinkvideosink` takes. Video whose interlacing does not match the mode
//! fails the flow with an element error: nothing here interlaces or
//! deinterlaces. Audio goes to `decklinkaudiosink` as it arrives (#1068).
//!
//! Uses GStreamer's DeckLink plugin (gst-plugins-bad) for hardware integration.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gpu;
use crate::gst::download_front;
use crate::gst::video_adapt::VIDEO_CONVERT_FACTORY;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use strom_types::{
    block::{StreamMode, *},
    element::ElementPadRef,
    EnumValue, MediaType, PropertyValue,
};
use tracing::{info, warn};

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

        let synchronized_capture = properties
            .get("synchronized_capture")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let videosrc_id = format!("{}:decklinkvideosrc", instance_id);
        let videosrc = gst::ElementFactory::make("decklinkvideosrc")
            .name(&videosrc_id)
            .property("device-number", device_number)
            .property_from_str("mode", video_mode)
            .property_from_str("connection", connection)
            .property_from_str("video-format", video_format)
            .property("drop-no-signal-frames", drop_no_signal_frames)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("decklinkvideosrc: {}", e)))?;

        // BMD synced capture groups only work for inputs armed atomically as
        // a unit, which means all members must live in the same pipeline. We
        // derive the capture group ID from the flow ID so two synchronized_
        // capture blocks in the same flow always land in the same group, and
        // can never collide with another flow's group on the same card.
        // The capture-group property only exists on our patched plugin; if
        // the runtime has the stock plugin we just skip silently.
        if synchronized_capture && videosrc.has_property("capture-group") {
            let flow_id = properties
                .get("_flow_id")
                .and_then(|v| match v {
                    PropertyValue::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let mut hasher = DefaultHasher::new();
            flow_id.hash(&mut hasher);
            let group_id = ((hasher.finish() & 0x7FFF_FFFF) as i32).max(0);
            videosrc.set_property("capture-group", group_id);
        }

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
                .property_from_str("connection", audio_connection)
                .property_from_str("channels", &audio_channels)
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
            "DeckLink Input configured: device={}, stream_mode={}, mode={}, connection={}, video-format={}, drop-no-signal-frames={}",
            device_number, stream_mode.as_str(), video_mode, connection, video_format, drop_no_signal_frames
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
                internal_element_id: VIDEO_INPUT_ELEMENT.to_string(),
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

        let mut elements: Vec<(String, gst::Element)> =
            vec![(videosink_id.clone(), videosink.clone())];
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

        if stream_mode.has_video() {
            let (video_elements, video_links) =
                build_video_input(instance_id, &videosink, video_mode)?;
            elements.extend(video_elements);
            internal_links.extend(video_links);
        }

        if stream_mode.has_audio() {
            let audiosink_id = format!("{}:decklinkaudiosink", instance_id);
            // Don't wait for a first buffer to start: an input with no data
            // yet would keep the flow from reaching PLAYING.
            let audiosink = gst::ElementFactory::make("decklinkaudiosink")
                .name(&audiosink_id)
                .property("device-number", device_number)
                .property("async", false)
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

/// The element DeckLink Output's `video_in` pad points at: its input front.
pub const VIDEO_INPUT_ELEMENT: &str = "input";

/// Elements of a block chain, by id.
type BlockElements = Vec<(String, gst::Element)>;
/// Internal links of a block chain.
type BlockLinks = Vec<(ElementPadRef, ElementPadRef)>;

/// DeckLink Output's video input chain, ending in `videosink`:
/// `input` (identity front) → `videorate` → `videoscale` → `videoconvert` →
/// `ratecaps`. The rate goes first, so frames it drops are never scaled or
/// converted.
///
/// The front downloads GL, CUDA, D3D12 or D3D11 memory to system memory,
/// decided from the negotiated caps (`gst::download_front`). The converters
/// then negotiate against the caps `decklinkvideosink` answers for the
/// configured mode, so a pixel format or size the card does not take is
/// converted on the way in. The sink leaves the framerate open, so `ratecaps`
/// pins the mode's own rate for `videorate` to produce.
fn build_video_input(
    instance_id: &str,
    videosink: &gst::Element,
    mode: &str,
) -> Result<(BlockElements, BlockLinks), BlockBuildError> {
    let make = |factory: &str, name: &str| -> Result<(String, gst::Element), BlockBuildError> {
        let id = format!("{}:{}", instance_id, name);
        let element = gst::ElementFactory::make(factory)
            .name(&id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))?;
        Ok((id, element))
    };
    let (input_id, input) = make("identity", VIDEO_INPUT_ELEMENT)?;
    input.set_property("silent", true);
    let (convert_id, convert) = make(VIDEO_CONVERT_FACTORY, "videoconvert")?;
    let (scale_id, scale) = make("videoscale", "videoscale")?;
    let (rate_id, rate) = make("videorate", "videorate")?;
    let (ratecaps_id, ratecaps) = make("capsfilter", "ratecaps")?;
    gpu::configure_video_convert(&convert);
    gpu::configure_video_convert(&scale);
    let framerate = mode_framerate(mode);
    if let Some(framerate) = framerate {
        ratecaps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", framerate)
                .build(),
        );
    }

    let rate_sink = rate
        .static_pad("sink")
        .ok_or_else(|| BlockBuildError::ElementCreation("videorate has no sink pad".into()))?;
    download_front::install(&input, &rate_sink, &input_id, "DeckLink Output");
    let card_sink = videosink.static_pad("sink").ok_or_else(|| {
        BlockBuildError::ElementCreation("decklinkvideosink has no sink pad".into())
    })?;
    watch_input_caps(&input, &card_sink, mode, framerate);
    leave_interlacing_open(&ratecaps);

    let chain = [&input_id, &rate_id, &scale_id, &convert_id, &ratecaps_id];
    let mut links: Vec<(ElementPadRef, ElementPadRef)> = chain
        .windows(2)
        .map(|pair| {
            (
                ElementPadRef::pad(pair[0], "src"),
                ElementPadRef::pad(pair[1], "sink"),
            )
        })
        .collect();
    links.push((
        ElementPadRef::pad(&ratecaps_id, "src"),
        ElementPadRef::pad(videosink.name().as_str(), "sink"),
    ));
    Ok((
        vec![
            (input_id, input),
            (convert_id, convert),
            (scale_id, scale),
            (rate_id, rate),
            (ratecaps_id, ratecaps),
        ],
        links,
    ))
}

/// The frame rate of a mode the block offers (see `video_mode_enum_values`):
/// `1080p2997` is 30000/1001, `1080i50` (50 fields) is 25/1. `None` for `auto`
/// and any mode not listed here, which leaves the framerate to the source.
fn mode_framerate(mode: &str) -> Option<gst::Fraction> {
    let (num, den) = match mode.trim_start_matches(|c: char| c.is_ascii_digit()) {
        "p2398" => (24000, 1001),
        "p24" => (24, 1),
        "p25" | "i50" => (25, 1),
        "p2997" | "i5994" => (30000, 1001),
        "p30" | "i60" => (30, 1),
        "p50" => (50, 1),
        "p5994" => (60000, 1001),
        "p60" => (60, 1),
        _ => return None,
    };
    Some(gst::Fraction::new(num, den))
}

/// Answer caps queries arriving at `last`'s source pad, from upstream, with
/// the card's caps and the same caps without `interlace-mode`.
///
/// No element in the chain changes interlacing, so the card's interlacing
/// would otherwise travel back to the source as a requirement, and the source
/// would fail with a bare `not-negotiated`. With it left open, the video
/// arrives as it is and [`watch_input_caps`] fails the flow with a message.
///
/// A QUERY_DOWNSTREAM probe that answers the query itself: per query, never
/// per buffer.
fn leave_interlacing_open(last: &gst::Element) {
    let Some(src) = last.static_pad("src") else {
        return;
    };
    src.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |pad, info| {
        if info.mask.contains(gst::PadProbeType::PULL) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Query(ref mut query)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::QueryViewMut::Caps(q) = query.view_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        // Answered here, once: the card's caps unfiltered, since a filter
        // that says progressive empties an answer that says interleaved.
        // Asked of the peer pad itself, since a peer query from `pad` would
        // come back here.
        let Some(card) = pad.peer().map(|peer| peer.query_caps(None)) else {
            return gst::PadProbeReturn::Ok;
        };
        if card.is_any() || card.is_empty() {
            return gst::PadProbeReturn::Ok;
        }
        let mut open = card.clone();
        {
            let open = open.make_mut();
            for (structure, features) in card.iter_with_features() {
                if structure.has_field("interlace-mode") {
                    let mut structure = structure.to_owned();
                    structure.remove_fields(["interlace-mode", "field-order"]);
                    open.append_structure_full(structure, Some(features.to_owned()));
                }
            }
        }
        let answer = match q.filter() {
            Some(filter) => filter.intersect_with_mode(&open, gst::CapsIntersectMode::First),
            None => open,
        };
        q.set_result(&answer);
        gst::PadProbeReturn::Handled
    });
}

/// Watch the raw video arriving at `input` against `card_sink`'s caps, what
/// the card takes for `mode`.
///
/// Interlacing is the one thing the chain cannot adapt: progressive video into
/// an interlaced mode, or the reverse, fails the flow with an element error
/// that names both. A size or framerate the chain changes is logged.
///
/// An EVENT_DOWNSTREAM probe: per CAPS event, never per buffer. Weak handles
/// only, since the probe is owned by a pad of `input` itself.
fn watch_input_caps(
    input: &gst::Element,
    card_sink: &gst::Pad,
    mode: &str,
    framerate: Option<gst::Fraction>,
) {
    let Some(input_sink) = input.static_pad("sink") else {
        warn!(
            "DeckLink Output: the video input front has no sink pad, so its caps are not checked"
        );
        return;
    };
    let input_weak = input.downgrade();
    let card_sink = card_sink.downgrade();
    let mode = mode.to_string();
    input_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let (Some(input), Some(card_sink)) = (input_weak.upgrade(), card_sink.upgrade()) else {
            return gst::PadProbeReturn::Ok;
        };
        let card = card_sink.query_caps(None);
        match check_input(caps_event.caps(), &card, &mode, framerate) {
            Some(InputCheck::Refuse(reason)) => {
                gst::element_error!(input, gst::StreamError::Format, ("{}", reason));
            }
            Some(InputCheck::Adapted(what)) => info!("DeckLink Output {}: {}", input.name(), what),
            None => {}
        }
        gst::PadProbeReturn::Ok
    });
}

/// What [`check_input`] found.
#[derive(Debug, PartialEq, Eq)]
enum InputCheck {
    /// The chain cannot adapt this input; the reason is for the operator.
    Refuse(String),
    /// The chain scales or changes the framerate, as described.
    Adapted(String),
}

/// Compare raw video `arrived` with `card`, the caps the card takes for
/// `mode`, and `framerate`, the mode's rate. Encoded video and memory types
/// are the input front's to report.
fn check_input(
    arrived: &gst::CapsRef,
    card: &gst::Caps,
    mode: &str,
    framerate: Option<gst::Fraction>,
) -> Option<InputCheck> {
    let input = arrived.structure(0)?;
    if input.name() != "video/x-raw" || card.is_any() || card.is_empty() {
        return None;
    }
    let interlace = input.get::<&str>("interlace-mode").unwrap_or("progressive");
    // Exact: nothing in the chain turns `mixed` or `alternate` into the
    // card's `interleaved` either.
    let takes = |s: &gst::StructureRef| match s.get::<&str>("interlace-mode") {
        Ok(wanted) => wanted == interlace,
        // A list or no field: the card takes any.
        Err(_) => true,
    };
    if !card.iter().any(takes) {
        let mut wanted: Vec<&str> = card
            .iter()
            .filter_map(|s| s.get::<&str>("interlace-mode").ok())
            .collect();
        wanted.dedup();
        let wanted = wanted.join(" or ");
        return Some(InputCheck::Refuse(format!(
            "DeckLink Output receives {} video, and mode {} needs {} video. \
             Nothing in the block interlaces or deinterlaces: set the block's mode \
             to one that matches the input, or change the video upstream.",
            interlace, mode, wanted
        )));
    }

    let mut changed = Vec::new();
    for name in ["width", "height"] {
        // Only a size every structure agrees on (a set mode, not `auto`).
        let mut sizes = card.iter().map(|s| s.get::<i32>(name).ok());
        let Some(Some(to)) = sizes.next() else {
            continue;
        };
        if sizes.any(|size| size != Some(to)) {
            continue;
        }
        if let Ok(from) = input.get::<i32>(name) {
            if from != to {
                changed.push(format!("{} {} -> {}", name, from, to));
            }
        }
    }
    if let (Ok(from), Some(to)) = (input.get::<gst::Fraction>("framerate"), framerate) {
        if from != to {
            changed.push(format!(
                "framerate {}/{} -> {}/{}",
                from.numer(),
                from.denom(),
                to.numer(),
                to.denom()
            ));
        }
    }
    (!changed.is_empty()).then(|| {
        InputCheck::Adapted(format!(
            "adapting the input to mode {}: {}",
            mode,
            changed.join(", ")
        ))
    })
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
            },
            ExposedProperty {
                name: "synchronized_capture".to_string(),
                label: "Synchronized Capture".to_string(),
                description: "Sample-align frames with other DeckLink Input blocks in this flow that also have synchronized capture enabled. The hardware arms all members atomically so their frames share a common time origin — useful for cross-port audio cancellation, mixing, or multi-camera capture from a single card. Requires a card supporting BMDDeckLinkSupportsSynchronizeToCaptureGroup (e.g. Quad 2, 8K Pro) and the patched gst-plugins-bad decklink plugin; silently ignored on stock builds or unsupported cards. Capture groups are flow-scoped: blocks in different flows cannot share a group (BMD's hardware-level sync requires all members to be armed in the same start cycle, which only happens within one pipeline). KNOWN BMD DRIVER QUIRK: stopping a synchronized-capture pipeline blocks for ~12 s before the stop completes; this is a Blackmagic driver-internal timeout reproducible in their own SDK examples, not a strom bug. Subsequent restart works normally.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "synchronized_capture".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                persist: None,
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
                    internal_element_id: VIDEO_INPUT_ELEMENT.to_string(),
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

/// Get metadata for DeckLink blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![decklink_input_definition(), decklink_output_definition()]
}

#[cfg(test)]
mod input_tests {
    use super::*;

    #[test]
    fn mode_framerates() {
        let rate = |m| mode_framerate(m).map(|f| (f.numer(), f.denom()));
        assert_eq!(rate("1080p25"), Some((25, 1)));
        assert_eq!(rate("1080p2997"), Some((30000, 1001)));
        assert_eq!(rate("1080p2398"), Some((24000, 1001)));
        assert_eq!(rate("1080i50"), Some((25, 1)));
        assert_eq!(rate("1080i5994"), Some((30000, 1001)));
        assert_eq!(rate("720p5994"), Some((60000, 1001)));
        assert_eq!(rate("2160p60"), Some((60, 1)));
        assert_eq!(rate("1080i60"), Some((30, 1)));
        assert_eq!(rate("auto"), None);
    }

    fn card(interlace: &str) -> gst::Caps {
        format!("video/x-raw,format=UYVY,width=1920,height=1080,interlace-mode={interlace}")
            .parse()
            .unwrap()
    }

    #[test]
    fn progressive_into_an_interlaced_mode_is_refused() {
        gst::init().unwrap();
        let arrived: gst::Caps = "video/x-raw,width=1920,height=1080,interlace-mode=progressive"
            .parse()
            .unwrap();
        let found = check_input(&arrived, &card("interleaved"), "1080i50", None);
        assert!(
            matches!(found, Some(InputCheck::Refuse(ref m)) if m.contains("progressive") && m.contains("1080i50")),
            "{found:?}"
        );
    }

    #[test]
    fn interlaced_into_a_progressive_mode_is_refused() {
        gst::init().unwrap();
        let arrived: gst::Caps = "video/x-raw,width=1920,height=1080,interlace-mode=interleaved"
            .parse()
            .unwrap();
        assert!(matches!(
            check_input(&arrived, &card("progressive"), "1080p25", None),
            Some(InputCheck::Refuse(_))
        ));
    }

    #[test]
    fn mixed_into_an_interlaced_mode_is_refused() {
        gst::init().unwrap();
        let arrived: gst::Caps = "video/x-raw,width=1920,height=1080,interlace-mode=mixed"
            .parse()
            .unwrap();
        assert!(matches!(
            check_input(&arrived, &card("interleaved"), "1080i50", None),
            Some(InputCheck::Refuse(ref m)) if m.contains("mixed")
        ));
    }

    #[test]
    fn a_size_and_rate_change_is_reported() {
        gst::init().unwrap();
        let arrived: gst::Caps = "video/x-raw,width=1280,height=720,framerate=30/1"
            .parse()
            .unwrap();
        assert_eq!(
            check_input(
                &arrived,
                &card("progressive"),
                "1080p25",
                Some(gst::Fraction::new(25, 1))
            ),
            Some(InputCheck::Adapted(
                "adapting the input to mode 1080p25: width 1280 -> 1920, height 720 -> 1080, \
                 framerate 30/1 -> 25/1"
                    .to_string()
            ))
        );
    }
}
