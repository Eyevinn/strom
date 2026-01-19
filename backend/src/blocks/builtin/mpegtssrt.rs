//! MPEG-TS over SRT output block builder.
//!
//! This block muxes multiple video and audio streams into MPEG Transport Stream
//! and outputs via SRT (Secure Reliable Transport).
//!
//! Features:
//! - Dynamic parser insertion for video (h264parse/h265parse with config-interval=1)
//! - Dynamic audio chain: supports raw audio (encodes to AAC) and all encoded formats
//! - Configurable inputs: 1 video input + 1-32 audio inputs (default: 1 audio)
//! - Optimized for UDP streaming (alignment=7 on mpegtsmux)
//! - SRT with auto-reconnect and configurable latency
//!
//! Input handling:
//! - Video: Dynamically detects codec (H.264, H.265) and inserts appropriate parser
//!   - Parser uses config-interval=1 for SPS/PPS insertion at every keyframe
//!   - ⚠️  AV1 and VP9 are NOT supported by MPEG-TS standard
//! - Audio: Dynamically detects format and inserts appropriate chain
//!   - Raw audio (audio/x-raw): audioconvert -> audioresample -> avenc_aac -> aacparse
//!   - AAC (audio/mpeg mpegversion=2/4): aacparse
//!   - MP3 (audio/mpeg mpegversion=1): mpegaudioparse
//!   - AC3 (audio/x-ac3): ac3parse
//!   - DTS (audio/x-dts): dcaparse
//!   - Opus (audio/x-opus): opusparse
//!
//! Pipeline structure:
//! ```text
//! Video (encoded) -> identity -> [dynamic: h264parse/h265parse] -> mpegtsmux -> srtsink
//! Audio (raw)     -> identity -> [dynamic: audioconvert -> audioresample -> avenc_aac -> aacparse] -> mpegtsmux
//! Audio (encoded) -> identity -> [dynamic: parser based on codec] -> mpegtsmux
//! ```

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};

/// MPEG-TS/SRT Output block builder.
pub struct MpegTsSrtOutputBuilder;

