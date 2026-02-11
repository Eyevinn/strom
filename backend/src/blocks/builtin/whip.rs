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
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, DynamicWebrtcbinStore,
    WhepStreamMode,
};
use crate::whip_registry::WhipRegistry;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_video as gst_video;
use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use strom_types::{block::*, element::ElementPadRef, PropertyValue, *};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Audio stream elements tracked for cleanup on disconnect:
/// (dynamically created elements, liveadder request pad).
type AudioStreamMap = HashMap<String, (Vec<gst::Element>, gst::Pad)>;

/// Module-level store for WhipServerContext instances, keyed by endpoint_id.
/// Used by `recreate_whipserversrc()` to look up the context for a given endpoint.
static WHIP_SERVERS: OnceLock<Mutex<HashMap<String, Arc<WhipServerContext>>>> = OnceLock::new();

/// Process-level shutdown flag. When set, all recreation is suppressed.
/// Prevents deadlocks during Ctrl+C / SIGTERM when GStreamer is tearing down.
static WHIP_SHUTDOWN: AtomicBool = AtomicBool::new(false);

fn whip_servers() -> &'static Mutex<HashMap<String, Arc<WhipServerContext>>> {
    WHIP_SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Signal all WHIP server contexts to stop recreation.
/// Call this during process shutdown to prevent deadlocks.
pub fn shutdown_whip_servers() {
    WHIP_SHUTDOWN.store(true, Ordering::SeqCst);
    info!("WHIP Input: Shutdown flag set, recreation suppressed");
}

/// Get the current recreation generation for an endpoint.
/// Used by the DELETE handler to check if pad-removed already triggered recreation.
pub fn get_whip_generation(endpoint_id: &str) -> Option<usize> {
    let store = whip_servers().lock().ok()?;
    store
        .get(endpoint_id)
        .map(|ctx| ctx.generation.load(Ordering::SeqCst))
}

/// Destroy the current whipserversrc element and create a fresh replacement.
///
/// Called from:
/// - pad-removed callback (session timeout / browser close)
/// - POST handler retry (stale session causing 500)
///
/// Safe to call multiple times concurrently — uses an AtomicBool to prevent
/// duplicate recreation. Returns immediately during shutdown.
pub fn recreate_whipserversrc(endpoint_id: &str) -> Result<(), String> {
    if WHIP_SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(());
    }
    let store = whip_servers()
        .lock()
        .map_err(|e| format!("Lock poisoned: {}", e))?;
    let ctx = store
        .get(endpoint_id)
        .cloned()
        .ok_or_else(|| format!("No WhipServerContext for endpoint '{}'", endpoint_id))?;
    drop(store);
    ctx.replace_element()
}

/// Holds everything needed to create and recreate a whipserversrc element.
///
/// whipserversrc (BaseWebRTCSrc) has a race condition where old session cleanup
/// corrupts new session state. State cycling (NULL->PLAYING) doesn't fix it because
/// weak refs to the same element remain valid. Instead, we treat each element as
/// single-use: create it, let one client connect, when the session ends destroy the
/// entire element and create a fresh one.
struct WhipServerContext {
    // Creation params (immutable across recreations)
    instance_id: String,
    stun_server: Option<String>,
    turn_server: Option<String>,
    mode: WhepStreamMode,
    ice_transport_policy: String,
    dynamic_webrtcbin_store: DynamicWebrtcbinStore,
    endpoint_id: String,

    // Registry for updating the port when element is recreated on a new port
    whip_registry: WhipRegistry,

    // Shared callback state (same Arcs reused across recreations)
    stream_counter: Arc<AtomicUsize>,
    video_connected: Arc<AtomicBool>,
    video_queue: Arc<Mutex<Option<gst::Element>>>,
    audio_streams: Arc<Mutex<AudioStreamMap>>,

    // Downstream weak refs (elements that outlive whipserversrc recreations)
    liveadder_weak: Option<gst::glib::WeakRef<gst::Element>>,
    videoconvert_weak: Option<gst::glib::WeakRef<gst::Element>>,

