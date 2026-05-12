//! WHEP (WebRTC-HTTP Egress Protocol) block builders.
//!
//! WHEP Input - Receives streams from external WHEP servers:
//! - `whepclientsrc` (new): Uses signaller interface
//! - `whepsrc` (stable): Simpler implementation with direct properties
//!
//! WHEP Output - Hosts a WHEP server for clients to connect and receive streams:
//! - `whepserversink`: Hosts HTTP endpoint, clients connect via WHEP to receive
//!
//! Handles dynamic pad creation by linking new audio streams to a liveadder mixer.

use crate::blocks::{BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder};
use crate::gst::whep_probe::{self, WhepProbeRegistry};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use strom_types::block::StreamMode;
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// WHEP Input block builder.
pub struct WHEPInputBuilder;

/// WHEP Output block builder (hosts WHEP server).
pub struct WHEPOutputBuilder;

impl BlockBuilder for WHEPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Output block instance: {}", instance_id);
        build_whepserversink(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
        let has_video = num_video_tracks > 0;
        let has_audio = num_audio_tracks > 0;

        let mut inputs = Vec::new();

        for slot in 0..num_video_tracks {
            // Slot 0 keeps the unsuffixed names (video_in / video_queue) so
            // existing flows continue to link without modification when
            // num_video_tracks grows past 1.
            let (pad_name, queue_id) = if slot == 0 {
                ("video_in".to_string(), "video_queue".to_string())
            } else {
                (
                    format!("video_in_{}", slot),
                    format!("video_queue_{}", slot),
                )
            };

            let label = if has_audio || num_video_tracks > 1 {
                Some(format!("V{}", slot))
            } else {
                None
            };

            inputs.push(ExternalPad {
                label,
                name: pad_name,
                media_type: MediaType::Video,
                internal_element_id: queue_id,
                internal_pad_name: "sink".to_string(),
            });
        }

        for slot in 0..num_audio_tracks {
            // Slot 0 keeps the unsuffixed names (audio_in / audio_queue) so
            // existing flows continue to link without modification when
            // num_audio_tracks grows past 1. Other blocks use audio_in_0 for
            // slot 0; this asymmetry is deliberate for backwards compat.
            let (pad_name, queue_id) = if slot == 0 {
                ("audio_in".to_string(), "audio_queue".to_string())
            } else {
                (
                    format!("audio_in_{}", slot),
                    format!("audio_queue_{}", slot),
                )
            };

            let label = if has_video || num_audio_tracks > 1 {
                Some(format!("A{}", slot))
            } else {
                None
            };

            inputs.push(ExternalPad {
                label,
                name: pad_name,
                media_type: MediaType::Audio,
                internal_element_id: queue_id,
                internal_pad_name: "sink".to_string(),
            });
        }

        Some(ExternalPads {
            inputs,
            outputs: vec![],
        })
    }
}

/// Resolve audio and video track counts from block properties.
///
/// Returns `(num_audio_tracks, num_video_tracks)`. 0 means the media type is
/// disabled on this endpoint; 1..=8 produces that many request pads.
///
/// Resolution order per media type:
/// 1. Explicit `num_audio_tracks` / `num_video_tracks` property (clamped to 0..=8).
/// 2. Legacy `mode` enum (`"audio"` / `"video"` / `"audio_video"`): translated
///    to 0 or 1 based on which media types it enabled. Allows old flows saved
///    before the count-based API to keep working.
/// 3. Default `1` when no property is present, matching the previous behaviour
///    where a missing `mode` was treated as audio+video.
fn resolve_track_counts(properties: &HashMap<String, PropertyValue>) -> (usize, usize) {
    let explicit_audio = explicit_track_count(properties, "num_audio_tracks");
    let explicit_video = explicit_track_count(properties, "num_video_tracks");
    let legacy_mode = properties.get("mode").and_then(|v| match v {
        PropertyValue::String(s) => Some(StreamMode::parse(s)),
        _ => None,
    });

    let num_audio = match (explicit_audio, &legacy_mode) {
        (Some(n), _) => n,
        (None, Some(m)) => {
            if m.has_audio() {
                1
            } else {
                0
            }
        }
        (None, None) => 1,
    };
    let num_video = match (explicit_video, &legacy_mode) {
        (Some(n), _) => n,
        (None, Some(m)) => {
            if m.has_video() {
                1
            } else {
                0
            }
        }
        (None, None) => 1,
    };

    (num_audio, num_video)
}

fn explicit_track_count(properties: &HashMap<String, PropertyValue>, name: &str) -> Option<usize> {
    properties.get(name).and_then(|v| match v {
        PropertyValue::UInt(u) => Some((*u as usize).min(8)),
        PropertyValue::Int(i) => Some((*i).clamp(0, 8) as usize),
        _ => None,
    })
}

impl BlockBuilder for WHEPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHEP Input block instance: {}", instance_id);

        // Get implementation choice (default to stable whepsrc)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whepclientsrc")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whepclientsrc(instance_id, properties, ctx)
        } else {
            build_whepsrc(instance_id, properties, ctx)
        }
    }
}

