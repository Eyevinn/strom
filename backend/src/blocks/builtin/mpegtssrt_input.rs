//! MPEG-TS over SRT input block builder.
//!
//! This block receives an SRT stream carrying MPEG Transport Stream and demuxes
//! it into separate video and audio output pads.
//!
//! Pipeline structure (decode=true, default):
//! ```text
//! srtsrc -> decodebin -> videoconvert -> video_output (identity) -> [external video_out]
//!                     -> audioconvert -> audioresample -> audio_output_0 (identity) -> [external audio_out_0]
//! ```
//!
//! Pipeline structure (decode=false, passthrough):
//! ```text
//! srtsrc -> tsdemux -> video_output (identity) -> [external video_out]
//!                   -> audio_output_0 (identity) -> [external audio_out_0]
//! ```
//!
//! Both `decodebin` and `tsdemux` have dynamic pads — uses `connect_pad_added`
//! to link to identity elements based on caps (video/ or audio/).

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

/// MPEG-TS/SRT Input block builder.
pub struct MpegTsSrtInputBuilder;

impl BlockBuilder for MpegTsSrtInputBuilder {
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

        let mut outputs = Vec::new();

        // Add video outputs
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

        // Add audio outputs
        for i in 0..num_audio_tracks {
            outputs.push(ExternalPad {
                label: Some(format!("A{}", i)),
                name: format!("audio_out_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_output_{}", i),
                internal_pad_name: "src".to_string(),
            });
        }