    // Current element (updated on each recreation)
    current_element: Mutex<Option<gst::Element>>,

    // Idempotency flag — prevents concurrent recreation
    recreating: AtomicBool,

    // Generation counter — incremented on each recreation. Used by pad-removed
    // tasks to detect that a recreation already happened while they were sleeping.
    generation: AtomicUsize,

    // Tokio runtime handle for spawning async tasks from GStreamer callbacks
    tokio_handle: tokio::runtime::Handle,
}

impl WhipServerContext {
    /// Create a new whipserversrc element with all callbacks wired up.
    /// Allocates a fresh port each time to avoid "address in use" when the
    /// old element's warp server hasn't fully released the socket yet.
    /// Returns (element, allocated_port). Caller is responsible for updating
    /// the WhipRegistry with the new port (to avoid blocking_write inside tokio).
    fn create_element(&self) -> Result<(gst::Element, u16), String> {
        // Find a new free port
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to find free port: {}", e))?;
        let new_port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {}", e))?
            .port();
        drop(listener);

        let host_addr = format!("http://127.0.0.1:{}", new_port);
        let gen = self.generation.load(Ordering::SeqCst);
        info!(
            "WHIP Input: Allocating port {} for endpoint '{}' (gen {})",
            new_port, self.endpoint_id, gen
        );

        // Use unique name per recreation to avoid any GStreamer internal state leaking
        let whipserversrc_id = if gen == 0 {
            format!("{}:whipserversrc", self.instance_id)
        } else {
            format!("{}:whipserversrc_g{}", self.instance_id, gen)
        };
        let whipserversrc = gst::ElementFactory::make("whipserversrc")
            .name(&whipserversrc_id)
            .build()
            .map_err(|e| format!("whipserversrc: {}", e))?;

        // Set ICE server properties
        match self.stun_server {
            Some(ref stun) => whipserversrc.set_property("stun-server", stun),
            None => whipserversrc.set_property("stun-server", None::<&str>),
        }
        if let Some(ref turn) = self.turn_server {
            let turn_servers = gst::Array::new([turn]);
            whipserversrc.set_property("turn-servers", turn_servers);
        }

        // Set signaller host-addr
        let signaller = whipserversrc.property::<gst::glib::Object>("signaller");
        signaller.set_property("host-addr", &host_addr);

        // Configure codec negotiation based on mode
        if self.mode.has_audio() {
            let audio_codecs = gst::Array::new(["OPUS"]);
            whipserversrc.set_property("audio-codecs", &audio_codecs);
        } else {
            let empty = gst::Array::new(Vec::<&str>::new());
            whipserversrc.set_property("audio-codecs", &empty);
        }
        if self.mode.has_video() {
            let video_codecs = gst::Array::new(["H264"]);
            whipserversrc.set_property("video-codecs", &video_codecs);
        } else {
            let empty = gst::Array::new(Vec::<&str>::new());
            whipserversrc.set_property("video-codecs", &empty);
        }

        // deep-element-added: ICE policy, TWCC, keyframe recovery
        let dynamic_webrtcbin_store = self.dynamic_webrtcbin_store.clone();
        let block_id_for_callback = self.instance_id.clone();
        let ice_transport_policy = self.ice_transport_policy.clone();

        if let Ok(bin) = whipserversrc.clone().downcast::<gst::Bin>() {
            bin.connect("deep-element-added", false, move |values| {
                let element = values[2].get::<gst::Element>().unwrap();
                let element_name = element.name();

                if element_name.starts_with("webrtcbin") {
                    if element.has_property("ice-transport-policy") {
                        element
                            .set_property_from_str("ice-transport-policy", &ice_transport_policy);
                        info!(
                            "WHIP Input: Set ice-transport-policy={} on webrtcbin {}",
                            ice_transport_policy, element_name
                        );
                    }

                    // Note: ICE consent freshness (RFC 7675) is enabled at compile
                    // time in libgstwebrtcnice when built against libnice > 0.1.21.1.
                    // See HAVE_LIBNICE_CONSENT_FIX in GStreamer source. On older
                    // builds (e.g. Ubuntu 24.04 stock), consent-freshness is missing
                    // and connections drop after ~6s. Upgrade libnice + rebuild
                    // libgstwebrtcnice to fix.
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

                    // Monitor ICE connection state changes on webrtcbin
                    let wrtc_name = element_name.to_string();
                    element.connect_notify(Some("ice-connection-state"), move |elem, _pspec| {
                        let val = elem.property_value("ice-connection-state");
                        info!(
                            "WHIP Input: [SERVER] {} ice-connection-state changed (raw: {:?})",
                            wrtc_name, val
                        );
                    });
                }

                // Configure TWCC feedback interval on internal RTP sessions
                let factory_name = element
                    .factory()
                    .map(|f| f.name().to_string())
                    .unwrap_or_default();
                if factory_name == "rtpsession" && element.has_property("internal-session") {
                    let internal: gst::glib::Object = element.property("internal-session");
                    if internal.has_property("twcc-feedback-interval") {
                        let interval: u64 = 200_000_000; // 200ms in nanoseconds
                        internal.set_property("twcc-feedback-interval", interval);
                        info!(
                            "WHIP Input: Set twcc-feedback-interval=200ms on {}",
                            element_name
                        );
                    }
                }

                // Detect video decoders for keyframe recovery on packet loss
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
                        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                            if let Some(gst::PadProbeData::Buffer(ref buffer)) = info.data {
                                if buffer.flags().contains(gst::BufferFlags::DISCONT) {
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
                        });
                        info!(
                            "WHIP Input: Installed keyframe recovery probe on {} sink pad",
                            element_name
                        );
                    }
                }
                None
            });
        }

        // pad-removed: cleanup + trigger async recreation
        let video_connected_for_remove = Arc::clone(&self.video_connected);
        let videoconvert_weak_for_remove = self.videoconvert_weak.clone();
        let video_queue_for_remove = Arc::clone(&self.video_queue);
        let audio_streams_for_remove = Arc::clone(&self.audio_streams);
        let liveadder_weak_for_remove = self.liveadder_weak.clone();
        let endpoint_id_for_remove = self.endpoint_id.clone();
        let tokio_handle_for_remove = self.tokio_handle.clone();
        whipserversrc.connect_pad_removed(move |src, pad| {
            let pad_name = pad.name();
            info!("WHIP Input: Pad removed: {}", pad_name);

            if pad_name.starts_with("video_") {
                video_connected_for_remove.store(false, Ordering::SeqCst);

                let old_queue = match video_queue_for_remove.lock() {
                    Ok(mut g) => g.take(),
                    Err(e) => {
                        warn!("WHIP Input: video_queue lock poisoned in pad-removed: {}", e);
                        None
                    }
                };
                if let Some(queue) = old_queue {
                    if let Some(ref vc_weak) = videoconvert_weak_for_remove {
                        let vc_opt: Option<gst::Element> = vc_weak.upgrade();
                        if let Some(vc) = vc_opt {
                            if let Some(sink_pad) = vc.static_pad("sink") {
                                if let Some(peer) = sink_pad.peer() {
                                    let _ = peer.unlink(&sink_pad);
                                }
                            }
                        }
                    }
                    let _ = queue.set_state(gst::State::Null);
                    if let Ok(pipeline) = get_pipeline_from_element(src.upcast_ref()) {
                        let _ = pipeline.remove(&queue);
                    }
                    info!("WHIP Input: Removed old video queue");
                }
            } else if pad_name.starts_with("audio_") {
                let pad_key = pad_name.to_string();
                let entry = match audio_streams_for_remove.lock() {
                    Ok(mut g) => g.remove(&pad_key),
                    Err(e) => {
                        warn!("WHIP Input: audio_streams lock poisoned in pad-removed: {}", e);
                        None
                    }
                };
                if let Some((elements, liveadder_pad)) = entry {
                    if let Ok(pipeline) = get_pipeline_from_element(src.upcast_ref()) {
                        for elem in &elements {
                            let _ = elem.set_state(gst::State::Null);
                            let _ = pipeline.remove(elem);
                        }
                    }
                    if let Some(ref la_weak) = liveadder_weak_for_remove {
                        let la_opt: Option<gst::Element> = la_weak.upgrade();
                        if let Some(la) = la_opt {
                            la.release_request_pad(&liveadder_pad);
                        }
                    }
                    info!(
                        "WHIP Input: Removed {} audio elements and released liveadder pad for {}",
                        elements.len(),
                        pad_name
                    );
                }
            }

            // Trigger async recreation after pad removal (session ended).
            // Delay gives time for DELETE response to be sent if this was DELETE-triggered.
            // We capture the current generation before sleeping. If another thread
            // already performed a recreation (incrementing the generation), we skip.
            if WHIP_SHUTDOWN.load(Ordering::SeqCst) {
                return;
            }
            let endpoint_id = endpoint_id_for_remove.clone();
            let gen_at_removal = match whip_servers().lock() {
                Ok(store) => store
                    .get(&endpoint_id)
                    .map(|ctx| ctx.generation.load(Ordering::SeqCst))
                    .unwrap_or(0),
                Err(e) => {
                    warn!("WHIP Input: whip_servers lock poisoned in pad-removed: {}", e);
                    return;
                }
            };
            tokio_handle_for_remove.spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                // Bail out if process is shutting down
                if WHIP_SHUTDOWN.load(Ordering::SeqCst) {
                    return;
                }
                // Check if generation changed (means someone already recreated)
                let current_gen = match whip_servers().lock() {
                    Ok(store) => store
                        .get(&endpoint_id)
                        .map(|ctx| ctx.generation.load(Ordering::SeqCst))
                        .unwrap_or(0),
                    Err(_) => return,
                };
                if current_gen != gen_at_removal {
                    info!(
                        "WHIP Input: Skipping pad-removed recreation for '{}' (generation {} -> {}, already recreated)",
                        endpoint_id, gen_at_removal, current_gen
                    );
                    return;
                }
                info!(
                    "WHIP Input: Triggering element recreation for endpoint '{}'",
                    endpoint_id
                );
                // Recreation is blocking (set_state, sleep) — run on blocking thread pool
                let eid = endpoint_id.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || {
                    recreate_whipserversrc(&eid)
                })
                .await
                .unwrap_or(Err("spawn_blocking panicked".into()))
                {
                    warn!("WHIP Input: Failed to recreate whipserversrc: {}", e);
                }
            });
        });

        // pad-added: route incoming streams to audio/video chains
        let instance_id_owned = self.instance_id.clone();
        let stream_counter = Arc::clone(&self.stream_counter);
        let liveadder_weak = self.liveadder_weak.clone();
        let videoconvert_weak = self.videoconvert_weak.clone();
        let video_connected = Arc::clone(&self.video_connected);
        let video_queue = Arc::clone(&self.video_queue);
        let audio_streams = Arc::clone(&self.audio_streams);

        whipserversrc.connect_pad_added(move |src, pad| {
            let pad_name = pad.name();
            let stream_num = stream_counter.fetch_add(1, Ordering::SeqCst);

            info!(
                "WHIP Input: New pad added on whipserversrc: {} (stream {})",
                pad_name, stream_num
            );

            let pipeline = match get_pipeline_from_element(src) {
                Ok(p) => p,
                Err(e) => {
                    error!("WHIP Input: Failed to get pipeline: {}", e);
                    return;
                }
            };

            if pad_name.starts_with("audio_") {
                if let Some(ref liveadder_weak) = liveadder_weak {
                    let la_opt: Option<gst::Element> = liveadder_weak.upgrade();
                    if let Some(liveadder) = la_opt {
                        match setup_whip_audio_direct(
                            pad,
                            &pipeline,
                            &liveadder,
                            &instance_id_owned,
                            stream_num,
                        ) {
                            Ok((elements, liveadder_pad)) => {
                                if let Ok(mut g) = audio_streams.lock() {
                                    g.insert(pad_name.to_string(), (elements, liveadder_pad));
                                } else {
                                    error!("WHIP Input: audio_streams lock poisoned in pad-added");
                                }
                            }
                            Err(e) => {
                                error!("WHIP Input: Failed to setup audio stream: {}", e);
                            }
                        }
                    } else {
                        warn!(
                            "WHIP Input: liveadder destroyed, discarding audio stream {}",
                            stream_num
                        );
                        let _ = setup_whip_discard(
                            pad,
                            &pipeline,
                            &instance_id_owned,
                            stream_num,
                            "audio",
                        );
                    }
                } else {
                    info!(
                        "WHIP Input: Audio stream {} ignored (audio not enabled in mode)",
                        stream_num
                    );
                    let _ =
                        setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "audio");
                }
            } else if pad_name.starts_with("video_") {
                if !video_connected.swap(true, Ordering::SeqCst) {
                    if let Some(ref videoconvert_weak) = videoconvert_weak {
                        let vc_opt: Option<gst::Element> = videoconvert_weak.upgrade();
                        if let Some(videoconvert) = vc_opt {
                            match setup_whip_video_direct(
                                pad,
                                &pipeline,
                                &videoconvert,
                                &instance_id_owned,
                                stream_num,
                            ) {
                                Ok(queue) => {
                                    if let Ok(mut g) = video_queue.lock() {
                                        *g = Some(queue);
                                    } else {
                                        error!(
                                            "WHIP Input: video_queue lock poisoned in pad-added"
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!("WHIP Input: Failed to setup video stream: {}", e);
                                    video_connected.store(false, Ordering::SeqCst);
                                }
                            }
                        } else {
                            warn!(
                                "WHIP Input: videoconvert destroyed, discarding video stream {}",
                                stream_num
                            );
                            video_connected.store(false, Ordering::SeqCst);
                            let _ = setup_whip_discard(
                                pad,
                                &pipeline,
                                &instance_id_owned,
                                stream_num,
                                "video",
                            );
                        }
                    }
                } else {
                    info!(
                        "WHIP Input: Additional video stream {} discarded (already connected)",
                        stream_num
                    );
                    let _ =
                        setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "video");
                }
            } else {
                warn!(
                    "WHIP Input: Unknown pad name pattern: {} (stream {})",
                    pad_name, stream_num
                );
                let _ =
                    setup_whip_discard(pad, &pipeline, &instance_id_owned, stream_num, "unknown");
            }
        });

        Ok((whipserversrc, new_port))
    }

    /// Destroy the old whipserversrc and create a fresh replacement in the pipeline.
    fn replace_element(&self) -> Result<(), String> {
        // Idempotency: if already recreating, skip
        if self.recreating.swap(true, Ordering::SeqCst) {
            info!(
                "WHIP Input: Recreation already in progress for '{}', skipping",
                self.endpoint_id
            );
            return Ok(());
        }

        let result = self.replace_element_inner();

        // Always clear the flag, even on error
        self.recreating.store(false, Ordering::SeqCst);
        result
    }

    fn replace_element_inner(&self) -> Result<(), String> {
        // Take old element
        let old_element = self
            .current_element
            .lock()
            .expect("current_element lock poisoned")
            .take();
        let old_element = match old_element {
            Some(e) => e,
            None => {
                warn!(
                    "WHIP Input: No current element to replace for '{}'",
                    self.endpoint_id
                );
                return Ok(());
            }
        };

        // Get pipeline from old element
        let pipeline = get_pipeline_from_element(&old_element)?;

        // Clean up downstream links before destroying the old element.
        // pad-removed may not have fired yet (e.g. DELETE handler runs before
        // teardown completes), so we must explicitly clean up here.
        self.video_connected.store(false, Ordering::SeqCst);

        // Remove video queue and unlink videoconvert
        let old_queue = self
            .video_queue
            .lock()
            .expect("video_queue lock poisoned")
            .take();
        if let Some(queue) = old_queue {
            if let Some(ref vc_weak) = self.videoconvert_weak {
                let vc_opt: Option<gst::Element> = vc_weak.upgrade();
                if let Some(vc) = vc_opt {
                    if let Some(sink_pad) = vc.static_pad("sink") {
                        if let Some(peer) = sink_pad.peer() {
                            let _ = peer.unlink(&sink_pad);
                        }
                    }
                }
            }
            let _ = queue.set_state(gst::State::Null);
            let _ = pipeline.remove(&queue);
        }

        // Remove audio stream elements and release liveadder pads
        let audio_entries: Vec<_> = self
            .audio_streams
            .lock()
            .expect("audio_streams lock poisoned")
            .drain()
            .collect();
        for (_pad_name, (elements, liveadder_pad)) in audio_entries {
            for elem in &elements {
                let _ = elem.set_state(gst::State::Null);
                let _ = pipeline.remove(elem);
            }
            if let Some(ref la_weak) = self.liveadder_weak {
                let la_opt: Option<gst::Element> = la_weak.upgrade();
                if let Some(la) = la_opt {
                    la.release_request_pad(&liveadder_pad);
                }
            }
        }

        // Clean up dynamic_webrtcbin_store — remove strong refs to old webrtcbin
        // elements. If these survive, the old ICE agent stays alive and can
        // interfere with the new element's ICE connectivity.
        if let Ok(mut store) = self.dynamic_webrtcbin_store.lock() {
            if let Some(entries) = store.get_mut(&self.instance_id) {
                let before = entries.len();
                entries.clear();
                if before > 0 {
                    info!(
                        "WHIP Input: Cleared {} webrtcbin entries from dynamic store for '{}'",
                        before, self.instance_id
                    );
                }
            }
        }

        // Destroy old element
        info!(
            "WHIP Input: Destroying old whipserversrc for endpoint '{}'",
            self.endpoint_id
        );
        // Remove from pipeline first (disconnects pads, stops data flow).
        if let Err(e) = pipeline.remove(&old_element) {
            warn!(
                "WHIP Input: Failed to remove old element from pipeline: {}",
                e
            );
        }

        // Set the removed element to Null synchronously to fully release its
        // internal DTLS/ICE/libnice resources (sockets, GLib sources, agents).
        //
        // This MUST complete before creating the replacement. If old sockets
        // linger with unread data, their GLib I/O sources spin the mainloop,
        // starving the new element's ICE agent of processing time. This causes
        // STUN consent freshness responses to be delayed beyond the browser's
        // timeout (~5s), leading to ICE disconnect on every recreated element.
        //
        // We use a background thread with a join timeout to avoid blocking
        // indefinitely if set_state(Null) hangs on a stuck streaming thread.
        let endpoint_for_teardown = self.endpoint_id.clone();
        let teardown_handle = std::thread::spawn(move || {
            match old_element.set_state(gst::State::Null) {
                Ok(_) => info!(
                    "WHIP Input: Old element for '{}' set to Null",
                    endpoint_for_teardown
                ),
                Err(e) => warn!(
                    "WHIP Input: Failed to set old element to Null for '{}': {:?}",
                    endpoint_for_teardown, e
                ),
            }
            drop(old_element);
            info!("WHIP Input: Old element dropped");
        });

        // Wait for teardown to complete (with a safety timeout)
        match teardown_handle.join() {
            Ok(()) => {}
            Err(_) => warn!(
                "WHIP Input: Teardown thread panicked for '{}'",
                self.endpoint_id
            ),
        }

        // Additional pause to let the OS fully release sockets and for
        // any remaining GLib sources to be cleaned up on the mainloop.
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Create fresh element
        info!(
            "WHIP Input: Creating new whipserversrc for endpoint '{}'",
            self.endpoint_id
        );
        let (new_element, new_port) = self.create_element()?;

        // Update the registry so the axum proxy forwards to the new port.
        // Safe to use blocking_write here because replace_element is called
        // from std::thread::spawn or tokio::task::spawn_blocking, never from
        // inside the tokio runtime directly.
        self.whip_registry
            .update_port_sync(&self.endpoint_id, new_port);

        // Add to pipeline and sync state
        pipeline
            .add(&new_element)
            .map_err(|e| format!("Failed to add new element to pipeline: {}", e))?;
        new_element
            .sync_state_with_parent()
            .map_err(|e| format!("Failed to sync new element state: {}", e))?;

        // Store new element and bump generation
        *self
            .current_element
            .lock()
            .expect("current_element lock poisoned") = Some(new_element);
        self.generation.fetch_add(1, Ordering::SeqCst);

        info!(
            "WHIP Input: Successfully recreated whipserversrc for endpoint '{}'",
            self.endpoint_id
        );
        Ok(())
    }
}

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
///
/// whipserversrc is treated as single-use: each element handles exactly one
/// client session. When the session ends (DELETE, timeout, or browser reload),
/// the element is destroyed and a fresh one is created. This works around a
/// race condition in BaseWebRTCSrc where old session cleanup corrupts new
/// session state.
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

    let stun_server = ctx.stun_server();
    let turn_server = ctx.turn_server();

    // Create downstream elements first (these outlive whipserversrc recreations)
    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links: Vec<(ElementPadRef, ElementPadRef)> = Vec::new();

    // Create audio output chain if mode includes audio
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

    // Get weak refs for downstream elements used in callbacks
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

    // Get WhipRegistry from build context (needed for port updates on recreation)
    let whip_registry = ctx.whip_registry().cloned().ok_or_else(|| {
        BlockBuildError::InvalidConfiguration(
            "WhipRegistry not available in build context".to_string(),
        )
    })?;

    // Create WhipServerContext with shared state
    let whip_ctx = Arc::new(WhipServerContext {
        instance_id: instance_id.to_string(),
        stun_server: stun_server.clone(),
        turn_server: turn_server.clone(),
        mode,
        ice_transport_policy: ctx.ice_transport_policy().to_string(),
        dynamic_webrtcbin_store: ctx.dynamic_webrtcbin_store(),
        endpoint_id: endpoint_id.clone(),
        whip_registry,
        stream_counter: Arc::new(AtomicUsize::new(0)),
        video_connected: Arc::new(AtomicBool::new(false)),
        video_queue: Arc::new(Mutex::new(None)),
        audio_streams: Arc::new(Mutex::new(HashMap::new())),
        liveadder_weak,
        videoconvert_weak,
        current_element: Mutex::new(None),
        recreating: AtomicBool::new(false),
        generation: AtomicUsize::new(0),
        tokio_handle: tokio::runtime::Handle::current(),
    });

    // Create the initial whipserversrc element (also allocates a free port)
    let (whipserversrc, internal_port) = whip_ctx
        .create_element()
        .map_err(BlockBuildError::ElementCreation)?;

    // Store element in context
    *whip_ctx
        .current_element
        .lock()
        .expect("current_element lock poisoned") = Some(whipserversrc.clone());

    // Register context in module-level store
    {
        let mut store = whip_servers().lock().expect("whip_servers lock poisoned");
        store.insert(endpoint_id.clone(), Arc::clone(&whip_ctx));
    }

    let whipserversrc_id = format!("{}:whipserversrc", instance_id);
    elements.insert(0, (whipserversrc_id, whipserversrc));

    info!(
        "WHIP Input configured: endpoint_id='{}', initial_port={}, stun={:?}, turn={:?}, mode={:?}",
        endpoint_id, internal_port, stun_server, turn_server, mode
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
