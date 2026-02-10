//! WHIP (WebRTC-HTTP Ingestion Protocol) block builders.
//!
//! WHIP Output - Sends media to an external WHIP server:
//! - `whipclientsink` (new): Uses signaller interface, handles encoding internally
//! - `whipsink` (legacy): Simpler implementation, requires pre-encoded RTP input
//!
//! WHIP Input - Hosts a WHIP server for clients to connect and send media:
//! - `whipserversrc`: Hosts HTTP endpoint, clients connect via WHIP to send media
//!
//! Note: WHIP is a send-only protocol, but SMB (Symphony Media Bridge) may still
//! send RTP back to the whipsink. For `whipsink`, we handle this by detecting the
//! internal webrtcbin and linking any incoming source pads to a fakesink to prevent
//! "not-linked" errors. This workaround does not work for `whipclientsink` due to
//! its different internal structure (webrtcbin is not a direct child of the sink bin).

use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, WhepStreamMode,
};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Audio stream elements tracked for cleanup on disconnect:
/// (dynamically created elements, liveadder request pad).
type AudioStreamMap = HashMap<String, (Vec<gst::Element>, gst::Pad)>;

/// WHIP Output block builder.
pub struct WHIPOutputBuilder;

/// WHIP Input block builder (hosts WHIP server).
pub struct WHIPInputBuilder;

impl BlockBuilder for WHIPOutputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Output block instance: {}", instance_id);

        // Get implementation choice (default to stable whipsink)
        let use_new = properties
            .get("implementation")
            .and_then(|v| {
                if let PropertyValue::String(s) = v {
                    Some(s == "whipclientsink")
                } else {
                    None
                }
            })
            .unwrap_or(false);

        if use_new {
            build_whipclientsink(instance_id, properties, ctx)
        } else {
            build_whipsink(instance_id, properties, ctx)
        }
    }
}

impl BlockBuilder for WHIPInputBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        debug!("Building WHIP Input block instance: {}", instance_id);
        build_whipserversrc(instance_id, properties, ctx)
    }

    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let mode = properties
            .get("mode")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(WhepStreamMode::parse(s)),
                _ => None,
            })
            .unwrap_or(WhepStreamMode::AudioVideo);

        let mut outputs = Vec::new();

        if mode.has_audio() {
            outputs.push(ExternalPad {
                name: "audio_out".to_string(),
                media_type: MediaType::Audio,
                internal_element_id: "output_audioresample".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }

        if mode.has_video() {
            outputs.push(ExternalPad {
                name: "video_out".to_string(),
                media_type: MediaType::Video,
                internal_element_id: "output_videoconvert".to_string(),
                internal_pad_name: "src".to_string(),
            });
        }

        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }
}

// ============================================================================
// WHIP Input (whipserversrc - hosts WHIP server)
// ============================================================================

