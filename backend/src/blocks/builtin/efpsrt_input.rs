//! EFP over SRT input block builder.
//!
//! This block receives an SRT stream carrying EFP (Elastic Frame Protocol) and demuxes
//! it into separate video and audio output pads, plus an optional embedded-data
//! output carrying efpdemux's `embedded` pad (default: 0 data tracks).
//!
//! Pipeline structure (decode=true, default):
//! ```text
//! srtsrc -> efpdemux -> h264parse -> nvh264dec/avdec_h264 -> video_output (identity)
//!                    -> opusdec -> audioconvert -> audioresample -> audio_output_0 (identity)
//! ```
//!
//! Pipeline structure (decode=false, passthrough):
//! ```text
//! srtsrc -> efpdemux -> h264parse -> video_output (identity) -> [external video_out]
//!                    -> audio_output_0 (identity) -> [external audio_out_0]
//! ```
//!
//! Decode mode uses explicit decoder elements (not decodebin) to avoid the
//! chain-completeness / no-more-pads issues that prevent decodebin from
//! exposing its output pads with live EFP streams.
//!
//! No videoconvert is inserted in the decoded video path to preserve GPU memory
//! (e.g. CUDAMemory from nvh264dec) for downstream elements.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, FlowId, PropertyValue, *};
use tracing::{debug, error, warn};

/// Caps name of `efpdemux`'s embedded-data src pad.
const EFP_EMBEDDED_CAPS_NAME: &str = "application/x-efp-embedded";

/// EFP/SRT Input block builder.
pub struct EfpSrtInputBuilder;

