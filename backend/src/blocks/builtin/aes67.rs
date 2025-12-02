//! AES67 audio-over-IP block builders.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::str::FromStr;
use strom_types::{block::*, element::ElementPadRef, EnumValue, PropertyValue, *};
use tracing::{debug, info, warn};

// AES67 Input defaults
const AES67_INPUT_DEFAULT_DECODE: bool = true;
const AES67_INPUT_DEFAULT_LATENCY_MS: i64 = 20;
const AES67_INPUT_DEFAULT_TIMEOUT_MS: i64 = 0;

/// AES67 Input block builder.
pub struct AES67InputBuilder;

impl BlockBuilder for AES67InputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building AES67 Input block instance: {}", instance_id);

        // Get SDP property
        let sdp_content = properties
            .get("SDP")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .ok_or_else(|| BlockBuildError::InvalidProperty("SDP property required".to_string()))?;

        // Get decode property
        let decode = properties
            .get("decode")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                PropertyValue::String(s) => s.parse::<bool>().ok(),
                _ => None,
            })
            .unwrap_or(AES67_INPUT_DEFAULT_DECODE);

        // Get latency_ms property
        let latency_ms = properties
            .get("latency_ms")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u32),
                PropertyValue::String(s) => s.parse::<u32>().ok(),
                _ => None,
            })
            .unwrap_or(AES67_INPUT_DEFAULT_LATENCY_MS as u32);

        // Get timeout_ms property (0 = disabled/indefinite)
        let timeout_ms = properties
            .get("timeout_ms")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u64),
                PropertyValue::String(s) => s.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(AES67_INPUT_DEFAULT_TIMEOUT_MS as u64);

        debug!(
            "AES67 Input [{}]: decode={}, latency_ms={}, timeout_ms={}",
            instance_id, decode, latency_ms, timeout_ms
        );

        // Write SDP to temp file
        let sdp_file_path = write_temp_file(sdp_content)?;

        // Create elements with namespaced IDs
        let filesrc_id = format!("{}:filesrc", instance_id);
        let sdpdemux_id = format!("{}:sdpdemux", instance_id);

        let filesrc = gst::ElementFactory::make("filesrc")
            .name(&filesrc_id)
            .property("location", &sdp_file_path)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("filesrc: {}", e)))?;

        let sdpdemux = gst::ElementFactory::make("sdpdemux")
            .name(&sdpdemux_id)
            .property("latency", latency_ms) // Jitterbuffer latency in ms
            .property("timeout", timeout_ms * 1000) // Convert ms to microseconds (0 = disabled)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("sdpdemux: {}", e)))?;

        // Disable RTCP for AES67 input - set as string enum value
        sdpdemux.set_property_from_str("rtcp-mode", "inactivate");

        // Set up pad-added handler on sdpdemux to log new streams
        let sdpdemux_id_for_pad_handler = sdpdemux_id.clone();
        sdpdemux.connect_pad_added(move |element, new_pad| {
            let pad_name = new_pad.name();
            info!(
                "AES67 Input [{}]: New pad added: {}",
                sdpdemux_id_for_pad_handler, pad_name
            );

            // Log element state for debugging
            let (_, current_state, _) = element.state(gst::ClockTime::ZERO);
            info!(
                "AES67 Input [{}]: Element state when pad added: {:?}",
                sdpdemux_id_for_pad_handler, current_state
            );

            // Check if pad is already linked
            if new_pad.is_linked() {
                info!(
                    "AES67 Input [{}]: Pad {} is already linked",
                    sdpdemux_id_for_pad_handler, pad_name
                );
            } else {
                warn!(
                    "AES67 Input [{}]: Pad {} is NOT linked - downstream needs to handle this!",
                    sdpdemux_id_for_pad_handler, pad_name
                );
            }
        });

        // sdpdemux is a GstBin - we can listen for element-added to find internal rtpbin
        // and attach handlers for SSRC changes
        let sdpdemux_id_for_element_handler = sdpdemux_id.clone();
        let sdpdemux_bin = sdpdemux
            .clone()
            .dynamic_cast::<gst::Bin>()
            .expect("sdpdemux should be a Bin");

        sdpdemux_bin.connect_element_added(move |bin, element| {
            let element_name = element.name();
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            info!(
                "AES67 Input [{}]: Internal element added: {} (type: {})",
                sdpdemux_id_for_element_handler, element_name, factory_name
            );

            // Look for rtpbin to attach SSRC change handlers
            if factory_name == "rtpbin" {
                info!(
                    "AES67 Input [{}]: Found rtpbin '{}', attaching SSRC handlers",
                    sdpdemux_id_for_element_handler, element_name
                );

                let sdpdemux_id_for_rtpbin = sdpdemux_id_for_element_handler.clone();
                let bin_weak = bin.downgrade();

                // Handle new pads from rtpbin (new SSRCs)
                element.connect_pad_added(move |_rtpbin, new_pad| {
                    let pad_name = new_pad.name();
                    info!(
                        "AES67 Input [{}]: rtpbin pad added: {}",
                        sdpdemux_id_for_rtpbin, pad_name
                    );

                    // rtpbin pads are named like: recv_rtp_src_<session>_<ssrc>_<pt>
                    // e.g., recv_rtp_src_0_2370698924_96
                    // Split: [recv, rtp, src, 0, 2370698924, 96] -> indices 0-5
                    if pad_name.to_string().starts_with("recv_rtp_src_") {
                        // Extract SSRC from pad name
                        let parts: Vec<&str> = pad_name.split('_').collect();
                        if parts.len() >= 6 {
                            let session = parts[3];
                            let ssrc = parts[4];
                            let pt = parts[5];
                            info!(
                                "AES67 Input [{}]: New SSRC detected: {} (session: {}, PT: {})",
                                sdpdemux_id_for_rtpbin, ssrc, session, pt
                            );
                        }

                        // Check if pad is linked
                        if new_pad.is_linked() {
                            info!(
                                "AES67 Input [{}]: rtpbin pad {} is linked",
                                sdpdemux_id_for_rtpbin, pad_name
                            );
                        } else {
                            warn!(
                                "AES67 Input [{}]: rtpbin pad {} is NOT linked - SSRC change may need handling!",
                                sdpdemux_id_for_rtpbin, pad_name
                            );

                            // Try to find the ghost pad (stream_0) and reconnect
                            // bin_weak points to sdpdemux itself (from connect_element_added)
                            if let Some(sdpdemux_bin) = bin_weak.upgrade() {
                                // sdpdemux_bin IS the sdpdemux, stream_0 is directly on it
                                // Look for stream_0 ghost pad
                                if let Some(stream_pad) = sdpdemux_bin.static_pad("stream_0") {
                                    info!(
                                        "AES67 Input [{}]: Found stream_0 pad, attempting retarget",
                                        sdpdemux_id_for_rtpbin
                                    );

                                    // Check if stream_0's internal target is linked to old SSRC
                                    if let Some(ghost_pad) =
                                        stream_pad.dynamic_cast_ref::<gst::GhostPad>()
                                    {
                                        if let Some(target) = ghost_pad.target() {
                                            info!(
                                                "AES67 Input [{}]: stream_0 current target: {}",
                                                sdpdemux_id_for_rtpbin,
                                                target.name()
                                            );
                                        }

                                        // Retarget ghost pad to new SSRC pad
                                        if ghost_pad.set_target(Some(new_pad)).is_ok() {
                                            info!(
                                                "AES67 Input [{}]: Successfully retargeted stream_0 to new SSRC pad {}",
                                                sdpdemux_id_for_rtpbin, pad_name
                                            );
                                        } else {
                                            warn!(
                                                "AES67 Input [{}]: Failed to retarget stream_0 to {}",
                                                sdpdemux_id_for_rtpbin, pad_name
                                            );
                                        }
                                    } else {
                                        warn!(
                                            "AES67 Input [{}]: stream_0 is not a GhostPad",
                                            sdpdemux_id_for_rtpbin
                                        );
                                    }
                                } else {
                                    warn!(
                                        "AES67 Input [{}]: Could not find stream_0 pad on sdpdemux",
                                        sdpdemux_id_for_rtpbin
                                    );
                                }
                            } else {
                                warn!(
                                    "AES67 Input [{}]: Could not upgrade weak reference to sdpdemux",
                                    sdpdemux_id_for_rtpbin
                                );
                            }
                        }
                    }
                });

                // Also handle pad-removed for cleanup
                let sdpdemux_id_for_removed = sdpdemux_id_for_element_handler.clone();
                element.connect_pad_removed(move |_rtpbin, removed_pad| {
                    let pad_name = removed_pad.name();
                    info!(
                        "AES67 Input [{}]: rtpbin pad removed: {}",
                        sdpdemux_id_for_removed, pad_name
                    );
                });
            }
        });

        // Build result depends on decode setting
        if decode {
            // Create decode chain: decodebin -> audioconvert -> audioresample
            let decodebin_id = format!("{}:decodebin", instance_id);
            let audioconvert_id = format!("{}:audioconvert", instance_id);
            let audioresample_id = format!("{}:audioresample", instance_id);

            let decodebin = gst::ElementFactory::make("decodebin")
                .name(&decodebin_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("decodebin: {}", e)))?;

            let audioconvert = gst::ElementFactory::make("audioconvert")
                .name(&audioconvert_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

            let audioresample = gst::ElementFactory::make("audioresample")
                .name(&audioresample_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

            // Set up pad-added handler on decodebin to link to audioconvert
            let audioconvert_weak = audioconvert.downgrade();
            let decodebin_id_clone = decodebin_id.clone();
            decodebin.connect_pad_added(move |_element, new_pad| {
                let pad_name = new_pad.name();
                info!(
                    "AES67 Input decodebin [{}]: New pad added: {}",
                    decodebin_id_clone, pad_name
                );

                // Only link audio pads
                if let Some(caps) = new_pad.current_caps() {
                    let structure = caps.structure(0);
                    if let Some(s) = structure {
                        let name = s.name();
                        if name.starts_with("audio/") {
                            if let Some(audioconvert) = audioconvert_weak.upgrade() {
                                if let Some(sink_pad) = audioconvert.static_pad("sink") {
                                    if !sink_pad.is_linked() && new_pad.link(&sink_pad).is_ok() {
                                        info!(
                                            "AES67 Input decodebin [{}]: Linked {} to audioconvert",
                                            decodebin_id_clone, pad_name
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });

            Ok(BlockBuildResult {
                elements: vec![
                    (filesrc_id.clone(), filesrc),
                    (sdpdemux_id.clone(), sdpdemux),
                    (decodebin_id.clone(), decodebin),
                    (audioconvert_id.clone(), audioconvert),
                    (audioresample_id.clone(), audioresample),
                ],
                internal_links: vec![
                    (
                        ElementPadRef::pad(&filesrc_id, "src"),
                        ElementPadRef::pad(&sdpdemux_id, "sink"),
                    ),
                    // sdpdemux:stream_0 -> decodebin:sink (dynamic pad - pipeline builder handles)
                    (
                        ElementPadRef::pad(&sdpdemux_id, "stream_0"),
                        ElementPadRef::pad(&decodebin_id, "sink"),
                    ),
                    // decodebin -> audioconvert is dynamic (handled by pad-added above)
                    (
                        ElementPadRef::pad(&audioconvert_id, "src"),
                        ElementPadRef::pad(&audioresample_id, "sink"),
                    ),
                ],
                bus_message_handler: None,
                pad_properties: HashMap::new(),
            })
        } else {
            // No decode - output RTP stream directly
            Ok(BlockBuildResult {
                elements: vec![
                    (filesrc_id.clone(), filesrc),
                    (sdpdemux_id.clone(), sdpdemux),
                ],
                internal_links: vec![(
                    ElementPadRef::pad(&filesrc_id, "src"),
                    ElementPadRef::pad(&sdpdemux_id, "sink"),
                )],
                bus_message_handler: None,
                pad_properties: HashMap::new(),
            })
        }
    }
}

/// AES67 Output block builder.
pub struct AES67OutputBuilder;

impl BlockBuilder for AES67OutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building AES67 Output block instance: {}", instance_id);

        // Extract properties with defaults
        let bit_depth = properties
            .get("bit_depth")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i),
                PropertyValue::String(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(24);

        let sample_rate = properties
            .get("sample_rate")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i),
                PropertyValue::String(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(48000);

        let channels = properties
            .get("channels")
            .and_then(|v| {
                if let PropertyValue::Int(i) = v {
                    Some(*i)
                } else {
                    None
                }
            })
            .unwrap_or(2);

        let ptime_ms = properties
            .get("ptime")
            .and_then(|v| match v {
                PropertyValue::Float(f) => Some(*f),
                PropertyValue::String(s) => s.parse::<f64>().ok(),
                _ => None,
            })
            .unwrap_or(1.0);

        let host = properties
            .get("host")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "239.69.1.1".to_string());

        let port = properties
            .get("port")
            .and_then(|v| {
                if let PropertyValue::Int(i) = v {
                    Some(*i as i32)
                } else {
                    None
                }
            })
            .unwrap_or(5004);

        // Create namespaced element IDs
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);
        let payloader_id = format!("{}:payloader", instance_id);
        let udpsink_id = format!("{}:udpsink", instance_id);

        // Create elements
        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        // Build caps string
        let caps_str = format!("audio/x-raw,channels={},rate={}", channels, sample_rate);
        let caps = gst::Caps::from_str(&caps_str)
            .map_err(|_| BlockBuildError::InvalidProperty(format!("Invalid caps: {}", caps_str)))?;

        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        // Select payloader based on bit depth
        let payloader_type = match bit_depth {
            16 => "rtpL16pay",
            24 => "rtpL24pay",
            _ => {
                return Err(BlockBuildError::InvalidConfiguration(format!(
                    "Unsupported bit depth: {}. Must be 16 or 24.",
                    bit_depth
                )))
            }
        };

        // Convert ptime from ms to ns
        let ptime_ns = (ptime_ms * 1_000_000.0) as i64;

        let payloader = gst::ElementFactory::make(payloader_type)
            .name(&payloader_id)
            .property("timestamp-offset", 0u32)
            .property("min-ptime", ptime_ns)
            .property("max-ptime", ptime_ns)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", payloader_type, e)))?;

        // Set processing-deadline to match ptime for proper timing
        let processing_deadline_ns = ptime_ns as u64;

        let udpsink = gst::ElementFactory::make("udpsink")
            .name(&udpsink_id)
            .property("host", &host)
            .property("port", port)
            .property("async", false)
            .property("sync", true)
            .property(
                "processing-deadline",
                gst::ClockTime::from_nseconds(processing_deadline_ns),
            )
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("udpsink: {}", e)))?;

        // Define internal links
        let internal_links = vec![
            (
                ElementPadRef::pad(&audioconvert_id, "src"),
                ElementPadRef::pad(&audioresample_id, "sink"),
            ),
            (
                ElementPadRef::pad(&audioresample_id, "src"),
                ElementPadRef::pad(&capsfilter_id, "sink"),
            ),
            (
                ElementPadRef::pad(&capsfilter_id, "src"),
                ElementPadRef::pad(&payloader_id, "sink"),
            ),
            (
                ElementPadRef::pad(&payloader_id, "src"),
                ElementPadRef::pad(&udpsink_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (audioconvert_id, audioconvert),
                (audioresample_id, audioresample),
                (capsfilter_id, capsfilter),
                (payloader_id, payloader),
                (udpsink_id, udpsink),
            ],
            internal_links,
            bus_message_handler: None,
            pad_properties: HashMap::new(),
        })
    }
}

