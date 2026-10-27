//! WHEP Input stream handling: audio is decoded into the liveadder, video is
//! decoded into its slot's output tee when that output is linked, and
//! discarded undecoded otherwise.

use super::video::{configure_keyframe_recovery, VideoRoute, VideoSlots};
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
/// - Video: claim the next video slot (arrival order). Decode into the slot's
///   tee if its output is linked; otherwise, or when every slot is taken,
///   discard via fakesink without decoding.
pub(super) fn setup_stream_with_caps_detection(
    src: &gst::Element,
    src_pad: &gst::Pad,
    liveadder: &gst::Element,
    video: &Arc<VideoSlots>,
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
    let video = Arc::clone(video);
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
                            let (claimed, chain) = match video.claim() {
                                VideoRoute::Decode { slot, tee } => {
                                    info!(
                                        "WHEP: Stream {} is video, decoding into video slot {}",
                                        stream_num, slot
                                    );
                                    let chain = setup_video_decode_chain(
                                        pad,
                                        &pipeline,
                                        &tee,
                                        &instance_id_owned,
                                        stream_num,
                                    )
                                    .inspect_err(|e| {
                                        error!("WHEP: Failed to setup video decode chain: {}", e)
                                    });
                                    (Some(slot), chain.ok())
                                }
                                VideoRoute::Discard { slot } => {
                                    match slot {
                                        Some(slot) => info!(
                                            "WHEP: Stream {} is video for slot {}, whose output is not linked; discarding via fakesink (no decode)",
                                            stream_num, slot
                                        ),
                                        None => info!(
                                            "WHEP: Stream {} is video beyond the declared video tracks; discarding via fakesink (no decode)",
                                            stream_num
                                        ),
                                    }
                                    let chain = setup_video_discard(
                                        pad,
                                        &pipeline,
                                        &instance_id_owned,
                                        stream_num,
                                    )
                                    .inspect_err(|e| {
                                        error!("WHEP: Failed to setup video discard: {}", e)
                                    });
                                    (slot, chain.ok())
                                }
                            };
                            if let Some(identity) = pad.parent_element() {
                                release_when_stream_ends(
                                    &identity,
                                    &pipeline,
                                    &video,
                                    claimed,
                                    chain,
                                    stream_num,
                                );
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

    // Link src_pad to identity now, still inside pad-added, so auto-tee
    // cannot claim the pad. Linked only after the probe is in place: data
    // can flow the moment the pad is linked, and a caps event that passed
    // before the probe existed would leave the stream unrouted.
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

/// Setup video decode chain: decodebin -> slot tee.
///
/// The decoder's output goes to the tee as it is, in whatever memory the
/// decoder produced: the consuming block adapts its own input.
fn setup_video_decode_chain(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    instance_id: &str,
    stream_num: usize,
) -> Result<gst::Element, String> {
    let decodebin_name = format!("{}:video_decodebin_{}", instance_id, stream_num);
    let decodebin = gst::ElementFactory::make("decodebin")
        .name(&decodebin_name)
        .build()
        .map_err(|e| format!("Failed to create video decodebin: {}", e))?;

    // Ask the sender for a keyframe after loss; see configure_keyframe_recovery.
    // The element arrives as a signal argument, so nothing is captured.
    decodebin.connect("deep-element-added", false, |values| {
        if let Ok(element) = values[2].get::<gst::Element>() {
            configure_keyframe_recovery(&element);
        }
        None
    });

    // Weak: the decodebin owns this handler, and the tee is its sibling.
    let tee_weak = tee.downgrade();
    decodebin.connect_pad_added(move |_decodebin, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let is_video = caps
            .structure(0)
            .is_some_and(|s| s.name().starts_with("video/"));
        if !is_video {
            warn!(
                "WHEP: Stream {} video decodebin exposed a non-video pad ({}), leaving it unlinked",
                stream_num, caps
            );
            return;
        }
        let Some(tee) = tee_weak.upgrade() else {
            return;
        };
        let Some(tee_sink) = tee.static_pad("sink") else {
            error!("WHEP: Stream {} video tee has no sink pad", stream_num);
            return;
        };
        if tee_sink.is_linked() {
            error!(
                "WHEP: Stream {} video tee {} is already fed, cannot link a second decoder output",
                stream_num,
                tee.name()
            );
            return;
        }
        match pad.link(&tee_sink) {
            Ok(_) => info!(
                "WHEP: Stream {} decoded video linked to {} ({})",
                stream_num,
                tee.name(),
                caps
            ),
            Err(e) => error!(
                "WHEP: Stream {} failed to link decoded video to {}: {:?}",
                stream_num,
                tee.name(),
                e
            ),
        }
    });

    pipeline
        .add(&decodebin)
        .map_err(|e| format!("Failed to add video decodebin to pipeline: {}", e))?;
    let decodebin_sink = decodebin
        .static_pad("sink")
        .ok_or("Video decodebin has no sink pad")?;
    src_pad
        .link(&decodebin_sink)
        .map_err(|e| format!("Failed to link to video decodebin: {:?}", e))?;
    decodebin
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync video decodebin state: {}", e))?;

    info!(
        "WHEP: Video decode chain setup complete for stream {}",
        stream_num
    );
    Ok(decodebin)
}

/// Setup video discard: fakesink (no decoding, just discard the video stream)
fn setup_video_discard(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    instance_id: &str,
    stream_num: usize,
) -> Result<gst::Element, String> {
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
    Ok(fakesink)
}

/// When a video stream's source pad goes away, free its slot and remove the
/// elements this block added for it, so the stream that replaces it can claim
/// the slot and feed its tee.
///
/// The source unlinks the pad from `identity` when it removes it. The removal
/// runs from the pipeline's async call thread, not the source's thread that
/// emits `unlinked`. Everything is held weakly: the handler belongs to the
/// identity's pad, and the pipeline and the chain are its siblings.
fn release_when_stream_ends(
    identity: &gst::Element,
    pipeline: &gst::Pipeline,
    video: &Arc<VideoSlots>,
    slot: Option<usize>,
    chain: Option<gst::Element>,
    stream_num: usize,
) {
    let Some(sink) = identity.static_pad("sink") else {
        return;
    };
    let pipeline_weak = pipeline.downgrade();
    let video = Arc::clone(video);
    let stream_elements = [Some(identity.downgrade()), chain.map(|c| c.downgrade())];
    sink.connect_unlinked(move |_, _| {
        if let Some(slot) = slot {
            video.release(slot);
        }
        // The source also unlinks its pads when the flow stops. The pipeline
        // removes everything then, and an async call here would hold it past
        // its drop.
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        if pipeline.current_state() < gst::State::Paused
            || matches!(
                pipeline.pending_state(),
                gst::State::Null | gst::State::Ready
            )
        {
            return;
        }
        info!(
            "WHEP: Stream {} ended, freeing video slot {:?}",
            stream_num, slot
        );
        let stream_elements = stream_elements.clone();
        pipeline.call_async(move |pipeline| {
            for element in stream_elements.iter().flatten().filter_map(|e| e.upgrade()) {
                let _ = element.set_state(gst::State::Null);
                let _ = pipeline.remove(&element);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// A source bin that exposes RTP VP8 through ghost pads the test adds and
    /// removes, as whepsrc does with a session's pads. The tee keeps the live
    /// source running while no pad is exposed.
    fn rtp_source() -> (gst::Bin, gst::Element) {
        let bin = gst::Bin::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let enc = gst::ElementFactory::make("vp8enc")
            .property("deadline", 1i64)
            .property("keyframe-max-dist", 10i32)
            .build()
            .unwrap();
        let pay = gst::ElementFactory::make("rtpvp8pay").build().unwrap();
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()
            .unwrap();
        bin.add_many([&src, &enc, &pay, &tee]).unwrap();
        gst::Element::link_many([&src, &enc, &pay, &tee]).unwrap();
        (bin, tee)
    }

    fn expose(bin: &gst::Bin, tee: &gst::Element) -> gst::Pad {
        let target = tee.request_pad_simple("src_%u").unwrap();
        let ghost = gst::GhostPad::with_target(&target).unwrap();
        ghost.set_active(true).unwrap();
        bin.add_pad(&ghost).unwrap();
        ghost.upcast()
    }

    fn unexpose(bin: &gst::Bin, tee: &gst::Element, pad: &gst::Pad) {
        let target = pad
            .downcast_ref::<gst::GhostPad>()
            .and_then(|g| g.target())
            .unwrap();
        pad.set_active(false).unwrap();
        bin.remove_pad(pad).unwrap();
        tee.release_request_pad(&target);
    }

    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A session that ends and starts again within one run: the new video
    /// stream must decode into the same output, not be discarded because the
    /// slot still counts as taken or the old decoder still feeds the tee.
    #[test]
    fn a_video_stream_that_comes_back_decodes_into_its_slot_again() {
        gst::init().unwrap();
        for factory in [
            "videotestsrc",
            "vp8enc",
            "rtpvp8pay",
            "vp8dec",
            "rtpvp8depay",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                assert!(
                    std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
                    "{factory} is missing"
                );
                return;
            }
        }
        let pipeline = gst::Pipeline::new();
        let (bin, source_tee) = rtp_source();
        let liveadder = gst::ElementFactory::make("identity").build().unwrap();
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .unwrap();
        pipeline
            .add_many([bin.upcast_ref(), &liveadder, &tee, &sink])
            .unwrap();
        tee.link(&sink).unwrap();
        let decoded = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&decoded);
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        let video = Arc::new(VideoSlots::for_test(&[(&tee, true)]));
        pipeline.set_state(gst::State::Playing).unwrap();

        let first = expose(&bin, &source_tee);
        setup_stream_with_caps_detection(
            bin.upcast_ref(),
            &first,
            &liveadder,
            &video,
            "whep",
            0,
            &None,
        )
        .unwrap();
        wait_until("the first stream to decode", || {
            decoded.load(Ordering::Relaxed) >= 5
        });

        // The session ends: its pad goes away.
        unexpose(&bin, &source_tee, &first);
        let tee_sink = tee.static_pad("sink").unwrap();
        wait_until("the old decoder to leave the tee", || !tee_sink.is_linked());

        // A new session: a new pad, a new stream.
        let before = decoded.load(Ordering::Relaxed);
        let second = expose(&bin, &source_tee);
        setup_stream_with_caps_detection(
            bin.upcast_ref(),
            &second,
            &liveadder,
            &video,
            "whep",
            1,
            &None,
        )
        .unwrap();
        wait_until("the second stream to decode into the same slot", || {
            decoded.load(Ordering::Relaxed) >= before + 5
        });

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