/// Build WHIP Input using whipserversrc (hosts HTTP server for WHIP clients).
///
/// This element creates an HTTP server that WHIP clients can connect to
/// in order to send WebRTC media (audio/video) into the pipeline.
///
/// whipserversrc is based on webrtcsrc and handles decoding internally.
/// It creates dynamic src pads when media arrives from WHIP clients.
///
/// The server binds to localhost on an auto-assigned free port.
/// Axum proxies requests from /whip/{endpoint_id}/... to the internal port.
fn build_whipserversrc(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Input using whipserversrc (server mode)");

    // Get mode (audio_video, audio, or video)
    let mode = properties
        .get("mode")
        .and_then(|v| match v {
            PropertyValue::String(s) => Some(WhepStreamMode::parse(s)),
            _ => None,
        })
        .unwrap_or(WhepStreamMode::AudioVideo);

    info!("WHIP Input mode: {:?}", mode);

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
    // Drop the listener to free the port for whipserversrc
    drop(listener);

    info!(
        "WHIP Input: Found free port {} for endpoint_id '{}'",
        internal_port, endpoint_id
    );

    // Get ICE servers from application config
    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Create whipserversrc element
    let whipserversrc_id = format!("{}:whipserversrc", instance_id);
    let whipserversrc = gst::ElementFactory::make("whipserversrc")
        .name(&whipserversrc_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("whipserversrc: {}", e)))?;

    // Set ICE server properties (explicitly clear defaults when not configured,
    // since webrtcsrc defaults to stun://stun.l.google.com:19302)
    match stun_server {
        Some(ref stun) => whipserversrc.set_property("stun-server", stun),
        None => whipserversrc.set_property("stun-server", None::<&str>),
    }
    if let Some(ref turn) = turn_server {
        let turn_servers = gst::Array::new([turn]);
        whipserversrc.set_property("turn-servers", turn_servers);
    }

    // Access the signaller child and set its properties
    // Bind to localhost only - axum will proxy external requests
    let signaller = whipserversrc.property::<gst::glib::Object>("signaller");
    let host_addr = format!("http://127.0.0.1:{}", internal_port);
    signaller.set_property("host-addr", &host_addr);

    // Configure codec negotiation based on mode.
    // whipserversrc uses video-codecs/audio-codecs properties (gst::Array of
    // codec name strings) to control SDP negotiation. Setting explicit codecs
    // avoids RED/RTX/ULPFEC meta-encodings that can cause issues.
    // whipserversrc decodes internally, so source pads output raw media.
    if mode.has_audio() {
        let audio_codecs = gst::Array::new(["OPUS"]);
        whipserversrc.set_property("audio-codecs", &audio_codecs);
    } else {
        let empty = gst::Array::new(Vec::<&str>::new());
        whipserversrc.set_property("audio-codecs", &empty);
    }
    if mode.has_video() {
        let video_codecs = gst::Array::new(["H264"]);
        whipserversrc.set_property("video-codecs", &video_codecs);
    } else {
        let empty = gst::Array::new(Vec::<&str>::new());
        whipserversrc.set_property("video-codecs", &empty);
    }

    // Set ICE transport policy on internal webrtcbin via deep-element-added
    let dynamic_webrtcbin_store = ctx.dynamic_webrtcbin_store();
    let block_id_for_callback = instance_id.to_string();
    let ice_transport_policy = ctx.ice_transport_policy().to_string();

    if let Ok(bin) = whipserversrc.clone().downcast::<gst::Bin>() {
        bin.connect("deep-element-added", false, move |values| {
            let element = values[2].get::<gst::Element>().unwrap();
            let element_name = element.name();

            if element_name.starts_with("webrtcbin") {
                // Set ICE transport policy
                if element.has_property("ice-transport-policy") {
                    element.set_property_from_str("ice-transport-policy", &ice_transport_policy);
                    info!(
                        "WHIP Input: Set ice-transport-policy={} on webrtcbin {}",
                        ice_transport_policy, element_name
                    );
                }

                // Register webrtcbin for stats collection
                if let Ok(mut store) = dynamic_webrtcbin_store.lock() {
                    store
                        .entry(block_id_for_callback.clone())
                        .or_default()
                        .push(("whip-client".to_string(), element.clone()));
                    debug!(
                        "WHIP Input: Registered webrtcbin for block {}",
                        block_id_for_callback
                    );
                }
            }

            // Configure TWCC feedback interval on internal RTP sessions.
            // By default, twcc-feedback-interval is G_MAXUINT64 (marker-bit based),
            // which may not generate enough feedback for Chrome's GCC bandwidth
            // estimator. Set a fixed 50ms interval to ensure regular feedback.
            let factory_name = element
                .factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default();
            if factory_name == "rtpsession" && element.has_property("internal-session") {
                let internal: gst::glib::Object = element.property("internal-session");
                if internal.has_property("twcc-feedback-interval") {
                    let interval: u64 = 50_000_000; // 50ms in nanoseconds
                    internal.set_property("twcc-feedback-interval", interval);
                    info!(
                        "WHIP Input: Set twcc-feedback-interval=50ms on {}",
                        element_name
                    );
                }
            }

            // Detect video decoders for keyframe recovery on packet loss.
            // When packet loss occurs, the jitterbuffer/depayloader sets the
            // DISCONT flag on buffers entering the decoder's sink pad. We detect
            // this and send UpstreamForceKeyUnit upstream, which webrtcbin
            // translates to PLI RTCP back to the browser.
            let element_klass = element
                .factory()
                .and_then(|f| f.metadata("klass").map(|s| s.to_string()))
                .unwrap_or_default();
            if element_klass.contains("Decoder") && element_klass.contains("Video") {
                let decoder_name = element_name.to_string();
                let decoder_weak = element.downgrade();
                let last_fku_time = Arc::new(AtomicU64::new(0));
                let block_id = block_id_for_callback.clone();

                if let Some(sink_pad) = element.static_pad("sink") {
                    sink_pad.add_probe(
                        gst::PadProbeType::BUFFER,
                        move |_pad, info| {
                            if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                                if buffer.flags().contains(gst::BufferFlags::DISCONT) {
                                    // Rate-limit: max 1 FKU per second
                                    let now_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis()
                                        as u64;
                                    let last = last_fku_time.load(Ordering::Relaxed);
                                    if now_ms.saturating_sub(last) >= 1000 {
                                        last_fku_time.store(now_ms, Ordering::Relaxed);
                                        if let Some(decoder) = decoder_weak.upgrade() {
                                            debug!(
                                                "WHIP Input [{}]: Discontinuity on {} sink, requesting keyframe (PLI)",
                                                block_id, decoder_name
                                            );
                                            let fku =
                                                gst_video::UpstreamForceKeyUnitEvent::builder()
                                                    .all_headers(true)
                                                    .build();
                                            decoder.send_event(fku);
                                        }
                                    }
                                }
                            }
                            gst::PadProbeReturn::Ok
                        },
                    );
                    info!(
                        "WHIP Input: Installed keyframe recovery probe on {} sink pad",
                        element_name
                    );
                }
            }
            None
        });
    }

    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // Create audio output chain if mode includes audio
    // liveadder handles reconnections / multiple streams gracefully
    if mode.has_audio() {
        let liveadder_id = format!("{}:liveadder", instance_id);
        let capsfilter_id = format!("{}:capsfilter", instance_id);
        let output_audioconvert_id = format!("{}:output_audioconvert", instance_id);
        let output_audioresample_id = format!("{}:output_audioresample", instance_id);

        let liveadder = gst::ElementFactory::make("liveadder")
            .name(&liveadder_id)
            .property("latency", 30u32)
            .property("force-live", true)
            .property_from_str("start-time-selection", "first")
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("liveadder: {}", e)))?;

        let caps = gst::Caps::builder("audio/x-raw")
            .field("rate", 48000i32)
            .field("channels", 2i32)
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .name(&capsfilter_id)
            .property("caps", &caps)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("capsfilter: {}", e)))?;

        let output_audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&output_audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("output_audioconvert: {}", e)))?;

        let output_audioresample = gst::ElementFactory::make("audioresample")
            .name(&output_audioresample_id)
            .build()
            .map_err(|e| {
                BlockBuildError::ElementCreation(format!("output_audioresample: {}", e))
            })?;

        internal_links.push((
            ElementPadRef::pad(&liveadder_id, "src"),
            ElementPadRef::pad(&capsfilter_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&capsfilter_id, "src"),
            ElementPadRef::pad(&output_audioconvert_id, "sink"),
        ));
        internal_links.push((
            ElementPadRef::pad(&output_audioconvert_id, "src"),
            ElementPadRef::pad(&output_audioresample_id, "sink"),
        ));

        elements.push((liveadder_id, liveadder));
        elements.push((capsfilter_id, capsfilter));
        elements.push((output_audioconvert_id, output_audioconvert));
        elements.push((output_audioresample_id, output_audioresample));
    }

    // Create video output chain if mode includes video
    if mode.has_video() {
        let output_videoconvert_id = format!("{}:output_videoconvert", instance_id);

        let output_videoconvert = gst::ElementFactory::make("videoconvert")
            .name(&output_videoconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("output_videoconvert: {}", e)))?;

        elements.push((output_videoconvert_id, output_videoconvert));
    }

    // Set up pad-added callback on whipserversrc to route incoming streams
    let instance_id_owned = instance_id.to_string();
    let stream_counter = Arc::new(AtomicUsize::new(0));

    // Get weak refs for elements used in callbacks
    let liveadder_weak = if mode.has_audio() {
        elements
            .iter()
            .find(|(id, _)| id.ends_with(":liveadder"))
            .map(|(_, e)| e.downgrade())
    } else {
        None
    };
    let videoconvert_weak = if mode.has_video() {
        elements
            .iter()
            .find(|(id, _)| id.ends_with(":output_videoconvert"))
            .map(|(_, e)| e.downgrade())
    } else {
        None
    };

    // Only one video stream is accepted at a time. swap(true) atomically sets
    // the flag and returns the previous value: if it was false, we are the first
    // and proceed to link; if true, a video stream is already connected and the
    // new one is discarded to a fakesink. The flag is reset to false in the
    // pad-removed callback when the active client disconnects.
    let video_connected = Arc::new(AtomicBool::new(false));

    // Track the current video queue so we can remove it on disconnect.
    let video_queue: Arc<Mutex<Option<gst::Element>>> = Arc::new(Mutex::new(None));

    // Track audio stream elements per pad name so we can clean them up on disconnect.
    // Each entry: (dynamically created elements, liveadder request pad).
    let audio_streams: Arc<Mutex<AudioStreamMap>> = Arc::new(Mutex::new(HashMap::new()));

    // Reset video_connected when video pad is removed (client disconnect).
    // Also clean up dynamically added elements for that stream.
    let video_connected_for_remove = Arc::clone(&video_connected);
    let videoconvert_weak_for_remove = videoconvert_weak.clone();
    let video_queue_for_remove = Arc::clone(&video_queue);
    let audio_streams_for_remove = Arc::clone(&audio_streams);
    let liveadder_weak_for_remove = liveadder_weak.clone();
    whipserversrc.connect_pad_removed(move |src, pad| {
        let pad_name = pad.name();
        info!("WHIP Input: Pad removed: {}", pad_name);

        // Workaround for gst-plugins-rs BaseWebRTCSrc bug: force all internal
        // session elements to NULL before whipserversrc drops them. This prevents
        // GStreamer-CRITICAL warnings about elements disposed in PLAYING state,
        // which can corrupt internal state and cause 500 errors on reconnect.
        //
        // Strategy: collect strong refs to ALL descendants of whipserversrc
        // synchronously (fast, no locks needed). This prevents finalization while
        // whipserversrc tears down the session. Then spawn a thread to set them
        // to NULL (must be async to avoid deadlock from streaming thread locks).
        if let Ok(bin) = src.clone().downcast::<gst::Bin>() {
            let to_null: Vec<gst::Element> = bin
                .iterate_recurse()
                .into_iter()
                .flatten()
                .filter(|e| {
                    let (_, current, _) = e.state(gst::ClockTime::ZERO);
                    current != gst::State::Null
                })
                .collect();

            if !to_null.is_empty() {
                let count = to_null.len();
                std::thread::spawn(move || {
                    for elem in &to_null {
                        let _ = elem.set_state(gst::State::Null);
                    }
                    info!(
                        "WHIP Input: Force-set {} internal elements to NULL (async cleanup)",
                        count
                    );
                });
            }
        }

        if pad_name.starts_with("video_") {
            video_connected_for_remove.store(false, Ordering::SeqCst);

            // Remove the old video queue from the pipeline so the next
            // connection can create a fresh one and link to videoconvert.
            let old_queue = video_queue_for_remove.lock().unwrap().take();
            if let Some(queue) = old_queue {
                // Unlink queue -> videoconvert, then remove queue from pipeline.
                // The new connection will create a fresh queue and link it.
                // whipserversrc sends proper stream-start → caps → segment events
                // on the new pad, which resets downstream streaming state naturally.
                if let Some(ref vc_weak) = videoconvert_weak_for_remove {
                    if let Some(vc) = vc_weak.upgrade() {
                        if let Some(sink_pad) = vc.static_pad("sink") {
                            if let Some(peer) = sink_pad.peer() {
                                let _ = peer.unlink(&sink_pad);
                            }
                        }
                    }
                }
                // Remove queue from pipeline
                let _ = queue.set_state(gst::State::Null);
                if let Ok(pipeline) = get_pipeline_from_element(src.upcast_ref()) {
                    let _ = pipeline.remove(&queue);
                }
                info!("WHIP Input: Removed old video queue, ready for new connection");
            } else {
                info!("WHIP Input: Video disconnected (no queue to clean up)");
            }
        } else if pad_name.starts_with("audio_") {
            // Clean up dynamically created audio elements for this stream.
            let pad_key = pad_name.to_string();
            let entry = audio_streams_for_remove.lock().unwrap().remove(&pad_key);
            if let Some((elements, liveadder_pad)) = entry {
                if let Ok(pipeline) = get_pipeline_from_element(src.upcast_ref()) {
                    for elem in &elements {
                        let _ = elem.set_state(gst::State::Null);
                        let _ = pipeline.remove(elem);
                    }
                }
                // Release the liveadder request pad
                if let Some(ref la_weak) = liveadder_weak_for_remove {
                    if let Some(la) = la_weak.upgrade() {
                        la.release_request_pad(&liveadder_pad);
                    }
                }
                info!(
                    "WHIP Input: Removed {} audio elements and released liveadder pad for {}",
                    elements.len(),
                    pad_name
                );
            } else {
                info!(
                    "WHIP Input: Audio disconnected {} (no elements to clean up)",
                    pad_name
                );
            }
        }
    });

    whipserversrc.connect_pad_added(move |src, pad| {
        let pad_name = pad.name();
        let stream_num = stream_counter.fetch_add(1, Ordering::SeqCst);

        info!(
            "WHIP Input: New pad added on whipserversrc: {} (stream {})",
            pad_name, stream_num
        );

        // whipserversrc decodes internally (BaseWebRTCSrc → webrtcbin → parsebin
        // → decodebin3 → ghost pad). Output pads are already decoded (audio/x-raw,
        // video/x-raw). Route by pad name, link directly like the whipserver.rs example.
        let pipeline = match get_pipeline_from_element(src) {
            Ok(p) => p,
            Err(e) => {
                error!("WHIP Input: Failed to get pipeline: {}", e);
                return;
            }
        };

        if pad_name.starts_with("audio_") {
            if let Some(ref liveadder_weak) = liveadder_weak {
                if let Some(liveadder) = liveadder_weak.upgrade() {
                    match setup_whip_audio_direct(
                        pad,
                        &pipeline,
                        &liveadder,
                        &instance_id_owned,
                        stream_num,
                    ) {
                        Ok((elements, liveadder_pad)) => {
                            audio_streams
                                .lock()
                                .unwrap()
                                .insert(pad_name.to_string(), (elements, liveadder_pad));
                        }
                        Err(e) => {
                            error!("WHIP Input: Failed to setup audio stream: {}", e);
                        }
                    }
                }
            } else {
                info!(
                    "WHIP Input: Audio stream {} ignored (audio not enabled in mode)",
                    stream_num
                );
                let _ = setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "audio");
            }
        } else if pad_name.starts_with("video_") {
            if !video_connected.swap(true, Ordering::SeqCst) {
                if let Some(ref videoconvert_weak) = videoconvert_weak {
                    if let Some(videoconvert) = videoconvert_weak.upgrade() {
                        match setup_whip_video_direct(
                            pad,
                            &pipeline,
                            &videoconvert,
                            &instance_id_owned,
                            stream_num,
                        ) {
                            Ok(queue) => {
                                *video_queue.lock().unwrap() = Some(queue);
                            }
                            Err(e) => {
                                error!("WHIP Input: Failed to setup video stream: {}", e);
                                video_connected.store(false, Ordering::SeqCst);
                            }
                        }
                    }
                }
            } else {
                info!(
                    "WHIP Input: Additional video stream {} discarded (already connected)",
                    stream_num
                );
                let _ = setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "video");
            }
        } else {
            warn!(
                "WHIP Input: Unknown pad name pattern: {} (stream {})",
                pad_name, stream_num
            );
            let _ = setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "unknown");
        }
    });

    // Add whipserversrc first
    elements.insert(0, (whipserversrc_id, whipserversrc));

    info!(
        "WHIP Input configured: endpoint_id='{}', internal_host={}, stun={:?}, turn={:?}, mode={:?}",
        endpoint_id, host_addr, stun_server, turn_server, mode
    );

    // Register WHIP endpoint with the build context
    ctx.register_whip_endpoint(instance_id, &endpoint_id, internal_port, mode);

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: None,
        pad_properties: HashMap::new(),
    })
}