/// Get metadata for AES67 blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![aes67_input_definition(), aes67_output_definition()]
}

/// Get AES67 Input block definition (metadata only).
fn aes67_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.aes67_input".to_string(),
        name: "AES67 Input".to_string(),
        description: "Receives AES67/Ravenna audio via RTP multicast. Uses sdpdemux to parse SDP and decode the incoming stream.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "SDP".to_string(),
                label: "SDP".to_string(),
                description: "Session Description Protocol content describing the stream source"
                    .to_string(),
                property_type: PropertyType::Multiline,
                default_value: None,
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "SDP".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode RTP to raw audio (decodebin + audioconvert + audioresample)"
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(AES67_INPUT_DEFAULT_DECODE)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "latency_ms".to_string(),
                label: "Latency (ms)".to_string(),
                description: "Jitterbuffer latency in milliseconds".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(AES67_INPUT_DEFAULT_LATENCY_MS)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "latency_ms".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "timeout_ms".to_string(),
                label: "Timeout (ms)".to_string(),
                description: "UDP timeout in milliseconds (0 = disabled/indefinite)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(AES67_INPUT_DEFAULT_TIMEOUT_MS)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "timeout_ms".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                // When decode=true (default), output is from audioresample
                // When decode=false, pipeline builder will use sdpdemux:stream_0 directly
                internal_element_id: "audioresample".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🎵".to_string()),
            color: Some("#4CAF50".to_string()),
            width: Some(2.0),
            height: Some(1.5),
        }),
    }
}

