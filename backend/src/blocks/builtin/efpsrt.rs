//! EFP over SRT output block builder.
//!
//! This block muxes multiple video and audio streams into EFP (Elastic Frame Protocol)
//! and outputs via SRT (Secure Reliable Transport).
//!
//! Features:
//! - Dynamic parser insertion for video (h264parse/h265parse with config-interval=1)
//! - Dynamic audio chain: supports raw audio (encodes to Opus) and encoded formats
//! - Configurable inputs: 1 video input + 1-32 audio inputs (default: 1 audio)
//! - SRT with auto-reconnect and configurable latency
//! - Configurable MTU for EFP fragmentation
//!
//! Input handling:
//! - Video: Dynamically detects codec (H.264, H.265) and inserts appropriate parser
//!   - Parser uses config-interval=1 for SPS/PPS insertion at every keyframe
//! - Audio: Dynamically detects format and inserts appropriate chain
//!   - Raw audio (audio/x-raw): audioconvert -> audioresample -> opusenc -> opusparse
//!   - Opus (audio/x-opus): opusparse
//!   - Other encoded formats: passed directly to efpmux (tagged as private data)
//!
//! Pipeline structure:
//! ```text
//! Video (encoded) -> identity -> [dynamic: h264parse/h265parse] -> efpmux -> srtsink
//! Audio (raw)     -> identity -> [dynamic: audioconvert -> audioresample -> opusenc -> opusparse] -> efpmux
//! Audio (opus)    -> identity -> [dynamic: opusparse] -> efpmux
//! Audio (other)   -> identity -> efpmux
//! ```

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

/// EFP/SRT Output block builder.
pub struct EfpSrtOutputBuilder;

impl BlockBuilder for EfpSrtOutputBuilder {
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

        let mut inputs = Vec::new();

        for i in 0..num_video_tracks {
            inputs.push(ExternalPad {
                label: Some(format!("V{}", i)),
                name: if num_video_tracks == 1 {
                    "video_in".to_string()
                } else {
                    format!("video_in_{}", i)
                },
                media_type: MediaType::Video,
                internal_element_id: if num_video_tracks == 1 {
                    "video_input".to_string()
                } else {
                    format!("video_input_{}", i)
                },
                internal_pad_name: "sink".to_string(),
            });
        }