/// Setup audio stream from whipserversrc: pad (decoded) -> queue -> audioconvert -> audioresample -> liveadder.
/// whipserversrc decodes internally, so the pad already outputs audio/x-raw.
/// Returns the dynamically created elements and the liveadder request pad for cleanup on disconnect.
fn setup_whip_audio_direct(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    liveadder: &gst::Element,
    instance_id: &str,
    stream_num: usize,
) -> Result<(Vec<gst::Element>, gst::Pad), String> {
    let queue_name = format!("{}:whip_audio_queue_{}", instance_id, stream_num);
    let audioconvert_name = format!("{}:whip_audioconvert_{}", instance_id, stream_num);
    let audioresample_name = format!("{}:whip_audioresample_{}", instance_id, stream_num);

    let queue = gst::ElementFactory::make("queue")
        .name(&queue_name)
        .property("max-size-buffers", 3u32)
        .property("max-size-time", 0u64)
        .property("max-size-bytes", 0u32)
        .build()
        .map_err(|e| format!("Failed to create queue: {}", e))?;

    let audioconvert = gst::ElementFactory::make("audioconvert")
        .name(&audioconvert_name)
        .build()
        .map_err(|e| format!("Failed to create audioconvert: {}", e))?;

    let audioresample = gst::ElementFactory::make("audioresample")
        .name(&audioresample_name)
        .build()
        .map_err(|e| format!("Failed to create audioresample: {}", e))?;

    pipeline
        .add_many([&queue, &audioconvert, &audioresample])
        .map_err(|e| format!("Failed to add audio elements: {}", e))?;

    // Link: src_pad -> queue -> audioconvert -> audioresample -> liveadder
    let queue_sink = queue.static_pad("sink").ok_or("queue has no sink pad")?;
    src_pad
        .link(&queue_sink)
        .map_err(|e| format!("Failed to link pad to queue: {:?}", e))?;

    queue
        .link(&audioconvert)
        .map_err(|e| format!("Failed to link queue to audioconvert: {:?}", e))?;

    audioconvert
        .link(&audioresample)
        .map_err(|e| format!("Failed to link audioconvert to audioresample: {:?}", e))?;

    let liveadder_sink = liveadder
        .request_pad_simple("sink_%u")
        .ok_or("Failed to request sink pad from liveadder")?;
    liveadder_sink.set_property("qos-messages", true);
    let audioresample_src = audioresample.static_pad("src").unwrap();
    audioresample_src
        .link(&liveadder_sink)
        .map_err(|e| format!("Failed to link audioresample to liveadder: {:?}", e))?;

    queue
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync queue state: {}", e))?;
    audioconvert
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync audioconvert state: {}", e))?;
    audioresample
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync audioresample state: {}", e))?;

    info!(
        "WHIP Input: Audio stream {} linked directly (queue -> audioconvert -> audioresample -> liveadder)",
        stream_num
    );
    Ok((vec![queue, audioconvert, audioresample], liveadder_sink))
}

