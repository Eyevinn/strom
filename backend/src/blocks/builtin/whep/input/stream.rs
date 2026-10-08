//! WHEP Input stream handling: audio is decoded into the liveadder, video is discarded.

use crate::gst::whep_probe::{self, WhepProbeRegistry};
use gstreamer as gst;
use gstreamer::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

/// Setup a stream from whepclientsrc/whepsrc with caps detection.
/// Uses an identity element to immediately claim the pad (preventing auto-tee),
/// then a pad probe to detect actual caps before deciding how to handle the stream:
/// - Audio: decode and route to liveadder
/// - Video: discard via fakesink (no decode to avoid expensive video decoding)
pub(super) fn setup_stream_with_caps_detection(
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
pub(super) fn get_pipeline_from_element(element: &gst::Element) -> Result<gst::Pipeline, String> {
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
