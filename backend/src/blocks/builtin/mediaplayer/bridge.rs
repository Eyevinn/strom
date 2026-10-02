//! Internal pipeline creation and appsink→appsrc bridge.
//!
//! The internal pipeline runs `uridecodebin` (decode mode) or `urisourcebin` (passthrough
//! mode) with `clocksync` for real-time pacing. Appsink callbacks push samples to the
//! corresponding appsrc in the main pipeline.

use super::state::MediaPlayerState;
use super::timing;
use crate::blocks::BlockBuildError;
use crate::events::EventBroadcaster;
use crate::gst::rtp_hdrext;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::Arc;
use strom_types::{FlowId, StromEvent};
use tracing::{debug, error, info, warn};

/// Which element decodes in decode mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoder {
    /// `uridecodebin`. HLS and DASH go through the old `hlsdemux` and
    /// `dashdemux`, which rebuild the whole decoder chain on every quality
    /// switch - a hiccup each time.
    Classic,
    /// `uridecodebin3`, with `hlsdemux2` and `dashdemux2`: quality switches
    /// without a rebuild, and buffering of its own. It decodes only the
    /// streams the block has outputs for.
    Decodebin3,
}

impl Decoder {
    /// The `decoder` property's values.
    pub const CLASSIC: &'static str = "classic";
    pub const DECODEBIN3: &'static str = "decodebin3";

    pub fn from_property(value: Option<&str>) -> Self {
        match value {
            Some(Self::DECODEBIN3) => Decoder::Decodebin3,
            _ => Decoder::Classic,
        }
    }

    fn factory(self) -> &'static str {
        match self {
            Decoder::Classic => "uridecodebin",
            Decoder::Decodebin3 => "uridecodebin3",
        }
    }
}

/// Create the internal pipeline for decode mode.
///
/// Pipeline: `uridecodebin` or `uridecodebin3` → (pad-added) → `clocksync` → `appsink`
/// The appsink callbacks push samples to the corresponding appsrc in the main pipeline.
pub fn create_decode_pipeline(
    instance_id: &str,
    state: &Arc<MediaPlayerState>,
    initial_uri: Option<&str>,
    decoder: Decoder,
) -> Result<gst::Pipeline, BlockBuildError> {
    let pipeline_name = format!("mediaplayer-internal-{}", instance_id);
    let pipeline = gst::Pipeline::builder().name(&pipeline_name).build();

    let factory = decoder.factory();
    let source_id = format!("{}_{}", instance_id, factory);
    let source = gst::ElementFactory::make(factory)
        .name(&source_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", factory, e)))?;

    if decoder == Decoder::Decodebin3 {
        // decodebin3 decodes one stream of each type unless told otherwise.
        // Pick as many as the block has outputs for, in the order the
        // collection lists them, and nothing else - a stream nobody plays is
        // not worth decoding.
        let (videos, audios) = (state.video_appsrcs.len(), state.audio_appsrcs.len());
        source.connect("select-stream", false, move |args| {
            let collection = args[1].get::<gst::StreamCollection>().ok()?;
            let stream = args[2].get::<gst::Stream>().ok()?;
            Some(select_stream(&collection, &stream, videos, audios).to_value())
        });
    }

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

        let kind = TrackKind::of_pad(pad);
        route_pad(&pipeline, pad, &state, &instance_id_owned, sync, kind, "");
    });

    free_slot_on_pad_removed(&source, state);

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
        debug!(
            "Media Player {}: Pad {} caps: {:?}",
            instance_id_owned,
            pad_name,
            caps.as_ref()
                .and_then(|c| c.structure(0))
                .map(|s| s.name().to_string())
        );

        match caps.as_ref().map(|c| TrackKind::from_caps(Some(c))) {
            Some(kind) => route_pad(
                &pipeline,
                pad,
                &state,
                &instance_id_owned,
                sync,
                kind,
                " (passthrough)",
            ),
            // No caps yet: a stream whose type is not known until data flows.
            // Offer it to a free video slot, then a free audio slot, the way
            // the block always has; with neither free, discard it.
            None => {
                for kind in [TrackKind::Video, TrackKind::Audio] {
                    if try_slot(&pipeline, pad, &state, &instance_id_owned, sync, kind) {
                        info!(
                            "Media Player {}: Linked untyped pad {} to {} (heuristic)",
                            instance_id_owned,
                            pad_name,
                            kind.name()
                        );
                        return;
                    }
                }
                discard_pad(&pipeline, pad, &instance_id_owned, "its type is not known");
            }
        }
    });

    free_slot_on_pad_removed(&source, state);

    // An `rtsp://` URI makes the source bin autoplug an RTP depayloader, so this
    // internal pipeline needs the same gstreamer#5057 workaround as the main one.
    rtp_hdrext::install(&pipeline);

    Ok(pipeline)
}

