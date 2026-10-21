//! WHIP Output using the legacy `whipsink`.

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
use tracing::{debug, info, warn};

/// Build using the stable whipsink implementation
pub(super) fn build_whipsink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Output using whipsink (stable)");

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

    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    let (num_audio_tracks, num_video_tracks) = resolve_track_counts(properties);
    let whipsink_id = format!("{}:whipsink", instance_id);

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

    info!(
        "WHIP Output opusenc: complexity={}, bitrate={}",
        opus_complexity, opus_bitrate
    );

    let whipsink = gst::ElementFactory::make("whipsink")
        .name(&whipsink_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whipsink: {}", e)))?;

    whipsink.set_property("whip-endpoint", &whip_endpoint);
    // Explicitly clear defaults when not configured,
    // since whipsink defaults to stun://stun.l.google.com:19302
    match stun_server {
        Some(ref stun) => whipsink.set_property("stun-server", stun),
        None => whipsink.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        whipsink.set_property("turn-server", turn);
    }
    if let Some(token) = &auth_token {
        whipsink.set_property("auth-token", token);
    }

    let ice_transport_policy = ctx.resolve_ice_transport_policy(properties);

    // whipsink forwards this to the webrtcbin it owns. The handler installed
    // below sets the same value on that webrtcbin, which covers a whipsink that
    // does not expose the property.
    set_ice_transport_policy(&whipsink, &ice_transport_policy, "WHIP Output (whipsink)");

    debug!(
        "WHIP Output (whipsink legacy) configured: endpoint={}, stun={:?}, turn={:?}, ice_transport_policy={}",
        whip_endpoint, stun_server, turn_server, ice_transport_policy
    );

    setup_incoming_rtp_handler(&whipsink, instance_id, &ice_transport_policy);

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();
    let mut tracks: Vec<Track> = Vec::new();

    // Audio: raw in, audioconvert -> audioresample -> opusenc -> rtpopuspay.
    for slot in 0..num_audio_tracks {
        let chain = [
            (
                "audioconvert",
                element_id(instance_id, "audioconvert", slot),
            ),
            (
                "audioresample",
                element_id(instance_id, "audioresample", slot),
            ),
            ("opusenc", element_id(instance_id, "opusenc", slot)),
            ("rtpopuspay", element_id(instance_id, "rtpopuspay", slot)),
        ];
        let mut built = Vec::new();
        for (factory, id) in &chain {
            built.push(make(factory, id)?);
        }
        built[2].set_property("complexity", opus_complexity);
        built[2].set_property("bitrate", opus_bitrate);
        for pair in chain.windows(2) {
            internal_links.push((
                ElementPadRef::pad(&pair[0].1, "src"),
                ElementPadRef::pad(&pair[1].1, "sink"),
            ));
        }
        tracks.push(Track {
            kind: TrackKind::Audio,
            slot,
            input: built[0].downgrade(),
            tail: built[3].downgrade(),
            payloader: Some(built[3].downgrade()),
        });
        elements.extend(chain.into_iter().map(|(_, id)| id).zip(built));
    }

    // Video: encoded H.264 in, checked by the input probe, which links it on
    // to h264parse -> rtph264pay once it is accepted.
    for slot in 0..num_video_tracks {
        let parse_id = element_id(instance_id, "h264parse", slot);
        let parse = make("h264parse", &parse_id)?;
        let (input_id, input) =
            build_video_input(instance_id, slot, OnAccept::LinkTo(parse.downgrade()))?;
        let pay_id = element_id(instance_id, "rtph264pay", slot);
        let pay = make("rtph264pay", &pay_id)?;
        // SPS and PPS with every IDR, so the receiver can start decoding at
        // any keyframe it asks for, not only the first.
        pay.set_property("config-interval", -1i32);
        // Send each access unit as soon as it is packetised, without waiting
        // for the next one: the mode webrtcsink uses for WebRTC.
        if pay.find_property("aggregate-mode").is_some() {
            pay.set_property_from_str("aggregate-mode", "zero-latency");
        }
        internal_links.push((
            ElementPadRef::pad(&parse_id, "src"),
            ElementPadRef::pad(&pay_id, "sink"),
        ));
        tracks.push(Track {
            kind: TrackKind::Video,
            slot,
            input: input.downgrade(),
            tail: pay.downgrade(),
            payloader: Some(pay.downgrade()),
        });
        elements.push((input_id, input));
        elements.push((parse_id, parse));
        elements.push((pay_id, pay));
    }

    info!(
        "WHIP Output {} (whipsink): {} audio and {} video track(s) declared",
        instance_id, num_audio_tracks, num_video_tracks
    );

    register_sink_pad_requests(ctx, instance_id, &whipsink, Sink::WhipSink, tracks);
    elements.push((whipsink_id, whipsink));

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Setup handler for unexpected incoming RTP on WHIP sink elements.
fn setup_incoming_rtp_handler(
    whip_element: &gst::Element,
    instance_id: &str,
    ice_transport_policy: &str,
) {
    let bin = match whip_element.clone().downcast::<gst::Bin>() {
        Ok(b) => b,
        Err(_) => {
            warn!("WHIP: Element is not a bin, cannot setup incoming RTP handler");
            return;
        }
    };

    let ice_transport_policy = ice_transport_policy.to_string();

    bin.connect("deep-element-added", false, move |values| {
        let parent_bin = values[0].get::<gst::Bin>().unwrap();
        let element = values[2].get::<gst::Element>().unwrap();
        let element_name = element.name();
        let element_type = element.type_().name();

        if element_name.starts_with("webrtcbin") && element.has_property("ice-transport-policy") {
            element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
            info!(
                "WHIP: Set ice-transport-policy={} on webrtcbin {}",
                ice_transport_policy, element_name
            );
        }

        if element_type == "TransportReceiveBin" {
            info!(
                "WHIP: Found {} (parent bin: {}), checking for unlinked src pads",
                element_name,
                parent_bin.name()
            );

            let element_name_clone = element_name.to_string();

            for pad in element.src_pads() {
                let pad_name = pad.name();
                if !pad.is_linked() && pad_name.contains("rtp_src") {
                    let direct_parent = match element.parent() {
                        Some(p) => match p.downcast::<gst::Bin>() {
                            Ok(bin) => bin,
                            Err(_) => continue,
                        },
                        None => continue,
                    };

                    let fakesink_name = format!("whip_fakesink_{}", pad_name);
                    if let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                        .name(&fakesink_name)
                        .property("sync", false)
                        .property("async", false)
                        .build()
                    {
                        if direct_parent.add(&fakesink).is_err() {
                            continue;
                        }
                        let _ = fakesink.sync_state_with_parent();
                        if let Some(sink_pad) = fakesink.static_pad("sink") {
                            if pad.link(&sink_pad).is_ok() {
                                info!("WHIP: Linked {} to fakesink", pad_name);
                            }
                        }
                    }
                }
            }

            element.connect_pad_added(move |elem, pad| {
                let pad_name = pad.name();
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }

                info!("WHIP: {} pad-added: {}", element_name_clone, pad_name);

                if pad.is_linked() || !pad_name.contains("rtp_src") {
                    return;
                }

                let direct_parent = match elem.parent() {
                    Some(p) => match p.downcast::<gst::Bin>() {
                        Ok(bin) => bin,
                        Err(_) => return,
                    },
                    None => return,
                };

                let fakesink_name = format!("whip_fakesink_{}", pad_name);
                if let Ok(fakesink) = gst::ElementFactory::make("fakesink")
                    .name(&fakesink_name)
                    .property("sync", false)
                    .property("async", false)
                    .build()
                {
                    if direct_parent.add(&fakesink).is_err() {
                        return;
                    }
                    let _ = fakesink.sync_state_with_parent();
                    if let Some(sink_pad) = fakesink.static_pad("sink") {
                        if pad.link(&sink_pad).is_ok() {
                            info!("WHIP: Linked new pad {} to fakesink", pad_name);
                        }
                    }
                }
            });
        }

        None
    });

    info!("WHIP: Incoming RTP handler installed for {}", instance_id);
}