impl BlockBuilder for MpegTsSrtOutputBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        // Get number of video and audio tracks from properties
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

        // Build dynamic input pads
        let mut inputs = Vec::new();

        // Add video inputs
        for i in 0..num_video_tracks {
            inputs.push(ExternalPad {
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

        // Add audio inputs
        for i in 0..num_audio_tracks {
            inputs.push(ExternalPad {
                name: format!("audio_in_{}", i),
                media_type: MediaType::Audio,
                internal_element_id: format!("audio_input_{}", i),
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![], // No outputs
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!(
            "Building MPEG-TS/SRT Output block instance: {}",
            instance_id
        );

        // Get SRT URI (required)
        let srt_uri = properties
            .get("srt_uri")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                BlockBuildError::InvalidProperty("srt_uri property required".to_string())
            })?;

        // Get SRT latency (optional, default 125ms)
        let latency = properties
            .get("latency")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u as i32),
                PropertyValue::Int(i) => Some(*i as i32),
                _ => None,
            })
            .unwrap_or(125);

        // Get wait_for_connection (optional, default false per notes.txt)
        let wait_for_connection = properties
            .get("wait_for_connection")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        // Get auto_reconnect (optional, default true per notes.txt)
        let auto_reconnect = properties
            .get("auto_reconnect")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // Get sync (optional, default true)
        // sync=false is useful for transcoding workloads where timestamps may be discontinuous
        let sync = properties
            .get("sync")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        // Create mpegtsmux with alignment=7 for UDP streaming
        let mux_id = format!("{}:mpegtsmux", instance_id);
        let mux = gst::ElementFactory::make("mpegtsmux")
            .name(&mux_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("mpegtsmux: {}", e)))?;

        // Set alignment=7 for UDP streaming (7 MPEG-TS packets = 1316 bytes, fits in typical MTU)
        mux.set_property("alignment", 7i32);

        // Set PCR interval to 40ms for proper clock recovery (MPEG-TS standard recommends 40-100ms)
        if mux.has_property("pcr-interval") {
            mux.set_property("pcr-interval", 40u32);
        }

        // Enable bitrate for CBR-like behavior if available
        if mux.has_property("bitrate") {
            mux.set_property("bitrate", 0u64); // 0 = auto-detect from streams
        }

        info!("MPEG-TS muxer configured: alignment=7, pcr-interval=40ms");

        // Create srtsink
        let sink_id = format!("{}:srtsink", instance_id);
        let srtsink = gst::ElementFactory::make("srtsink")
            .name(&sink_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("srtsink: {}", e)))?;

        // Configure srtsink
        srtsink.set_property("uri", &srt_uri);
        srtsink.set_property("latency", latency);
        srtsink.set_property("wait-for-connection", wait_for_connection);
        // auto-reconnect property was added in newer GStreamer versions
        let has_auto_reconnect = srtsink.has_property("auto-reconnect");
        if has_auto_reconnect {
            srtsink.set_property("auto-reconnect", auto_reconnect);
        }

        // IMPORTANT: sync=false for transcoding workloads
        //
        // Background:
        // When receiving SRT streams from remote encoders, the timestamps in the stream
        // reflect the remote encoder's clock (which may have started hours ago).
        // This creates massive timestamp discontinuities relative to the local pipeline clock.
        //
        // With sync=true, srtsink tries to play buffers according to their timestamps:
        // - It sees timestamps from 7+ hours ago
        // - Thinks it's massively behind schedule
        // - Sends QoS events upstream telling elements to drop frames
        // - Creates false "falling behind" warnings even when GPU/CPU performance is fine
        //
        // Example symptoms:
        // - QoS warnings: "falling behind 10-75%" despite good performance
        // - Massive jitter: 26+ trillion nanoseconds (= 7+ hours)
        // - Decoder reports low proportion (0.2-0.9) even though it's working efficiently
        //
        // Solution for transcoding (encode-as-fast-as-possible):
        // - sync=false: Don't try to maintain real-time clock synchronization
        // - qos=true: Enable QoS events so sink can report back pressure to upstream
        // - Buffers are pushed as fast as they're produced
        //
        // When you WOULD want sync=true:
        // - Live playback/monitoring where you need real-time output
        // - Synchronized multi-stream outputs
        // - When timestamps are consistent with pipeline clock
        //
        // See also: notes.txt "QoS/SYNC ISSUE IN TRANSCODING PIPELINES"
        // Fixed: 2025-12-01
        srtsink.set_property("sync", sync);
        srtsink.set_property("qos", true);

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

        // Get number of video and audio tracks from properties
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

        let mut internal_links = vec![];

        // Get weak reference to mux BEFORE moving it into elements (for dynamic linking in pad probe)
        let mux_weak = mux.downgrade();

        let mut elements = vec![(mux_id.clone(), mux), (sink_id.clone(), srtsink)];

        // Create video input chain if requested
        // Video linking is DYNAMIC - we don't link to mpegtsmux at construction time.
        // Instead, we use a pad probe to detect the codec and insert the appropriate parser.
        // This prevents mpegtsmux's byte-stream requirement from propagating upstream.
        if num_video_tracks > 0 {
            let video_input_id = format!("{}:video_input", instance_id);

            let video_input = gst::ElementFactory::make("identity")
                .name(&video_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("video identity: {}", e)))?;

            // Setup dynamic parser insertion via pad probe on the identity's src pad
            // When we receive caps, we'll create the appropriate parser and link to mpegtsmux
            let mux_weak_clone = mux_weak.clone();
            let instance_id_clone = instance_id.to_string();
            let parser_inserted = Arc::new(AtomicBool::new(false));

            if let Some(src_pad) = video_input.static_pad("src") {
                src_pad.add_probe(
                    gst::PadProbeType::EVENT_DOWNSTREAM,
                    move |pad, info| {
                        // Only process CAPS events
                        let event = match &info.data {
                            Some(gst::PadProbeData::Event(event)) => event,
                            _ => return gst::PadProbeReturn::Ok,
                        };

                        if event.type_() != gst::EventType::Caps {
                            return gst::PadProbeReturn::Ok;
                        }

                        // Only insert parser once
                        if parser_inserted.swap(true, Ordering::SeqCst) {
                            return gst::PadProbeReturn::Ok;
                        }

                        // Get the caps from the event
                        let caps = match event.view() {
                            gst::EventView::Caps(caps_event) => caps_event.caps().to_owned(),
                            _ => return gst::PadProbeReturn::Ok,
                        };

                        let structure = match caps.structure(0) {
                            Some(s) => s,
                            None => {
                                error!("MPEGTSSRT {}: No structure in video caps", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let caps_name = structure.name().to_string();
                        debug!(
                            "MPEGTSSRT {}: Video caps detected: {}",
                            instance_id_clone, caps_name
                        );

                        // Determine which parser to use based on codec
                        let (parser_factory, parser_name) = if caps_name == "video/x-h264" {
                            ("h264parse", "h264parse")
                        } else if caps_name == "video/x-h265" {
                            ("h265parse", "h265parse")
                        } else {
                            warn!(
                                "MPEGTSSRT {}: Unsupported video codec: {} (only H.264 and H.265 supported)",
                                instance_id_clone, caps_name
                            );
                            return gst::PadProbeReturn::Ok;
                        };

                        // Get the elements we need
                        let mux = match mux_weak_clone.upgrade() {
                            Some(m) => m,
                            None => {
                                error!("MPEGTSSRT {}: mux element no longer exists", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        // Get the pipeline (parent of mux)
                        let pipeline = match mux.parent() {
                            Some(p) => p,
                            None => {
                                error!("MPEGTSSRT {}: mux has no parent", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let bin = match pipeline.downcast::<gst::Bin>() {
                            Ok(b) => b,
                            Err(_) => {
                                error!("MPEGTSSRT {}: parent is not a Bin", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        // Create the parser with config-interval=1 for SPS/PPS insertion
                        let parser_element_name = format!("{}:video_parser", instance_id_clone);
                        let parser = match gst::ElementFactory::make(parser_factory)
                            .name(&parser_element_name)
                            .property("config-interval", 1i32)
                            .build()
                        {
                            Ok(p) => p,
                            Err(e) => {
                                error!(
                                    "MPEGTSSRT {}: Failed to create {}: {}",
                                    instance_id_clone, parser_factory, e
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        info!(
                            "MPEGTSSRT {}: Inserting {} with config-interval=1 for video stream",
                            instance_id_clone, parser_name
                        );

                        // Add parser to bin
                        if let Err(e) = bin.add(&parser) {
                            error!("MPEGTSSRT {}: Failed to add parser to bin: {}", instance_id_clone, e);
                            return gst::PadProbeReturn::Ok;
                        }

                        // Sync state with parent
                        if let Err(e) = parser.sync_state_with_parent() {
                            error!("MPEGTSSRT {}: Failed to sync parser state: {}", instance_id_clone, e);
                            return gst::PadProbeReturn::Ok;
                        }

                        // Get pads
                        let parser_sink = match parser.static_pad("sink") {
                            Some(p) => p,
                            None => {
                                error!("MPEGTSSRT {}: Parser has no sink pad", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let parser_src = match parser.static_pad("src") {
                            Some(p) => p,
                            None => {
                                error!("MPEGTSSRT {}: Parser has no src pad", instance_id_clone);
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        // Request a sink pad from mpegtsmux using the pad template
                        // This lets mpegtsmux assign the appropriate PID automatically
                        let pad_template = match mux.pad_template("sink_%d") {
                            Some(t) => t,
                            None => {
                                error!(
                                    "MPEGTSSRT {}: mpegtsmux has no sink_%d pad template",
                                    instance_id_clone
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        let mux_sink = match mux.request_pad(&pad_template, None, None) {
                            Some(p) => p,
                            None => {
                                error!(
                                    "MPEGTSSRT {}: Failed to request pad from mpegtsmux",
                                    instance_id_clone
                                );
                                return gst::PadProbeReturn::Ok;
                            }
                        };

                        // Link: identity src -> parser sink
                        if let Err(e) = pad.link(&parser_sink) {
                            error!(
                                "MPEGTSSRT {}: Failed to link identity to parser: {:?}",
                                instance_id_clone, e
                            );
                            return gst::PadProbeReturn::Ok;
                        }

                        // Link: parser src -> mpegtsmux sink
                        if let Err(e) = parser_src.link(&mux_sink) {
                            error!(
                                "MPEGTSSRT {}: Failed to link parser to mux: {:?}",
                                instance_id_clone, e
                            );
                            return gst::PadProbeReturn::Ok;
                        }

                        info!(
                            "MPEGTSSRT {}: Video chain linked: identity -> {} -> mpegtsmux ({})",
                            instance_id_clone, parser_name, mux_sink.name()
                        );

                        gst::PadProbeReturn::Ok
                    },
                );
            }

            info!(
                "Video input: dynamic parser insertion enabled (H.264/H.265 with config-interval=1)"
            );

            // NOTE: No internal_links for video - linking happens dynamically in the pad probe
            elements.push((video_input_id.clone(), video_input));
        }

        // Create audio input chains with DYNAMIC linking (similar to video)
        // Audio linking is DYNAMIC - we don't link to mpegtsmux at construction time.
        // Instead, we use a pad probe to detect the audio format and insert the appropriate chain.
        //
        // Supported audio formats:
        // - audio/x-raw -> audioconvert -> audioresample -> avenc_aac -> aacparse -> mpegtsmux
        // - audio/mpeg (AAC, mpegversion 2/4) -> aacparse -> mpegtsmux
        // - audio/mpeg (MP3, mpegversion 1) -> mpegaudioparse -> mpegtsmux
        // - audio/x-ac3 -> ac3parse -> mpegtsmux
        // - audio/x-dts -> dcaparse -> mpegtsmux
        // - audio/x-opus -> opusparse -> mpegtsmux
        for i in 0..num_audio_tracks {
            let audio_input_id = format!("{}:audio_input_{}", instance_id, i);

            let audio_input = gst::ElementFactory::make("identity")
                .name(&audio_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audio identity: {}", e)))?;

            // Setup dynamic audio chain insertion via pad probe on the identity's src pad
            let mux_weak_clone = mux_weak.clone();
            let instance_id_clone = instance_id.to_string();
            let audio_chain_inserted = Arc::new(AtomicBool::new(false));
            let track_index = i;

            if let Some(src_pad) = audio_input.static_pad("src") {
                src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                    // Only process CAPS events
                    let event = match &info.data {
                        Some(gst::PadProbeData::Event(event)) => event,
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    if event.type_() != gst::EventType::Caps {
                        return gst::PadProbeReturn::Ok;
                    }

                    // Only insert chain once
                    if audio_chain_inserted.swap(true, Ordering::SeqCst) {
                        return gst::PadProbeReturn::Ok;
                    }

                    // Get the caps from the event
                    let caps = match event.view() {
                        gst::EventView::Caps(caps_event) => caps_event.caps().to_owned(),
                        _ => return gst::PadProbeReturn::Ok,
                    };

                    let structure = match caps.structure(0) {
                        Some(s) => s,
                        None => {
                            error!(
                                "MPEGTSSRT {}: No structure in audio caps (track {})",
                                instance_id_clone, track_index
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let caps_name = structure.name().to_string();
                    debug!(
                        "MPEGTSSRT {}: Audio caps detected (track {}): {}",
                        instance_id_clone, track_index, caps_name
                    );

                    // Get the mux element
                    let mux = match mux_weak_clone.upgrade() {
                        Some(m) => m,
                        None => {
                            error!(
                                "MPEGTSSRT {}: mux element no longer exists",
                                instance_id_clone
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Get the pipeline (parent of mux)
                    let pipeline = match mux.parent() {
                        Some(p) => p,
                        None => {
                            error!("MPEGTSSRT {}: mux has no parent", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    let bin = match pipeline.downcast::<gst::Bin>() {
                        Ok(b) => b,
                        Err(_) => {
                            error!("MPEGTSSRT {}: parent is not a Bin", instance_id_clone);
                            return gst::PadProbeReturn::Ok;
                        }
                    };

                    // Determine what audio chain to build based on caps
                    let result = if caps_name == "audio/x-raw" {
                        // Raw audio: need to encode to AAC
                        build_raw_audio_chain(&bin, &mux, pad, &instance_id_clone, track_index)
                    } else if caps_name == "audio/mpeg" {
                        // AAC or MP3 - check mpegversion
                        let mpegversion = structure.get::<i32>("mpegversion").unwrap_or(4);
                        if mpegversion == 1 {
                            // MP3
                            build_encoded_audio_chain(
                                &bin,
                                &mux,
                                pad,
                                &instance_id_clone,
                                track_index,
                                "mpegaudioparse",
                                "MP3",
                            )
                        } else {
                            // AAC (mpegversion 2 or 4)
                            build_encoded_audio_chain(
                                &bin,
                                &mux,
                                pad,
                                &instance_id_clone,
                                track_index,
                                "aacparse",
                                "AAC",
                            )
                        }
                    } else if caps_name == "audio/x-ac3" {
                        build_encoded_audio_chain(
                            &bin,
                            &mux,
                            pad,
                            &instance_id_clone,
                            track_index,
                            "ac3parse",
                            "AC3",
                        )
                    } else if caps_name == "audio/x-dts" {
                        build_encoded_audio_chain(
                            &bin,
                            &mux,
                            pad,
                            &instance_id_clone,
                            track_index,
                            "dcaparse",
                            "DTS",
                        )
                    } else if caps_name == "audio/x-opus" {
                        build_encoded_audio_chain(
                            &bin,
                            &mux,
                            pad,
                            &instance_id_clone,
                            track_index,
                            "opusparse",
                            "Opus",
                        )
                    } else {
                        error!(
                            "MPEGTSSRT {}: Unsupported audio format: {} (track {})",
                            instance_id_clone, caps_name, track_index
                        );
                        return gst::PadProbeReturn::Ok;
                    };

                    if let Err(e) = result {
                        error!(
                            "MPEGTSSRT {}: Failed to build audio chain (track {}): {}",
                            instance_id_clone, track_index, e
                        );
                    }

                    gst::PadProbeReturn::Ok
                });
            }

            info!(
                "Audio input {}: dynamic chain insertion enabled (raw->AAC, AAC, MP3, AC3, DTS, Opus)",
                i
            );

            // NOTE: No internal_links for audio - linking happens dynamically in the pad probe
            elements.push((audio_input_id, audio_input));
        }

        // Link mux to sink
        internal_links.push((
            ElementPadRef::pad(&mux_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        ));

        info!(
            "Created MPEG-TS/SRT block with {} video track(s) and {} audio chain(s)",
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

/// Build audio chain for raw audio input: audioconvert -> audioresample -> avenc_aac -> aacparse -> mux
fn build_raw_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
    track_index: usize,
) -> Result<(), String> {
    // Create elements
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

    let encoder = gst::ElementFactory::make("avenc_aac")
        .name(&encoder_name)
        .build()
        .map_err(|e| format!("avenc_aac: {}", e))?;

    let parser = gst::ElementFactory::make("aacparse")
        .name(&parser_name)
        .build()
        .map_err(|e| format!("aacparse: {}", e))?;

    // Add elements to bin
    bin.add_many([&audioconvert, &audioresample, &encoder, &parser])
        .map_err(|e| format!("add elements: {}", e))?;

    // Sync state
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

    // Get pads
    let audioconvert_sink = audioconvert
        .static_pad("sink")
        .ok_or("audioconvert has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;

    // Request mux pad
    let pad_template = mux
        .pad_template("sink_%d")
        .ok_or("mpegtsmux has no sink_%d pad template")?;
    let mux_sink = mux
        .request_pad(&pad_template, None, None)
        .ok_or("failed to request pad from mpegtsmux")?;

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
        "MPEGTSSRT {}: Audio chain linked (track {}): identity -> audioconvert -> audioresample -> avenc_aac -> aacparse -> mpegtsmux ({})",
        instance_id, track_index, mux_sink.name()
    );

    Ok(())
}

/// Build audio chain for already-encoded audio: parser -> mux
fn build_encoded_audio_chain(
    bin: &gst::Bin,
    mux: &gst::Element,
    identity_src_pad: &gst::Pad,
    instance_id: &str,
    track_index: usize,
    parser_factory: &str,
    codec_name: &str,
) -> Result<(), String> {
    let parser_name = format!("{}:audio_parser_{}", instance_id, track_index);

    let parser = gst::ElementFactory::make(parser_factory)
        .name(&parser_name)
        .build()
        .map_err(|e| format!("{}: {}", parser_factory, e))?;

    // Add parser to bin
    bin.add(&parser).map_err(|e| format!("add parser: {}", e))?;

    // Sync state
    parser
        .sync_state_with_parent()
        .map_err(|e| format!("sync parser: {}", e))?;

    // Get pads
    let parser_sink = parser.static_pad("sink").ok_or("parser has no sink pad")?;
    let parser_src = parser.static_pad("src").ok_or("parser has no src pad")?;

    // Request mux pad
    let pad_template = mux
        .pad_template("sink_%d")
        .ok_or("mpegtsmux has no sink_%d pad template")?;
    let mux_sink = mux
        .request_pad(&pad_template, None, None)
        .ok_or("failed to request pad from mpegtsmux")?;

    // Link chain
    identity_src_pad
        .link(&parser_sink)
        .map_err(|e| format!("link identity -> parser: {:?}", e))?;
    parser_src
        .link(&mux_sink)
        .map_err(|e| format!("link parser -> mux: {:?}", e))?;

    info!(
        "MPEGTSSRT {}: Audio chain linked (track {}): identity -> {} ({}) -> mpegtsmux ({})",
        instance_id,
        track_index,
        parser_factory,
        codec_name,
        mux_sink.name()
    );

    Ok(())
}

/// Get metadata for MPEG-TS/SRT output blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![mpegtssrt_output_definition()]
}

/// Get MPEG-TS/SRT Output block definition (metadata only).
fn mpegtssrt_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.mpegtssrt_output".to_string(),
        name: "MPEG-TS/SRT Output".to_string(),
        description: "Muxes multiple audio/video streams to MPEG Transport Stream and outputs via SRT. Supports H.264, H.265, and DIRAC video codecs only (AV1 and VP9 are NOT supported by MPEG-TS standard). Auto-encodes raw audio to AAC. Optimized for UDP streaming with alignment=7.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Number of Video Tracks".to_string(),
                description: "Number of video input tracks (0-16)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_video_tracks".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "num_audio_tracks".to_string(),
                label: "Number of Audio Tracks".to_string(),
                description: "Number of audio input tracks (0-32)".to_string(),
                property_type: PropertyType::UInt,
                default_value: Some(PropertyValue::UInt(1)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "num_audio_tracks".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "srt_uri".to_string(),
                label: "SRT URI".to_string(),
                description: "SRT URI (e.g., 'srt://127.0.0.1:5000?mode=caller' or 'srt://:5000?mode=listener')".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("srt://:5000?mode=listener".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "srt_uri".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "latency".to_string(),
                label: "SRT Latency (ms)".to_string(),
                description: "SRT latency in milliseconds (default: 125ms)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(125)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "latency".to_string(),
                    transform: None,
                },
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
            },
            ExposedProperty {
                name: "sync".to_string(),
                label: "Sync".to_string(),
                description: "Synchronize output to pipeline clock. Set to false for transcoding workloads with discontinuous timestamps (default: true)".to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sync".to_string(),
                    transform: None,
                },
            },
        ],
        // External pads are now computed dynamically based on num_video_tracks and num_audio_tracks properties
        // This is just the default/fallback configuration
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_input".to_string(),
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