/// Give a slot back when the pad holding it goes away, so the stream that
/// replaces it - an HLS variant switch swaps every stream pad - takes the same
/// output instead of being discarded as one too many.
fn free_slot_on_pad_removed(source: &gst::Element, state: &Arc<MediaPlayerState>) {
    let state_weak = Arc::downgrade(state);
    source.connect_pad_removed(move |_src, pad| {
        if let Some(state) = state_weak.upgrade() {
            debug!("Media Player: pad {} removed, freeing its slot", pad.name());
            state.free_slot_of(&pad.name());
        }
    });
}

/// Which output a stream from the source element can go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackKind {
    Video,
    Audio,
    /// Subtitles, metadata and anything else this block has no output for.
    Other,
}

impl TrackKind {
    /// What a decoded pad carries: its stream's type where it has one
    /// (decodebin3 pads may have no caps yet), else its caps.
    fn of_pad(pad: &gst::Pad) -> Self {
        if let Some(stream) = pad.stream() {
            let t = stream.stream_type();
            if t.contains(gst::StreamType::VIDEO) {
                return TrackKind::Video;
            }
            if t.contains(gst::StreamType::AUDIO) {
                return TrackKind::Audio;
            }
        }
        let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
        TrackKind::from_caps(caps.as_ref())
    }

    fn from_caps(caps: Option<&gst::Caps>) -> Self {
        match caps.and_then(|c| c.structure(0)).map(|s| s.name()) {
            Some(n) if n.starts_with("video/") => TrackKind::Video,
            Some(n) if n.starts_with("audio/") => TrackKind::Audio,
            _ => TrackKind::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
            TrackKind::Other => "other",
        }
    }
}

/// decodebin3's `select-stream` answer: 1 for the first `videos` video and
/// `audios` audio streams of `collection`, 0 for every other stream.
fn select_stream(
    collection: &gst::StreamCollection,
    stream: &gst::Stream,
    videos: usize,
    audios: usize,
) -> i32 {
    let kind_of = |s: &gst::Stream| {
        let t = s.stream_type();
        if t.contains(gst::StreamType::VIDEO) {
            TrackKind::Video
        } else if t.contains(gst::StreamType::AUDIO) {
            TrackKind::Audio
        } else {
            TrackKind::Other
        }
    };
    let kind = kind_of(stream);
    let room = match kind {
        TrackKind::Video => videos,
        TrackKind::Audio => audios,
        TrackKind::Other => 0,
    };
    let before = collection
        .iter()
        .take_while(|s| s.stream_id() != stream.stream_id())
        .filter(|s| kind_of(s) == kind)
        .count();
    i32::from(before < room)
}

/// Send a new stream to the next free output of its kind, or discard it.
///
/// A file can carry more tracks than the block has outputs for - a second
/// audio language, subtitles - and a stream pad left unlinked stops the whole
/// source with `not-linked`, so every pad is either bridged or discarded.
fn route_pad(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    state: &Arc<MediaPlayerState>,
    instance_id: &str,
    sync: bool,
    kind: TrackKind,
    mode: &str,
) {
    if kind == TrackKind::Other {
        discard_pad(pipeline, pad, instance_id, "it is not audio or video");
        return;
    }
    if !try_slot(pipeline, pad, state, instance_id, sync, kind) {
        let reason = format!(
            "every {} output is taken; raise num_{}_tracks to keep it",
            kind.name(),
            kind.name()
        );
        discard_pad(pipeline, pad, instance_id, &reason);
        return;
    }
    debug!(
        "Media Player {}: Linked internal {} chain{}",
        instance_id,
        kind.name(),
        mode
    );
}