        for i in 0..num_audio_tracks {
            inputs.push(ExternalPad {
                label: Some(format!("A{}", i)),
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_input_{}", i),
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
        info!("Building EFP/SRT Output block instance: {}", instance_id);

        let srt_uri = properties
            .get("srt_uri")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_SRT_OUTPUT_URI.to_string());

        let latency = properties
            .get("latency")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_LATENCY_MS);

        let wait_for_connection = properties
            .get("wait_for_connection")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let auto_reconnect = properties
            .get("auto_reconnect")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        let sync = properties
            .get("sync")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let mtu = properties
            .get("mtu")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as u32),
                PropertyValue::Int(i) => Some(*i as u32),
                _ => None,
            })
            .unwrap_or(DEFAULT_EFP_MTU);

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

        // Create efpmux
        let mux_id = format!("{}:efpmux", instance_id);
        let mux = gst::ElementFactory::make("efpmux")
            .name(&mux_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("efpmux: {}", e)))?;

        mux.set_property("mtu", mtu);
        info!("EFP muxer configured: mtu={}", mtu);

        // Create srtsink
        let sink_id = format!("{}:srtsink", instance_id);
        let srtsink = gst::ElementFactory::make("srtsink")
            .name(&sink_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("srtsink: {}", e)))?;

        srtsink.set_property("uri", &srt_uri);
        srtsink.set_property("latency", latency);
        srtsink.set_property("wait-for-connection", wait_for_connection);

        let has_auto_reconnect = srtsink.has_property("auto-reconnect");
        if has_auto_reconnect {
            srtsink.set_property("auto-reconnect", auto_reconnect);
        }

        srtsink.set_property("sync", sync);
        srtsink.set_property("qos", true);
        // async=false: don't block pipeline preroll waiting for the first buffer.
        // In listener mode without a connected client, the sink would otherwise
        // hold PAUSED->PLAYING indefinitely. Matches mpegtssrt and WHEP/WHIP/AES67 sinks.
        srtsink.set_property("async", false);

        if has_auto_reconnect {
            info!(
                "SRT sink configured: uri={}, latency={}ms, wait={}, auto-reconnect={}, sync={}, qos=true",
                srt_uri, latency, wait_for_connection, auto_reconnect, sync
            );
        } else {
            info!(
                "SRT sink configured: uri={}, latency={}ms, wait={}, sync={}, qos=true (auto-reconnect not available)",
                srt_uri, latency, wait_for_connection, sync
            );
        }

        let mut internal_links = vec![];
        let mux_weak = mux.downgrade();
        let mut elements = vec![(mux_id.clone(), mux), (sink_id.clone(), srtsink)];

        // Create video input chains with dynamic parser insertion
        if num_video_tracks > 0 {
            let video_input_id = format!("{}:video_input", instance_id);
            let video_input = gst::ElementFactory::make("identity")
                .name(&video_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("video identity: {}", e)))?;

            let mux_weak_clone = mux_weak.clone();
            let instance_id_clone = instance_id.to_string();
            let parser_inserted = Arc::new(AtomicBool::new(false));

            if let Some(src_pad) = video_input.static_pad("src") {
                src_pad.add_probe(
                    gst::PadProbeType::EVENT_DOWNSTREAM,
                    move |pad, info| {
                        let event = match &info.data {
                            Some(gst::PadProbeData::Event(event)) => event,
                            _ => return gst::PadProbeReturn::Ok,
                        };

                        if event.type_() != gst::EventType::Caps {
                            return gst::PadProbeReturn::Ok;
                        }

                        if parser_inserted.swap(true, Ordering::SeqCst) {
                            return gst::PadProbeReturn::Ok;
                        }

                        let caps = match event.view() {
                            gst::EventView::Caps(caps_event) => caps_event.caps().to_owned(),
                            _ => return gst::PadProbeReturn::Ok,
                        };

                        let structure = match caps.structure(0) {
                            Some(s) => s,
                            None => {
                                error!("EFPSRT {}: No structure in video caps", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let caps_name = structure.name().to_string();
                        debug!(
                            "EFPSRT {}: Video caps detected: {}",
                            instance_id_clone, caps_name
                        );

                        let (parser_factory, parser_name) = if caps_name == "video/x-h264" {
                            ("h264parse", "h264parse")
                        } else if caps_name == "video/x-h265" {
                            ("h265parse", "h265parse")
                        } else {
                            warn!(
                                "EFPSRT {}: Unsupported video codec: {} (only H.264 and H.265 supported)",
                                instance_id_clone, caps_name
                            );
                            return gst::PadProbeReturn::Ok;
                        };

                        let mux = match mux_weak_clone.upgrade() {
                            Some(m) => m,
                            None => {
                                error!("EFPSRT {}: mux element no longer exists", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let pipeline = match mux.parent() {
                            Some(p) => p,
                            None => {
                                error!("EFPSRT {}: mux has no parent", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let bin = match pipeline.downcast::<gst::Bin>() {
                            Ok(b) => b,
                            Err(_) => {
                                error!("EFPSRT {}: parent is not a Bin", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let parser_element_name = format!("{}:video_parser", instance_id_clone);
                        let parser = match gst::ElementFactory::make(parser_factory)
                            .name(&parser_element_name)
                            .property("config-interval", 1i32)
                            .build()
                        {
                            Ok(p) => p,
                            Err(e) => {
                                error!(
                                    "EFPSRT {}: Failed to create {}: {}",
                                    instance_id_clone, parser_factory, e
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        info!(
                            "EFPSRT {}: Inserting {} with config-interval=1 for video stream",
                            instance_id_clone, parser_name
                        );

                        if let Err(e) = bin.add(&parser) {
                            error!("EFPSRT {}: Failed to add parser to bin: {}", instance_id_clone, e);
                            return gst::PadProbeReturn::Ok;
                        }

                        if let Err(e) = parser.sync_state_with_parent() {
                            error!("EFPSRT {}: Failed to sync parser state: {}", instance_id_clone, e);
                            return gst::PadProbeReturn::Ok;
                        }

                        let parser_sink = match parser.static_pad("sink") {
                            Some(p) => p,
                            None => {
                                error!("EFPSRT {}: Parser has no sink pad", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let parser_src = match parser.static_pad("src") {
                            Some(p) => p,
                            None => {
                                error!("EFPSRT {}: Parser has no src pad", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        // Request a sink pad from efpmux
                        let pad_template = match mux.pad_template("sink_%u") {
                            Some(t) => t,
                            None => {
                                error!(
                                    "EFPSRT {}: efpmux has no sink_%u pad template",
                                    instance_id_clone
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let mux_sink = match mux.request_pad(&pad_template, None, None) {
                            Some(p) => p,
                            None => {
                                error!(
                                    "EFPSRT {}: Failed to request pad from efpmux",
                                    instance_id_clone
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        if let Err(e) = pad.link(&parser_sink) {
                            error!(
                                "EFPSRT {}: Failed to link identity to parser: {:?}",
                                instance_id_clone, e
                            );
                            return gst::PadProbeReturn::Ok;
                        }

                        if let Err(e) = parser_src.link(&mux_sink) {
                            error!(
                                "EFPSRT {}: Failed to link parser to mux: {:?}",
                                instance_id_clone, e
                            );
                            return gst::PadProbeReturn::Ok;
                        }

                        info!(
                            "EFPSRT {}: Video chain linked: identity -> {} -> efpmux ({})",
                            instance_id_clone, parser_name, mux_sink.name()
                        );

                        gst::PadProbeReturn::Ok
                    },
                );
            }

            info!(
                "Video input: dynamic parser insertion enabled (H.264/H.265 with config-interval=1)"
            );

            elements.push((video_input_id, video_input));
        }

        // Create audio input chains with dynamic linking
        // Supported audio formats:
        // - audio/x-raw -> audioconvert -> audioresample -> opusenc -> opusparse -> efpmux
        // - audio/x-opus -> opusparse -> efpmux
        // - Other encoded formats -> efpmux directly (tagged as private data)
        for i in 0..num_audio_tracks {
            let audio_input_id = format!("{}:audio_input_{}", instance_id, i);
            let audio_input = gst::ElementFactory::make("identity")
                .name(&audio_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audio identity: {}", e)))?;

            let mux_weak_clone = mux_weak.clone();
            let instance_id_clone = instance_id.to_string();
            let audio_chain_inserted = Arc::new(AtomicBool::new(false));
            let track_index = i;

            if let Some(src_pad) = audio_input.static_pad("src") {
                src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                    let event = match &info.data {
                        Some(gst::PadProbeData::Event(event)) => event,
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    if event.type_() != gst::EventType::Caps {
                        return gst::PadProbeReturn::Ok;
                    }

                    if audio_chain_inserted.swap(true, Ordering::SeqCst) {
                        return gst::PadProbeReturn::Ok;
                    }

                    let caps = match event.view() {
                        gst::EventView::Caps(caps_event) => caps_event.caps().to_owned(),
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    let structure = match caps.structure(0) {
                        Some(s) => s,
                        None => {
                            error!(
                                "EFPSRT {}: No structure in audio caps (track {})",
                                instance_id_clone, track_index
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let caps_name = structure.name().to_string();
                    debug!(
                        "EFPSRT {}: Audio caps detected (track {}): {}",
                        instance_id_clone, track_index, caps_name
                    );

                    let mux = match mux_weak_clone.upgrade() {
                        Some(m) => m,
                        None => {
                            error!("EFPSRT {}: mux element no longer exists", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let pipeline = match mux.parent() {
                        Some(p) => p,
                        None => {
                            error!("EFPSRT {}: mux has no parent", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let bin = match pipeline.downcast::<gst::Bin>() {
                        Ok(b) => b,
                        Err(_) => {
                            error!("EFPSRT {}: parent is not a Bin", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let result = if caps_name == "audio/x-raw" {
                        build_raw_audio_chain(&bin, &mux, pad, &instance_id_clone, track_index)
                    } else if caps_name == "audio/x-opus" {
                        build_opus_passthrough_chain(
                            &bin,
                            &mux,
                            pad,
                            &instance_id_clone,
                            track_index,
                        )
                    } else {
                        // Other encoded formats: link directly to efpmux (private data)
                        build_direct_audio_chain(&mux, pad, &instance_id_clone, track_index)
                    };

                    if let Err(e) = result {
                        error!(
                            "EFPSRT {}: Failed to build audio chain (track {}): {}",
                            instance_id_clone, track_index, e
                        );
                    }

                    gst::PadProbeReturn::Ok
                });
            }

            info!(
                "Audio input {}: dynamic chain insertion enabled (raw->Opus, Opus passthrough, direct)",
                i
            );

            elements.push((audio_input_id, audio_input));
        }

        // Link mux to sink
        internal_links.push((
            ElementPadRef::pad(&mux_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        ));

        info!(
            "Created EFP/SRT block with {} video track(s) and {} audio chain(s)",
            num_video_tracks, num_audio_tracks
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Build audio chain for raw audio input: audioconvert -> audioresample -> opusenc -> opusparse -> mux
fn build_raw_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
    track_index: usize,
) -> Result<(), String> {
    let audioconvert_name = format!("{}:audio_convert_{}", instance_id, track_index);
    let audioresample_name = format!("{}:audio_resample_{}", instance_id, track_index);
    let encoder_name = format!("{}:audio_encoder_{}", instance_id, track_index);
    let parser_name = format!("{}:audio_parser_{}", instance_id, track_index);

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_name)
        .build()
        .map_err(|e| format!("audioconvert: {}", e))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_name)
        .build()
        .map_err(|e| format!("audioresample: {}", e))?;

    let encoder = gst::ElementFactory::make("opusenc")
        .name(&encoder_name)
        .build()
        .map_err(|e| format!("opusenc: {}", e))?;

    let parser = gst::ElementFactory::make("opusparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("opusparse: {}", e))?;

    bin.add_many([&audioconvert, &audioresample, &encoder, &parser])
        .map_err(|e| format!("add elements: {}", e))?;

    audioconvert
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioconvert: {}", e))?;
    audioresample
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioresample: {}", e))?;
    encoder
        .sync_state_with_parent()
        .map_err(|e| format!("sync encoder: {}", e))?;
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let audioconvert_sink = audioconvert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;

    // Request mux pad
    let pad_template = mux
        .pad_template("sink_%u")
        .ok_or("efpmux has no sink_%u pad template")?;
    let mux_sink = mux
        .request_pad(&pad_template, None, None)
        .ok_or("failed to request pad from efpmux")?;

    // Link chain
    identity_src_pad
        .link(&audioconvert_sink)
        .map_err(|e| format!("link identity -> audioconvert: {:?}", e))?;
    audioconvert
        .link(&audioresample)
        .map_err(|e| format!("link audioconvert -> audioresample: {}", e))?;
    audioresample
        .link(&encoder)
        .map_err(|e| format!("link audioresample -> encoder: {}", e))?;
    encoder
        .link(&parser)
        .map_err(|e| format!("link encoder -> parser: {}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link parser -> mux: {:?}", e))?;

    info!(
        "EFPSRT {}: Audio chain linked (track {}): identity -> audioconvert -> audioresample -> opusenc -> opusparse -> efpmux ({})",
        instance_id, track_index, mux_sink.name()
    );

    Ok(())
}

/// Build audio chain for Opus passthrough: opusparse -> mux
fn build_opus_passthrough_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
    track_index: usize,
) -> Result<(), String> {
    let parser_name = format!("{}:audio_parser_{}", instance_id, track_index);

    let parser = gst::ElementFactory::make("opusparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("opusparse: {}", e))?;

    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;

    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;

    let pad_template = mux
        .pad_template("sink_%u")
        .ok_or("efpmux has no sink_%u pad template")?;
    let mux_sink = mux
        .request_pad(&pad_template, None, None)
        .ok_or("failed to request pad from efpmux")?;

    identity_src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link identity -> parser: {:?}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link parser -> mux: {:?}", e))?;

    info!(
        "EFPSRT {}: Audio chain linked (track {}): identity -> opusparse -> efpmux ({})",
        instance_id,
        track_index,
        mux_sink.name()
    );

    Ok(())
}

/// Build audio chain for other encoded formats: link directly to efpmux (private data)
fn build_direct_audio_chain(
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
    track_index: usize,
) -> Result<(), String> {
    let pad_template = mux
        .pad_template("sink_%u")
        .ok_or("efpmux has no sink_%u pad template")?;
    let mux_sink = mux
        .request_pad(&pad_template, None, None)
        .ok_or("failed to request pad from efpmux")?;

    identity_src_pad
        .link(&mux_sink)
        .map_err(|e| format!("link identity -> mux: {:?}", e))?;

    info!(
        "EFPSRT {}: Audio chain linked (track {}): identity -> efpmux ({}) (private data)",
        instance_id,
        track_index,
        mux_sink.name()
    );

    Ok(())
}

/// Get metadata for EFP/SRT output blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![efpsrt_output_definition()]
}

/// Get EFP/SRT Output block definition (metadata only).
fn efpsrt_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.efpsrt_output".to_string(),
        name: "EFP/SRT Output".to_string(),
        description: "Muxes multiple audio/video streams using EFP (Elastic Frame Protocol) and outputs via SRT. Supports H.264, H.265 video and Opus audio natively. Auto-encodes raw audio to Opus. Other formats are transported as private data.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Number of Video Tracks".to_string(),
                description: "Number of video input tracks".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Number of Audio Tracks".to_string(),
                description: "Number of audio input tracks".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "srt_uri".to_string(),
                label: "SRT URI".to_string(),
                description: "SRT URI (e.g., 'srt://127.0.0.1:5000?mode=caller' or 'srt://:5000?mode=listener')".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String(DEFAULT_SRT_OUTPUT_URI.to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "srt_uri".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "latency".to_string(),
                label: "SRT Latency (ms)".to_string(),
                description: "SRT latency in milliseconds (default: 125ms)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_SRT_LATENCY_MS as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "latency".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "wait_for_connection".to_string(),
                label: "Wait For Connection".to_string(),
                description: "Block the stream until a client connects (default: false)".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "wait_for_connection".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "auto_reconnect".to_string(),
                label: "Auto Reconnect".to_string(),
                description: "Automatically reconnect when connection fails (default: true)".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "auto_reconnect".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "sync".to_string(),
                label: "Sync".to_string(),
                description: "Synchronize output to pipeline clock (default: false). Enable for playout scenarios where timing matters.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sync".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "mtu".to_string(),
                label: "MTU".to_string(),
                description: "Maximum Transmission Unit for EFP fragments in bytes (default: 1400)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(DEFAULT_EFP_MTU as u64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mtu".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_input".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in_0".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_input_0".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.5),
            height: Some(3.0),
            ..Default::default()
        }),
    }
}
