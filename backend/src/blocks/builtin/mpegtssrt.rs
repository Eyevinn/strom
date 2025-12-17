//! MPEG-TS over SRT output block builder.
//!
//! This block muxes multiple video and audio streams into MPEG Transport Stream
//! and outputs via SRT (Secure Reliable Transport).
//!
//! Features:
//! - Direct passthrough for encoded video (no parsing overhead)
//! - Automatic AAC encoding for raw audio inputs
//! - Configurable inputs: 1 video input + 1-32 audio inputs (default: 1 audio)
//! - Optimized for UDP streaming (alignment=7 on mpegtsmux)
//! - SRT with auto-reconnect and configurable latency
//!
//! Input handling:
//! - Video: Expects properly encoded video in MPEG-TS compatible format (H.264, H.265, or DIRAC only)
//!   - ⚠️  AV1 and VP9 are NOT supported by MPEG-TS standard (will fail at pipeline setup)
//! - Audio: Accepts both raw audio (auto-encodes to AAC) or encoded AAC (adds parser)
//!
//! Pipeline structure:
//! ```text
//! Video (encoded) -> identity -> capsfilter (validate codec) -> mpegtsmux -> srtsink
//! Audio (raw)     -> audioconvert -> audioresample -> avenc_aac -> aacparse -> mpegtsmux
//! Audio (encoded) -> aacparse -> mpegtsmux
//! ```

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::info;

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
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!(
            "📡 Building MPEG-TS/SRT Output block instance: {}",
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

        info!("📡 MPEG-TS muxer configured: alignment=7, pcr-interval=40ms");

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
                "📡 SRT sink configured: uri={}, latency={}ms, wait={}, auto-reconnect={}, sync={}, qos=true",
                srt_uri, latency, wait_for_connection, auto_reconnect, sync
            );
        } else {
            info!(
                "📡 SRT sink configured: uri={}, latency={}ms, wait={}, sync={}, qos=true (auto-reconnect not available)",
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
        let mut elements = vec![(mux_id.clone(), mux), (sink_id.clone(), srtsink)];

        let mut next_mux_pad = 0;

        // Create video input chain if requested: identity -> capsfilter (validate codec) -> mpegtsmux
        // The capsfilter validates that only MPEG-TS compatible codecs are used
        if num_video_tracks > 0 {
            let video_input_id = format!("{}:video_input", instance_id);
            let video_capsfilter_id = format!("{}:video_capsfilter", instance_id);

            let identity = gst::ElementFactory::make("identity")
                .name(&video_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("identity: {}", e)))?;

            // Create capsfilter that only allows MPEG-TS compatible video codecs
            // This gives a clear error message if user tries to use AV1/VP9
            let caps_str = "video/x-h264; video/x-h265; video/x-dirac";
            let caps = caps_str.parse::<gst::Caps>().map_err(|_| {
                BlockBuildError::InvalidConfiguration(format!(
                    "Failed to create video caps filter: {}",
                    caps_str
                ))
            })?;

            let video_capsfilter = gst::ElementFactory::make("capsfilter")
                .name(&video_capsfilter_id)
                .property("caps", &caps)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video capsfilter: {}", e))
                })?;

            info!(
                "📡 Video input: capsfilter validates MPEG-TS compatible codecs (H.264, H.265, DIRAC only)"
            );

            // Link: identity -> capsfilter -> mpegtsmux
            internal_links.push((
                ElementPadRef::pad(&video_input_id, "src"),
                ElementPadRef::pad(&video_capsfilter_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&video_capsfilter_id, "src"),
                ElementPadRef::pad(&mux_id, format!("sink_{}", next_mux_pad)),
            ));
            next_mux_pad += 1;

            elements.push((video_input_id.clone(), identity));
            elements.push((video_capsfilter_id, video_capsfilter));
        }

        // Create audio input chains: audioconvert -> audioresample -> avenc_aac -> aacparse -> mpegtsmux
        // This chain handles both raw audio (encodes to AAC) and already-encoded AAC (passes through)
        for i in 0..num_audio_tracks {
            let audio_input_id = format!("{}:audio_input_{}", instance_id, i);
            let audioresample_id = format!("{}:audio_resample_{}", instance_id, i);
            let encoder_id = format!("{}:audio_encoder_{}", instance_id, i);
            let parser_id = format!("{}:audio_parser_{}", instance_id, i);

            // audioconvert is the entry point (audio_input_N)
            let audioconvert = gst::ElementFactory::make("audioconvert")
                .name(&audio_input_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

            let audioresample = gst::ElementFactory::make("audioresample")
                .name(&audioresample_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

            let encoder = gst::ElementFactory::make("avenc_aac")
                .name(&encoder_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("avenc_aac: {}", e)))?;

            let parser = gst::ElementFactory::make("aacparse")
                .name(&parser_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("aacparse: {}", e)))?;

            // Chain: audioconvert -> audioresample -> avenc_aac -> aacparse -> mpegtsmux
            internal_links.push((
                ElementPadRef::pad(&audio_input_id, "src"),
                ElementPadRef::pad(&audioresample_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&audioresample_id, "src"),
                ElementPadRef::pad(&encoder_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&encoder_id, "src"),
                ElementPadRef::pad(&parser_id, "sink"),
            ));
            // Link parser to mpegtsmux request pad
            internal_links.push((
                ElementPadRef::pad(&parser_id, "src"),
                ElementPadRef::pad(&mux_id, format!("sink_{}", next_mux_pad)),
            ));
            next_mux_pad += 1;

            elements.push((audio_input_id, audioconvert));
            elements.push((audioresample_id, audioresample));
            elements.push((encoder_id, encoder));
            elements.push((parser_id, parser));
        }

        // Link mux to sink
        internal_links.push((
            ElementPadRef::pad(&mux_id, "src"),
            ElementPadRef::pad(&sink_id, "sink"),
        ));

        info!(
            "📡 Created MPEG-TS/SRT block with {} video track(s) and {} audio chain(s)",
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
