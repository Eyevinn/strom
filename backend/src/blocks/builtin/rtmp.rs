//! RTMP output block.
//!
//! Publishes a programme to an RTMP server, muxed as FLV.
//!
//! Chain, built dynamically once the input caps are known:
//! - video: `h264parse` → `flvmux`
//! - audio, raw in: `audioconvert` → `audioresample` → `avenc_aac` → `aacparse` → `flvmux`
//! - audio, AAC in: `aacparse` → `flvmux`
//!
//! and `flvmux` → `rtmp2sink`.
//!
//! # Why the chains are built on caps rather than statically
//!
//! Two reasons, and the second is the one that bites.
//!
//! FLV carries a small closed set of codecs, so the muxer needs H.264 and AAC
//! whatever arrives. The audio side therefore has to encode raw input, because
//! **no block produces AAC as its output**: `avenc_aac` and `opusenc` exist
//! only inside `builtin.mpegtssrt_output`, `builtin.whip_output` and
//! `builtin.efpsrt_output` as internal chains, and there is no audio encoder
//! block the way `builtin.videoenc` is one for video.
//!
//! So a block that demanded encoded AAC on its pad could be fed only by an
//! input block in passthrough mode, where the encoded stream comes from
//! upstream rather than from Strom: `builtin.mpegtssrt_input`, or
//! `builtin.media_player`, whose passthrough pipeline is
//! `urisourcebin(parse-streams=true)` and so emits AAC from an AAC-bearing
//! file. AAC is the criterion rather than encoded audio, which is why
//! `builtin.efpsrt_input` is not a third: its passthrough carries Opus, and FLV
//! cannot. Neither carries a mixed programme, which is the point: `builtin.mixer`
//! outputs raw audio, so encoding inside this block is what makes it usable
//! downstream of mixing at all.
//!
//! And an aggregator's sink pad requested at build time for an input that never
//! carries data means `flvmux` never aggregates and nothing reaches the sink. So
//! a pad is requested only when its input proves it has data, which is the same
//! reason `builtin.mpegtssrt_output` links on caps and says so at its own
//! `internal_links`.
//!
//! # Video is expected to arrive encoded
//!
//! Deliberately asymmetric with audio, and for a reason rather than by
//! omission: `builtin.videoenc` already exposes encoded H.264 on an
//! `encoded_out` pad, so the operator has a block for it, and encoding video
//! inside an output block would hide a codec choice that belongs in the graph.
//! Raw video is refused with a message naming that block.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info};

/// The RTMP sink element. One element, no fallback.
///
/// `rtmp2sink` rather than `rtmpsink`: both take the URL in a `location`
/// property, but `rtmpsink` is built against librtmp and is absent from some
/// builds of `gst-plugins-bad`, while `rtmp2sink` has no external dependency and
/// is what upstream points at now.
const RTMP_SINK_FACTORY: &str = "rtmp2sink";

/// The package that ships the RTMP sink on this platform.
///
/// Same shape as `ice_package_hint`, so the operator-facing message names what
/// to install rather than what is missing.
pub const fn rtmp_package_hint() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "brew install gst-plugins-bad"
    }
    #[cfg(target_os = "windows")]
    {
        "the GStreamer MSI installer's full package set"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "gstreamer1.0-plugins-bad (Debian/Ubuntu), gstreamer1-plugins-bad-free (Fedora) \
         or gst-plugins-bad (Arch)"
    }
}

/// The message shown when the RTMP sink is unavailable.
///
/// Kept separate from the check so its wording is testable on a host where the
/// element is present.
pub fn rtmp_missing_message() -> String {
    format!(
        "RTMP Output needs the GStreamer {} element, which this installation does \
         not have. Install it with: {}",
        RTMP_SINK_FACTORY,
        rtmp_package_hint()
    )
}

/// Refuse to build when the RTMP sink is unavailable.
fn require_rtmp_sink() -> Result<(), BlockBuildError> {
    if gst::ElementFactory::find(RTMP_SINK_FACTORY).is_some() {
        return Ok(());
    }
    Err(BlockBuildError::MissingPlugin(rtmp_missing_message()))
}

/// RTMP Output block builder.
pub struct RtmpOutputBuilder;