impl BlockBuilder for EfpSrtInputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let num_video_tracks = properties
            .get("num_video_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(1);

        let num_audio_tracks = properties
            .get("num_audio_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(1);

        let num_data_tracks = properties
            .get("num_data_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(0);

        let mut outputs = Vec::new();

        for i in 0..num_video_tracks {
            outputs.push(ExternalPad {
                label: Some(format!("V{}", i)),
                name: if num_video_tracks == 1 {
                    "video_out".to_string()
                } else {
                    format!("video_out_{}", i)
                },
                media_type: MediaType::Video,
                internal_element_id: if num_video_tracks == 1 {
                    "video_output".to_string()
                } else {
                    format!("video_output_{}", i)
                },
                internal_pad_name: "src".to_string(),
            });
        }

        for i in 0..num_audio_tracks {
            outputs.push(ExternalPad {
                label: Some(format!("A{}", i)),
                name: format!("audio_out_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_output_{}", i),
                internal_pad_name: "src".to_string(),
            });
        }

        for i in 0..num_data_tracks {
            outputs.push(ExternalPad {
                label: Some(format!("D{}", i)),
                name: format!("data_out_{}", i),
                media_type: MediaType::Generic,
                internal_element_id: format!("data_output_{}", i),
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
        let decode = properties
            .get("decode")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        debug!(
            "Building EFP/SRT Input block instance: {} (decode={})",
            instance_id, decode
        );

        let srt_uri = properties
            .get("srt_uri")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_SRT_INPUT_URI.to_string());

        let latency = properties
            .get("latency")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_LATENCY_MS);

        let bucket_timeout = properties
            .get("bucket_timeout")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as u32),
                PropertyValue::Int(i) => Some(*i as u32),
                _ => None,
            })
            .unwrap_or(DEFAULT_EFP_BUCKET_TIMEOUT);

        let hol_timeout = properties
            .get("hol_timeout")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as u32),
                PropertyValue::Int(i) => Some(*i as u32),
                _ => None,
            })
            .unwrap_or(DEFAULT_EFP_HOL_TIMEOUT);

        let normalize_segment = properties
            .get("normalize_segment")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "auto".to_string());
        if !matches!(normalize_segment.as_str(), "auto" | "always" | "never") {
            return Err(BlockBuildError::InvalidProperty(format!(
                "normalize_segment must be one of 'auto', 'always', 'never' (got '{}')",
                normalize_segment
            )));
        }

        let num_video_tracks = properties
            .get("num_video_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(1);

        let num_audio_tracks = properties
            .get("num_audio_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(1);

        let num_data_tracks = properties
            .get("num_data_tracks")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as usize),
                PropertyValue::Int(i) => Some(*i as usize),
                _ => None,
            })
            .unwrap_or(0);

        // Create srtsrc
        let src_id = format!("{}:srtsrc", instance_id);
        let srtsrc = gst::ElementFactory::make("srtsrc")
            .name(&src_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("srtsrc: {}", e)))?;

        srtsrc.set_property("uri", &srt_uri);
        srtsrc.set_property("latency", latency);

        let keep_listening = properties
            .get("keep_listening")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_KEEP_LISTENING);

        if srtsrc.has_property("keep-listening") {
            srtsrc.set_property("keep-listening", keep_listening);
        }

        let auto_reconnect = properties
            .get("auto_reconnect")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_AUTO_RECONNECT);

        if srtsrc.has_property("auto-reconnect") {
            srtsrc.set_property("auto-reconnect", auto_reconnect);
        }

        let wait_for_connection = properties
            .get("wait_for_connection")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_WAIT_FOR_CONNECTION);

        if srtsrc.has_property("wait-for-connection") {
            srtsrc.set_property("wait-for-connection", wait_for_connection);
        }

        debug!(
            "SRT source configured: uri={}, latency={}ms, keep-listening={}, auto-reconnect={}, wait-for-connection={}",
            srt_uri, latency, keep_listening, auto_reconnect, wait_for_connection
        );

        // Always use efpdemux directly. In decode mode, explicit decoder elements
        // are created dynamically in the pad-added handler.
        let demux_id = format!("{}:efpdemux", instance_id);
        let demux_element = gst::ElementFactory::make("efpdemux")
            .name(&demux_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("efpdemux: {}", e)))?;
        demux_element.set_property("bucket-timeout", bucket_timeout);
        demux_element.set_property("hol-timeout", hol_timeout);
        if demux_element.has_property("normalize-segment") {
            demux_element.set_property_from_str("normalize-segment", &normalize_segment);
        } else {
            warn!(
                "EFP demuxer does not expose 'normalize-segment' (requires gst-plugin-efp >= 0.2.6); \
                 ignoring normalize_segment={}",
                normalize_segment
            );
        }

        debug!(
            "EFP demuxer configured: bucket-timeout={}, hol-timeout={}, normalize-segment={}",
            bucket_timeout, hol_timeout, normalize_segment
        );

        let mut elements = vec![
            (src_id.clone(), srtsrc),
            (demux_id.clone(), demux_element.clone()),
        ];

        // Create video output identity elements
        let mut video_guards = Vec::new();
        for i in 0..num_video_tracks {
            let element_id = if num_video_tracks == 1 {
                format!("{}:video_output", instance_id)
            } else {
                format!("{}:video_output_{}", instance_id, i)
            };
            let identity = gst::ElementFactory::make("identity")
                .name(&element_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video identity {}: {}", i, e))
                })?;
            let guard = Arc::new(AtomicBool::new(false));
            video_guards.push((identity.downgrade(), guard));
            elements.push((element_id, identity));
        }

        // Create audio output identity elements
        let mut audio_guards = Vec::new();
        for i in 0..num_audio_tracks {
            let element_id = format!("{}:audio_output_{}", instance_id, i);
            let identity = gst::ElementFactory::make("identity")
                .name(&element_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audio identity {}: {}", i, e))
                })?;
            let guard = Arc::new(AtomicBool::new(false));
            audio_guards.push((identity.downgrade(), guard));
            elements.push((element_id, identity));
        }

        // Create embedded-data output identity elements. efpdemux exposes a
        // single `embedded` src pad for all data types, so only the first of
        // these is ever linked.
        let mut data_guards = Vec::new();
        for i in 0..num_data_tracks {
            let element_id = format!("{}:data_output_{}", instance_id, i);
            let identity = gst::ElementFactory::make("identity")
                .name(&element_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("data identity {}: {}", i, e))
                })?;
            let guard = Arc::new(AtomicBool::new(false));
            data_guards.push((identity.downgrade(), guard));
            elements.push((element_id, identity));
        }

        // Setup dynamic pad linking on efpdemux pad-added.
        // - decode mode: video via h264parse + decoder, audio via opusdec + audioconvert + audioresample.
        // - passthrough mode: video via h264parse, audio linked directly.
        let instance_id_clone = instance_id.to_string();
        let mode_label = if decode { "decode" } else { "passthrough" };
        let mode_label_owned = mode_label.to_string();

        demux_element.connect_pad_added(move |element, pad| {
            let caps = pad.current_caps().or_else(|| {
                let query_caps = pad.query_caps(None);
                if !query_caps.is_any() && !query_caps.is_empty() {
                    Some(query_caps)
                } else {
                    None
                }
            });

            let caps_name = caps
                .as_ref()
                .and_then(|c| c.structure(0))
                .map(|s| s.name().to_string());

            let pad_name = pad.name().to_string();

            let is_video = caps_name
                .as_ref()
                .map(|n| n.starts_with("video/"))
                .unwrap_or(false);
            let is_audio = caps_name
                .as_ref()
                .map(|n| n.starts_with("audio/"))
                .unwrap_or(false);
            let is_data = caps_name
                .as_deref()
                .map(|n| n == EFP_EMBEDDED_CAPS_NAME)
                .unwrap_or(false);

            debug!(
                "EFPSRT Input {} ({}): pad added: {} (caps: {})",
                instance_id_clone,
                mode_label_owned,
                pad_name,
                caps_name.as_deref().unwrap_or("unknown")
            );

            if is_video {
                for (weak_identity, guard) in &video_guards {
                    if guard.swap(true, Ordering::SeqCst) {
                        continue;
                    }

                    if let Some(identity) = weak_identity.upgrade() {
                        if decode {
                            // Decode mode: h264parse -> nvh264dec/avdec_h264 -> identity
                            // No videoconvert to preserve GPU memory from hardware decoders.
                            if let Err(e) =
                                link_decoded_video(element, pad, &identity, &instance_id_clone)
                            {
                                error!(
                                    "EFPSRT Input {}: Failed to link decoded video pad {}: {}",
                                    instance_id_clone, pad_name, e
                                );
                                guard.store(false, Ordering::SeqCst);
                                continue;
                            }
                        } else {
                            // Passthrough mode: insert h264parse
                            if let Err(e) =
                                link_passthrough_video(element, pad, &identity, &instance_id_clone)
                            {
                                error!(
                                    "EFPSRT Input {}: Failed to link passthrough video pad {}: {}",
                                    instance_id_clone, pad_name, e
                                );
                                guard.store(false, Ordering::SeqCst);
                                continue;
                            }
                        }
                        debug!(
                            "EFPSRT Input {}: Linked video pad {} -> {}",
                            instance_id_clone,
                            pad_name,
                            identity.name()
                        );
                        return;
                    }
                }
                warn!(
                    "EFPSRT Input {}: No available video output for pad {}",
                    instance_id_clone, pad_name
                );
            } else if is_audio {
                for (weak_identity, guard) in &audio_guards {
                    if guard.swap(true, Ordering::SeqCst) {
                        continue;
                    }

                    if let Some(identity) = weak_identity.upgrade() {
                        if decode {
                            // Decode mode: opusdec -> audioconvert -> audioresample -> identity
                            if let Err(e) =
                                link_decoded_audio(element, pad, &identity, &instance_id_clone)
                            {
                                error!(
                                    "EFPSRT Input {}: Failed to link decoded audio pad {}: {}",
                                    instance_id_clone, pad_name, e
                                );
                                guard.store(false, Ordering::SeqCst);
                                continue;
                            }
                        } else {
                            // Passthrough mode: link directly
                            if let Some(sink_pad) = identity.static_pad("sink") {
                                if let Err(e) = pad.link(&sink_pad) {
                                    error!(
                                        "EFPSRT Input {}: Failed to link audio pad {}: {:?}",
                                        instance_id_clone, pad_name, e
                                    );
                                    guard.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            }
                        }
                        debug!(
                            "EFPSRT Input {}: Linked audio pad {} -> {}",
                            instance_id_clone,
                            pad_name,
                            identity.name()
                        );
                        return;
                    }
                }
                warn!(
                    "EFPSRT Input {}: No available audio output for pad {}",
                    instance_id_clone, pad_name
                );
            } else if is_data {
                // Embedded data is opaque bytes — nothing to parse or decode in
                // either mode, so it is always linked straight through.
                for (weak_identity, guard) in &data_guards {
                    if guard.swap(true, Ordering::SeqCst) {
                        continue;
                    }

                    if let Some(identity) = weak_identity.upgrade() {
                        let sink_pad = match identity.static_pad("sink") {
                            Some(p) => p,
                            None => {
                                guard.store(false, Ordering::SeqCst);
                                continue;
                            }
                        };
                        if let Err(e) = pad.link(&sink_pad) {
                            error!(
                                "EFPSRT Input {}: Failed to link embedded-data pad {}: {:?}",
                                instance_id_clone, pad_name, e
                            );
                            guard.store(false, Ordering::SeqCst);
                            continue;
                        }
                        debug!(
                            "EFPSRT Input {}: Linked embedded-data pad {} -> {}",
                            instance_id_clone,
                            pad_name,
                            identity.name()
                        );
                        return;
                    }
                }
                warn!(
                    "EFPSRT Input {}: No available data output for pad {}",
                    instance_id_clone, pad_name
                );
            } else {
                debug!(
                    "EFPSRT Input {}: Ignoring pad {} with caps {}",
                    instance_id_clone,
                    pad_name,
                    caps_name.as_deref().unwrap_or("unknown")
                );
            }
        });

        // Internal link: srtsrc -> efpdemux
        let internal_links = vec![(
            ElementPadRef::pad(&src_id, "src"),
            ElementPadRef::pad(&demux_id, "sink"),
        )];

        debug!(
            "Created EFP/SRT Input block ({}) with {} video output(s) and {} audio output(s)",
            mode_label, num_video_tracks, num_audio_tracks
        );

        // If the operator asked for `never`, absolute PTS must survive as running-time.
        // That only works when the pipeline clock is globally meaningful (realtime/TAI
        // or a network clock). Monotonic pipeline clock + normalize=never = nonsense
        // timestamps — warn the operator at pipeline-start.
        let bus_message_handler = if normalize_segment == "never" {
            let instance_id_for_handler = instance_id.to_string();
            let demux_weak = demux_element.downgrade();
            Some(Box::new(
                move |bus: &gst::Bus, _flow_id: FlowId, _events: EventBroadcaster| {
                    connect_clock_check_handler(bus, instance_id_for_handler, demux_weak)
                },
            ) as crate::blocks::BusMessageConnectFn)
        } else {
            None
        };

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler,
            pad_properties: HashMap::new(),
        })
    }
}