/// Setup video stream from whipserversrc: pad (decoded) -> queue -> output_videoconvert.
/// whipserversrc decodes internally, so the pad already outputs video/x-raw.
fn setup_whip_video_direct(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    output_videoconvert: &gst::Element,
    instance_id: &str,
    stream_num: usize,
) -> Result<gst::Element, String> {
    let queue_name = format!("{}:whip_video_queue_{}", instance_id, stream_num);

    let queue = gst::ElementFactory::make("queue")
        .name(&queue_name)
        .property("max-size-buffers", 3u32)
        .property("max-size-time", 0u64)
        .property("max-size-bytes", 0u32)
        .build()
        .map_err(|e| format!("Failed to create queue: {}", e))?;

    pipeline
        .add(&queue)
        .map_err(|e| format!("Failed to add queue: {}", e))?;

    // Link: src_pad -> queue -> output_videoconvert
    let queue_sink = queue.static_pad("sink").ok_or("queue has no sink pad")?;
    src_pad
        .link(&queue_sink)
        .map_err(|e| format!("Failed to link pad to queue: {:?}", e))?;

    let queue_src = queue.static_pad("src").ok_or("queue has no src pad")?;
    let videoconvert_sink = output_videoconvert
        .static_pad("sink")
        .ok_or("videoconvert has no sink pad")?;
    queue_src
        .link(&videoconvert_sink)
        .map_err(|e| format!("Failed to link queue to videoconvert: {:?}", e))?;

    queue
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync queue state: {}", e))?;

    info!(
        "WHIP Input: Video stream {} linked directly (queue -> videoconvert)",
        stream_num
    );
    Ok(queue)
}