        Some(ExternalPads {
            inputs: vec![], // No inputs — this is a source block
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

        info!(
            "Building MPEG-TS/SRT Input block instance: {} (decode={})",
            instance_id, decode
        );

        // Get SRT URI
        let srt_uri = properties
            .get("srt_uri")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| DEFAULT_SRT_INPUT_URI.to_string());

        // Get SRT latency
        let latency = properties
            .get("latency")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(DEFAULT_SRT_LATENCY_MS);

        // Get tsdemux latency (applies to tsdemux inside decodebin or in passthrough mode).
        // GStreamer accepts -1..=i32::MAX; clamp to prevent i64→i32 truncation.
        let tsdemux_latency = properties
            .get("tsdemux_latency")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some((*u).min(i32::MAX as u64) as i32),
                PropertyValue::Int(i) => Some((*i).clamp(-1, i32::MAX as i64) as i32),
                _ => None,
            })
            .unwrap_or(DEFAULT_TSDEMUX_LATENCY_MS);

        // Get ignore-pcr setting (default true for live pipelines where SRT handles jitter)
        let ignore_pcr = properties
            .get("ignore_pcr")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // Get number of video and audio tracks
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

        info!(
            "SRT source configured: uri={}, latency={}ms, tsdemux_latency={}ms, ignore_pcr={}, keep-listening={}, auto-reconnect={}, wait-for-connection={}",
            srt_uri, latency, tsdemux_latency, ignore_pcr, keep_listening, auto_reconnect, wait_for_connection
        );

        // Create demux/decode element
        // NOTE (2026-03-02): Using decodebin (v2) instead of decodebin3 because decodebin3
        // has known issues with MPEG-TS where it only exposes audio and skips video due to
        // its stream-selection model. The older decodebin exposes all streams by default.
        // See also: https://github.com/mavlink/qgroundcontrol/issues/9830
        let (demux_id, demux_element) = if decode {
            let id = format!("{}:decodebin", instance_id);
            let element = gst::ElementFactory::make("decodebin")
                .name(&id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("decodebin: {}", e)))?;

            // Catch tsdemux when decodebin creates it internally, and set its properties.
            let instance_for_deep = instance_id.to_string();
            element.connect("deep-element-added", false, move |args| {
                let added: gst::Element = args[2].get().unwrap();
                let factory = added.factory();
                if let Some(f) = factory {
                    if f.name() == "tsdemux" {
                        added.set_property("latency", tsdemux_latency);
                        if ignore_pcr && added.has_property("ignore-pcr") {
                            added.set_property("ignore-pcr", true);
                        }
                        info!(
                            "MPEGTSSRT Input {}: Set tsdemux latency={}ms, ignore-pcr={} (inside decodebin)",
                            instance_for_deep, tsdemux_latency, ignore_pcr
                        );
                    }
                }
                None
            });

            (id, element)
        } else {
            let id = format!("{}:tsdemux", instance_id);
            let element = gst::ElementFactory::make("tsdemux")
                .name(&id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("tsdemux: {}", e)))?;
            element.set_property("latency", tsdemux_latency);
            if ignore_pcr && element.has_property("ignore-pcr") {
                element.set_property("ignore-pcr", true);
            }
            (id, element)
        };

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

        // Setup dynamic pad linking
        // Both decodebin and tsdemux emit pad-added signals for their output pads.
        // - decode mode (decodebin): outputs raw pads, we insert videoconvert/audioconvert
        //   to bridge any format differences with downstream elements.
        // - passthrough mode (tsdemux): outputs encoded elementary streams, linked directly.
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

            debug!(
                "MPEGTSSRT Input {} ({}): pad added: {} (caps: {})",
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
                            // Decode mode: insert videoconvert between decodebin and identity
                            if let Err(e) =
                                link_decoded_video(element, pad, &identity, &instance_id_clone)
                            {
                                error!(
                                    "MPEGTSSRT Input {}: Failed to link decoded video pad {}: {}",
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
                                        "MPEGTSSRT Input {}: Failed to link video pad {}: {:?}",
                                        instance_id_clone, pad_name, e
                                    );
                                    guard.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            }
                        }
                        info!(
                            "MPEGTSSRT Input {}: Linked video pad {} -> {}",
                            instance_id_clone,
                            pad_name,
                            identity.name()
                        );
                        return;
                    }
                }
                warn!(
                    "MPEGTSSRT Input {}: No available video output for pad {}",
                    instance_id_clone, pad_name
                );
            } else if is_audio {
                for (weak_identity, guard) in &audio_guards {
                    if guard.swap(true, Ordering::SeqCst) {
                        continue;
                    }

                    if let Some(identity) = weak_identity.upgrade() {
                        if decode {
                            // Decode mode: insert audioconvert + audioresample
                            if let Err(e) =
                                link_decoded_audio(element, pad, &identity, &instance_id_clone)
                            {
                                error!(
                                    "MPEGTSSRT Input {}: Failed to link decoded audio pad {}: {}",
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
                                        "MPEGTSSRT Input {}: Failed to link audio pad {}: {:?}",
                                        instance_id_clone, pad_name, e
                                    );
                                    guard.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            }
                        }
                        info!(
                            "MPEGTSSRT Input {}: Linked audio pad {} -> {}",
                            instance_id_clone,
                            pad_name,
                            identity.name()
                        );
                        return;
                    }
                }
                warn!(
                    "MPEGTSSRT Input {}: No available audio output for pad {}",
                    instance_id_clone, pad_name
                );
            } else {
                debug!(
                    "MPEGTSSRT Input {}: Ignoring pad {} with caps {}",
                    instance_id_clone,
                    pad_name,
                    caps_name.as_deref().unwrap_or("unknown")
                );
            }
        });

        // Internal link: srtsrc -> decodebin/tsdemux
        let internal_links = vec![(
            ElementPadRef::pad(&src_id, "src"),
            ElementPadRef::pad(&demux_id, "sink"),
        )];

        info!(
            "Created MPEG-TS/SRT Input block ({}) with {} video output(s) and {} audio output(s)",
            mode_label, num_video_tracks, num_audio_tracks
        );

        Ok(BlockBuildResult {
            elements,
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Dynamically insert videoconvert between a decoded video pad and an identity element.
/// decodebin pad -> videoconvert -> identity
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

    let convert_name = format!("{}:videoconvert_{}", instance_id, src_pad.name());
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .name(&convert_name)
        .build()
        .map_err(|e| format!("videoconvert: {}", e))?;

    // Add to bin, link internal chain, sync state, then connect source pad LAST
    // to avoid data flowing before the chain is fully connected.
    bin.add(&videoconvert)
        .map_err(|e| format!("add videoconvert: {}", e))?;

    let convert_sink = videoconvert
        .static_pad("sink")
        .ok_or("videoconvert has no sink pad")?;
    let convert_src = videoconvert
        .static_pad("src")
        .ok_or("videoconvert has no src pad")?;
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;

    // Link downstream first: videoconvert -> identity
    convert_src
        .link(&identity_sink)
        .map_err(|e| format!("link videoconvert -> identity: {:?}", e))?;

    videoconvert
        .sync_state_with_parent()
        .map_err(|e| format!("sync videoconvert: {}", e))?;

    // Link source pad last to start data flow only when chain is ready
    src_pad
        .link(&convert_sink)
        .map_err(|e| format!("link pad -> videoconvert: {:?}", e))?;

    debug!(
        "MPEGTSSRT Input {}: Inserted videoconvert for pad {}",
        instance_id,
        src_pad.name()
    );
    Ok(())
}

/// Dynamically insert audioconvert + audioresample between a decoded audio pad and an identity element.
/// decodebin pad -> audioconvert -> audioresample -> identity
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

    let convert_name = format!("{}:audioconvert_{}", instance_id, src_pad.name());
    let resample_name = format!("{}:audioresample_{}", instance_id, src_pad.name());

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&convert_name)
        .build()
        .map_err(|e| format!("audioconvert: {}", e))?;
    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&resample_name)
        .build()
        .map_err(|e| format!("audioresample: {}", e))?;

    // Add to bin, link internal chain, sync state, then connect source pad LAST
    // to avoid data flowing before the chain is fully connected.
    bin.add_many([&audioconvert, &audioresample])
        .map_err(|e| format!("add audio converters: {}", e))?;

    let convert_sink = audioconvert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let resample_src = audioresample
        .static_pad("src")
        .ok_or("audioresample has no src pad")?;
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("identity has no sink pad")?;

    // Link downstream chain first: audioconvert -> audioresample -> identity
    audioconvert
        .link(&audioresample)
        .map_err(|e| format!("link audioconvert -> audioresample: {}", e))?;
    resample_src
        .link(&identity_sink)
        .map_err(|e| format!("link audioresample -> identity: {:?}", e))?;

    audioconvert
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioconvert: {}", e))?;
    audioresample
        .sync_state_with_parent()
        .map_err(|e| format!("sync audioresample: {}", e))?;

    // Link source pad last to start data flow only when chain is ready
    src_pad
        .link(&convert_sink)
        .map_err(|e| format!("link pad -> audioconvert: {:?}", e))?;

    debug!(
        "MPEGTSSRT Input {}: Inserted audioconvert + audioresample for pad {}",
        instance_id,
        src_pad.name()
    );
    Ok(())
}