/// Build using the stable whepsrc implementation
fn build_whepsrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Input using whepsrc (stable)");

    // Get required WHEP endpoint
    let whep_endpoint = properties
        .get("whep_endpoint")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            BlockBuildError::InvalidProperty("whep_endpoint property required".to_string())
        })?;

    // Get optional auth token
    let auth_token = properties.get("auth_token").and_then(|v| {
        if let PropertyValue::String(s) = v {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        } else {
            None
        }
    });

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Get mixer latency (default 30ms - lower than default 200ms for lower latency)
    let mixer_latency_ms = properties
        .get("mixer_latency_ms")
        .and_then(|v| {
            if let PropertyValue::Int(i) = v {
                Some(*i as u64)
            } else {
                None
            }
        })
        .unwrap_or(30);

    // Get jitterbuffer latency (default 200ms is GStreamer's webrtcbin default)
    let jitterbuffer_latency_ms = properties
        .get("jitterbuffer_latency_ms")
        .and_then(|v| {
            if let PropertyValue::Int(i) = v {
                Some(*i as u32)
            } else {
                None
            }
        })
        .unwrap_or(DEFAULT_JITTERBUFFER_LATENCY_MS as u32);

    // Create namespaced element IDs
    let instance_id_owned = instance_id.to_string();
    let whepsrc_id = format!("{}:whepsrc", instance_id);
    let liveadder_id = format!("{}:liveadder", instance_id);
    let capsfilter_id = format!("{}:capsfilter", instance_id);
    let output_audioconvert_id = format!("{}:output_audioconvert", instance_id);
    let output_audioresample_id = format!("{}:output_audioresample", instance_id);

    // Create whepsrc element (stable - direct properties)
    let whepsrc = gst::ElementFactory::make("whepsrc")
        .name(&whepsrc_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepsrc: {}", e)))?;

    // Set properties directly on whepsrc (no signaller child)
    whepsrc.set_property("whep-endpoint", &whep_endpoint);
    // Explicitly clear defaults when not configured,
    // since whepsrc defaults to stun://stun.l.google.com:19302
    match stun_server {
        Some(ref stun) => whepsrc.set_property("stun-server", stun),
        None => whepsrc.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        whepsrc.set_property("turn-server", turn);
    }

    if let Some(token) = &auth_token {
        whepsrc.set_property("auth-token", token);
    }

    // Set jitterbuffer latency on the internal webrtcbin.
    // webrtcbin is created during whepsrc construction, so we must iterate
    // existing children. We also install deep-element-added for any future additions.
    if let Ok(bin) = whepsrc.clone().downcast::<gst::Bin>() {
        // Set on already-existing children (webrtcbin and its internal rtpbin)
        for element in bin.iterate_recurse().into_iter().flatten() {
            let name = element.name();
            if name.starts_with("webrtcbin") && element.has_property("latency") {
                element.set_property("latency", jitterbuffer_latency_ms);
                info!(
                    "WHEP Input (whepsrc): Set jitterbuffer latency={}ms on existing {}",
                    jitterbuffer_latency_ms, name
                );
            }
            // Workaround for GStreamer rtpjitterbuffer packet_spacing bug:
            // After a mute gap (no RTP packets), calculate_packet_spacing sees
            // the large RTP timestamp jump as huge packet spacing. This corrupts
            // lost timer scheduling, causing packets to be held for the duration
            // of the mute gap instead of being output immediately.
            // Setting drop-on-latency on rtpbin propagates to all its
            // jitterbuffers, making them drop queued packets that exceed the
            // configured latency — breaking the stall.
            // Upstream: https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/951
            if name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                element.set_property("drop-on-latency", true);
                info!(
                    "WHEP Input (whepsrc): Set drop-on-latency=true on existing {}",
                    name
                );
            }
        }

        // Also catch any dynamically added webrtcbins, rtpbins and jitterbuffers
        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            if element_name.starts_with("webrtcbin") && element.has_property("latency") {
                element.set_property("latency", jitterbuffer_latency_ms);
                info!(
                    "WHEP Input (whepsrc): Set jitterbuffer latency={}ms on {}",
                    jitterbuffer_latency_ms, element_name
                );
            }

            None
        });
    }

    // Create liveadder - this is our always-present mixer for dynamic audio streams
    // force-live=true: operate in live mode and aggregate on timeout even without upstream live sources
    // start-time-selection=first: use the first buffer's timestamp as start time (essential for PTP clocks)
    //   Without this, liveadder defaults to start-time=0, but PTP clock running time is billions of ns
    let liveadder = gst::ElementFactory::make("liveadder")
        .name(&liveadder_id)
        .property("latency", mixer_latency_ms as u32)
        .property("force-live", true)
        .property_from_str("start-time-selection", "first")
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

    // Set min-upstream-latency so liveadder accounts for jitterbuffer buffering delay
    if liveadder.find_property("min-upstream-latency").is_some() {
        let min_upstream_ns = jitterbuffer_latency_ms as u64 * 1_000_000;
        liveadder.set_property(
            "min-upstream-latency",
            min_upstream_ns * gst::ClockTime::NSECOND,
        );
        info!(
            "WHEP Input (whepsrc): Set min-upstream-latency={}ms on liveadder",
            jitterbuffer_latency_ms
        );
    }

    // Create capsfilter to enforce 48kHz stereo audio after liveadder
    let caps = gst::Caps::builder("audio/x-raw")
        .field("rate", 48000i32)
        .field("channels", 2i32)
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name(&capsfilter_id)
        .property("caps", &caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

    // Create output audio processing chain
    let output_audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&output_audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioconvert: {}", e)))?;

    let output_audioresample = gst::ElementFactory::make("audioresample")
        .name(&output_audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioresample: {}", e)))?;

    // Set up WHEP diagnostic probes if enabled
    let probe_registry = whep_probe::setup_whep_probes(&whepsrc, &instance_id_owned);

    // Counter for unique element naming
    let stream_counter = Arc::new(AtomicUsize::new(0));

    // Clone references for the pad-added callback
    let liveadder_weak = liveadder.downgrade();
    let stream_counter_clone = Arc::clone(&stream_counter);
    let probe_registry_clone = probe_registry.clone();

    // Set up pad-added callback on whepsrc
    // whepsrc also creates dynamic src_%u pads like whepclientsrc
    whepsrc.connect_pad_added(move |src, pad| {
        let pad_name = pad.name();

        info!(
            "WHEP (stable): New pad added on whepsrc: {} - waiting for caps to determine media type",
            pad_name
        );

        if let Some(liveadder) = liveadder_weak.upgrade() {
            let stream_num = stream_counter_clone.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = setup_stream_with_caps_detection(
                src,
                pad,
                &liveadder,
                &instance_id_owned,
                stream_num,
                &probe_registry_clone,
            ) {
                error!("Failed to setup stream with caps detection: {}", e);
            }
        } else {
            error!("WHEP (stable): liveadder no longer exists");
        }
    });

    debug!(
        "WHEP Input (whepsrc stable) configured: endpoint={}, stun={:?}, turn={:?}",
        whep_endpoint, stun_server, turn_server
    );

    // Internal links: liveadder -> capsfilter -> audioconvert -> audioresample
    // Note: No silence generator - using force-live=true on liveadder instead
    // WHEP audio streams are linked dynamically via pad-added callback
    let internal_links = vec![
        (
            ElementPadRef::pad(&liveadder_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        ),
        (
            ElementPadRef::pad(&capsfilter_id, "src"),
            ElementPadRef::pad(&output_audioconvert_id, "sink"),
        ),
        (
            ElementPadRef::pad(&output_audioconvert_id, "src"),
            ElementPadRef::pad(&output_audioresample_id, "sink"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (whepsrc_id, whepsrc),
            (liveadder_id, liveadder),
            (capsfilter_id, capsfilter),
            (output_audioconvert_id, output_audioconvert),
            (output_audioresample_id, output_audioresample),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Build using the new whepclientsrc (signaller-based) implementation
fn build_whepclientsrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Input using whepclientsrc (new implementation)");

    // Get required WHEP endpoint
    let whep_endpoint = properties
        .get("whep_endpoint")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .ok_or_else(|| {
            BlockBuildError::InvalidProperty("whep_endpoint property required".to_string())
        })?;

    // Get optional auth token
    let auth_token = properties.get("auth_token").and_then(|v| {
        if let PropertyValue::String(s) = v {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        } else {
            None
        }
    });

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Get mixer latency (default 30ms - lower than default 200ms for lower latency)
    let mixer_latency_ms = properties
        .get("mixer_latency_ms")
        .and_then(|v| {
            if let PropertyValue::Int(i) = v {
                Some(*i as u64)
            } else {
                None
            }
        })
        .unwrap_or(30);

    // Get jitterbuffer latency (default 200ms is GStreamer's webrtcbin default)
    let jitterbuffer_latency_ms = properties
        .get("jitterbuffer_latency_ms")
        .and_then(|v| {
            if let PropertyValue::Int(i) = v {
                Some(*i as u32)
            } else {
                None
            }
        })
        .unwrap_or(DEFAULT_JITTERBUFFER_LATENCY_MS as u32);

    // Create namespaced element IDs
    let instance_id_owned = instance_id.to_string();
    let whepclientsrc_id = format!("{}:whepclientsrc", instance_id);
    let liveadder_id = format!("{}:liveadder", instance_id);
    let capsfilter_id = format!("{}:capsfilter", instance_id);
    let output_audioconvert_id = format!("{}:output_audioconvert", instance_id);
    let output_audioresample_id = format!("{}:output_audioresample", instance_id);

    // Create whepclientsrc element
    let whepclientsrc = gst::ElementFactory::make("whepclientsrc")
        .name(&whepclientsrc_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepclientsrc: {}", e)))?;

    // Set ICE server properties on the source (explicitly clear defaults when
    // not configured, since webrtcsrc defaults to stun://stun.l.google.com:19302)
    match stun_server {
        Some(ref stun) => whepclientsrc.set_property("stun-server", stun),
        None => whepclientsrc.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        whepclientsrc.set_property("turn-server", turn);
    }

    // Access the signaller child and set its properties
    let signaller = whepclientsrc.property::<gst::glib::Object>("signaller");
    signaller.set_property("whep-endpoint", &whep_endpoint);

    if let Some(token) = &auth_token {
        signaller.set_property("auth-token", token);
    }

    // Create liveadder - this is our always-present mixer for dynamic audio streams
    // force-live=true: operate in live mode and aggregate on timeout even without upstream live sources
    // start-time-selection=first: use the first buffer's timestamp as start time (essential for PTP clocks)
    //   Without this, liveadder defaults to start-time=0, but PTP clock running time is billions of ns
    let liveadder = gst::ElementFactory::make("liveadder")
        .name(&liveadder_id)
        .property("latency", mixer_latency_ms as u32)
        .property("force-live", true)
        .property_from_str("start-time-selection", "first")
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

    // Set min-upstream-latency so liveadder accounts for jitterbuffer buffering delay
    if liveadder.find_property("min-upstream-latency").is_some() {
        let min_upstream_ns = jitterbuffer_latency_ms as u64 * 1_000_000;
        liveadder.set_property(
            "min-upstream-latency",
            min_upstream_ns * gst::ClockTime::NSECOND,
        );
        info!(
            "WHEP Input (whepclientsrc): Set min-upstream-latency={}ms on liveadder",
            jitterbuffer_latency_ms
        );
    }

    // Create capsfilter to enforce 48kHz stereo audio after liveadder
    let caps = gst::Caps::builder("audio/x-raw")
        .field("rate", 48000i32)
        .field("channels", 2i32)
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .name(&capsfilter_id)
        .property("caps", &caps)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

    // Create output audio processing chain (after liveadder -> capsfilter)
    let output_audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&output_audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioconvert: {}", e)))?;

    let output_audioresample = gst::ElementFactory::make("audioresample")
        .name(&output_audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioresample: {}", e)))?;

    // Set up WHEP diagnostic probes if enabled
    let probe_registry = whep_probe::setup_whep_probes(&whepclientsrc, &instance_id_owned);

    // Counter for unique element naming
    let stream_counter = Arc::new(AtomicUsize::new(0));

    // Clone references for the pad-added callback
    let liveadder_weak = liveadder.downgrade();
    let stream_counter_clone = Arc::clone(&stream_counter);
    let probe_registry_clone = probe_registry.clone();

    // Set up pad-added callback on whepclientsrc
    // This handles dynamic pads created when WebRTC streams are negotiated
    // NOTE: We can't trust pad names OR query_caps at pad-added time.
    // The actual caps are only set after negotiation completes.
    // Strategy: Install a pad probe to detect actual caps, then:
    // - Audio: decode and route to liveadder
    // - Video: discard via fakesink (no decode - that would be expensive)
    whepclientsrc.connect_pad_added(move |src, pad| {
        let pad_name = pad.name();

        info!(
            "WHEP: New pad added on whepclientsrc: {} - waiting for caps to determine media type",
            pad_name
        );

        if let Some(liveadder) = liveadder_weak.upgrade() {
            let stream_num = stream_counter_clone.fetch_add(1, Ordering::SeqCst);
            if let Err(e) = setup_stream_with_caps_detection(
                src,
                pad,
                &liveadder,
                &instance_id_owned,
                stream_num,
                &probe_registry_clone,
            ) {
                error!("Failed to setup stream with caps detection: {}", e);
            }
        } else {
            error!("WHEP: liveadder no longer exists");
        }
    });

    // ALSO hook into the internal webrtcbin to catch pads that don't get ghostpadded
    // whepclientsrc is a GstBin - we need to find the webrtcbin inside and listen to its pad-added
    if let Ok(bin) = whepclientsrc.clone().downcast::<gst::Bin>() {
        let liveadder_weak2 = liveadder.downgrade();
        let whepclientsrc_weak = whepclientsrc.downgrade();
        let ice_transport_policy = ctx.ice_transport_policy().to_string();

        // Use deep-element-added to catch webrtcbin when it's created
        bin.connect("deep-element-added", false, move |values| {
                let _bin = values[0].get::<gst::Bin>().unwrap();
                let element = values[2].get::<gst::Element>().unwrap();
                let element_name = element.name();

                // Workaround for GStreamer rtpjitterbuffer packet_spacing bug:
                // see comment in build_whepsrc iterate_recurse for details.
                if element_name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                    element.set_property("drop-on-latency", true);
                    info!(
                        "WHEP Input (whepclientsrc): Set drop-on-latency=true on {}",
                        element_name
                    );
                }

                // Look for webrtcbin
                if element_name.starts_with("webrtcbin") {
                    info!("WHEP: Found webrtcbin: {}", element_name);

                    // Set jitterbuffer latency on webrtcbin
                    if element.has_property("latency") {
                        element.set_property("latency", jitterbuffer_latency_ms);
                        info!(
                            "WHEP Input (whepclientsrc): Set jitterbuffer latency={}ms on {}",
                            jitterbuffer_latency_ms, element_name
                        );
                    }

                    // Set ICE transport policy on webrtcbin (from config)
                    if element.has_property("ice-transport-policy") {
                        element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                        info!(
                            "WHEP Input: Set ice-transport-policy={} on webrtcbin {}",
                            ice_transport_policy, element_name
                        );
                    }

                    let liveadder_weak3 = liveadder_weak2.clone();
                    let whepclientsrc_weak2 = whepclientsrc_weak.clone();

                    // Connect to webrtcbin's pad-added signal
                    element.connect_pad_added(move |_webrtcbin, pad| {
                        let pad_name = pad.name();

                        // Only handle src pads
                        if pad.direction() != gst::PadDirection::Src {
                            return;
                        }

                        info!(
                            "WHEP: webrtcbin pad-added: {} (direction: {:?})",
                            pad_name,
                            pad.direction()
                        );

                        // Check if this pad is already linked (ghostpadded)
                        if pad.is_linked() {
                            info!(
                                "WHEP: webrtcbin pad {} is already linked, skipping",
                                pad_name
                            );
                            return;
                        }

                        // This pad is NOT linked - we need to handle it ourselves
                        info!(
                            "WHEP: webrtcbin pad {} is NOT linked - handling directly",
                            pad_name
                        );

                        // Get whepclientsrc - we need it to create ghost pads
                        let whepclientsrc = match whepclientsrc_weak2.upgrade() {
                            Some(e) => e,
                            None => {
                                error!("WHEP: whepclientsrc no longer exists");
                                return;
                            }
                        };

                        // We don't need the pipeline here anymore since the whepclientsrc pad-added
                        // callback will handle the stream setup, but keep the check to detect errors early
                        let _pipeline = match get_pipeline_from_element(&whepclientsrc) {
                            Ok(p) => p,
                            Err(e) => {
                                error!("WHEP: Failed to get pipeline: {}", e);
                                return;
                            }
                        };

                        if let Some(_liveadder) = liveadder_weak3.upgrade() {
                            // Don't increment stream counter here - the whepclientsrc pad-added callback will do it
                            info!(
                                "WHEP: Setting up unlinked webrtcbin pad {}",
                                pad_name
                            );

                            // We need to ghostpad through the bin hierarchy:
                            // webrtcbin (pad) -> whep-client bin (ghost) -> whepclientsrc (ghost)

                            // Step 1: Find the whep-client bin (parent of webrtcbin)
                            let webrtcbin = match pad.parent_element() {
                                Some(e) => e,
                                None => {
                                    error!("WHEP: Could not get parent element of pad {}", pad_name);
                                    return;
                                }
                            };

                            let whep_client_bin = match webrtcbin.parent() {
                                Some(p) => p,
                                None => {
                                    error!("WHEP: Could not get parent of webrtcbin");
                                    return;
                                }
                            };

                            let whep_client_bin = match whep_client_bin.downcast::<gst::Bin>() {
                                Ok(b) => b,
                                Err(_) => {
                                    error!("WHEP: Parent of webrtcbin is not a bin");
                                    return;
                                }
                            };

                            info!("WHEP: Found intermediate bin: {}", whep_client_bin.name());

                            // Step 2: Create ghost pad on whep-client bin to expose webrtcbin pad
                            let intermediate_ghost_name = format!("ghost_intermediate_{}", pad_name);
                            let intermediate_ghost = match gst::GhostPad::builder_with_target(pad) {
                                Ok(builder) => builder.name(&intermediate_ghost_name).build(),
                                Err(e) => {
                                    error!("WHEP: Failed to create intermediate ghost pad: {}", e);
                                    return;
                                }
                            };

                            if let Err(e) = whep_client_bin.add_pad(&intermediate_ghost) {
                                error!("WHEP: Failed to add intermediate ghost pad to whep-client bin: {}", e);
                                return;
                            }

                            if let Err(e) = intermediate_ghost.set_active(true) {
                                error!("WHEP: Failed to activate intermediate ghost pad: {}", e);
                                return;
                            }

                            info!("WHEP: Created intermediate ghost pad {} on whep-client bin", intermediate_ghost_name);

                            // Step 3: Create ghost pad on whepclientsrc to expose the intermediate ghost pad
                            let outer_ghost_name = format!("ghost_audio_{}", pad_name);
                            let outer_ghost = match gst::GhostPad::builder_with_target(&intermediate_ghost) {
                                Ok(builder) => builder.name(&outer_ghost_name).build(),
                                Err(e) => {
                                    error!("WHEP: Failed to create outer ghost pad: {}", e);
                                    return;
                                }
                            };

                            if let Ok(whepclientsrc_bin) = whepclientsrc.clone().downcast::<gst::Bin>() {
                                if let Err(e) = whepclientsrc_bin.add_pad(&outer_ghost) {
                                    error!("WHEP: Failed to add outer ghost pad to whepclientsrc: {}", e);
                                    return;
                                }

                                if let Err(e) = outer_ghost.set_active(true) {
                                    error!("WHEP: Failed to activate outer ghost pad: {}", e);
                                    return;
                                }

                                info!(
                                    "WHEP: Created outer ghost pad {} on whepclientsrc - will be handled by pad-added callback",
                                    outer_ghost_name
                                );
                            } else {
                                error!("WHEP: whepclientsrc is not a bin, cannot add ghost pad");
                            }
                        }
                    });
                }

                None
            });
    }

    debug!(
        "WHEP Input configured: endpoint={}, stun={:?}, turn={:?}",
        whep_endpoint, stun_server, turn_server
    );

    // Internal links: liveadder -> capsfilter -> audioconvert -> audioresample
    // Note: No silence generator - using force-live=true on liveadder instead
    // WHEP audio streams are linked dynamically via pad-added callback
    let internal_links = vec![
        (
            ElementPadRef::pad(&liveadder_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        ),
        (
            ElementPadRef::pad(&capsfilter_id, "src"),
            ElementPadRef::pad(&output_audioconvert_id, "sink"),
        ),
        (
            ElementPadRef::pad(&output_audioconvert_id, "src"),
            ElementPadRef::pad(&output_audioresample_id, "sink"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (whepclientsrc_id, whepclientsrc),
            (liveadder_id, liveadder),
            (capsfilter_id, capsfilter),
            (output_audioconvert_id, output_audioconvert),
            (output_audioresample_id, output_audioresample),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Build WHEP Output using whepserversink (hosts HTTP server for WHEP clients).
///
/// This element creates an HTTP server that WHEP clients can connect to
/// in order to receive the WebRTC stream.
///
/// whepserversink is based on webrtcsink and handles encoding internally.
/// It uses request pads (audio_0, video_0) similar to whipclientsink.
///
/// The server binds to localhost on an auto-assigned free port.
/// Axum proxies requests from /api/whep/{endpoint_id}/... to the internal port.
fn build_whepserversink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHEP Output using whepserversink (server mode)");

    // Number of audio/video tracks to expose. Each track gets its own
    // audio_in / video_in pad, queue and request pad on whepserversink
    // (audio_0, audio_1, ..., video_0, video_1, ...). 0 disables the media
    // type on this endpoint.
    let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
    let has_audio = num_audio_tracks > 0;
    let has_video = num_video_tracks > 0;

    info!(
        "WHEP Output: num_audio_tracks={}, num_video_tracks={}",
        num_audio_tracks, num_video_tracks
    );

    // Timestamp offset in milliseconds. A negative value shifts playout earlier,
    // reducing end-to-end latency for this output while maintaining A/V sync.
    // Applied as ts-offset on clocksync and appsink inside whepserversink.
    let ts_offset_ms = properties
        .get("ts_offset_ms")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);

    // Get endpoint_id (user-configurable, defaults to UUID)
    let endpoint_id = properties
        .get("endpoint_id")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                let trimmed = s.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            } else {
                None
            }
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // Find a free port by binding to port 0
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| {
        BlockBuildError::InvalidConfiguration(format!("Failed to find free port: {}", e))
    })?;
    let internal_port = listener
        .local_addr()
        .map_err(|e| {
            BlockBuildError::InvalidConfiguration(format!("Failed to get local address: {}", e))
        })?
        .port();
    // Drop the listener to free the port for whepserversink
    drop(listener);

    info!(
        "WHEP Output: Found free port {} for endpoint_id '{}'",
        internal_port, endpoint_id
    );

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Create whepserversink element
    // This is based on webrtcsink and handles encoding internally
    let whepserversink_id = format!("{}:whepserversink", instance_id);
    let whepserversink = gst::ElementFactory::make("whepserversink")
        .name(&whepserversink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whepserversink: {}", e)))?;

    // Set ICE server properties (explicitly clear defaults when not configured,
    // since webrtcsink defaults to stun://stun.l.google.com:19302)
    // Note: webrtcsink-based elements use "turn-servers" (plural, array) not "turn-server"
    match stun_server {
        Some(ref stun) => whepserversink.set_property("stun-server", stun),
        None => whepserversink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        let turn_servers = gst::Array::new([turn]);
        whepserversink.set_property("turn-servers", turn_servers);
    }

    // Disable FEC and RTX (retransmission) to avoid bandwidth overhead
    // These are enabled by default in webrtcsink and can significantly increase bandwidth:
    // - FEC adds redundancy packets (can add ~50% overhead)
    // - RTX sends duplicate packets for retransmission
    // For pre-encoded video at high bitrates, these can cause near-double bandwidth usage
    whepserversink.set_property("do-fec", false);
    whepserversink.set_property("do-retransmission", false);

    // Access the signaller child and set its properties
    // Bind to localhost only - axum will proxy external requests
    let signaller = whepserversink.property::<gst::glib::Object>("signaller");
    let host_addr = format!("http://127.0.0.1:{}", internal_port);
    signaller.set_property("host-addr", &host_addr);

    // Shift playout timing on clocksync and appsink inside whepserversink.
    // A negative ts_offset_ms makes this output release buffers earlier:
    //  - clocksync: negative ts-offset shifts its clock wait earlier
    //  - appsink: negative ts-offset shifts BaseSink's clock wait earlier
    //    (BaseSink formula: wait = running_time + latency + ts_offset)
    // ts-offset does NOT affect the latency query, so pipeline latency is
    // unchanged. Both elements keep sync=true — only the playout point shifts.
    //
    // Properties are applied via a deferred pad probe because webrtcsink's
    // StreamProducer configures these elements AFTER deep-element-added fires,
    // using direct C API calls that bypass g_object_notify.
    if ts_offset_ms != 0 {
        let ts_offset_ns = ts_offset_ms.saturating_mul(1_000_000);
        let instance_id_for_ts = instance_id.to_string();

        fn defer_ts_offset(element: &gst::Element, ts_offset_ns: i64, instance_id: &str) {
            if let Some(pad) = element.static_pad("sink") {
                let iid = instance_id.to_string();
                let name = element.name().to_string();
                pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, _info| {
                    if let Some(el) = pad.parent_element() {
                        el.set_property("ts-offset", ts_offset_ns);
                        info!(
                            "WHEP Output {}: Set ts-offset={}ns on {} (deferred)",
                            iid, ts_offset_ns, name
                        );
                    }
                    gst::PadProbeReturn::Remove
                });
            }
        }

        if let Ok(bin) = whepserversink.clone().downcast::<gst::Bin>() {
            for element in bin.iterate_recurse().into_iter().flatten() {
                let factory_name = element
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if (factory_name == "clocksync" || factory_name == "appsink")
                    && element.has_property("ts-offset")
                {
                    defer_ts_offset(&element, ts_offset_ns, &instance_id_for_ts);
                }
            }
            bin.connect("deep-element-added", false, move |args| {
                let added: gst::Element = args[2].get().unwrap();
                let factory_name = added
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if (factory_name == "clocksync" || factory_name == "appsink")
                    && added.has_property("ts-offset")
                {
                    defer_ts_offset(&added, ts_offset_ns, &instance_id_for_ts);
                }
                None
            });
        }
        info!(
            "WHEP Output: ts-offset={}ms applied to clocksync and appsink elements",
            ts_offset_ms
        );
    }

    // Configure audio/video caps based on which media types are enabled.
    // Video caps will be set dynamically when we detect the input codec.
    if !has_audio {
        whepserversink.set_property("audio-caps", gst::Caps::new_empty());
    }
    if !has_video {
        whepserversink.set_property("video-caps", gst::Caps::new_empty());
    }

    // Install thread priority on session pipelines via pad probes.
    //
    // consumer-pipeline-created fires BEFORE webrtcsink sets its bus sync handler,
    // giving us the session pipeline to connect deep-element-added. Each element
    // gets a one-shot EVENT_DOWNSTREAM probe that sets thread priority on the
    // streaming thread.
    //
    // We cannot use a bus sync handler because webrtcsink's own handler returns
    // BusSyncReply::Drop and routes all messages through an internal channel.
    // Replacing it breaks session lifecycle (sessions never terminate).
    let session_thread_config = ctx.session_thread_config();
    whepserversink.connect("consumer-pipeline-created", false, move |values| {
        if !session_thread_config.is_active() {
            return None;
        }
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let pipeline = values[2].get::<gst::Pipeline>().unwrap();
        session_thread_config.install_on_session_pipeline(&pipeline, &consumer_id);
        None
    });

    // WORKAROUND #1: Relax transceiver codec-preferences BEFORE SDP offer is processed.
    //
    // Problem: webrtcbin does strict caps matching on transceiver codec-preferences.
    // Browser offers profile=baseline, but transceivers have profile-level-id=42c015.
    // webrtcbin doesn't know these are compatible, so transceivers go inactive.
    //
    // Solution: Connect to consumer-added signal (fires BEFORE SDP offer is processed).
    // Modify all video transceivers' codec-preferences to remove profile constraints.
    //
    // Also register the webrtcbin for stats collection (since it's in a separate session pipeline).
    let dynamic_webrtcbin_store = ctx.dynamic_webrtcbin_store();
    let block_id_for_callback = instance_id.to_string();
    let ice_transport_policy = ctx.ice_transport_policy().to_string();
    whepserversink.connect("consumer-added", false, move |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let webrtcbin = values[2].get::<gst::Element>().unwrap();

        debug!(
            "WHEP Output: consumer-added for {}, modifying transceiver codec-preferences",
            consumer_id
        );

        // Set ICE transport policy on webrtcbin (from config)
        if webrtcbin.has_property("ice-transport-policy") {
            webrtcbin.set_property_from_str("ice-transport-policy", &ice_transport_policy);
            info!(
                "WHEP Output: Set ice-transport-policy={} on webrtcbin for consumer {}",
                ice_transport_policy, consumer_id
            );
        }

        // Register webrtcbin for stats collection
        if let Ok(mut store) = dynamic_webrtcbin_store.lock() {
            store
                .entry(block_id_for_callback.clone())
                .or_default()
                .push((consumer_id.clone(), webrtcbin.clone()));
            debug!(
                "WHEP Output: Registered webrtcbin for block {} consumer {}",
                block_id_for_callback, consumer_id
            );
        }

        // Access transceivers through webrtcbin's sink pads
        // Each sink pad has a "transceiver" property pointing to the associated transceiver
        let mut transceiver_count = 0;
        for pad in webrtcbin.sink_pads() {
            let pad_name = pad.name();

            // Check if this pad has a transceiver property
            if !pad.has_property("transceiver") {
                continue;
            }

            // Get transceiver property from the pad as a generic Object
            let transceiver_value = pad.property_value("transceiver");
            let transceiver = match transceiver_value.get::<gst::glib::Object>() {
                Ok(t) => t,
                Err(_) => continue,
            };

            transceiver_count += 1;

            // Check if transceiver has codec-preferences property
            if !transceiver.has_property("codec-preferences") {
                debug!(
                    "WHEP Output: Transceiver for pad {} has no codec-preferences property",
                    pad_name
                );
                continue;
            }

            // Get current codec-preferences
            let codec_prefs_value = transceiver.property_value("codec-preferences");
            let codec_prefs = match codec_prefs_value.get::<gst::Caps>() {
                Ok(c) => c,
                Err(_) => continue,
            };

            if codec_prefs.is_empty() {
                debug!(
                    "WHEP Output: Transceiver for pad {} has empty codec-preferences",
                    pad_name
                );
                continue;
            }

            debug!(
                "WHEP Output: Transceiver for pad {} codec-preferences: {:?}",
                pad_name, codec_prefs
            );

            // Filter codec-preferences: remove outdated codecs and relax profile constraints.
            // IMPORTANT: Only keep ONE entry per codec type to avoid duplicate streams.
            // Browser may offer multiple H.264 profiles (baseline, main, high) - if we
            // accept all of them after relaxing profile matching, webrtcsink sends the
            // same data on multiple payloads, doubling bandwidth.
            let mut new_caps = gst::Caps::new_empty();
            let mut seen_codecs = std::collections::HashSet::new();
            for i in 0..codec_prefs.size() {
                if let Some(structure) = codec_prefs.structure(i) {
                    let codec_name = structure.name().as_str();
                    // Skip VP8 - outdated codec, not worth offering
                    if codec_name == "video/x-vp8" {
                        continue;
                    }
                    // Only add first occurrence of each codec type
                    if seen_codecs.insert(codec_name.to_string()) {
                        let mut new_structure = structure.to_owned();
                        // H.264 / AV1
                        new_structure.remove_field("profile-level-id");
                        new_structure.remove_field("profile");
                        new_structure.remove_field("level-idx");
                        new_structure.remove_field("tier");
                        // H.265
                        new_structure.remove_field("profile-id");
                        new_structure.remove_field("tier-flag");
                        new_structure.remove_field("level-id");
                        new_structure.remove_field("tx-mode");
                        new_caps.get_mut().unwrap().append_structure(new_structure);
                    }
                }
            }
            if new_caps != codec_prefs {
                debug!(
                    "WHEP Output: Modified transceiver for pad {} codec-preferences: {:?} -> {:?}",
                    pad_name, codec_prefs, new_caps
                );
                transceiver.set_property("codec-preferences", &new_caps);
            }
        }

        debug!(
            "WHEP Output: Processed {} transceivers for consumer {}",
            transceiver_count, consumer_id
        );

        None
    });

    // WORKAROUND #2: Fix H.264 profile-level-id mismatch blocking video flow.
    //
    // Problem: webrtcsink creates capsfilters downstream of the payloader with
    // profile-level-id from the browser's SDP (e.g. 42001f = Baseline). When the
    // actual H.264 stream has a different profile (e.g. high-4:4:4 = f40028),
    // rtph264pay queries downstream, sees only baseline is acceptable, and
    // negotiation fails with NOT_NEGOTIATED.
    //
    // There are two cases:
    //   1. Discovery pipeline: the output_filter capsfilter already exists at
    //      payloader-setup time → we strip profile-level-id immediately.
    //   2. Consumer session: the pay_filter capsfilter is created later in
    //      connect_input_stream → we use element-added + notify::caps to strip
    //      profile-level-id synchronously when the capsfilter's caps are set,
    //      before negotiation occurs.
    whepserversink.connect("payloader-setup", false, |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();
        let payloader = values[3].get::<gst::Element>().unwrap();

        info!(
            "WHEP Output: payloader-setup fired: consumer_id={}, payloader={} (factory={})",
            consumer_id,
            payloader.name(),
            payloader.factory().map(|f| f.name().to_string()).unwrap_or_else(|| "unknown".to_string())
        );

        // Helper: walk downstream capsfilters from a pad and strip profile fields
        fn strip_downstream_capsfilters(start_pad: &gst::Pad, consumer_id: &str) -> u32 {
            let mut count = 0u32;
            let mut next_pad = start_pad.peer();
            while let Some(peer) = next_pad {
                if let Some(element) = peer.parent_element() {
                    let is_capsfilter = element
                        .factory()
                        .map(|f| f.name().as_str() == "capsfilter")
                        .unwrap_or(false);
                    if is_capsfilter {
                        let caps: gst::Caps = element.property("caps");
                        if let Some(s) = caps.structure(0) {
                            if s.has_field("profile-level-id")
                                || s.has_field("profile")
                                || s.has_field("profile-id")
                                || s.has_field("tier-flag")
                                || s.has_field("level-id")
                                || s.has_field("tx-mode")
                            {
                                let mut new_caps = gst::Caps::new_empty();
                                for i in 0..caps.size() {
                                    if let Some(structure) = caps.structure(i) {
                                        let mut ns = structure.to_owned();
                                        ns.remove_field("profile-level-id");
                                        ns.remove_field("profile");
                                        ns.remove_field("profile-id");
                                        ns.remove_field("tier-flag");
                                        ns.remove_field("level-id");
                                        ns.remove_field("tx-mode");
                                        new_caps.merge_structure(ns);
                                    }
                                }
                                info!(
                                    "WHEP Output: Stripped profile from capsfilter {} for {}: {:?}",
                                    element.name(),
                                    consumer_id,
                                    new_caps
                                );
                                element.set_property("caps", &new_caps);
                                count += 1;
                            }
                        }
                    }
                    next_pad = element.static_pad("src").and_then(|p| p.peer());
                } else {
                    break;
                }
            }
            count
        }

        if let Some(src_pad) = payloader.static_pad("src") {
            // Case 1: Strip any capsfilters that already exist downstream (discovery).
            strip_downstream_capsfilters(&src_pad, &consumer_id);
        }

        // Case 2: For consumer sessions, connect_input_stream creates a second
        // capsfilter (pay_filter) AFTER payloader-setup and sets it with SDP caps
        // including profile-level-id. We intercept this by watching for new
        // capsfilters added to the session pipeline and stripping profile-level-id
        // from their caps via notify::caps (fires synchronously during set_property,
        // BEFORE any caps negotiation occurs).
        if consumer_id != "discovery" {
            if let Some(parent) = payloader.parent() {
                if let Ok(bin) = parent.downcast::<gst::Bin>() {
                    let consumer_id_bin = consumer_id.clone();
                    bin.connect_element_added(move |_bin, element| {
                        let is_capsfilter = element
                            .factory()
                            .map(|f| f.name().as_str() == "capsfilter")
                            .unwrap_or(false);
                        if !is_capsfilter {
                            return;
                        }
                        let cid = consumer_id_bin.clone();
                        element.connect_notify(Some("caps"), move |el, _| {
                            let caps: gst::Caps = el.property("caps");
                            if let Some(s) = caps.structure(0) {
                                if s.has_field("profile-level-id")
                                    || s.has_field("profile")
                                    || s.has_field("profile-id")
                                    || s.has_field("tier-flag")
                                    || s.has_field("level-id")
                                    || s.has_field("tx-mode")
                                {
                                    let mut new_caps = gst::Caps::new_empty();
                                    for i in 0..caps.size() {
                                        if let Some(structure) = caps.structure(i) {
                                            let mut ns = structure.to_owned();
                                            ns.remove_field("profile-level-id");
                                            ns.remove_field("profile");
                                            ns.remove_field("profile-id");
                                            ns.remove_field("tier-flag");
                                            ns.remove_field("level-id");
                                            ns.remove_field("tx-mode");
                                            new_caps.merge_structure(ns);
                                        }
                                    }
                                    info!(
                                        "WHEP Output: notify::caps stripped profile from {} for {}: {:?}",
                                        el.name(), cid, new_caps
                                    );
                                    el.set_property("caps", &new_caps);
                                }
                            }
                        });
                    });
                }
            }
        }

        Some(false.to_value())
    });

    // Handle consumer-removed to clean up webrtcbin from stats storage
    let dynamic_webrtcbin_store_remove = ctx.dynamic_webrtcbin_store();
    let block_id_for_remove = instance_id.to_string();
    whepserversink.connect("consumer-removed", false, move |values| {
        let consumer_id = values[1].get::<String>().unwrap_or_default();

        // Remove webrtcbin from stats storage
        if let Ok(mut store) = dynamic_webrtcbin_store_remove.lock() {
            if let Some(consumers) = store.get_mut(&block_id_for_remove) {
                consumers.retain(|(cid, _)| cid != &consumer_id);
                debug!(
                    "WHEP Output: Unregistered webrtcbin for block {} consumer {}",
                    block_id_for_remove, consumer_id
                );
            }
        }

        None
    });

    // NOTE: Pre-encoded H.264 has a known limitation with webrtcsink:
    // webrtcsink runs codec discovery for each client, creating a fresh h264parse
    // that needs SPS/PPS from a keyframe. If discovery starts mid-GOP, it times out.
    // Workarounds:
    // 1. Use shorter GOP (30 frames / 1 second recommended for WebRTC)
    // 2. Feed raw video and let webrtcsink encode internally
    // 3. Use webrtcbin directly for full control

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // Create audio processing elements if mode includes audio.
    // For num_audio_tracks > 1 we expose multiple audio_in pads, each with its own
    // queue wired to a distinct request pad (audio_0, audio_1, ...) on
    // whepserversink. The audio-caps property on whepserversink is global, so only
    // the first queue's caps probe drives it — all audio inputs must share the
    // same format (all Opus or all raw). Mixed formats are not supported.
    if has_audio {
        // Shared latch: only the first input that sees a caps event sets audio-caps.
        let audio_caps_set = Arc::new(AtomicBool::new(false));

        for slot in 0..num_audio_tracks {
            // Slot 0 keeps the unsuffixed element id for backwards compatibility
            // with existing flows (matches get_external_pads above).
            let audio_queue_id = if slot == 0 {
                format!("{}:audio_queue", instance_id)
            } else {
                format!("{}:audio_queue_{}", instance_id, slot)
            };
            let audio_queue = gst::ElementFactory::make("queue")
                .name(&audio_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("audio_queue (slot {}): {}", slot, e))
                })?;

            let whepserversink_weak = whepserversink.downgrade();
            let audio_caps_set_clone = audio_caps_set.clone();
            let instance_id_owned = instance_id.to_string();

            let audio_queue_sink = audio_queue.static_pad("sink").expect("queue has sink pad");
            audio_queue_sink.add_probe(
                gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_pad, info| {
                    if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                        if event.type_() == gst::EventType::Caps {
                            // Atomically claim the latch — only one probe wins
                            // and proceeds to set audio-caps; the rest pass.
                            if audio_caps_set_clone
                                .compare_exchange(
                                    false,
                                    true,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                )
                                .is_err()
                            {
                                return gst::PadProbeReturn::Pass;
                            }

                            if let gst::EventView::Caps(caps_event) = event.view() {
                                let caps = caps_event.caps();
                                if let Some(structure) = caps.structure(0) {
                                    let caps_name = structure.name().as_str();

                                    let audio_caps: Option<gst::Caps> = match caps_name {
                                        "audio/x-opus" => {
                                            debug!(
                                                "WHEP Output {} (slot {}): Detected Opus input, setting audio-caps",
                                                instance_id_owned, slot
                                            );
                                            Some(gst::Caps::builder("audio/x-opus").build())
                                        }
                                        "audio/x-raw" => {
                                            debug!(
                                                "WHEP Output {} (slot {}): Detected raw audio, using default audio-caps",
                                                instance_id_owned, slot
                                            );
                                            None
                                        }
                                        _ => {
                                            warn!(
                                                "WHEP Output {} (slot {}): Unknown audio format '{}', using default",
                                                instance_id_owned, slot, caps_name
                                            );
                                            None
                                        }
                                    };

                                    if let Some(caps) = audio_caps {
                                        if let Some(whepserversink) = whepserversink_weak.upgrade()
                                        {
                                            whepserversink.set_property("audio-caps", &caps);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    gst::PadProbeReturn::Pass
                },
            );

            // Audio link: queue -> whepserversink (audio_<slot> request pad)
            internal_links.push((
                ElementPadRef::pad(&audio_queue_id, "src"),
                ElementPadRef::pad(&whepserversink_id, format!("audio_{}", slot)),
            ));

            elements.push((audio_queue_id, audio_queue));
        }

        info!(
            "WHEP Output {}: configured {} audio track(s)",
            instance_id, num_audio_tracks
        );
    }

    // Create video processing elements if mode includes video.
    // For num_video_tracks > 1 we expose multiple video_in pads, each with its own
    // queue wired to a distinct request pad (video_0, video_1, ...) on
    // whepserversink. The video-caps property on whepserversink is global, so only
    // the first queue's caps probe drives it — all video inputs must share the
    // same codec.
    if has_video {
        // Shared latch: only the first input that sees a caps event sets video-caps.
        let video_caps_set = Arc::new(AtomicBool::new(false));

        for slot in 0..num_video_tracks {
            // Slot 0 keeps the unsuffixed element id for backwards compatibility
            // with existing flows (matches get_external_pads above).
            let video_queue_id = if slot == 0 {
                format!("{}:video_queue", instance_id)
            } else {
                format!("{}:video_queue_{}", instance_id, slot)
            };
            let video_queue = gst::ElementFactory::make("queue")
                .name(&video_queue_id)
                .build()
                .map_err(|e| {
                    BlockBuildError::ElementCreation(format!("video_queue (slot {}): {}", slot, e))
                })?;

            // Dynamic video codec detection: detect input codec and set video-caps
            // on whepserversink before discovery runs. Works with any codec
            // (H264, H265, VP9, AV1, raw). Only the first slot to see a caps
            // event sets the global video-caps property — the rest pass through.
            let whepserversink_weak = whepserversink.downgrade();
            let video_caps_set_clone = video_caps_set.clone();
            let instance_id_owned = instance_id.to_string();

            let video_queue_sink = video_queue.static_pad("sink").expect("queue has sink pad");
            video_queue_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if event.type_() == gst::EventType::Caps {
                        // Atomically claim the latch — only one probe wins
                        // and proceeds to set video-caps; the rest pass.
                        if video_caps_set_clone
                            .compare_exchange(
                                false,
                                true,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
                            .is_err()
                        {
                            return gst::PadProbeReturn::Pass;
                        }

                        if let gst::EventView::Caps(caps_event) = event.view() {
                            let caps = caps_event.caps();
                            if let Some(structure) = caps.structure(0) {
                                let codec_name = structure.name().as_str();

                                // Map input caps to webrtc-compatible caps.
                                // For pre-encoded video, restrict to that codec only.
                                // For raw video, offer modern codecs (exclude VP8).
                                let video_caps: Option<gst::Caps> = match codec_name {
                                    "video/x-h264" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected H.264 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-h264").build())
                                    }
                                    "video/x-h265" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected H.265 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-h265").build())
                                    }
                                    "video/x-vp9" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected VP9 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-vp9").build())
                                    }
                                    "video/x-av1" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected AV1 input, setting video-caps",
                                            instance_id_owned, slot
                                        );
                                        Some(gst::Caps::builder("video/x-av1").build())
                                    }
                                    "video/x-raw" => {
                                        info!(
                                            "WHEP Output {} (slot {}): Detected raw video input, setting video-caps to H.264/H.265/VP9/AV1",
                                            instance_id_owned, slot
                                        );
                                        let mut caps = gst::Caps::new_empty();
                                        {
                                            let caps_mut = caps.get_mut().unwrap();
                                            caps_mut.append(gst::Caps::builder("video/x-h264").build());
                                            caps_mut.append(gst::Caps::builder("video/x-h265").build());
                                            caps_mut.append(gst::Caps::builder("video/x-vp9").build());
                                            caps_mut.append(gst::Caps::builder("video/x-av1").build());
                                        }
                                        Some(caps)
                                    }
                                    _ => {
                                        warn!(
                                            "WHEP Output {} (slot {}): Unknown video codec '{}', using default",
                                            instance_id_owned, slot, codec_name
                                        );
                                        None
                                    }
                                };

                                if let Some(caps) = video_caps {
                                    if let Some(whepserversink) = whepserversink_weak.upgrade() {
                                        whepserversink.set_property("video-caps", &caps);
                                    }
                                }
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Pass
            });

            // Normalize H.264/H.265 caps before they reach webrtcsink.
            // h264parse progressively adds fields (coded-picture-structure, chroma-format,
            // bit-depth-luma, bit-depth-chroma) as it parses the stream. webrtcsink's
            // input_caps_change_allowed() doesn't account for these and rejects them as
            // "renegotiation". This probe removes those fields from CAPS events to
            // prevent false renegotiation errors. Applied per-slot.
            let queue_src_pad = video_queue.static_pad("src").expect("queue has src pad");
            queue_src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref event)) = info.data {
                    if event.type_() == gst::EventType::Caps {
                        if let gst::EventView::Caps(caps_event) = event.view() {
                            let caps = caps_event.caps();
                            if let Some(structure) = caps.structure(0) {
                                if structure.name() == "video/x-h264"
                                    || structure.name() == "video/x-h265"
                                {
                                    let mut new_caps = caps.copy();
                                    if let Some(s) = new_caps.make_mut().structure_mut(0) {
                                        s.remove_fields([
                                            "coded-picture-structure",
                                            "chroma-format",
                                            "bit-depth-luma",
                                            "bit-depth-chroma",
                                        ]);
                                    }
                                    let new_event = gst::event::Caps::new(&new_caps);
                                    info.data = Some(gst::PadProbeData::Event(new_event));
                                }
                            }
                        }
                    }
                }
                gst::PadProbeReturn::Ok
            });

            // Video link: queue -> whepserversink (video_<slot> request pad)
            internal_links.push((
                ElementPadRef::pad(&video_queue_id, "src"),
                ElementPadRef::pad(&whepserversink_id, format!("video_{}", slot)),
            ));

            elements.push((video_queue_id, video_queue));
        }

        info!(
            "WHEP Output {}: configured {} video track(s)",
            instance_id, num_video_tracks
        );
    }

    // Add whepserversink last (after audio/video processing elements)
    elements.push((whepserversink_id.clone(), whepserversink));

    info!(
        "WHEP Output configured: endpoint_id='{}', internal_host={}, stun={:?}, turn={:?}, audio_tracks={}, video_tracks={}",
        endpoint_id, host_addr, stun_server, turn_server, num_audio_tracks, num_video_tracks
    );

    // Register WHEP endpoint with the build context
    ctx.register_whep_endpoint(
        instance_id,
        &endpoint_id,
        internal_port,
        num_audio_tracks,
        num_video_tracks,
    );

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Setup a stream from whepclientsrc/whepsrc with caps detection.
/// Uses an identity element to immediately claim the pad (preventing auto-tee),
/// then a pad probe to detect actual caps before deciding how to handle the stream:
/// - Audio: decode and route to liveadder
/// - Video: discard via fakesink (no decode to avoid expensive video decoding)
fn setup_stream_with_caps_detection(
    src: &gst::Element,
    src_pad: &gst::Pad,
    liveadder: &gst::Element,
    instance_id: &str,
    stream_num: usize,
    probe_registry: &Option<Arc<WhepProbeRegistry>>,
) -> Result<(), String> {
    // Get the pipeline
    let pipeline = get_pipeline_from_element(src)?;

    // Create identity element IMMEDIATELY to claim the pad and prevent auto-tee
    let identity_name = format!("{}:stream_identity_{}", instance_id, stream_num);
    let identity = gst::ElementFactory::make("identity")
        .name(&identity_name)
        .build()
        .map_err(|e| format!("Failed to create identity: {}", e))?;

    // Add identity to pipeline
    pipeline
        .add(&identity)
        .map_err(|e| format!("Failed to add identity to pipeline: {}", e))?;

    // Sync identity state with pipeline
    identity
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync identity state: {}", e))?;

    // Link src_pad to identity IMMEDIATELY - this prevents auto-tee from claiming the pad
    let identity_sink = identity
        .static_pad("sink")
        .ok_or("Identity has no sink pad")?;
    src_pad
        .link(&identity_sink)
        .map_err(|e| format!("Failed to link to identity: {:?}", e))?;

    info!(
        "WHEP: Stream {} linked to identity (preventing auto-tee)",
        stream_num
    );

    // Install diagnostic probe on identity if enabled
    if let Some(ref registry) = probe_registry {
        whep_probe::probe_element_src(registry, &identity);
    }

    // Get identity's src pad for the probe
    let identity_src = identity
        .static_pad("src")
        .ok_or("Identity has no src pad")?;

    // Create weak references for the probe callback
    let pipeline_weak = pipeline.downgrade();
    let liveadder_weak = liveadder.downgrade();
    let instance_id_owned = instance_id.to_string();
    let probe_registry_clone = probe_registry.clone();

    // Flag to ensure we only handle this once
    let handled = Arc::new(AtomicBool::new(false));
    let handled_clone = Arc::clone(&handled);

    // Add a probe on identity's src pad to detect caps events
    identity_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        // Only handle once
        if handled_clone.load(Ordering::SeqCst) {
            return gst::PadProbeReturn::Pass;
        }

        if let Some(gst::PadProbeData::Event(ref event)) = info.data {
            if event.type_() == gst::EventType::Caps {
                // Get the caps from the event by viewing it as a Caps event
                if let gst::EventView::Caps(c) = event.view() {
                    let caps = c.caps();
                    if let Some(structure) = caps.structure(0) {
                        let caps_name = structure.name();
                        info!("WHEP: Stream {} detected caps: {}", stream_num, caps_name);

                        // Determine media type - for RTP, look at the "media" field
                        let is_audio = if caps_name == "application/x-rtp" {
                            // RTP caps - check the "media" field
                            let media_field = structure.get::<&str>("media").ok().unwrap_or("");
                            let encoding = structure
                                .get::<&str>("encoding-name")
                                .ok()
                                .unwrap_or("unknown");
                            info!(
                                "WHEP: Stream {} RTP media={}, encoding={}",
                                stream_num, media_field, encoding
                            );
                            media_field == "audio"
                        } else {
                            caps_name.starts_with("audio/")
                        };

                        let is_video = if caps_name == "application/x-rtp" {
                            let media_field = structure.get::<&str>("media").ok().unwrap_or("");
                            media_field == "video"
                        } else {
                            caps_name.starts_with("video/")
                        };

                        // Mark as handled
                        handled_clone.store(true, Ordering::SeqCst);

                        // Get pipeline and liveadder
                        let pipeline = match pipeline_weak.upgrade() {
                            Some(p) => p,
                            None => {
                                error!("WHEP: Pipeline no longer exists");
                                return gst::PadProbeReturn::Remove;
                            }
                        };

                        if is_audio {
                            // Audio stream - use decodebin to decode, then route to liveadder
                            info!(
                                "WHEP: Stream {} is audio, setting up decode chain",
                                stream_num
                            );
                            if let Some(liveadder) = liveadder_weak.upgrade() {
                                if let Err(e) = setup_audio_decode_chain(
                                    pad,
                                    &pipeline,
                                    &liveadder,
                                    &instance_id_owned,
                                    stream_num,
                                    &probe_registry_clone,
                                ) {
                                    error!("WHEP: Failed to setup audio decode chain: {}", e);
                                }
                            }
                        } else if is_video {
                            // Video stream - use fakesink to discard (no decode)
                            info!(
                                "WHEP: Stream {} is video, discarding via fakesink (no decode)",
                                stream_num
                            );
                            if let Err(e) =
                                setup_video_discard(pad, &pipeline, &instance_id_owned, stream_num)
                            {
                                error!("WHEP: Failed to setup video discard: {}", e);
                            }
                        } else {
                            warn!(
                                "WHEP: Stream {} has unknown media type: {}",
                                stream_num, caps_name
                            );
                        }

                        return gst::PadProbeReturn::Remove;
                    }
                }
            }
        }

        gst::PadProbeReturn::Pass
    });

    info!(
        "WHEP: Caps probe installed on stream {} (via identity)",
        stream_num
    );
    Ok(())
}

