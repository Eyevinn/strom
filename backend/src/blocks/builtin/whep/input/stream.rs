//! WHEP Input stream handling: audio is decoded into the liveadder, video is
//! handed to [`VideoSlots`], which routes it to a video output.

use super::video::VideoSlots;
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
/// - Video: register with the video slots, which decide whether and where it
///   is decoded.
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
                            match pad.parent_element() {
                                Some(identity) => {
                                    if let Err(e) = video.register(&identity, &pipeline, stream_num)
                                    {
                                        error!(
                                            "WHEP: Stream {} failed to set up video: {}",
                                            stream_num, e
                                        );
                                    }
                                }
                                None => error!("WHEP: Stream {} identity is gone", stream_num),
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

#[cfg(test)]
mod tests {
    use super::super::slot_plan::Assignment;
    use super::super::video::make_selector;
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    const REQUIRED: [&str; 7] = [
        "videotestsrc",
        "vp8enc",
        "rtpvp8pay",
        "vp8dec",
        "rtpvp8depay",
        "input-selector",
        "valve",
    ];

    fn plugins_available() -> bool {
        gst::init().unwrap();
        for factory in REQUIRED {
            if gst::ElementFactory::find(factory).is_none() {
                assert!(
                    std::env::var("STROM_REQUIRE_GST_PLUGINS").is_err(),
                    "{factory} is missing"
                );
                return false;
            }
        }
        true
    }

    /// A source bin that exposes RTP VP8 at `width` x 180 through ghost pads
    /// the test adds and removes, as whepsrc does with a session's pads. The
    /// tee keeps the live source running while no pad is exposed; closing the
    /// valve makes the stream go silent while its pad stays, as an SFU does.
    fn rtp_source(width: i32) -> (gst::Bin, gst::Element, gst::Element) {
        let bin = gst::Bin::new();
        let src = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", 180i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .unwrap();
        let enc = gst::ElementFactory::make("vp8enc")
            .property("deadline", 1i64)
            .property("keyframe-max-dist", 10i32)
            .build()
            .unwrap();
        let pay = gst::ElementFactory::make("rtpvp8pay").build().unwrap();
        let valve = gst::ElementFactory::make("valve").build().unwrap();
        let tee = gst::ElementFactory::make("tee")
            .property("allow-not-linked", true)
            .build()
            .unwrap();
        bin.add_many([&src, &caps, &enc, &pay, &valve, &tee])
            .unwrap();
        gst::Element::link_many([&src, &caps, &enc, &pay, &valve, &tee]).unwrap();
        (bin, tee, valve)
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

    /// One video output (selector -> tee -> fakesink) in a pipeline, with a
    /// count of decoded frames and the width of the frames that reach it.
    struct Output {
        pipeline: gst::Pipeline,
        liveadder: gst::Element,
        sink: gst::Element,
        decoded: Arc<AtomicUsize>,
        video: Arc<VideoSlots>,
    }

    impl Output {
        fn new(policy: Assignment, timeout_ms: u64) -> Self {
            let pipeline = gst::Pipeline::new();
            let liveadder = gst::ElementFactory::make("identity").build().unwrap();
            let selector = make_selector("selector").unwrap();
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
                .add_many([&liveadder, &selector, &tee, &sink])
                .unwrap();
            gst::Element::link_many([&selector, &tee, &sink]).unwrap();
            let decoded = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&decoded);
            sink.static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            let video = VideoSlots::for_test(&[(&selector, &tee, true)], policy, timeout_ms);
            Output {
                pipeline,
                liveadder,
                sink,
                decoded,
                video,
            }
        }

        fn add_source(&self, width: i32) -> (gst::Bin, gst::Element, gst::Element) {
            let (bin, tee, valve) = rtp_source(width);
            self.pipeline.add(&bin).unwrap();
            bin.sync_state_with_parent().unwrap();
            (bin, tee, valve)
        }

        fn connect(&self, bin: &gst::Bin, pad: &gst::Pad, stream: usize) {
            setup_stream_with_caps_detection(
                bin.upcast_ref(),
                pad,
                &self.liveadder,
                &self.video,
                "whep",
                stream,
                &None,
            )
            .unwrap();
        }

        fn decoded(&self) -> usize {
            self.decoded.load(Ordering::Relaxed)
        }

        fn width(&self) -> Option<i32> {
            self.sink
                .static_pad("sink")
                .unwrap()
                .current_caps()
                .and_then(|c| c.structure(0).and_then(|s| s.get::<i32>("width").ok()))
        }

        /// Wait until frames of `width` keep arriving at the output.
        fn wait_for_width(&self, what: &str, width: i32) {
            wait_until(what, || self.width() == Some(width));
            let before = self.decoded();
            wait_until(what, || self.decoded() >= before + 5);
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// A session that ends and starts again within one run: the new video
    /// stream must decode into the same output, not be discarded because the
    /// slot still counts as taken or the old decoder still feeds it.
    #[test]
    fn a_video_stream_that_comes_back_decodes_into_its_slot_again() {
        if !plugins_available() {
            return;
        }
        let out = Output::new(Assignment::Fill, 1000);
        let (bin, source_tee, _valve) = out.add_source(320);
        out.pipeline.set_state(gst::State::Playing).unwrap();

        let first = expose(&bin, &source_tee);
        out.connect(&bin, &first, 0);
        out.wait_for_width("the first stream to decode", 320);

        // The session ends: its pad goes away, and its decoder with it.
        unexpose(&bin, &source_tee, &first);
        wait_until("the old decoder to be removed", || {
            !out.pipeline
                .children()
                .iter()
                .any(|e| e.name().starts_with("whep:video_decodebin_0_"))
        });

        // A new session: a new pad, a new stream.
        let second = expose(&bin, &source_tee);
        out.connect(&bin, &second, 1);
        let before = out.decoded();
        wait_until("the second stream to decode into the same slot", || {
            out.decoded() >= before + 5
        });
    }

    /// A decoder added to a running flow must not put the flow back to
    /// PAUSED. Its preroll would reach the pipeline, which then waits for
    /// every sink to preroll; in a flow the sink of an output no stream feeds
    /// never does, and the flow hangs in PAUSED.
    #[test]
    fn adding_a_decoder_keeps_the_flow_playing() {
        if !plugins_available() {
            return;
        }
        let out = Output::new(Assignment::Fill, 1000);
        let (bin, source_tee, _valve) = out.add_source(320);
        out.pipeline.set_state(gst::State::Playing).unwrap();
        wait_until("the flow to play", || {
            out.pipeline.current_state() == gst::State::Playing
        });
        let bus = out.pipeline.bus().unwrap();
        while bus.pop().is_some() {}

        let pad = expose(&bin, &source_tee);
        out.connect(&bin, &pad, 0);
        out.wait_for_width("the stream to decode", 320);

        while let Some(msg) = bus.pop() {
            if let gst::MessageView::StateChanged(sc) = msg.view() {
                let from_pipeline = msg.src() == Some(out.pipeline.upcast_ref::<gst::Object>());
                assert!(
                    !(from_pipeline && sc.current() == gst::State::Paused),
                    "the flow went back to PAUSED when the decoder was added"
                );
            }
        }
    }

    /// Fill: a second stream waits while the first holds the only output.
    /// When the first goes silent, its pad still in place as an SFU leaves
    /// it, the second takes the output after the timeout.
    #[test]
    fn fill_hands_the_output_to_a_waiting_stream_when_the_holder_goes_silent() {
        if !plugins_available() {
            return;
        }
        let out = Output::new(Assignment::Fill, 300);
        let (bin_a, tee_a, valve_a) = out.add_source(320);
        let (bin_b, tee_b, _valve_b) = out.add_source(160);
        out.pipeline.set_state(gst::State::Playing).unwrap();

        let pad_a = expose(&bin_a, &tee_a);
        out.connect(&bin_a, &pad_a, 0);
        out.wait_for_width("stream A to decode", 320);

        let pad_b = expose(&bin_b, &tee_b);
        out.connect(&bin_b, &pad_b, 1);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(out.width(), Some(320), "B must wait while A sends");

        valve_a.set_property("drop", true);
        out.wait_for_width("stream B to take over after A went silent", 160);
    }

    /// Newest: a second stream takes the only output at once, well before
    /// the timeout, as when an SFU switches its pinned source to a new SSRC.
    #[test]
    fn newest_hands_the_output_to_a_new_stream_at_once() {
        if !plugins_available() {
            return;
        }
        let out = Output::new(Assignment::Newest, 5000);
        let (bin_a, tee_a, _valve_a) = out.add_source(320);
        let (bin_b, tee_b, _valve_b) = out.add_source(160);
        out.pipeline.set_state(gst::State::Playing).unwrap();

        let pad_a = expose(&bin_a, &tee_a);
        out.connect(&bin_a, &pad_a, 0);
        out.wait_for_width("stream A to decode", 320);

        let started = Instant::now();
        let pad_b = expose(&bin_b, &tee_b);
        out.connect(&bin_b, &pad_b, 1);
        out.wait_for_width("stream B to take over", 160);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "B took over through the timeout, not at once"
        );
    }
}
