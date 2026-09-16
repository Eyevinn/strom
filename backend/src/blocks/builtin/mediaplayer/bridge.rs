//! Internal pipeline creation and appsink→appsrc bridge.
//!
//! The internal pipeline runs `uridecodebin` (decode mode) or `urisourcebin` (passthrough
//! mode) with `clocksync` for real-time pacing. Appsink callbacks push samples to the
//! corresponding appsrc in the main pipeline.

use super::state::MediaPlayerState;
use crate::blocks::BlockBuildError;
use crate::events::EventBroadcaster;
use crate::gst::rtp_hdrext;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use strom_types::{FlowId, StromEvent};
use tracing::{debug, error, info, warn};

/// Create the internal pipeline for decode mode.
///
/// Pipeline: `uridecodebin` → (pad-added) → `clocksync` → `appsink`
/// The appsink callbacks push samples to the corresponding appsrc in the main pipeline.
pub fn create_decode_pipeline(
    instance_id: &str,
    state: &Arc<MediaPlayerState>,
    initial_uri: Option<&str>,
) -> Result<gst::Pipeline, BlockBuildError> {
    let pipeline_name = format!("mediaplayer-internal-{}", instance_id);
    let pipeline = gst::Pipeline::builder().name(&pipeline_name).build();

    let source_id = format!("{}_uridecodebin", instance_id);
    let source = gst::ElementFactory::make("uridecodebin")
        .name(&source_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("uridecodebin: {}", e)))?;

    if let Some(uri) = initial_uri {
        source.set_property("uri", uri);
    }

    // Store weak ref to source element in state
    state.source_element.set(Some(&source));

    pipeline
        .add(&source)
        .map_err(|e| BlockBuildError::ElementCreation(format!("add uridecodebin: {}", e)))?;

    // Connect pad-added to dynamically create clocksync → appsink chains
    let pipeline_weak = pipeline.downgrade();
    let state_weak = Arc::downgrade(state);
    let instance_id_owned = instance_id.to_string();
    let sync = state.sync;

    source.connect_pad_added(move |_src, pad| {
        let pad_name = pad.name();
        debug!("Media Player: Internal pad added: {}", pad_name);

        let pipeline = match pipeline_weak.upgrade() {
            Some(p) => p,
            None => return,
        };
        let state = match state_weak.upgrade() {
            Some(s) => s,
            None => return,
        };

        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        let caps_name = caps
            .as_ref()
            .and_then(|c| c.structure(0))
            .map(|s| s.name().to_string());

        let is_video = caps_name
            .as_ref()
            .map(|n| n.starts_with("video/"))
            .unwrap_or(false);
        let is_audio = caps_name
            .as_ref()
            .map(|n| n.starts_with("audio/"))
            .unwrap_or(false);

        if is_video && !state.video_linked.load(Ordering::SeqCst) {
            if let Some(ref appsrc) = state.video_appsrc {
                if let Err(e) = link_pad_through_clocksync(
                    &pipeline,
                    pad,
                    appsrc,
                    &state,
                    &format!("{}_clocksync_video", instance_id_owned),
                    &format!("{}_appsink_video", instance_id_owned),
                    sync,
                    "video",
                ) {
                    error!("Media Player: Failed to link video chain: {}", e);
                } else {
                    state.video_linked.store(true, Ordering::SeqCst);
                    info!(
                        "Media Player {}: Linked internal video chain",
                        instance_id_owned
                    );
                }
            }
        } else if is_audio && !state.audio_linked.load(Ordering::SeqCst) {
            if let Some(ref appsrc) = state.audio_appsrc {
                if let Err(e) = link_pad_through_clocksync(
                    &pipeline,
                    pad,
                    appsrc,
                    &state,
                    &format!("{}_clocksync_audio", instance_id_owned),
                    &format!("{}_appsink_audio", instance_id_owned),
                    sync,
                    "audio",
                ) {
                    error!("Media Player: Failed to link audio chain: {}", e);
                } else {
                    state.audio_linked.store(true, Ordering::SeqCst);
                    info!(
                        "Media Player {}: Linked internal audio chain",
                        instance_id_owned
                    );
                }
            }
        }
    });

    // An `rtsp://` URI makes the source bin autoplug an RTP depayloader, so this
    // internal pipeline needs the same gstreamer#5057 workaround as the main one.
    rtp_hdrext::install(&pipeline);

    Ok(pipeline)
}