/// Claim the next free slot of `kind` and bridge `pad` to it. Returns false,
/// with the slot given back, when there is no free slot or the link fails.
fn try_slot(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    state: &Arc<MediaPlayerState>,
    instance_id: &str,
    sync: bool,
    kind: TrackKind,
) -> bool {
    let (slots, appsrcs) = match kind {
        TrackKind::Video => (&state.video_slots, &state.video_appsrcs),
        TrackKind::Audio => (&state.audio_slots, &state.audio_appsrcs),
        TrackKind::Other => return false,
    };
    // pad-added can fire from several streaming threads at once, so the slot
    // is claimed under the lock. Not a per-buffer path.
    let slot = {
        let mut slots = slots.lock().unwrap_or_else(|p| p.into_inner());
        let Some(free) = slots.iter().position(Option::is_none) else {
            return false;
        };
        slots[free] = Some(pad.name().to_string());
        free
    };
    let Some(appsrc) = appsrcs.get(slot) else {
        state.free_slot_of(&pad.name());
        return false;
    };
    let sfx = super::builder::slot_suffix(slot);
    match link_pad_through_clocksync(
        pipeline,
        pad,
        appsrc,
        state,
        &format!("{}_clocksync_{}{}", instance_id, kind.name(), sfx),
        &format!("{}_appsink_{}{}", instance_id, kind.name(), sfx),
        sync,
        kind.name(),
    ) {
        Ok(()) => {
            info!(
                "Media Player {}: {} track {} -> {}_out{}",
                instance_id,
                kind.name(),
                slot,
                kind.name(),
                sfx
            );
            true
        }
        Err(e) => {
            error!(
                "Media Player {}: Failed to link {} chain: {}",
                instance_id,
                kind.name(),
                e
            );
            state.free_slot_of(&pad.name());
            false
        }
    }
}