/// Discard a stream via fakesink (no decoding)
fn setup_whip_discard(
    src_pad: &gst::Pad,
    pipeline: &gst::Pipeline,
    instance_id: &str,
    stream_num: usize,
    media_type: &str,
) -> Result<(), String> {
    let fakesink_name = format!(
        "{}:whip_{}_fakesink_{}",
        instance_id, media_type, stream_num
    );

    let fakesink = gst::ElementFactory::make("fakesink")
        .name(&fakesink_name)
        .property("sync", false)
        .property("async", false)
        .build()
        .map_err(|e| format!("Failed to create fakesink: {}", e))?;

    pipeline
        .add(&fakesink)
        .map_err(|e| format!("Failed to add fakesink: {}", e))?;

    let fakesink_sink = fakesink
        .static_pad("sink")
        .ok_or("Fakesink has no sink pad")?;
    src_pad
        .link(&fakesink_sink)
        .map_err(|e| format!("Failed to link to fakesink: {:?}", e))?;

    fakesink
        .sync_state_with_parent()
        .map_err(|e| format!("Failed to sync fakesink state: {}", e))?;

    info!(
        "WHIP Input: {} discard setup for stream {}",
        media_type, stream_num
    );
    Ok(())
}

/// Get pipeline from element, handling nested bins
fn get_pipeline_from_element(element: &gst::Element) -> Result<gst::Pipeline, String> {
    let parent = element
        .parent()
        .ok_or("Could not get parent from element")?;

    if let Ok(pipeline) = parent.clone().downcast::<gst::Pipeline>() {
        return Ok(pipeline);
    }

    if let Some(grandparent) = parent.parent() {
        if let Ok(pipeline) = grandparent.downcast::<gst::Pipeline>() {
            return Ok(pipeline);
        }
    }

    if let Ok(bin) = parent.downcast::<gst::Bin>() {
        if let Some(p) = bin.parent() {
            if let Ok(pipeline) = p.downcast::<gst::Pipeline>() {
                return Ok(pipeline);
            }
        }
    }

    Err("Could not find pipeline from element".to_string())
}