/// Create the internal pipeline for passthrough mode.
///
/// Pipeline: `urisourcebin(parse-streams=true)` → (pad-added) → `clocksync` → `appsink`
pub fn create_passthrough_pipeline(
    instance_id: &str,
    state: &Arc<MediaPlayerState>,
    initial_uri: Option<&str>,
) -> Result<gst::Pipeline, BlockBuildError> {
    let pipeline_name = format!("mediaplayer-internal-{}", instance_id);
    let pipeline = gst::Pipeline::builder().name(&pipeline_name).build();

    let source_id = format!("{}_urisourcebin", instance_id);
    let source = gst::ElementFactory::make("urisourcebin")
        .name(&source_id)
        .property("parse-streams", true)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("urisourcebin: {}", e)))?;

    if let Some(uri) = initial_uri {
        source.set_property("uri", uri);
    }

    // Store weak ref to source element in state
    state.source_element.set(Some(&source));

    pipeline
        .add(&source)
        .map_err(|e| BlockBuildError::ElementCreation(format!("add urisourcebin: {}", e)))?;

    // Connect pad-added for passthrough streams
    let pipeline_weak = pipeline.downgrade();
    let state_weak = Arc::downgrade(state);
    let instance_id_owned = instance_id.to_string();
    let sync = state.sync;

    source.connect_pad_added(move |_src, pad| {
        let pad_name = pad.name();
        debug!(
            "Media Player {}: urisourcebin pad added: {}",
            instance_id_owned, pad_name
        );

        if pad.direction() != gst::PadDirection::Src {
            return;
        }

        let pipeline = match pipeline_weak.upgrade() {
            Some(p) => p,
            None => return,
        };
        let state = match state_weak.upgrade() {
            Some(s) => s,
            None => return,
        };

        let caps = pad.current_caps().or_else(|| {
            let query_caps = pad.query_caps(None);
            if !query_caps.is_any() && !query_caps.is_empty() {
                Some(query_caps)
            } else {
                None
            }
        });

        let caps_name = caps
            .as_ref()
            .and_then(|c| c.structure(0))
            .map(|s| s.name().to_string());

        debug!(
            "Media Player {}: Pad {} caps: {:?}",
            instance_id_owned, pad_name, caps_name
        );

        let is_video = caps_name
            .as_ref()
            .map(|n| n.starts_with("video/"))
            .unwrap_or(false);
        let is_audio = caps_name
            .as_ref()
            .map(|n| n.starts_with("audio/"))
            .unwrap_or(false);

        if is_video && !state.video_linked.load(Ordering::SeqCst) {
            if let Some(ref appsrc) = state.video_appsrc {
                if let Err(e) = link_pad_through_clocksync(
                    &pipeline,
                    pad,
                    appsrc,
                    &state,
                    &format!("{}_clocksync_video", instance_id_owned),
                    &format!("{}_appsink_video", instance_id_owned),
                    sync,
                    "video",
                ) {
                    error!("Media Player: Failed to link video chain: {}", e);
                } else {
                    state.video_linked.store(true, Ordering::SeqCst);
                    info!(
                        "Media Player {}: Linked internal video chain (passthrough)",
                        instance_id_owned
                    );
                }
            }
        } else if is_audio && !state.audio_linked.load(Ordering::SeqCst) {
            if let Some(ref appsrc) = state.audio_appsrc {
                if let Err(e) = link_pad_through_clocksync(
                    &pipeline,
                    pad,
                    appsrc,
                    &state,
                    &format!("{}_clocksync_audio", instance_id_owned),
                    &format!("{}_appsink_audio", instance_id_owned),
                    sync,
                    "audio",
                ) {
                    error!("Media Player: Failed to link audio chain: {}", e);
                } else {
                    state.audio_linked.store(true, Ordering::SeqCst);
                    info!(
                        "Media Player {}: Linked internal audio chain (passthrough)",
                        instance_id_owned
                    );
                }
            }
        } else if !is_video && !is_audio {
            // Heuristic: try video first, then audio
            debug!(
                "Media Player {}: Pad {} media type unknown, trying heuristic linking",
                instance_id_owned, pad_name
            );

            if !state.video_linked.load(Ordering::SeqCst) {
                if let Some(ref appsrc) = state.video_appsrc {
                    if link_pad_through_clocksync(
                        &pipeline,
                        pad,
                        appsrc,
                        &state,
                        &format!("{}_clocksync_video", instance_id_owned),
                        &format!("{}_appsink_video", instance_id_owned),
                        sync,
                        "video",
                    )
                    .is_ok()
                    {
                        state.video_linked.store(true, Ordering::SeqCst);
                        info!(
                            "Media Player {}: Linked unknown pad {} to video (heuristic)",
                            instance_id_owned, pad_name
                        );
                        return;
                    }
                }
            }

            if !state.audio_linked.load(Ordering::SeqCst) {
                if let Some(ref appsrc) = state.audio_appsrc {
                    if link_pad_through_clocksync(
                        &pipeline,
                        pad,
                        appsrc,
                        &state,
                        &format!("{}_clocksync_audio", instance_id_owned),
                        &format!("{}_appsink_audio", instance_id_owned),
                        sync,
                        "audio",
                    )
                    .is_ok()
                    {
                        state.audio_linked.store(true, Ordering::SeqCst);
                        info!(
                            "Media Player {}: Linked unknown pad {} to audio (heuristic)",
                            instance_id_owned, pad_name
                        );
                        return;
                    }
                }
            }

            debug!(
                "Media Player {}: Could not link pad {} (video_linked={}, audio_linked={})",
                instance_id_owned,
                pad_name,
                state.video_linked.load(Ordering::SeqCst),
                state.audio_linked.load(Ordering::SeqCst)
            );
        }
    });

    // An `rtsp://` URI makes the source bin autoplug an RTP depayloader, so this
    // internal pipeline needs the same gstreamer#5057 workaround as the main one.
    rtp_hdrext::install(&pipeline);

    Ok(pipeline)
}