/// Link a stream nobody will use to a fakesink, so it does not stop the source.
fn discard_pad(pipeline: &gst::Pipeline, pad: &gst::Pad, instance_id: &str, reason: &str) {
    let sink = match gst::ElementFactory::make("fakesink")
        .name(format!("{}_discard_{}", instance_id, pad.name()))
        .property("sync", false)
        .property("async", false)
        .build()
    {
        Ok(sink) => sink,
        Err(e) => {
            error!("Media Player {}: fakesink: {}", instance_id, e);
            return;
        }
    };
    if pipeline.add(&sink).is_err() {
        return;
    }
    let _ = sink.sync_state_with_parent();
    match sink.static_pad("sink").map(|sp| pad.link(&sp)) {
        Some(Ok(_)) => info!(
            "Media Player {}: Discarding track {}: {}",
            instance_id,
            pad.name(),
            reason
        ),
        _ => warn!(
            "Media Player {}: Could not discard track {}",
            instance_id,
            pad.name()
        ),
    }
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
    // The slot's chain is already there when an earlier pad held it - an HLS
    // variant switch replaces the stream pads mid-playback. The new pad takes
    // over the same chain, so the output carries on.
    if let Some(existing) = pipeline.by_name(clocksync_name) {
        let sink = existing
            .static_pad("sink")
            .ok_or("clocksync has no sink pad")?;
        if sink.is_linked() {
            return Err(format!("{} is still fed by another pad", clocksync_name));
        }
        return src_pad
            .link(&sink)
            .map(|_| ())
            .map_err(|e| format!("relink pad to clocksync: {:?}", e));
    }

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

    if sync {
        timing::arm(&clocksync, &state.timing, &state.main_pipeline);
    }

    // Set up bridge callback: appsink -> appsrc, restamped into the main
    // pipeline's running time.
    let appsrc_weak = appsrc.downgrade();
    let media_type_owned = media_type.to_string();
    let timing = Arc::clone(&state.timing);
    let main_pipeline_weak = state.main_pipeline.clone();
    let internal_pipeline_weak = pipeline.downgrade();
    let instance = clocksync_name
        .split("_clocksync_")
        .next()
        .unwrap_or_default()
        .to_string();
    let mut pushed: u64 = 0;
    // The main pipeline's clock and base time, read once instead of per buffer.
    let mut main_clock: Option<(gst::Clock, gst::ClockTime)> = None;

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
                let Some(pts) = buffer.pts() else {
                    let _ = appsrc.push_sample(&sample);
                    return Ok(gst::FlowSuccess::Ok);
                };
                let rt = sample
                    .segment()
                    .and_then(|s| s.downcast_ref::<gst::ClockTime>())
                    .and_then(|s| s.to_running_time(pts))
                    .unwrap_or(pts);

                if main_clock.is_none() {
                    main_clock = main_pipeline_weak
                        .upgrade()
                        .and_then(|p| Some((p.clock()?, p.base_time()?)));
                }
                // The main pipeline is not playing yet: the push would fail
                // anyway, so take no baseline from it.
                let Some((clock, base)) = main_clock.as_ref() else {
                    return Ok(gst::FlowSuccess::Ok);
                };
                let now = clock.time().saturating_sub(*base).nseconds() as i64;
                let placed = timing.place(rt.nseconds() as i64, now, sync);
                if let Some(lateness) = placed.resynced_after {
                    timing::log_resync(&instance, &media_type_owned, lateness, &timing);
                    if let (Some(internal), Some(offset)) =
                        (internal_pipeline_weak.upgrade(), timing.sync_offset())
                    {
                        timing::apply_sync_offset(&internal, offset);
                    }
                }

                // Move PTS and DTS by the same step, so their spacing holds.
                let step = placed.running_time.max(0) - pts.nseconds() as i64;
                let mut new_buf = buffer.copy();
                {
                    let buf_ref = new_buf.make_mut();
                    buf_ref.set_pts(gst::ClockTime::from_nseconds(placed.running_time.max(0) as u64));
                    if let Some(dts) = buffer.dts() {
                        let adj = (dts.nseconds() as i64 + step).max(0) as u64;
                        buf_ref.set_dts(gst::ClockTime::from_nseconds(adj));
                    }
                }
                let caps = sample.caps_owned();
                let mut builder = gst::Sample::builder().buffer(&new_buf);
                if let Some(caps) = caps.as_ref() {
                    builder = builder.caps(caps);
                }
                let push_result = appsrc.push_sample(&builder.build());

                // Don't kill the appsink on transient errors (e.g. FLUSHING while
                // the main pipeline is still starting). Drop the sample and retry
                // on the next one — the appsrc will accept data once it's ready.
                match push_result {
                    Ok(_) => {
                        if pushed == 0 {
                            info!(
                                "Media Player bridge: {} first sample delivered, pts={:?}",
                                media_type_owned, pts
                            );
                        }
                        pushed += 1;
                        Ok(gst::FlowSuccess::Ok)
                    }
                    Err(e) => {
                        if pushed == 0 {
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
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, RwLock};

    /// The bare minimum for the two pipeline constructors: they read `sync` and
    /// stash a weak ref to the source element, nothing else, before returning.
    fn test_state() -> Arc<MediaPlayerState> {
        Arc::new(MediaPlayerState {
            instance_id: uuid::Uuid::new_v4(),
            source_element: gst::glib::WeakRef::new(),
            internal_pipeline: RwLock::new(None),
            video_appsrcs: Vec::new(),
            audio_appsrcs: Vec::new(),
            playlist: RwLock::new(Playlist {
                files: Vec::new(),
                current_index: 0,
            }),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(false),
            block_id: "test".to_string(),
            flow_id: uuid::Uuid::new_v4(),
            switching_file: AtomicBool::new(false),
            video_slots: MediaPlayerState::free_slots(0),
            audio_slots: MediaPlayerState::free_slots(0),
            decode: true,
            sync: true,
            media_path: std::env::temp_dir(),
            timing: Arc::new(super::super::timing::Timing::new(0)),
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

    /// A Matroska file with one video track and two audio tracks, all raw so no
    /// decoder is needed, each about a second long: decodebin3 can let a 0.2 s
    /// track end before it ever exposes a pad for it. matroska and the test sources are in
    /// gstreamer1.0-plugins-good/-base, installed in CI.
    fn write_two_audio_track_file(dir: &std::path::Path) -> String {
        let path = dir.join("two-audio.mkv");
        let pipeline = gst::parse::launch(&format!(
            "matroskamux name=mux ! filesink location={} \
             videotestsrc num-buffers=15 ! video/x-raw,format=I420,width=64,height=48,framerate=15/1 ! mux. \
             audiotestsrc num-buffers=47 ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! mux. \
             audiotestsrc num-buffers=47 wave=silence ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! mux.",
            path.display()
        ))
        .expect("matroskamux, videotestsrc and audiotestsrc are installed in CI");
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(20),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("writing the test file finishes");
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{:?}", msg);
        pipeline.set_state(gst::State::Null).unwrap();
        format!("file://{}", path.display())
    }

    /// A state with `video` and `audio` slots, backed by appsrcs that are in no
    /// pipeline: pushes to them fail, which the bridge treats as "not ready
    /// yet", so the internal pipeline runs to its end on its own.
    fn state_with_slots(video: usize, audio: usize) -> Arc<MediaPlayerState> {
        let appsrcs = |n: usize| (0..n).map(|_| gst_app::AppSrc::builder().build()).collect();
        let mut state = Arc::try_unwrap(test_state()).ok().unwrap();
        state.video_appsrcs = appsrcs(video);
        state.audio_appsrcs = appsrcs(audio);
        state.video_slots = MediaPlayerState::free_slots(video);
        state.audio_slots = MediaPlayerState::free_slots(audio);
        state.sync = false;
        Arc::new(state)
    }

    /// Play `uri` through a Media Player internal pipeline and return the
    /// message it ends with: EOS when every track found a home, an error when
    /// one was left unlinked.
    fn play_to_end(pipeline: &gst::Pipeline) -> gst::Message {
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(20),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("the internal pipeline reaches EOS or an error");
        pipeline.set_state(gst::State::Null).unwrap();
        msg
    }

    /// The element a routed pad ended up linked to.
    fn peer_factory(pad: &gst::Pad) -> Option<String> {
        pad.peer()
            .and_then(|peer| peer.parent_element())
            .and_then(|el| el.factory())
            .map(|f| f.name().to_string())
    }

    /// A stream with nowhere to go - a second audio rendition, subtitles - used
    /// to be left unlinked. On SVT's live HLS that stopped the whole source
    /// with `not-linked` within a few frames. Every pad has to end up linked:
    /// to the bridge when there is a free output, to a fakesink otherwise.
    #[test]
    fn a_track_with_no_free_output_is_linked_to_a_fakesink() {
        let _ = gst::init();
        for (kind, caps) in [
            (TrackKind::Audio, "audio/x-raw"),
            (TrackKind::Video, "video/x-raw"),
            (TrackKind::Other, "text/x-raw"),
        ] {
            let pipeline = gst::Pipeline::new();
            let src = gst::ElementFactory::make("identity").build().unwrap();
            pipeline.add(&src).unwrap();
            let pad = src.static_pad("src").unwrap();
            // No output slots at all: every track is one too many.
            let state = state_with_slots(0, 0);

            route_pad(&pipeline, &pad, &state, "test", false, kind, "");

            assert!(pad.is_linked(), "a {} track was left unlinked", caps);
            assert_eq!(peer_factory(&pad).as_deref(), Some("fakesink"), "{}", caps);
        }
    }

    #[test]
    fn a_track_with_a_free_output_is_bridged_and_the_next_one_discarded() {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let state = state_with_slots(0, 1);
        let mut pads = Vec::new();
        for _ in 0..2 {
            let src = gst::ElementFactory::make("identity").build().unwrap();
            pipeline.add(&src).unwrap();
            let pad = src.static_pad("src").unwrap();
            route_pad(&pipeline, &pad, &state, "test", false, TrackKind::Audio, "");
            pads.push(pad);
        }
        assert_eq!(peer_factory(&pads[0]).as_deref(), Some("clocksync"));
        assert_eq!(peer_factory(&pads[1]).as_deref(), Some("fakesink"));
        let _ = pipeline.set_state(gst::State::Null);
    }

    /// A source pad on a bin, the way uridecodebin exposes its streams.
    fn ghost_src(bin: &gst::Bin, name: &str) -> gst::Pad {
        let inner = gst::ElementFactory::make("identity").build().unwrap();
        bin.add(&inner).unwrap();
        let pad = gst::GhostPad::builder_with_target(&inner.static_pad("src").unwrap())
            .unwrap()
            .name(name)
            .build();
        pad.set_active(true).unwrap();
        bin.add_pad(&pad).unwrap();
        pad.upcast()
    }

    /// An HLS variant switch removes every stream pad and exposes new ones. The
    /// slots used to stay taken, so after the first quality switch the new
    /// video and audio were discarded as one too many and the outputs went
    /// silent. The pad that replaces one has to take over the same output.
    #[test]
    fn a_stream_that_replaces_a_removed_one_takes_over_its_output() {
        let _ = gst::init();
        let pipeline = gst::Pipeline::new();
        let source = gst::Bin::with_name("source");
        pipeline.add(&source).unwrap();
        let state = state_with_slots(0, 1);
        free_slot_on_pad_removed(source.upcast_ref(), &state);

        let first = ghost_src(&source, "src_0");
        route_pad(
            &pipeline,
            &first,
            &state,
            "test",
            false,
            TrackKind::Audio,
            "",
        );
        assert_eq!(peer_factory(&first).as_deref(), Some("clocksync"));

        // The variant switch: the old pad goes, a new one comes.
        source.remove_pad(&first).unwrap();
        let second = ghost_src(&source, "src_3");
        route_pad(
            &pipeline,
            &second,
            &state,
            "test",
            false,
            TrackKind::Audio,
            "",
        );

        assert_eq!(
            peer_factory(&second).as_deref(),
            Some("clocksync"),
            "the replacing stream was discarded instead of taking the free output"
        );
        assert_eq!(
            second
                .peer()
                .and_then(|p| p.parent_element())
                .map(|e| e.name().to_string()),
            Some("test_clocksync_audio".to_string()),
            "it has to feed the same chain, so audio_out carries on"
        );
        let _ = pipeline.set_state(gst::State::Null);
    }

    /// End to end on a real file: two audio tracks with two outputs give each
    /// its own bridge, and playback runs to the end - with either decoder.
    #[test]
    fn each_audio_track_gets_its_own_output_when_there_are_enough() {
        let _ = gst::init();
        let dir = tempfile::tempdir().unwrap();
        let uri = write_two_audio_track_file(dir.path());

        for decoder in [Decoder::Classic, Decoder::Decodebin3] {
            let state = state_with_slots(1, 2);
            let pipeline = create_decode_pipeline("test", &state, Some(&uri), decoder).unwrap();
            let msg = play_to_end(&pipeline);
            assert!(
                matches!(msg.view(), gst::MessageView::Eos(_)),
                "{:?}: {:?}",
                decoder,
                msg
            );
            for name in [
                "test_appsink_audio",
                "test_appsink_audio_1",
                "test_appsink_video",
            ] {
                assert!(
                    pipeline.by_name(name).is_some(),
                    "{:?}: {} was not created: each track needs its own bridge",
                    decoder,
                    name
                );
            }
            assert!(
                pipeline.by_name("test_appsink_audio_2").is_none(),
                "there is no third audio track"
            );
        }
    }

    /// decodebin3 decodes one stream of each type unless it is told which
    /// ones to take, and decoding a track nobody plays is wasted work. With
    /// one audio output it has to decode one audio track - and still play to
    /// the end, which an unhandled second audio pad would stop.
    #[test]
    fn decodebin3_decodes_only_the_tracks_it_has_outputs_for() {
        let _ = gst::init();
        let dir = tempfile::tempdir().unwrap();
        let uri = write_two_audio_track_file(dir.path());

        let state = state_with_slots(1, 1);
        let pipeline =
            create_decode_pipeline("test", &state, Some(&uri), Decoder::Decodebin3).unwrap();
        let msg = play_to_end(&pipeline);
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{:?}", msg);
        assert!(pipeline.by_name("test_appsink_audio").is_some());
        assert!(pipeline.by_name("test_appsink_video").is_some());
        let discarded: Vec<String> = pipeline
            .iterate_elements()
            .into_iter()
            .flatten()
            .map(|e| e.name().to_string())
            .filter(|n| n.contains("_discard_"))
            .collect();
        assert!(
            discarded.is_empty(),
            "the second audio track was decoded only to be thrown away: {:?}",
            discarded
        );
    }

    /// A live stream carries the broadcaster's timeline - SVT's HLS is over a
    /// thousand hours in - and its network stalls now and then. Paced against
    /// that raw running time the clocksync waited a thousand hours, so nothing
    /// came out; and a stall used to leave every later buffer late for good.
    /// Every buffer has to reach the main pipeline on time, stall or not.
    #[test]
    fn a_live_timeline_with_a_stall_plays_on_time_in_the_main_pipeline() {
        let _ = gst::init();
        const CHUNK_MS: u64 = 20;
        let caps = gst::Caps::builder("audio/x-raw")
            .field("format", "S16LE")
            .field("layout", "interleaved")
            .field("rate", 48_000i32)
            .field("channels", 2i32)
            .build();

        // The main pipeline: the slot's appsrc, and a sink that notes when
        // each buffer arrives against what it is stamped with.
        let main = gst::Pipeline::new();
        let out = gst_app::AppSrc::builder()
            .format(gst::Format::Time)
            .is_live(true)
            .build();
        let sink = gst_app::AppSink::builder().sync(false).build();
        main.add_many([out.upcast_ref(), sink.upcast_ref::<gst::Element>()])
            .unwrap();
        out.link(&sink).unwrap();
        let arrivals = Arc::new(Mutex::new(Vec::<(i64, i64)>::new()));
        let main_weak = main.downgrade();
        let arrivals_cb = Arc::clone(&arrivals);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let main = main_weak.upgrade().ok_or(gst::FlowError::Eos)?;
                    let now = main.current_running_time().unwrap().nseconds() as i64;
                    let pts = sample.buffer().unwrap().pts().unwrap().nseconds() as i64;
                    arrivals_cb.lock().unwrap().push((pts, now));
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        // A flow can run on another clock than the internal pipeline's
        // default monotonic one; realtime is 56 years ahead of it.
        let realtime = gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build();
        main.use_clock(Some(&realtime));
        main.set_state(gst::State::Playing).unwrap();

        let mut state = Arc::try_unwrap(test_state()).ok().unwrap();
        state.audio_appsrcs = vec![out];
        state.audio_slots = MediaPlayerState::free_slots(1);
        state.timing = Arc::new(timing::Timing::new(100));
        state.main_pipeline.set(Some(&main));
        let state = Arc::new(state);

        // The internal pipeline, fed like a live stream, on the flow's clock
        // as the block runs it.
        let internal = gst::Pipeline::new();
        assert!(state.follow_main_clock(&internal));
        let src = gst_app::AppSrc::builder()
            .format(gst::Format::Time)
            .caps(&caps)
            .build();
        internal.add(&src).unwrap();
        route_pad(
            &internal,
            &src.static_pad("src").unwrap(),
            &state,
            "test",
            true,
            TrackKind::Audio,
            "",
        );
        internal.set_state(gst::State::Playing).unwrap();

        let start = gst::ClockTime::from_seconds(1000 * 3600);
        let push = |i: u64| {
            let mut buf = gst::Buffer::with_size((48 * CHUNK_MS * 4) as usize).unwrap();
            {
                let b = buf.get_mut().unwrap();
                b.set_pts(start + gst::ClockTime::from_mseconds(i * CHUNK_MS));
                b.set_duration(gst::ClockTime::from_mseconds(CHUNK_MS));
            }
            src.push_buffer(buf).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(CHUNK_MS));
        };
        for i in 0..15 {
            push(i);
        }
        // The stall: nothing for 400 ms, far longer than the 100 ms delay.
        std::thread::sleep(std::time::Duration::from_millis(400));
        for i in 15..30 {
            push(i);
        }
        std::thread::sleep(std::time::Duration::from_millis(400));

        let arrivals = arrivals.lock().unwrap().clone();
        let _ = internal.set_state(gst::State::Null);
        let _ = main.set_state(gst::State::Null);

        assert!(
            arrivals.len() >= 25,
            "only {} of 30 buffers reached the main pipeline",
            arrivals.len()
        );
        // The one buffer that finds the stall goes out early, by the delay.
        // Every buffer must be on time: stamped no earlier than it arrives,
        // less some slack for the hop through the main pipeline on a busy CI
        // runner. Without the re-sync, the buffers after the stall are 350 ms
        // late.
        const SLACK_NS: i64 = 150_000_000;
        for (n, (pts, now)) in arrivals.iter().enumerate() {
            assert!(
                *pts >= now - SLACK_NS,
                "buffer {} arrived {} ms after its time in the main pipeline",
                n,
                (now - pts) / 1_000_000
            );
        }
    }

    /// A file switch takes the internal pipeline through READY and back, which
    /// gave it a new base time while the clocksync offset still counted from
    /// the old one: after the switch one frame came through, then nothing for
    /// seconds, and the encoders downstream reported buffer age. Playback has
    /// to carry on, on time, after a switch - audio and video both, on a flow
    /// whose clock is not the internal pipeline's default one.
    #[test]
    fn playback_carries_on_in_time_after_a_file_switch() {
        for decoder in [Decoder::Classic, Decoder::Decodebin3] {
            file_switch_keeps_time(decoder);
        }
    }

    fn file_switch_keeps_time(decoder: Decoder) {
        let _ = gst::init();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("av.mkv");
        let writer = gst::parse::launch(&format!(
            "matroskamux name=mux ! filesink location={} \
             videotestsrc num-buffers=100 ! video/x-raw,format=I420,width=64,height=48,framerate=25/1 ! mux. \
             audiotestsrc num-buffers=200 samplesperbuffer=960 ! audio/x-raw,format=S16LE,rate=48000,channels=2 ! mux.",
            path.display()
        ))
        .expect("matroskamux, videotestsrc and audiotestsrc are installed in CI");
        writer.set_state(gst::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::from_seconds(20), &[gst::MessageType::Eos])
            .expect("writing the test file finishes");
        writer.set_state(gst::State::Null).unwrap();
        let uri = format!("file://{}", path.display());

        // The flow, on a realtime clock, with a sink per output that notes
        // when each buffer arrives against what it is stamped with.
        let main = gst::Pipeline::new();
        let realtime = gst::glib::Object::builder::<gst::SystemClock>()
            .property("clock-type", gst::ClockType::Realtime)
            .build();
        main.use_clock(Some(&realtime));
        let arrivals = Arc::new(Mutex::new(Vec::<(&str, i64, i64)>::new()));
        let mut outs = Vec::new();
        for kind in ["video", "audio"] {
            let out = gst_app::AppSrc::builder()
                .format(gst::Format::Time)
                .is_live(true)
                .build();
            let sink = gst_app::AppSink::builder().sync(false).build();
            main.add_many([out.upcast_ref(), sink.upcast_ref::<gst::Element>()])
                .unwrap();
            out.link(&sink).unwrap();
            let main_weak = main.downgrade();
            let arrivals = Arc::clone(&arrivals);
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let main = main_weak.upgrade().ok_or(gst::FlowError::Eos)?;
                        let now = main.current_running_time().unwrap().nseconds() as i64;
                        let pts = sample.buffer().unwrap().pts().unwrap().nseconds() as i64;
                        arrivals.lock().unwrap().push((kind, pts, now));
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
            outs.push(out);
        }
        main.set_state(gst::State::Playing).unwrap();
        let _ = main.state(gst::ClockTime::from_seconds(5));

        let mut state = Arc::try_unwrap(test_state()).ok().unwrap();
        state.audio_appsrcs = vec![outs.pop().unwrap()];
        state.video_appsrcs = vec![outs.pop().unwrap()];
        state.video_slots = MediaPlayerState::free_slots(1);
        state.audio_slots = MediaPlayerState::free_slots(1);
        state.timing = Arc::new(timing::Timing::new(200));
        state.main_pipeline.set(Some(&main));
        let state = Arc::new(state);
        let internal = create_decode_pipeline("test", &state, Some(&uri), decoder).unwrap();
        *state.internal_pipeline.write().unwrap() = Some(internal.clone());
        state.set_playlist(vec![uri.clone(), uri]);
        assert!(state.follow_main_clock(&internal));
        internal.set_state(gst::State::Playing).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1000));
        let switched_at = main.current_running_time().unwrap().nseconds() as i64;
        state.goto(1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = internal.set_state(gst::State::Null);
        let _ = main.set_state(gst::State::Null);

        let arrivals = arrivals.lock().unwrap().clone();
        for kind in ["video", "audio"] {
            let after = arrivals
                .iter()
                .filter(|(k, _, now)| *k == kind && *now > switched_at)
                .count();
            // 1.5 s after the switch, less the 200 ms delay: about 32 video
            // frames and 65 audio buffers.
            let expected = if kind == "video" { 20 } else { 40 };
            assert!(
                after >= expected,
                "{:?}: only {} {} buffers in 1.5 s after the switch",
                decoder,
                after,
                kind
            );
        }
        const SLACK_NS: i64 = 150_000_000;
        for (kind, pts, now) in &arrivals {
            assert!(
                *pts >= now - SLACK_NS,
                "{:?}: a {} buffer arrived {} ms after its time in the flow",
                decoder,
                kind,
                (now - pts) / 1_000_000
            );
        }
    }

    #[test]
    fn decode_pipeline_disables_hdrext_aggregation() {
        let _ = gst::init();
        if !rtp_hdrext::is_supported() {
            // GStreamer < 1.24: aggregation does not exist, so neither does the bug.
            return;
        }
        for decoder in [Decoder::Classic, Decoder::Decodebin3] {
            let state = test_state();
            let pipeline = create_decode_pipeline("test", &state, None, decoder).unwrap();
            assert_hdrext_disabled_on_late_depayloader(&pipeline);
        }
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