/// Dynamically insert h264parse + video decoder between an efpdemux video pad and identity.
/// efpdemux pad -> h264parse -> nvh264dec/avdec_h264 -> deinterlace -> identity
fn link_decoded_video(
    element: &gst::Element,
    src_pad: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
) -> Result<(), String> {
    let bin = element
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("parent is not a Bin")?;

    let parser_name = format!("{}:video_parser_{}", instance_id, src_pad.name());
    let parser = gst::ElementFactory::make("h264parse")
        .name(&parser_name)
        .property("config-interval", 1i32)
        .build()
        .map_err(|e| format!("h264parse: {}", e))?;

    // Try hardware decoder first (nvh264dec), fall back to software (avdec_h264)
    let decoder_name = format!("{}:video_decoder_{}", instance_id, src_pad.name());
    let decoder = gst::ElementFactory::make("nvh264dec")
        .name(&decoder_name)
        .build()
        .or_else(|_| {
            gst::ElementFactory::make("avdec_h264")
                .name(&decoder_name)
                .build()
        })
        .map_err(|e| format!("video decoder (nvh264dec/avdec_h264): {}", e))?;

    bin.add_many([&parser, &decoder])
        .map_err(|e| format!("add video decode chain: {}", e))?;

    // Link downstream: h264parse -> decoder
    parser
        .link(&decoder)
        .map_err(|e| format!("link h264parse -> decoder: {}", e))?;

    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync h264parse: {}", e))?;
    decoder
        .sync_state_with_parent()
        .map_err(|e| format!("sync decoder: {}", e))?;

    // decoder -> deinterlace -> identity. Interlaced broadcast feeds (e.g.
    // 1080i50) must be deinterlaced before the GL vision mixer; the helper's
    // caps probe forces deinterlacing only when the source is interlaced/mixed.
    let decoder_src = decoder.static_pad("src").ok_or("decoder has no src pad")?;
    super::decode_chain::link_raw_video_through_deinterlace(
        &bin,
        &decoder_src,
        identity,
        instance_id,
        &src_pad.name(),
    )?;

    // Link source pad last
    let parser_sink = parser
        .static_pad("sink")
        .ok_or("h264parse has no sink pad")?;
    src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link efpdemux -> h264parse: {:?}", e))?;

    debug!(
        "EFPSRT Input {}: Linked video decode chain: {} -> h264parse -> {} -> {}",
        instance_id,
        src_pad.name(),
        decoder
            .factory()
            .map(|f| f.name().to_string())
            .unwrap_or_default(),
        identity.name()
    );
    Ok(())
}