impl BlockBuilder for RtmpOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building RTMP Output block instance: {}", instance_id);
        require_rtmp_sink()?;

        let location = properties
            .get("location")
            .and_then(|v| match v {
                PropertyValue::String(s) if !s.trim().is_empty() => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_RTMP_LOCATION.to_string());

        let sync = properties
            .get("sync")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // FLV muxer. streamable=true writes no seek table and no duration: the
        // non-streamable form rewrites the header at end of file, and a live
        // stream has no end.
        let mux_id = format!("{}:rtmp_flvmux", instance_id);
        let mux = gst::ElementFactory::make("flvmux")
            .name(&mux_id)
            .property("streamable", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("flvmux: {}", e)))?;

        let sink_id = format!("{}:rtmp_sink", instance_id);
        let sink = gst::ElementFactory::make(RTMP_SINK_FACTORY)
            .name(&sink_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("{}: {}", RTMP_SINK_FACTORY, e))
            })?;
        sink.set_property("location", &location);
        sink.set_property("sync", sync);
        // async=false: don't block pipeline preroll waiting for the first
        // buffer. Without a reachable RTMP server the sink would otherwise hold
        // PAUSED->PLAYING indefinitely. Matches the SRT, WHEP, WHIP and AES67
        // sinks; block-built elements bypass add_element, so nothing sets this
        // for us.
        sink.set_property("async", false);
        sink.set_property("qos", true);

        info!(
            "RTMP Output configured: location={}, sync={}, sink={}",
            location, sync, RTMP_SINK_FACTORY
        );

        let mux_weak = mux.downgrade();

        // Video input. An identity, with the parser inserted and linked once the
        // caps arrive; see the module docs for why nothing is linked statically.
        let video_input_id = format!("{}:rtmp_video_input", instance_id);
        let video_input = gst::ElementFactory::make("identity")
            .name(&video_input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("video identity: {}", e)))?;

        if let Some(src_pad) = video_input.static_pad("src") {
            let mux_weak_clone = mux_weak.clone();
            let instance = instance_id.to_string();
            let inserted = Arc::new(AtomicBool::new(false));
            src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(caps) = caps_from_probe(info) else {
                    return gst::PadProbeReturn::Ok;
                };
                if inserted.swap(true, Ordering::SeqCst) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some((bin, mux)) = bin_and_mux(&mux_weak_clone, &instance) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(structure) = caps.structure(0) else {
                    error!("RTMP {}: no structure in video caps", instance);
                    return gst::PadProbeReturn::Ok;
                };
                let caps_name = structure.name().to_string();
                debug!("RTMP {}: video caps detected: {}", instance, caps_name);

                let result = video_plan(&caps_name)
                    .and_then(|()| build_video_chain(&bin, &mux, pad, &instance));
                if let Err(e) = result {
                    error!("RTMP {}: {}", instance, e);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Audio input. Raw is encoded here, AAC is parsed only; see the module
        // docs for why the encode lives inside the block.
        let audio_input_id = format!("{}:rtmp_audio_input", instance_id);
        let audio_input = gst::ElementFactory::make("identity")
            .name(&audio_input_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audio identity: {}", e)))?;

        if let Some(src_pad) = audio_input.static_pad("src") {
            let mux_weak_clone = mux_weak.clone();
            let instance = instance_id.to_string();
            let inserted = Arc::new(AtomicBool::new(false));
            src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(caps) = caps_from_probe(info) else {
                    return gst::PadProbeReturn::Ok;
                };
                if inserted.swap(true, Ordering::SeqCst) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some((bin, mux)) = bin_and_mux(&mux_weak_clone, &instance) else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(structure) = caps.structure(0) else {
                    error!("RTMP {}: no structure in audio caps", instance);
                    return gst::PadProbeReturn::Ok;
                };
                let caps_name = structure.name().to_string();
                debug!("RTMP {}: audio caps detected: {}", instance, caps_name);

                let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(4);
                let layer = structure.get::<i32>("layer").unwrap_or(3);
                let result =
                    audio_plan(&caps_name, mpegversion, layer).and_then(|plan| match plan {
                        AudioPlan::Encode => build_raw_audio_chain(&bin, &mux, pad, &instance),
                        AudioPlan::Parse => build_aac_audio_chain(&bin, &mux, pad, &instance),
                    });
                if let Err(e) = result {
                    error!("RTMP {}: {}", instance, e);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Only the mux to sink link is static. Both mux sink pads are requested
        // from inside the probes, once their input has proved it has data.
        let internal_links = vec![(
            ElementPadRef::pad(&mux_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        )];

        Ok(BlockBuildResult {
            elements: vec![
                (video_input_id, video_input),
                (audio_input_id, audio_input),
                (mux_id, mux),
                (sink_id, sink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// What this block does with an audio stream, once its caps are known.
#[derive(Debug, PartialEq, Eq)]
pub enum AudioPlan {
    /// Raw input: encode to AAC inside the block. See the module docs for why.
    Encode,
    /// Already AAC: parse only, so the encoder is not run twice.
    Parse,
}

/// Decide what to do with video caps, or refuse with the operator-facing reason.
///
/// Split out from the pad probe so the decision can be tested without a
/// pipeline. The wiring it leads to still needs a running flow.
pub fn video_plan(caps_name: &str) -> Result<(), String> {
    match caps_name {
        "video/x-h264" => Ok(()),
        "video/x-raw" => Err(
            "RTMP Output needs H.264 video, but its video input is raw. \
                              Place a builtin.videoenc block before it and link its \
                              encoded_out pad"
                .to_string(),
        ),
        other => Err(format!(
            "RTMP Output needs H.264 video, but its video input carries {}. \
             FLV cannot carry that codec",
            other
        )),
    }
}

/// Decide what to do with audio caps, or refuse with the operator-facing reason.
///
/// `mpegversion` and `layer` are only read for `audio/mpeg`, and both are
/// optional on the wire, so callers pass the value to assume when absent rather than an
/// `Option`. FLV does carry MPEG-1 layer 3 at 5512, 8000, 11025, 22050 and
/// 44100 Hz, per `flvmux`'s own sink caps, but this block does not implement
/// that path: it would need a rate check and a resample, and every producer we
/// care about emits AAC or raw.
pub fn audio_plan(caps_name: &str, mpegversion: i32, layer: i32) -> Result<AudioPlan, String> {
    match caps_name {
        "audio/x-raw" => Ok(AudioPlan::Encode),
        "audio/mpeg" if mpegversion == 2 || mpegversion == 4 => Ok(AudioPlan::Parse),
        "audio/mpeg" if mpegversion == 1 && layer == 3 => Err(
            "RTMP Output needs AAC or raw audio, but its audio input carries MP3. \
                 FLV can carry MP3 and this block does not implement that path"
                .to_string(),
        ),
        "audio/mpeg" => Err(format!(
            "RTMP Output needs AAC or raw audio, but its audio input carries MPEG-{} \
             audio layer {}, which FLV cannot carry",
            mpegversion, layer
        )),
        other => Err(format!(
            "RTMP Output needs AAC or raw audio, but its audio input carries {}",
            other
        )),
    }
}

/// The caps carried by a downstream CAPS event, or `None` for any other event.
fn caps_from_probe(info: &gst::PadProbeInfo) -> Option<gst::Caps> {
    let Some(gst::PadProbeData::Event(event)) = &info.data else {
        return None;
    };
    if event.type_() != gst::EventType::Caps {
        return None;
    }
    match event.view() {
        gst::EventView::Caps(caps_event) => Some(caps_event.caps().to_owned()),
        _ => None,
    }
}

/// The pipeline bin and the muxer, or `None` once either has gone away.
fn bin_and_mux(
    mux_weak: &gst::glib::WeakRef<gst::Element>,
    instance_id: &str,
) -> Option<(gst::Bin, gst::Element)> {
    let mux = mux_weak.upgrade()?;
    let Some(parent) = mux.parent() else {
        error!("RTMP {}: mux has no parent", instance_id);
        return None;
    };
    match parent.downcast::<gst::Bin>() {
        Ok(bin) => Some((bin, mux)),
        Err(_) => {
            error!("RTMP {}: mux parent is not a Bin", instance_id);
            None
        }
    }
}

/// Request one of flvmux's fixed-name sink pads.
///
/// `flvmux` names them `video` and `audio` rather than following a `%u`
/// template, so they are requested by name.
fn request_mux_pad(mux: &gst::Element, name: &str) -> Result<gst::Pad, String> {
    mux.request_pad_simple(name)
        .ok_or_else(|| format!("flvmux refused a '{}' pad", name))
}

/// Video: `h264parse` into the muxer.
///
/// `config-interval=-1` repeats SPS and PPS on every keyframe, which is what
/// lets a viewer who joins mid-stream decode at all. Without it the stream looks
/// broken to every late joiner and the fault reads as a network problem.
fn build_video_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let parser_name = format!("{}:rtmp_h264parse", instance_id);
    let parser = gst::ElementFactory::make("h264parse")
        .name(&parser_name)
        .property("config-interval", -1i32)
        .build()
        .map_err(|e| format!("h264parse: {}", e))?;

    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = request_mux_pad(mux, "video")?;

    identity_src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link identity -> h264parse: {:?}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link h264parse -> flvmux: {:?}", e))?;

    info!(
        "RTMP {}: video chain linked: identity -> h264parse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Audio, raw in: convert, resample, encode to AAC, parse, into the muxer.
fn build_raw_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let convert_name = format!("{}:rtmp_audio_convert", instance_id);
    let resample_name = format!("{}:rtmp_audio_resample", instance_id);
    let encoder_name = format!("{}:rtmp_audio_encoder", instance_id);
    let parser_name = format!("{}:rtmp_aacparse", instance_id);

    let convert = gst::ElementFactory::make("audioconvert")
        .name(&convert_name)
        .build()
        .map_err(|e| format!("audioconvert: {}", e))?;
    let resample = gst::ElementFactory::make("audioresample")
        .name(&resample_name)
        .build()
        .map_err(|e| format!("audioresample: {}", e))?;
    let encoder = gst::ElementFactory::make("avenc_aac")
        .name(&encoder_name)
        .build()
        .map_err(|e| format!("avenc_aac: {}", e))?;
    let parser = gst::ElementFactory::make("aacparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("aacparse: {}", e))?;

    bin.add_many([&convert, &resample, &encoder, &parser])
        .map_err(|e| format!("add elements: {}", e))?;
    for element in [&convert, &resample, &encoder, &parser] {
        element
            .sync_state_with_parent()
            .map_err(|e| format!("sync {}: {}", element.name(), e))?;
    }

    let convert_sink = convert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = request_mux_pad(mux, "audio")?;

    identity_src_pad
        .link(&convert_sink)
        .map_err(|e| format!("link identity -> audioconvert: {:?}", e))?;
    convert
        .link(&resample)
        .map_err(|e| format!("link audioconvert -> audioresample: {}", e))?;
    resample
        .link(&encoder)
        .map_err(|e| format!("link audioresample -> avenc_aac: {}", e))?;
    encoder
        .link(&parser)
        .map_err(|e| format!("link avenc_aac -> aacparse: {}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link aacparse -> flvmux: {:?}", e))?;

    info!(
        "RTMP {}: audio chain linked: identity -> audioconvert -> audioresample -> \
         avenc_aac -> aacparse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Audio, AAC in: parse only, into the muxer.
fn build_aac_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
) -> Result<(), String> {
    let parser_name = format!("{}:rtmp_aacparse", instance_id);
    let parser = gst::ElementFactory::make("aacparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("aacparse: {}", e))?;

    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;
    let mux_sink = request_mux_pad(mux, "audio")?;

    identity_src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link identity -> aacparse: {:?}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link aacparse -> flvmux: {:?}", e))?;

    info!(
        "RTMP {}: audio chain linked: identity -> aacparse -> flvmux ({})",
        instance_id,
        mux_sink.name()
    );
    Ok(())
}

/// Get metadata for RTMP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![rtmp_output_definition()]
}

/// Get RTMP Output block definition (metadata only).
fn rtmp_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.rtmp_output".to_string(),
        name: "RTMP Output".to_string(),
        description: "Publish a programme to an RTMP server, muxed as FLV. Takes H.264 \
                      video, so place a Video Encoder before it; audio may be raw or AAC \
                      and is encoded here when raw."
            .to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "location".to_string(),
                label: "RTMP URL".to_string(),
                description: "Where to publish, for example rtmp://host:1935/live/streamkey."
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_RTMP_LOCATION.to_string())),
                mapping: PropertyMapping {
                    element_id: "rtmp_sink".to_string(),
                    property_name: "location".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "sync".to_string(),
                label: "Synchronise to clock".to_string(),
                description: "Pace output against the pipeline clock. Turn off when the input \
                              carries a remote encoder's timestamps, which otherwise makes the \
                              sink believe it is behind and drop frames."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "rtmp_sink".to_string(),
                    property_name: "sync".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "rtmp_video_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "rtmp_audio_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(1.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}