// ============================================================================
// WHIP Output (whipclientsink / whipsink)
// ============================================================================

/// Build using the new whipclientsink (signaller-based) implementation
fn build_whipclientsink(
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
                Some(s.clone())
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

    // Disable video codecs by setting video-caps to empty
    whipclientsink.set_property("video-caps", gst::Caps::new_empty());

    // Access the signaller child and set its properties
    let signaller = whipclientsink.property::<gst::glib::Object>("signaller");
    signaller.set_property("whip-endpoint", &whip_endpoint);

    if let Some(token) = &auth_token {
        signaller.set_property("auth-token", token);
    }

    // Set ICE transport policy on internal webrtcbin via deep-element-added
    if let Ok(bin) = whipclientsink.clone().downcast::<gst::Bin>() {
        let ice_transport_policy = ctx.ice_transport_policy().to_string();
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
            None
        });
    }

    debug!(
        "WHIP Output (whipclientsink) configured: endpoint={}, stun={:?}, turn={:?}",
        whip_endpoint, stun_server, turn_server
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
fn build_whipsink(
    instance_id: &str,
    properties: &HashMap<String, PropertyValue>,
    ctx: &BlockBuildContext,
) -> Result<BlockBuildResult, BlockBuildError> {
    info!("Building WHIP Output using whipsink (stable)");

    let whip_endpoint = properties
        .get("whip_endpoint")
        .and_then(|v| {
            if let PropertyValue::String(s) = v {
                Some(s.clone())
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

    let opusenc = gst::ElementFactory::make("opusenc")
        .name(&opusenc_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("opusenc: {}", e)))?;

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

    debug!(
        "WHIP Output (whipsink legacy) configured: endpoint={}, stun={:?}, turn={:?}",
        whip_endpoint, stun_server, turn_server
    );

    setup_incoming_rtp_handler(&whipsink, instance_id, ctx.ice_transport_policy());

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
// Block Definitions
// ============================================================================

/// Get metadata for WHIP blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![whip_output_definition(), whip_input_definition()]
}

/// WHIP Output block definition.
fn whip_output_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_output".to_string(),
        name: "WHIP Output".to_string(),
        description: "Sends audio via WebRTC WHIP protocol. Default uses stable whipsink element.".to_string(),
        category: "Outputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "implementation".to_string(),
                label: "Implementation".to_string(),
                description: "Choose GStreamer element: whipsink (stable) or whipclientsink (new, may have issues with some servers)".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "whipsink".to_string(),
                            label: Some("whipsink (stable)".to_string()),
                        },
                        EnumValue {
                            value: "whipclientsink".to_string(),
                            label: Some("whipclientsink (new)".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("whipsink".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "implementation".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "whip_endpoint".to_string(),
                label: "WHIP Endpoint".to_string(),
                description: "WHIP server endpoint URL (e.g., https://example.com/whip/room1)"
                    .to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "whip_endpoint".to_string(),
                    transform: None,
                },
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
            icon: Some("🌐".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
}

/// WHIP Input block definition (server mode - hosts WHIP endpoint).
fn whip_input_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.whip_input".to_string(),
        name: "WHIP Input".to_string(),
        description: "Hosts a WHIP server endpoint. Clients (browsers, OBS, encoders) connect via WHIP to send media. Access ingest page at /player/whip-ingest".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "mode".to_string(),
                label: "Stream Mode".to_string(),
                description: "What media to accept: audio + video, audio only, or video only".to_string(),
                property_type: PropertyType::Enum {
                    values: vec![
                        EnumValue {
                            value: "audio_video".to_string(),
                            label: Some("Audio + Video".to_string()),
                        },
                        EnumValue {
                            value: "audio".to_string(),
                            label: Some("Audio Only".to_string()),
                        },
                        EnumValue {
                            value: "video".to_string(),
                            label: Some("Video Only".to_string()),
                        },
                    ],
                },
                default_value: Some(PropertyValue::String("audio_video".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "mode".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "endpoint_id".to_string(),
                label: "Endpoint ID".to_string(),
                description: "Unique identifier for this WHIP endpoint. Leave empty to auto-generate. Ingest at /whip/{endpoint_id}".to_string(),
                property_type: PropertyType::String,
                default_value: Some(PropertyValue::String("".to_string())),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "endpoint_id".to_string(),
                    transform: None,
                },
            },
        ],
        // Note: external_pads here are the static defaults for audio_video mode.
        // Actual pads are determined dynamically by WHIPInputBuilder::get_external_pads().
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "output_audioresample".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "output_videoconvert".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: Some("📹".to_string()),
            width: Some(2.5),
            height: Some(1.5),
            ..Default::default()
        }),
    }
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