/// Get metadata for MPEG-TS/SRT input blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![mpegtssrt_input_definition()]
}

/// Get MPEG-TS/SRT Input block definition (metadata only).
fn mpegtssrt_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.mpegtssrt_input".to_string(),
        name: "MPEG-TS/SRT Input".to_string(),
        description: "Receives an SRT stream carrying MPEG Transport Stream and demuxes it into separate video and audio outputs. Supports decode (default) and passthrough modes.".to_string(),
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
            },            ExposedProperty {
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
            },
            ExposedProperty {
                name: "tsdemux_latency".to_string(),
                label: "TS Demux Latency (ms)".to_string(),
                description: "MPEG-TS demuxer latency in milliseconds. GStreamer defaults to 700ms for PCR synchronization; lower values reduce end-to-end latency in live pipelines.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_TSDEMUX_LATENCY_MS as i64)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "tsdemux_latency".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "ignore_pcr".to_string(),
                label: "Ignore PCR".to_string(),
                description: "Ignore PCR (Program Clock Reference) timestamps in the MPEG-TS stream. Recommended for live pipelines where SRT already handles clock recovery and jitter. Disabling PCR avoids unnecessary timestamp adjustments.".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ignore_pcr".to_string(),
                    transform: None,
                },
                live: false,
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