/// Get the pipeline from an element, handling nested bins
fn get_pipeline_from_element(element: &gst::Element) -> Result<gst::Pipeline, String> {
    let parent = element
        .parent()
        .ok_or("Could not get parent from element")?;

    // Try direct pipeline
    if let Ok(pipeline) = parent.clone().downcast::<gst::Pipeline>() {
        return Ok(pipeline);
    }

    // Try parent of parent (for nested bins)
    if let Some(grandparent) = parent.parent() {
        if let Ok(pipeline) = grandparent.downcast::<gst::Pipeline>() {
            return Ok(pipeline);
        }
    }

    // Try to get from bin
    if let Ok(bin) = parent.downcast::<gst::Bin>() {
        if let Some(p) = bin.parent() {
            if let Ok(pipeline) = p.downcast::<gst::Pipeline>() {
                return Ok(pipeline);
            }
        }
    }

    Err("Could not find pipeline from element".to_string())
}

/// Setup audio decode chain: decodebin -> audioconvert -> audioresample -> liveadder
fn setup_audio_decode_chain(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    liveadder: &gst::Element,
    instance_id: &str,
    stream_num: usize,
    probe_registry: &Option<Arc<WhepProbeRegistry>>,
) -> Result<(), String> {
    // Create unique element names
    let decodebin_name = format!("{}:decodebin_{}", instance_id, stream_num);
    let audioconvert_name = format!("{}:stream_audioconvert_{}", instance_id, stream_num);
    let audioresample_name = format!("{}:stream_audioresample_{}", instance_id, stream_num);

    // Create decodebin for audio decoding
    let decodebin = gst::ElementFactory::make("decodebin")
        .name(&decodebin_name)
        .build()
        .map_err(|e| format!("Failed to create decodebin: {}", e))?;

    // Create audioconvert and audioresample
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_name)
        .build()
        .map_err(|e| format!("Failed to create audioconvert: {}", e))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_name)
        .build()
        .map_err(|e| format!("Failed to create audioresample: {}", e))?;

    // Add elements to pipeline IMMEDIATELY so they don't get dropped when this function returns
    // The callback will fire later, and we need these elements to still exist
    pipeline
        .add(&audioconvert)
        .map_err(|e| format!("Failed to add audioconvert to pipeline: {}", e))?;
    pipeline
        .add(&audioresample)
        .map_err(|e| format!("Failed to add audioresample to pipeline: {}", e))?;

    info!(
        "WHEP: Added stream {} audioconvert and audioresample to pipeline",
        stream_num
    );

    // Clone references for decodebin's pad-added callback
    let audioconvert_weak = audioconvert.downgrade();
    let audioresample_weak = audioresample.downgrade();
    let liveadder_weak = liveadder.downgrade();
    let stream_num_clone = stream_num;
    let probe_registry_clone = probe_registry.clone();

    // Set up decodebin's pad-added callback to link to audioconvert
    decodebin.connect_pad_added(move |_decodebin, pad| {
        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        if let Some(caps) = caps {
            if let Some(structure) = caps.structure(0) {
                if structure.name().starts_with("audio/") {
                    info!(
                        "WHEP: Stream {} decodebin output pad is audio, linking to processing chain",
                        stream_num_clone
                    );

                    // Upgrade weak refs - elements are already in the pipeline so they should exist
                    let (audioconvert, audioresample, liveadder) = match (
                        audioconvert_weak.upgrade(),
                        audioresample_weak.upgrade(),
                        liveadder_weak.upgrade(),
                    ) {
                        (Some(a), Some(b), Some(c)) => (a, b, c),
                        _ => {
                            error!(
                                "WHEP: Stream {} - Failed to upgrade element refs in callback",
                                stream_num_clone
                            );
                            return;
                        }
                    };

                    // Sync element states BEFORE linking (need at least READY state)
                    if let Err(e) = audioconvert.sync_state_with_parent() {
                        error!("Failed to sync audioconvert state: {}", e);
                        return;
                    }
                    if let Err(e) = audioresample.sync_state_with_parent() {
                        error!("Failed to sync audioresample state: {}", e);
                        return;
                    }
                    info!(
                        "WHEP: Stream {} synced audioconvert and audioresample states",
                        stream_num_clone
                    );

                    // Link decodebin -> audioconvert
                    let audioconvert_sink = audioconvert.static_pad("sink").unwrap();
                    if let Err(e) = pad.link(&audioconvert_sink) {
                        error!("Failed to link decodebin to audioconvert: {:?}", e);
                        return;
                    }
                    info!("WHEP: Stream {} linked decodebin to audioconvert", stream_num_clone);

                    // Link audioconvert -> audioresample
                    if let Err(e) = audioconvert.link(&audioresample) {
                        error!("Failed to link audioconvert to audioresample: {:?}", e);
                        return;
                    }
                    info!(
                        "WHEP: Stream {} linked audioconvert to audioresample",
                        stream_num_clone
                    );

                    // Request a sink pad from liveadder and link
                    if let Some(liveadder_sink) = liveadder.request_pad_simple("sink_%u") {
                        info!(
                            "WHEP: Stream {} got liveadder sink pad: {}",
                            stream_num_clone,
                            liveadder_sink.name()
                        );
                        // Enable QoS messages on this pad so we can see if buffers are being dropped
                        liveadder_sink.set_property("qos-messages", true);
                        let audioresample_src = audioresample.static_pad("src").unwrap();
                        if let Err(e) = audioresample_src.link(&liveadder_sink) {
                            error!("Failed to link audioresample to liveadder: {:?}", e);
                            return;
                        }
                        info!(
                            "WHEP: Stream {} successfully linked audio stream to liveadder",
                            stream_num_clone
                        );

                        // Install diagnostic probes on the decode chain
                        if let Some(ref registry) = probe_registry_clone {
                            whep_probe::probe_element_src(registry, &audioconvert);
                            whep_probe::probe_element_src(registry, &audioresample);
                            whep_probe::probe_pad(registry, &liveadder, &liveadder_sink);
                        }
                    } else {
                        error!("Failed to request sink pad from liveadder");
                    }
                }
            }
        }
    });

    // Add decodebin to pipeline
    pipeline
        .add(&decodebin)
        .map_err(|e| format!("Failed to add decodebin to pipeline: {}", e))?;

    // Link src_pad to decodebin sink
    let decodebin_sink = decodebin
        .static_pad("sink")
        .ok_or("Decodebin has no sink pad")?;
    src_pad
        .link(&decodebin_sink)
        .map_err(|e| format!("Failed to link to decodebin: {:?}", e))?;

    // Sync decodebin state with pipeline
    decodebin
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync decodebin state: {}", e))?;

    info!(
        "WHEP: Audio decode chain setup complete for stream {}",
        stream_num
    );
    Ok(())
}

