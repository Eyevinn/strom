//! WHEP Input using the stable `whepsrc`.

use crate::blocks::{
    set_ice_transport_policy, BlockBuildContext, BlockBuildError, BlockBuildResult,
};
use crate::gst::whep_probe;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use strom_types::{block::*, element::ElementPadRef, PropertyValue};
use tracing::{debug, error, info};

use super::stream::setup_stream_with_caps_detection;
use crate::blocks::builtin::webrtc_props::parse_drop_on_latency;

/// Build using the stable whepsrc implementation
pub(super) fn build_whepsrc(
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
    let ice_transport_policy = ctx.resolve_ice_transport_policy(properties);

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
    let drop_on_latency = parse_drop_on_latency(properties);

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

    // whepsrc owns the webrtcbin it builds and forwards this to it, so setting
    // it here beats hooking the child: it lands before the element leaves NULL,
    // which is the state webrtcbin requires for this property.
    set_ice_transport_policy(&whepsrc, &ice_transport_policy, "WHEP Input (whepsrc)");

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
            // configured latency — breaking the stall. Configurable because the
            // workaround costs late packets a downstream WebRTC endpoint could
            // still have used.
            // Upstream: https://gitlab.freedesktop.org/gstreamer/gst-plugins-good/-/merge_requests/951
            if name.starts_with("rtpbin") && element.has_property("drop-on-latency") {
                element.set_property("drop-on-latency", drop_on_latency);
                info!(
                    "WHEP Input (whepsrc): Set drop-on-latency={} on existing {}",
                    drop_on_latency, name
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
        "WHEP Input (whepsrc stable) configured: endpoint={}, stun={:?}, turn={:?}, ice_transport_policy={}",
        whep_endpoint, stun_server, turn_server, ice_transport_policy
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
