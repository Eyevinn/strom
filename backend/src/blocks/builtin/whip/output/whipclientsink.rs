//! WHIP Output using `whipclientsink` (signaller-based).

use super::video::{build_video_input, OnAccept};
use super::{
    element_id, make, register_sink_pad_requests, resolve_track_counts, Sink, Track, TrackKind,
};
use crate::blocks::{
    set_ice_transport_policy, BlockBuildContext, BlockBuildError, BlockBuildResult,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue};
use tracing::{debug, info};

/// Build using the new whipclientsink (signaller-based) implementation
pub(super) fn build_whipclientsink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Output using whipclientsink (new implementation)");

    // Get required WHIP endpoint
    let whip_endpoint = properties
        .get("whip_endpoint")
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
            BlockBuildError::InvalidProperty("whip_endpoint property required".to_string())
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

    let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
    let whipclientsink_id = format!("{}:whipclientsink", instance_id);

    // Create whipclientsink element
    let whipclientsink = gst::ElementFactory::make("whipclientsink")
        .name(&whipclientsink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whipclientsink: {}", e)))?;

    // Set ICE server properties (explicitly clear defaults when not configured,
    // since webrtcsink defaults to stun://stun.l.google.com:19302)
    match stun_server {
        Some(ref stun) => whipclientsink.set_property("stun-server", stun),
        None => whipclientsink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        let turn_servers = gst::Array::new([turn]);
        whipclientsink.set_property("turn-servers", turn_servers);
    }

    // webrtcsink applies this to the webrtcbin it creates for the session, as
    // it creates it. The deep-element-added handler below sets the same value
    // again, which covers a webrtcsink that does not expose the property.
    set_ice_transport_policy(
        &whipclientsink,
        &ice_transport_policy,
        "WHIP Output (whipclientsink)",
    );

    // No video tracks: offer no video codecs. With video tracks the input
    // probe narrows video-caps to the codec that arrives, and the setup hook
    // empties it again if no video input is linked.
    if num_video_tracks == 0 {
        whipclientsink.set_property("video-caps", gst::Caps::new_empty());
    }

    // Access the signaller child and set its properties
    let signaller = whipclientsink.property::<gst::glib::Object>("signaller");
    signaller.set_property("whip-endpoint", &whip_endpoint);

    if let Some(token) = &auth_token {
        signaller.set_property("auth-token", token);
    }

    // Read Opus encoder settings
    let opus_complexity = properties
        .get("opus_complexity")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_COMPLEXITY);

    let opus_bitrate = properties
        .get("opus_bitrate")
        .and_then(|v| match v {
            PropertyValue::Int(i) => Some(*i as i32),
            _ => None,
        })
        .unwrap_or(DEFAULT_OPUS_BITRATE);

    // Configure internal elements via deep-element-added:
    // - ICE transport policy on webrtcbin
    // - Opus encoder settings on opusenc
    if let Ok(bin) = whipclientsink.clone().downcast::<gst::Bin>() {
        let ice_transport_policy = ice_transport_policy.clone();
        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            if element_name.starts_with("webrtcbin") && element.has_property("ice-transport-policy")
            {
                element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                info!(
                    "WHIP (whipclientsink): Set ice-transport-policy={} on webrtcbin {}",
                    ice_transport_policy, element_name
                );
            }

            if element_name.starts_with("opusenc") {
                element.set_property("complexity", opus_complexity);
                element.set_property("bitrate", opus_bitrate);
                info!(
                    "WHIP (whipclientsink): Set opusenc {}: complexity={}, bitrate={}",
                    element_name, opus_complexity, opus_bitrate
                );
            }
            None
        });
    }

    // Zero the processing deadline on whipclientsink's input appsinks.
    //
    // Each input ends in a syncing appsink that only hands buffers to the
    // session pipeline, so BaseSink's default 20 ms deadline buys nothing. It
    // is still added to the pipeline latency, which the appsink waits out and
    // then forwards to the session's appsrc, where webrtcbin's clocksync waits
    // for it again. A flow adopts the largest latency any sink reports, so
    // the deadline would also delay every other sink in the flow.
    //
    // The cost: the deadline is slack against a stall in a thread upstream of
    // a queue, and without it buffers delayed by such a stall leave off beat.
    // In the block's own streaming thread a deadline only shifts the wait.
    // WHIP Input relayed straight into WHIP Output is the shape that pays;
    // mixers and routers pace their own output.
    //
    // webrtcsink creates these appsinks when a pad is requested, which happens
    // in the setup hook before the pipeline starts, so the handler has to be
    // in place now.
    if let Ok(bin) = whipclientsink.clone().downcast::<gst::Bin>() {
        bin.connect("deep-element-added", false, |args| {
            let owner: gst::Bin = args[0].get().ok()?;
            let parent: gst::Bin = args[1].get().ok()?;
            let added: gst::Element = args[2].get().ok()?;
            if parent == owner
                && added.factory().is_some_and(|f| f.name() == "appsink")
                && added.has_property("processing-deadline")
            {
                added.set_property("processing-deadline", 0u64);
            }
            None
        });
    }

    debug!(
        "WHIP Output (whipclientsink) configured: endpoint={}, stun={:?}, turn={:?}, ice_transport_policy={}",
        whip_endpoint, stun_server, turn_server, ice_transport_policy
    );

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();
    let mut tracks: Vec<Track> = Vec::new();

    // Audio: raw in, audioconvert -> audioresample; webrtcsink encodes.
    for slot in 0..num_audio_tracks {
        let convert_id = element_id(instance_id, "audioconvert", slot);
        let convert = make("audioconvert", &convert_id)?;
        let resample_id = element_id(instance_id, "audioresample", slot);
        let resample = make("audioresample", &resample_id)?;
        internal_links.push((
            ElementPadRef::pad(&convert_id, "src"),
            ElementPadRef::pad(&resample_id, "sink"),
        ));
        tracks.push(Track {
            kind: TrackKind::Audio,
            slot,
            input: convert.downgrade(),
            tail: resample.downgrade(),
            payloader: None,
        });
        elements.push((convert_id, convert));
        elements.push((resample_id, resample));
    }

    // Video: encoded H.264 in, checked by the input probe, which also tells
    // whipclientsink the codec. webrtcsink parses and payloads it.
    for slot in 0..num_video_tracks {
        let (input_id, input) = build_video_input(
            instance_id,
            slot,
            OnAccept::TellWebrtcSink(whipclientsink.downgrade()),
        )?;
        tracks.push(Track {
            kind: TrackKind::Video,
            slot,
            input: input.downgrade(),
            tail: input.downgrade(),
            payloader: None,
        });
        elements.push((input_id, input));
    }

    info!(
        "WHIP Output {} (whipclientsink): {} audio and {} video track(s) declared",
        instance_id, num_audio_tracks, num_video_tracks
    );

    register_sink_pad_requests(
        ctx,
        instance_id,
        &whipclientsink,
        Sink::WhipClientSink,
        tracks,
    );
    elements.push((whipclientsink_id, whipclientsink));

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}