/// Setup video discard: fakesink (no decoding, just discard the video stream)
fn setup_video_discard(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    instance_id: &str,
    stream_num: usize,
) -> Result<(), String> {
    let fakesink_name = format!("{}:video_fakesink_{}", instance_id, stream_num);

    // Create fakesink to discard video without decoding
    let fakesink = gst::ElementFactory::make("fakesink")
        .name(&fakesink_name)
        .property("sync", false) // Don't sync, just drop
        .property("async", false)
        .build()
        .map_err(|e| format!("Failed to create fakesink: {}", e))?;

    // Add to pipeline
    pipeline
        .add(&fakesink)
        .map_err(|e| format!("Failed to add fakesink to pipeline: {}", e))?;

    // Link src_pad to fakesink
    let fakesink_sink = fakesink
        .static_pad("sink")
        .ok_or("Fakesink has no sink pad")?;
    src_pad
        .link(&fakesink_sink)
        .map_err(|e| format!("Failed to link to fakesink: {:?}", e))?;

    // Sync fakesink state with pipeline
    fakesink
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync fakesink state: {}", e))?;

    info!(
        "WHEP: Video discard (fakesink) setup complete for stream {}",
        stream_num
    );
    Ok(())
}

/// Get metadata for WHEP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whep_input_definition(), whep_output_definition()]
}