/// Dynamically insert opusdec + audioconvert + audioresample between an efpdemux audio pad and identity.
/// efpdemux pad -> opusdec -> audioconvert -> audioresample -> identity
fn link_decoded_audio(
    element: &gst::Element,
    src_pad: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
) -> Result<(), String> {
    let bin = element
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("parent is not a Bin")?;

    let dec_name = format!("{}:opusdec_{}", instance_id, src_pad.name());
    let convert_name = format!("{}:audioconvert_{}", instance_id, src_pad.name());
    let resample_name = format!("{}:audioresample_{}", instance_id, src_pad.name());

    let opusdec = gst::ElementFactory::make("opusdec")
        .name(&dec_name)
        .build()
        .map_err(|e| format!("opusdec: {}", e))?;
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&convert_name)
        .build()
        .map_err(|e| format!("audioconvert: {}", e))?;
    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&resample_name)
        .build()
        .map_err(|e| format!("audioresample: {}", e))?;

    bin.add_many([&opusdec, &audioconvert, &audioresample])
        .map_err(|e| format!("add audio decode chain: {}", e))?;

    // Link downstream: opusdec -> audioconvert -> audioresample -> identity
    opusdec
        .link(&audioconvert)
        .map_err(|e| format!("link opusdec -> audioconvert: {}", e))?;
    audioconvert
        .link(&audioresample)
        .map_err(|e| format!("link audioconvert -> audioresample: {}", e))?;
    let resample_src = audioresample
        .static_pad("src")
        .ok_or("audioresample has no src pad")?;
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;
    resample_src
        .link(&identity_sink)
        .map_err(|e| format!("link audioresample -> identity: {:?}", e))?;

    opusdec
        .sync_state_with_parent()
        .map_err(|e| format!("sync opusdec: {}", e))?;
    audioconvert
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioconvert: {}", e))?;
    audioresample
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioresample: {}", e))?;

    // Link source pad last
    let dec_sink = opusdec
        .static_pad("sink")
        .ok_or("opusdec has no sink pad")?;
    src_pad
        .link(&dec_sink)
        .map_err(|e| format!("link efpdemux -> opusdec: {:?}", e))?;

    debug!(
        "EFPSRT Input {}: Linked audio decode chain: {} -> opusdec -> audioconvert -> audioresample -> {}",
        instance_id,
        src_pad.name(),
        identity.name()
    );
    Ok(())
}