/// Link a dynamic pad through clocksync → appsink, with appsink bridging to the given appsrc.
///
/// The bridge computes a timestamp offset from the first buffer:
///   `offset = main_running_time - buffer_pts`
/// and applies it to all subsequent buffers so PTS aligns with the main pipeline clock.
/// The offset is shared (via `ts_offset`) between audio and video streams for A/V sync.
#[allow(clippy::too_many_arguments)]
fn link_pad_through_clocksync(
    pipeline: &gst::Pipeline,
    src_pad: &gst::Pad,
    appsrc: &gst_app::AppSrc,
    state: &Arc<MediaPlayerState>,
    clocksync_name: &str,
    appsink_name: &str,
    sync: bool,
    media_type: &str,
) -> Result<(), String> {
    let clocksync = gst::ElementFactory::make("clocksync")
        .name(clocksync_name)
        .property("sync", sync)
        .build()
        .map_err(|e| format!("clocksync: {}", e))?;

    let appsink = gst_app::AppSink::builder()
        .name(appsink_name)
        .sync(false)
        .build();

    let appsink_element = appsink.upcast_ref::<gst::Element>();

    pipeline
        .add_many([&clocksync, appsink_element])
        .map_err(|e| format!("add elements: {}", e))?;

    // Link downstream first: clocksync → appsink (no data flows yet)
    clocksync
        .link(appsink_element)
        .map_err(|e| format!("link clocksync to appsink: {:?}", e))?;

    // Start elements BEFORE linking upstream. urisourcebin's internal
    // multiqueue pushes buffered data the instant the upstream pad is
    // linked — elements must be ready to accept it.
    //
    // clocksync must reach PLAYING explicitly: its internal flushing
    // flag is only cleared during PAUSED→PLAYING. sync_state_with_parent
    // would leave it in PAUSED (matching the pipeline during preroll),
    // which silently drops all buffers via FLUSHING.
    clocksync
        .set_state(gst::State::Playing)
        .map_err(|e| format!("set clocksync to Playing: {:?}", e))?;
    appsink_element
        .sync_state_with_parent()
        .map_err(|e| format!("sync appsink state: {:?}", e))?;

    // Set up bridge callback: appsink → appsrc with timestamp offset
    let appsrc_weak = appsrc.downgrade();
    let media_type_owned = media_type.to_string();
    let ts_offset = Arc::clone(&state.ts_offset);
    let main_pipeline_weak = state.main_pipeline.clone();
    let pushed_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let appsrc = match appsrc_weak.upgrade() {
                    Some(a) => a,
                    None => {
                        debug!(
                            "Media Player bridge: {} appsrc gone, stopping",
                            media_type_owned
                        );
                        return Err(gst::FlowError::Eos);
                    }
                };

                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let pts = buffer.pts();

                // Compute or reuse timestamp offset
                let offset_ns = {
                    let current = ts_offset.load(Ordering::Relaxed);
                    if current != i64::MIN {
                        current
                    } else if let (Some(pts_val), Some(main_pipe)) =
                        (pts, main_pipeline_weak.upgrade())
                    {
                        let clock = main_pipe.clock();
                        let base_time = main_pipe.base_time();
                        if let (Some(clock), Some(base_time)) = (clock, base_time) {
                            let running = clock.time().saturating_sub(base_time);
                            let offset = running.nseconds() as i64 - pts_val.nseconds() as i64;
                            ts_offset.store(offset, Ordering::Relaxed);
                            offset
                        } else {
                            0
                        }
                    } else {
                        0
                    }
                };

                // Build the sample to push, applying timestamp offset if needed
                let push_result = if offset_ns != 0 {
                    if let Some(pts_val) = pts {
                        let adjusted = (pts_val.nseconds() as i64 + offset_ns).max(0) as u64;
                        let mut new_buf = buffer.copy();
                        {
                            let buf_ref = new_buf.get_mut().unwrap();
                            buf_ref.set_pts(gst::ClockTime::from_nseconds(adjusted));
                            if let Some(dts) = buffer.dts() {
                                let adj_dts = (dts.nseconds() as i64 + offset_ns).max(0) as u64;
                                buf_ref.set_dts(gst::ClockTime::from_nseconds(adj_dts));
                            }
                        }
                        let owned_caps = sample.caps().map(|c| c.to_owned());
                        let mut builder = gst::Sample::builder().buffer(&new_buf);
                        if let Some(ref caps) = owned_caps {
                            builder = builder.caps(caps);
                        }
                        appsrc.push_sample(&builder.build())
                    } else {
                        appsrc.push_sample(&sample)
                    }
                } else {
                    appsrc.push_sample(&sample)
                };

                // Don't kill the appsink on transient errors (e.g. FLUSHING while
                // the main pipeline is still starting). Drop the sample and retry
                // on the next one — the appsrc will accept data once it's ready.
                match push_result {
                    Ok(_) => {
                        if pushed_count.fetch_add(1, Ordering::Relaxed) == 0 {
                            info!(
                                "Media Player bridge: {} first sample delivered, pts={:?}",
                                media_type_owned, pts
                            );
                        }
                        Ok(gst::FlowSuccess::Ok)
                    }
                    Err(e) => {
                        if pushed_count.load(Ordering::Relaxed) == 0 {
                            debug!(
                                "Media Player bridge: {} push_sample failed ({:?}), waiting for appsrc",
                                media_type_owned, e
                            );
                        }
                        Ok(gst::FlowSuccess::Ok)
                    }
                }
            })
            .build(),
    );

    // Link upstream LAST: this triggers data flow from urisourcebin's
    // multiqueue. All downstream elements are now started and the
    // callback is installed, so the first buffer will be handled correctly.
    let clocksync_sink = clocksync
        .static_pad("sink")
        .ok_or("clocksync has no sink pad")?;
    src_pad
        .link(&clocksync_sink)
        .map_err(|e| format!("link pad to clocksync: {:?}", e))?;

    debug!(
        "Media Player: Created {} bridge chain: clocksync(sync={}) -> appsink -> appsrc",
        media_type, sync
    );

    Ok(())
}