/// Get WHEP Input block definition (metadata only).
fn whep_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_input".to_string(),
        name: "WHEP Input".to_string(),
        description: "Receives audio/video via WebRTC WHEP protocol. Default uses stable whepsrc element.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whepsrc (stable) or whepclientsrc (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whepsrc".to_string(),
                            label: Some("whepsrc (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whepclientsrc".to_string(),
                            label: Some("whepclientsrc (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whepsrc".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "whep_endpoint".to_string(),
                label: "WHEP Endpoint".to_string(),
                description: "WHEP server endpoint URL (e.g., https://example.com/whep/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whep_endpoint".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "auth_token".to_string(),
                label: "Auth Token".to_string(),
                description: "Bearer token for authentication (optional)".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "auth_token".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "mixer_latency_ms".to_string(),
                label: "Mixer Latency (ms)".to_string(),
                description: "Latency of the audio mixer in milliseconds (default 30ms, lower = less delay but may cause glitches)".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(30)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mixer_latency_ms".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "jitterbuffer_latency_ms".to_string(),
                label: "Jitterbuffer Latency (ms)".to_string(),
                description: "WebRTC jitterbuffer latency in milliseconds (default 200ms). Lower values reduce delay but increase sensitivity to network jitter. For LAN use, 40-80ms is recommended.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(DEFAULT_JITTERBUFFER_LATENCY_MS)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "jitterbuffer_latency_ms".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![ExternalPad {
                label: None,
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "output_audioresample".to_string(),
                internal_pad_name: "src".to_string(),
            }],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// Get WHEP Output block definition (server mode - hosts WHEP endpoint).