/// Dynamically insert h264parse between an efpdemux video pad and an identity element (passthrough mode).
/// efpdemux pad -> h264parse (config-interval=1) -> identity
fn link_passthrough_video(
    element: &gst::Element,
    src_pad: &gst::Pad,
    identity: &gst::Element,
    instance_id: &str,
) -> Result<(), String> {
    let bin = element
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("parent is not a Bin")?;

    let parser_name = format!("{}:video_parser_{}", instance_id, src_pad.name());
    let parser = gst::ElementFactory::make("h264parse")
        .name(&parser_name)
        .property("config-interval", 1i32)
        .build()
        .map_err(|e| format!("h264parse: {}", e))?;

    bin.add(&parser)
        .map_err(|e| format!("add h264parse: {}", e))?;

    // Link downstream first: h264parse -> identity
    let parser_src = parser.static_pad("src").ok_or("h264parse has no src pad")?;
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;
    parser_src
        .link(&identity_sink)
        .map_err(|e| format!("link h264parse -> identity: {:?}", e))?;

    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync h264parse: {}", e))?;

    // Link source pad last to start data flow when chain is ready
    let parser_sink = parser
        .static_pad("sink")
        .ok_or("h264parse has no sink pad")?;
    src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link efpdemux -> h264parse: {:?}", e))?;

    debug!(
        "EFPSRT Input {}: Inserted h264parse (config-interval=1) for pad {}",
        instance_id,
        src_pad.name()
    );
    Ok(())
}