/// Watch the internal pipeline's bus for EOS, errors, and state changes.
///
/// EOS triggers advancing to the next file instead of propagating downstream.
/// State changes are broadcast as `MediaPlayerStateChanged` events.
pub fn watch_internal_bus(
    pipeline: &gst::Pipeline,
    state: Arc<MediaPlayerState>,
    flow_id: FlowId,
    block_id: String,
    events: EventBroadcaster,
) {
    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            warn!("Media Player {}: Internal pipeline has no bus", block_id);
            return;
        }
    };

    bus.add_signal_watch();

    let state_for_bus = Arc::clone(&state);
    let block_id_for_bus = block_id.clone();
    let block_id_for_watch = block_id.clone();

    let handler = bus.connect_message(None, move |_bus, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(_) => {
                // Ignore EOS during file switch — the Ready→Playing transition
                // can produce a spurious EOS from the old stream.
                if state_for_bus
                    .switching_file
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    debug!(
                        "Media Player {}: Ignoring EOS during file switch",
                        block_id_for_bus
                    );
                    return;
                }

                info!("Media Player {}: Internal pipeline EOS", block_id_for_bus);

                match state_for_bus.next() {
                    Ok(_) => {
                        info!("Media Player {}: Advanced to next file", block_id_for_bus);
                    }
                    Err(e) => {
                        info!("Media Player {}: End of playlist: {}", block_id_for_bus, e);
                        events.broadcast(StromEvent::MediaPlayerStateChanged {
                            flow_id,
                            block_id: block_id_for_bus.clone(),
                            state: strom_types::mediaplayer::PlayerState::Stopped,
                            current_file: None,
                        });
                    }
                }
            }
            MessageView::StateChanged(state_msg) => {
                let is_pipeline = msg
                    .src()
                    .map(|s| s.type_() == gst::Pipeline::static_type())
                    .unwrap_or(false);
                if is_pipeline {
                    let new_state = state_msg.current();
                    let player_state = match new_state {
                        gst::State::Playing => strom_types::mediaplayer::PlayerState::Playing,
                        gst::State::Paused => strom_types::mediaplayer::PlayerState::Paused,
                        _ => strom_types::mediaplayer::PlayerState::Stopped,
                    };

                    events.broadcast(StromEvent::MediaPlayerStateChanged {
                        flow_id,
                        block_id: block_id.clone(),
                        state: player_state,
                        current_file: state_for_bus.current_file(),
                    });
                }
            }
            MessageView::Error(err) => {
                error!(
                    "Media Player {}: Internal pipeline error: {} ({:?})",
                    block_id,
                    err.error(),
                    err.debug()
                );
            }
            _ => {}
        }
    });

    // The closure just took an `Arc` on the state that owns this pipeline, so
    // the state can only ever be dropped if someone disconnects the handler.
    // `shutdown()` is that someone, and this id is how it finds it. A second
    // watch on the same bus would strand the first, so retire it here.
    let previous = match state.bus_watch.lock() {
        Ok(mut guard) => guard.replace(handler),
        Err(poisoned) => poisoned.into_inner().replace(handler),
    };
    if let Some(previous) = previous {
        warn!(
            "Media Player {}: Replacing an existing internal bus watch",
            block_id_for_watch
        );
        bus.disconnect(previous);
        bus.remove_signal_watch();
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::Playlist;
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64};
    use std::sync::{Mutex, RwLock};

    /// The bare minimum for the two pipeline constructors: they read `sync` and
    /// stash a weak ref to the source element, nothing else, before returning.
    fn test_state() -> Arc<MediaPlayerState> {
        Arc::new(MediaPlayerState {
            instance_id: uuid::Uuid::new_v4(),
            source_element: gst::glib::WeakRef::new(),
            internal_pipeline: RwLock::new(None),
            video_appsrc: None,
            audio_appsrc: None,
            playlist: RwLock::new(Playlist {
                files: Vec::new(),
                current_index: 0,
            }),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(false),
            block_id: "test".to_string(),
            flow_id: uuid::Uuid::new_v4(),
            switching_file: AtomicBool::new(false),
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode: true,
            sync: true,
            media_path: std::env::temp_dir(),
            ts_offset: Arc::new(AtomicI64::new(i64::MIN)),
            main_pipeline: gst::glib::WeakRef::new(),
            bus_watch: Mutex::new(None),
        })
    }

    /// An `rtsp://` URI makes `uridecodebin`/`urisourcebin` autoplug an RTP
    /// depayloader inside this pipeline, at which point it needs the
    /// gstreamer#5057 workaround exactly as much as the main pipeline does —
    /// the abort takes down the whole process, not just this flow.
    ///
    /// CI cannot serve an RTSP stream, so the test stands in for the autoplugged
    /// element by adding a depayloader to a nested bin after construction. That
    /// is the same path `deep-element-added` sees, and it fails if the
    /// `rtp_hdrext::install()` call is removed from the constructor.
    fn assert_hdrext_disabled_on_late_depayloader(pipeline: &gst::Pipeline) {
        let inner = gst::Bin::builder().name("inner").build();
        pipeline.add(&inner).unwrap();

        let depay = gst::ElementFactory::make("rtph264depay")
            .build()
            .expect("rtph264depay is in gstreamer1.0-plugins-good, installed in CI")
            .downcast::<gstreamer_rtp::RTPBaseDepayload>()
            .expect("rtph264depay derives from GstRTPBaseDepayload");
        inner.add(&depay).unwrap();

        assert_eq!(
            rtp_hdrext::is_enabled(&depay),
            Some(false),
            "a depayloader autoplugged in the Media Player's internal pipeline still \
             has RTP header extension aggregation enabled — an interrupted H264 \
             fragmentation unit from an rtsp:// source will abort the whole process \
             (gstreamer#5057)"
        );
    }

    #[test]
    fn decode_pipeline_disables_hdrext_aggregation() {
        let _ = gst::init();
        if !rtp_hdrext::is_supported() {
            // GStreamer < 1.24: aggregation does not exist, so neither does the bug.
            return;
        }
        let state = test_state();
        let pipeline = create_decode_pipeline("test", &state, None).unwrap();
        assert_hdrext_disabled_on_late_depayloader(&pipeline);
    }

    #[test]
    fn passthrough_pipeline_disables_hdrext_aggregation() {
        let _ = gst::init();
        if !rtp_hdrext::is_supported() {
            return;
        }
        let state = test_state();
        let pipeline = create_passthrough_pipeline("test", &state, None).unwrap();
        assert_hdrext_disabled_on_late_depayloader(&pipeline);
    }
}
