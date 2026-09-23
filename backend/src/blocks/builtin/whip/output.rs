//! WHIP Output: whipclientsink and the legacy whipsink.

use crate::blocks::{
    set_ice_transport_policy, BlockBuildContext, BlockBuildError, BlockBuildResult,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use strom_types::{block::*, element::ElementPadRef, PropertyValue};
use tracing::{debug, info, warn};

// ============================================================================
// WHIP Output (whipclientsink / whipsink)
// ============================================================================

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

    // Create namespaced element IDs
    let whipclientsink_id = format!("{}:whipclientsink", instance_id);
    let audioconvert_id = format!("{}:audioconvert", instance_id);
    let audioresample_id = format!("{}:audioresample", instance_id);

    // Create audio processing elements
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

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

    // Disable video codecs by setting video-caps to empty
    whipclientsink.set_property("video-caps", gst::Caps::new_empty());

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

    debug!(
        "WHIP Output (whipclientsink) configured: endpoint={}, stun={:?}, turn={:?}, ice_transport_policy={}",
        whip_endpoint, stun_server, turn_server, ice_transport_policy
    );

    // Define internal links
    let internal_links = vec![
        (
            ElementPadRef::pad(&audioconvert_id, "src"),
            ElementPadRef::pad(&audioresample_id, "sink"),
        ),
        (
            ElementPadRef::pad(&audioresample_id, "src"),
            ElementPadRef::pad(&whipclientsink_id, "audio_0"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (audioconvert_id, audioconvert),
            (audioresample_id, audioresample),
            (whipclientsink_id, whipclientsink),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

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

    let whipsink_id = format!("{}:whipsink", instance_id);
    let audioconvert_id = format!("{}:audioconvert", instance_id);
    let audioresample_id = format!("{}:audioresample", instance_id);
    let opusenc_id = format!("{}:opusenc", instance_id);
    let rtpopuspay_id = format!("{}:rtpopuspay", instance_id);

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

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

    let opusenc = gst::ElementFactory::make("opusenc")
        .name(&opusenc_id)
        .property("complexity", opus_complexity)
        .property("bitrate", opus_bitrate)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("opusenc: {}", e)))?;

    info!(
        "WHIP Output opusenc: complexity={}, bitrate={}",
        opus_complexity, opus_bitrate
    );

    let rtpopuspay = gst::ElementFactory::make("rtpopuspay")
        .name(&rtpopuspay_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("rtpopuspay: {}", e)))?;

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

    let internal_links = vec![
        (
            ElementPadRef::pad(&audioconvert_id, "src"),
            ElementPadRef::pad(&audioresample_id, "sink"),
        ),
        (
            ElementPadRef::pad(&audioresample_id, "src"),
            ElementPadRef::pad(&opusenc_id, "sink"),
        ),
        (
            ElementPadRef::pad(&opusenc_id, "src"),
            ElementPadRef::pad(&rtpopuspay_id, "sink"),
        ),
        (
            ElementPadRef::pad(&rtpopuspay_id, "src"),
            ElementPadRef::pad(&whipsink_id, "sink_0"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (audioconvert_id, audioconvert),
            (audioresample_id, audioresample),
            (opusenc_id, opusenc),
            (rtpopuspay_id, rtpopuspay),
            (whipsink_id, whipsink),
        ],
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

// ============================================================================
// WHIP Output Helper (incoming RTP handler for legacy whipsink)
// ============================================================================

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