/// Connect a handler that inspects the pipeline clock once the pipeline enters
/// Playing, and warns if the operator chose `normalize_segment=never` while
/// running on a monotonic clock. That combination produces running-times
/// anchored to a sender-local wallclock with a receiver-local monotonic base
/// — i.e. nonsense — and is almost always a configuration error.
fn connect_clock_check_handler(
    bus: &gst::Bus,
    instance_id: String,
    demux: gst::glib::WeakRef<gst::Element>,
) -> gst::glib::SignalHandlerId {
    use gst::MessageView;
    let already_checked = Arc::new(AtomicBool::new(false));
    bus.connect_message(Some("state-changed"), move |_bus, msg| {
        if already_checked.load(Ordering::SeqCst) {
            return;
        }
        let state_changed = match msg.view() {
            MessageView::StateChanged(sc) => sc,
            _ => return,
        };
        if state_changed.current() != gst::State::Playing {
            return;
        }
        // Only care about the pipeline's state change, not individual elements.
        let src = match msg.src() {
            Some(s) => s,
            None => return,
        };
        if src.downcast_ref::<gst::Pipeline>().is_none() {
            return;
        }
        let Some(demux) = demux.upgrade() else {
            return;
        };
        let Some(clock) = demux.clock() else {
            return;
        };
        let type_name = clock.type_().name();
        let is_monotonic = match type_name {
            "GstPtpClock" | "GstNtpClock" | "GstNetClientClock" => false,
            "GstSystemClock" => matches!(
                clock.property::<gst::ClockType>("clock-type"),
                gst::ClockType::Monotonic
            ),
            _ => false,
        };
        already_checked.store(true, Ordering::SeqCst);
        if is_monotonic {
            warn!(
                "EFPSRT Input {}: normalize_segment=never but pipeline clock is monotonic ({}). \
                 Absolute PTS won't map to a meaningful running-time. Configure the pipeline \
                 with a realtime/NTP/PTP clock, or set normalize_segment=auto.",
                instance_id, type_name
            );
        } else {
            debug!(
                "EFPSRT Input {}: normalize_segment=never running on clock '{}' — OK",
                instance_id, type_name
            );
        }
    })
}

/// Get metadata for EFP/SRT input blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![efpsrt_input_definition()]
}