fn whep_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whep_output".to_string(),
        name: "WHEP Output".to_string(),
        description: "Hosts a WHEP server endpoint. Clients can connect via WHEP to receive the WebRTC stream. Access at /api/whep/{endpoint_id}. Set Number of Video/Audio Tracks to 0 to disable that media type.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "num_video_tracks".to_string(),
                label: "Number of Video Tracks".to_string(),
                description: "Number of video input tracks (0 disables video on this endpoint).".to_string(),
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
                description: "Number of audio input tracks (0 disables audio on this endpoint).".to_string(),
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
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHEP endpoint. Leave empty to auto-generate a UUID. Access at /api/whep/{endpoint_id}".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "endpoint_id".to_string(),
                    transform: None,
                },
                live: false,
            },
            ExposedProperty {
                name: "ts_offset_ms".to_string(),
                label: "TS Offset (ms)".to_string(),
                description: "Timestamp offset for playout timing. A negative value (e.g. -200) makes this output release buffers earlier than the pipeline latency dictates. A/V sync is maintained — only the playout point shifts. Useful for multiview outputs that should display with minimal delay.".to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(0)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "ts_offset_ms".to_string(),
                    transform: None,
                },
                live: false,
            },
        ],
        // Note: external_pads here are the static defaults (1 video + 1 audio).
        // The actual pads are determined dynamically by WHEPOutputBuilder::get_external_pads()
        // based on num_audio_tracks / num_video_tracks (and the legacy mode property if present).
        external_pads: ExternalPads {
            inputs: vec![
                ExternalPad {
                    label: Some("V0".to_string()),
                    name: "video_in".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "video_queue".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
                ExternalPad {
                    label: Some("A0".to_string()),
                    name: "audio_in".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_queue".to_string(),
                    internal_pad_name: "sink".to_string(),
                },
            ],
            outputs: vec![],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📡".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a property map. `legacy_mode` populates the old "mode" enum
    /// (`Some("audio")` etc.) to exercise the migration path; pass `None` for
    /// new flows. Count values pass through `num_audio_tracks` /
    /// `num_video_tracks`.
    fn props(
        legacy_mode: Option<&str>,
        num_audio_tracks: Option<i64>,
        num_video_tracks: Option<i64>,
    ) -> HashMap<String, PropertyValue> {
        let mut p = HashMap::new();
        if let Some(m) = legacy_mode {
            p.insert("mode".to_string(), PropertyValue::String(m.to_string()));
        }
        if let Some(c) = num_audio_tracks {
            p.insert("num_audio_tracks".to_string(), PropertyValue::Int(c));
        }
        if let Some(c) = num_video_tracks {
            p.insert("num_video_tracks".to_string(), PropertyValue::Int(c));
        }
        p
    }

    fn audio_pad_names(pads: &ExternalPads) -> Vec<String> {
        pads.inputs
            .iter()
            .filter(|p| matches!(p.media_type, MediaType::Audio))
            .map(|p| p.name.clone())
            .collect()
    }

    fn video_pad_names(pads: &ExternalPads) -> Vec<String> {
        pads.inputs
            .iter()
            .filter(|p| matches!(p.media_type, MediaType::Video))
            .map(|p| p.name.clone())
            .collect()
    }

    #[test]
    fn external_pads_default_is_one_video_and_one_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn external_pads_zero_audio_disables_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(0), None))
            .expect("expected pads");
        assert!(audio_pad_names(&pads).is_empty());
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn external_pads_zero_video_disables_video() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, None, Some(0)))
            .expect("expected pads");
        assert!(video_pad_names(&pads).is_empty());
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
    }

    #[test]
    fn external_pads_count_clamped_to_max_eight() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(99), Some(99)))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads).len(), 8);
        assert_eq!(video_pad_names(&pads).len(), 8);
    }

    #[test]
    fn external_pads_count_clamped_to_min_zero() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(-5), Some(-1)))
            .expect("expected pads");
        assert!(audio_pad_names(&pads).is_empty());
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_audio_count_four() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(4), Some(0)))
            .expect("expected pads");
        // Slot 0 keeps the unsuffixed name; subsequent slots are suffixed.
        assert_eq!(
            audio_pad_names(&pads),
            vec!["audio_in", "audio_in_1", "audio_in_2", "audio_in_3"],
        );
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_video_count_four() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(0), Some(4)))
            .expect("expected pads");
        assert_eq!(
            video_pad_names(&pads),
            vec!["video_in", "video_in_1", "video_in_2", "video_in_3"],
        );
        assert!(audio_pad_names(&pads).is_empty());
    }

    #[test]
    fn external_pads_video_three_with_audio_one() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(None, Some(1), Some(3)))
            .expect("expected pads");
        assert_eq!(
            video_pad_names(&pads),
            vec!["video_in", "video_in_1", "video_in_2"],
        );
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
    }

    // --- Legacy mode migration ---

    #[test]
    fn legacy_mode_audio_video_with_no_counts_yields_one_plus_one() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio_video"), None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
    }

    #[test]
    fn legacy_mode_audio_only_with_no_counts_disables_video() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio"), None, None))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads), vec!["audio_in"]);
        assert!(video_pad_names(&pads).is_empty());
    }

    #[test]
    fn legacy_mode_video_only_with_no_counts_disables_audio() {
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("video"), None, None))
            .expect("expected pads");
        assert_eq!(video_pad_names(&pads), vec!["video_in"]);
        assert!(audio_pad_names(&pads).is_empty());
    }

    #[test]
    fn explicit_counts_override_legacy_mode() {
        // Counts are authoritative when present; legacy mode is ignored.
        let pads = WHEPOutputBuilder
            .get_external_pads(&props(Some("audio"), Some(2), Some(3)))
            .expect("expected pads");
        assert_eq!(audio_pad_names(&pads).len(), 2);
        assert_eq!(video_pad_names(&pads).len(), 3);
    }
}