/// Get AES67 Output block definition (metadata only).
fn aes67_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.aes67_output".to_string(),
        name: "AES67 Output".to_string(),
        description: "Sends AES67/Ravenna audio via RTP multicast. Supports L16/L24 encoding with configurable packet time.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "bit_depth".to_string(),
                label: "Bit Depth".to_string(),
                description: "Audio sample bit depth (16 or 24 bit PCM)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "16".to_string(), label: Some("16-bit".to_string()) },
                        EnumValue { value: "24".to_string(), label: Some("24-bit".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("24".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "bit_depth".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "sample_rate".to_string(),
                label: "Sample Rate".to_string(),
                description: "Audio sample rate in Hz".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "32000".to_string(), label: Some("32 kHz".to_string()) },
                        EnumValue { value: "44100".to_string(), label: Some("44.1 kHz".to_string()) },
                        EnumValue { value: "48000".to_string(), label: Some("48 kHz".to_string()) },
                        EnumValue { value: "88200".to_string(), label: Some("88.2 kHz".to_string()) },
                        EnumValue { value: "96000".to_string(), label: Some("96 kHz".to_string()) },
                        EnumValue { value: "176400".to_string(), label: Some("176.4 kHz".to_string()) },
                        EnumValue { value: "192000".to_string(), label: Some("192 kHz".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("48000".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "sample_rate".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "channels".to_string(),
                label: "Channels".to_string(),
                description: "Number of audio channels (1-8)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(2)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "channels".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "ptime".to_string(),
                label: "Packet Time (ms)".to_string(),
                description: "RTP packet duration in milliseconds".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue { value: "0.125".to_string(), label: Some("0.125 ms".to_string()) },
                        EnumValue { value: "0.25".to_string(), label: Some("0.25 ms".to_string()) },
                        EnumValue { value: "1.0".to_string(), label: Some("1.0 ms".to_string()) },
                        EnumValue { value: "4.0".to_string(), label: Some("4.0 ms".to_string()) },
                    ],
                },
                default_value: Some(PropertyValue::String("1.0".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ptime".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "host".to_string(),
                label: "Multicast Address".to_string(),
                description: "Destination multicast IP address".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("239.69.1.1".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "host".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "port".to_string(),
                label: "UDP Port".to_string(),
                description: "Destination UDP port number".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(5004)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "port".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![ExternalPad {
                name: "audio_in".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "audioconvert".to_string(),
                internal_pad_name: "sink".to_string(),
            }],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            color: Some("#2196F3".to_string()),
            width: Some(2.5),
            height: Some(2.0),
        }),
    }
}

/// Write content to a temporary file and return its path.
fn write_temp_file(content: &str) -> Result<String, BlockBuildError> {
    use tempfile::NamedTempFile;

    let mut temp_file = NamedTempFile::new().map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to create temp file: {}", e))
    })?;

    temp_file.write_all(content.as_bytes()).map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to write temp file: {}", e))
    })?;

    temp_file.flush().map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to flush temp file: {}", e))
    })?;

    let (_file, path) = temp_file.keep().map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to keep temp file: {}", e))
    })?;

    let path_str = path.to_string_lossy().to_string();
    debug!("Created temp file for SDP: {}", path_str);

    Ok(path_str)
}