/// Get EFP/SRT Input block definition (metadata only).
fn efpsrt_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.efpsrt_input".to_string(),
        name: "EFP/SRT Input".to_string(),
        description: "Receives an SRT stream carrying EFP (Elastic Frame Protocol) and demuxes it into separate video and audio outputs. Supports decode (default) and passthrough modes.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Number of Video Tracks".to_string(),
                description: "Number of video output tracks".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Number of Audio Tracks".to_string(),
                description: "Number of audio output tracks".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "num_data_tracks".to_string(),
                label: "Number of Data Tracks".to_string(),
                description: "Number of EFP embedded-data output tracks (default: 0). efpdemux exposes a single 'embedded' pad carrying every data type, so only the first track is ever linked.".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_data_tracks".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "srt_uri".to_string(),
                label: "SRT URI".to_string(),
                description: "SRT URI (e.g., 'srt://:4000?mode=listener' or 'srt://192.0.2.1:4000?mode=caller')".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(
                    DEFAULT_SRT_INPUT_URI.to_string(),
                )),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "srt_uri".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "latency".to_string(),
                label: "SRT Latency (ms)".to_string(),
                description: "SRT latency in milliseconds".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_SRT_LATENCY_MS as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "latency".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "wait_for_connection".to_string(),
                label: "Wait For Connection".to_string(),
                description: "Block the stream until a peer connects (default: false). Same default across all SRT input/output blocks.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(DEFAULT_SRT_WAIT_FOR_CONNECTION)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "wait_for_connection".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "auto_reconnect".to_string(),
                label: "Auto Reconnect".to_string(),
                description: "Automatically reconnect when connection fails (default: true). Same default across all SRT input/output blocks.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(DEFAULT_SRT_AUTO_RECONNECT)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "auto_reconnect".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "keep_listening".to_string(),
                label: "Keep Listening".to_string(),
                description: "Keep SRT source alive after peer disconnect so reconnects don't require a flow restart (default: true). Same default across all SRT input/output blocks (where applicable).".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(DEFAULT_SRT_KEEP_LISTENING)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "keep_listening".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode video/audio streams (true) or pass through encoded elementary streams (false)".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "bucket_timeout".to_string(),
                label: "Bucket Timeout".to_string(),
                description: "EFP bucket timeout in units of 10ms (default: 5 = 50ms)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_EFP_BUCKET_TIMEOUT as u64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bucket_timeout".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "hol_timeout".to_string(),
                label: "HOL Timeout".to_string(),
                description: "EFP head-of-line timeout in units of 10ms (default: 5 = 50ms)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_EFP_HOL_TIMEOUT as u64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "hol_timeout".to_string(),
                    transform: None,
                },
                live: false,
                persist: None,
            },
            ExposedProperty {
                name: "normalize_segment".to_string(),
                label: "Normalize Segment".to_string(),
                description: "How to set segment.start on outgoing pads. 'auto' (default) \
                    picks based on the pipeline clock: monotonic → normalize, \
                    realtime/TAI/NTP/PTP → pass absolute PTS through. 'always' forces \
                    normalization (legacy). 'never' preserves absolute PTS — required \
                    for cross-source synchronization.".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "auto".to_string(),
                            label: Some("Auto (based on pipeline clock)".to_string()),
                        },
                        EnumValue {
                            value: "always".to_string(),
                            label: Some("Always normalize".to_string()),
                        },
                        EnumValue {
                            value: "never".to_string(),
                            label: Some("Never (preserve absolute PTS)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("auto".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "normalize_segment".to_string(),
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
                    label: Some("V0".to_string()),
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_output".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_out_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_output_0".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.5),
            height: Some(2.0),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;

    fn init_gst() {
        let _ = gst::init();
    }

    fn efpdemux_available() -> bool {
        let registry = gst::Registry::get();
        registry
            .find_feature("efpdemux", gst::ElementFactory::static_type())
            .is_some()
    }

    fn build_with_normalize_segment(value: &str) -> Result<BlockBuildResult, BlockBuildError> {
        let mut properties = HashMap::new();
        properties.insert(
            "normalize_segment".to_string(),
            PropertyValue::String(value.to_string()),
        );
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        EfpSrtInputBuilder.build("test_instance", &properties, &ctx)
    }

    fn find_demux(result: &BlockBuildResult) -> gst::Element {
        result
            .elements
            .iter()
            .find(|(id, _)| id.ends_with(":efpdemux"))
            .map(|(_, e)| e.clone())
            .expect("block result must contain an efpdemux element")
    }

    #[test]
    fn normalize_segment_never_is_applied_to_demux() {
        init_gst();
        if !efpdemux_available() {
            eprintln!("efpdemux plugin not available — skipping");
            return;
        }
        let result = build_with_normalize_segment("never").expect("build should succeed");
        let demux = find_demux(&result);
        let mode: i32 = demux
            .property_value("normalize-segment")
            .get()
            .unwrap_or(-1);
        // Matches the enum ordering in gst-plugin-efp: Auto=0, Always=1, Never=2.
        assert_eq!(
            mode, 2,
            "normalize_segment=never must set demux property to Never"
        );
    }

    #[test]
    fn normalize_segment_auto_is_default() {
        init_gst();
        if !efpdemux_available() {
            eprintln!("efpdemux plugin not available — skipping");
            return;
        }
        let ctx = BlockBuildContext::new(vec![], "all".to_string());
        let result = EfpSrtInputBuilder
            .build("test_instance", &HashMap::new(), &ctx)
            .expect("build should succeed with no properties");
        let demux = find_demux(&result);
        let mode: i32 = demux
            .property_value("normalize-segment")
            .get()
            .unwrap_or(-1);
        assert_eq!(mode, 0, "default normalize_segment must be Auto");
    }

    #[test]
    fn normalize_segment_invalid_value_errors() {
        init_gst();
        let result = build_with_normalize_segment("sometimes");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("invalid value should produce InvalidProperty, got Ok"),
        };
        match err {
            BlockBuildError::InvalidProperty(msg) => {
                assert!(
                    msg.contains("normalize_segment"),
                    "error should mention the offending property, got: {}",
                    msg
                );
            }
            other => panic!("expected InvalidProperty, got {:?}", other),
        }
    }
}
