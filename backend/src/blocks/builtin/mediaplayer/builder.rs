//! Media player block builder — creates main pipeline elements and internal pipeline.

use super::bridge;
use super::normalize_uri;
use super::state::{MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY};
use super::timing::{self, Timing};
use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, BusMessageConnectFn,
    APPSRC_MAX_BYTES_AUDIO, APPSRC_MAX_BYTES_VIDEO, APPSRC_MAX_TIME,
};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use strom_types::block::{ExternalPad, ExternalPads};
use strom_types::element::ElementPadRef;
use strom_types::{FlowId, MediaType, PropertyValue, StromEvent};
use tracing::{debug, info};
use uuid::Uuid;

/// Media player block builder.
pub struct MediaPlayerBuilder;

/// How many video and audio tracks of a file reach the block's outputs, from
/// `num_video_tracks` and `num_audio_tracks` (0..=8, default 1 each). Tracks
/// beyond these, and tracks that are neither video nor audio, are discarded
/// rather than left unlinked, which would stop playback.
pub(super) fn track_counts(properties: &HashMap<String, PropertyValue>) -> (usize, usize) {
    use crate::blocks::builtin::whep::explicit_track_count;
    (
        explicit_track_count(properties, "num_video_tracks").unwrap_or(1),
        explicit_track_count(properties, "num_audio_tracks").unwrap_or(1),
    )
}

/// The suffix that names a track slot's elements and pads. Slot 0 has none, so
/// `video_out` and `audio_out` stay what they were and existing flows link as
/// before; slot 1 is `_1`, and so on.
pub(super) fn slot_suffix(slot: usize) -> String {
    if slot == 0 {
        String::new()
    } else {
        format!("_{}", slot)
    }
}

impl BlockBuilder for MediaPlayerBuilder {
    fn get_external_pads(
        &self,
        properties: &HashMap<String, PropertyValue>,
    ) -> Option<ExternalPads> {
        let (num_video, num_audio) = track_counts(properties);
        let mut outputs = Vec::new();
        for (count, media_type, kind, tag) in [
            (num_video, MediaType::Video, "video", "V"),
            (num_audio, MediaType::Audio, "audio", "A"),
        ] {
            for slot in 0..count {
                let name = format!("{}_out{}", kind, slot_suffix(slot));
                outputs.push(ExternalPad {
                    label: (count > 1).then(|| format!("{}{}", tag, slot)),
                    name: name.clone(),
                    media_type,
                    internal_element_id: name,
                    internal_pad_name: "src".to_string(),
                });
            }
        }
        Some(ExternalPads {
            inputs: vec![],
            outputs,
        })
    }

    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
        _ctx: &BlockBuildContext,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Media Player block instance: {}", instance_id);

        let loop_playlist = properties
            .get("loop_playlist")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        let decode = properties
            .get("decode")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);

        let decoder =
            bridge::Decoder::from_property(properties.get("decoder").and_then(|v| match v {
                PropertyValue::String(s) => Some(s.as_str()),
                _ => None,
            }));

        let sync = properties
            .get("sync")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(true);

        let playout_delay_ms = properties
            .get("playout_delay_ms")
            .and_then(|v| match v {
                PropertyValue::UInt(u) => Some(*u),
                PropertyValue::Int(i) => u64::try_from(*i).ok(),
                _ => None,
            })
            .unwrap_or(timing::DEFAULT_PLAYOUT_DELAY_MS);

        let position_update_interval_ms = properties
            .get("position_update_interval")
            .and_then(|v| match v {
                PropertyValue::Int(i) => Some(*i as u64),
                _ => None,
            })
            .unwrap_or(200);

        let flow_id: FlowId = properties
            .get("_flow_id")
            .and_then(|v| match v {
                PropertyValue::String(s) => Uuid::parse_str(s).ok(),
                _ => None,
            })
            .unwrap_or_else(Uuid::nil);

        let block_id = instance_id.to_string();

        info!(
            "Media Player {}: decode={} ({:?}), sync={}, playout_delay={} ms ({})",
            instance_id,
            decode,
            decoder,
            sync,
            playout_delay_ms,
            if decode {
                "decoding to raw"
            } else {
                "passthrough encoded"
            }
        );

        let media_path: std::path::PathBuf = properties
            .get("_media_path")
            .and_then(|v| match v {
                PropertyValue::String(s) => Some(std::path::PathBuf::from(s)),
                _ => None,
            })
            .unwrap_or_else(|| std::path::PathBuf::from("./media"));

        let initial_playlist: Vec<String> = properties
            .get("playlist")
            .and_then(|v| match v {
                PropertyValue::String(s) => serde_json::from_str(s).ok(),
                _ => None,
            })
            .unwrap_or_default();

        if !initial_playlist.is_empty() {
            info!(
                "Media Player {}: Loading playlist with {} files from properties",
                instance_id,
                initial_playlist.len()
            );
        }

        let (num_video_tracks, num_audio_tracks) = track_counts(properties);

        build_media_player(
            instance_id,
            &block_id,
            flow_id,
            loop_playlist,
            decode,
            decoder,
            sync,
            playout_delay_ms,
            position_update_interval_ms,
            initial_playlist,
            media_path,
            num_video_tracks,
            num_audio_tracks,
        )
    }
}

/// Build the media player block.
///
/// Creates appsrc+identity elements for the main pipeline, and an internal pipeline
/// with uridecodebin/urisourcebin + clocksync + appsink for isolated playback.
#[allow(clippy::too_many_arguments)]
fn build_media_player(
    instance_id: &str,
    block_id: &str,
    flow_id: FlowId,
    loop_playlist: bool,
    decode: bool,
    decoder: bridge::Decoder,
    sync: bool,
    playout_delay_ms: u64,
    position_update_interval_ms: u64,
    initial_playlist: Vec<String>,
    media_path: std::path::PathBuf,
    num_video_tracks: usize,
    num_audio_tracks: usize,
) -> Result<BlockBuildResult, BlockBuildError> {
    // --- Main pipeline elements, per track slot: appsrc → queue → identity ---
    // The queue keeps downstream sinks (e.g. srtsink) from warning about
    // insufficient buffering for their processing deadline.
    let mut elements: Vec<(String, gst::Element)> = Vec::new();
    let mut internal_links = Vec::new();
    let mut video_appsrcs = Vec::new();
    let mut audio_appsrcs = Vec::new();
    for (count, kind, max_bytes) in [
        (num_video_tracks, "video", APPSRC_MAX_BYTES_VIDEO),
        (num_audio_tracks, "audio", APPSRC_MAX_BYTES_AUDIO),
    ] {
        for slot in 0..count {
            let sfx = slot_suffix(slot);
            let appsrc_id = format!("{}:appsrc_{}{}", instance_id, kind, sfx);
            let queue_id = format!("{}:queue_{}{}", instance_id, kind, sfx);
            let out_id = format!("{}:{}_out{}", instance_id, kind, sfx);

            let appsrc = gst_app::AppSrc::builder()
                .name(&appsrc_id)
                .format(gst::Format::Time)
                .is_live(true)
                .automatic_eos(false)
                .max_bytes(max_bytes)
                .max_time(APPSRC_MAX_TIME)
                .leaky_type(gst_app::AppLeakyType::Upstream)
                .build();
            let queue = gst::ElementFactory::make("queue")
                .name(&queue_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", queue_id, e)))?;
            let out = gst::ElementFactory::make("identity")
                .name(&out_id)
                .build()
                .map_err(|e| BlockBuildError::ElementCreation(format!("{}: {}", out_id, e)))?;

            internal_links.push((
                ElementPadRef::pad(&appsrc_id, "src"),
                ElementPadRef::pad(&queue_id, "sink"),
            ));
            internal_links.push((
                ElementPadRef::pad(&queue_id, "src"),
                ElementPadRef::pad(&out_id, "sink"),
            ));
            if kind == "video" {
                video_appsrcs.push(appsrc.clone());
            } else {
                audio_appsrcs.push(appsrc.clone());
            }
            elements.push((appsrc_id, appsrc.upcast()));
            elements.push((queue_id, queue));
            elements.push((out_id, out));
        }
    }

    // --- Create shared state ---
    let (num_video_slots, num_audio_slots) = (video_appsrcs.len(), audio_appsrcs.len());
    let player_instance_id = Uuid::new_v4();
    let source_element_weak = gst::glib::WeakRef::new();
    let state = Arc::new(MediaPlayerState {
        instance_id: player_instance_id,
        source_element: source_element_weak,
        internal_pipeline: RwLock::new(None),
        video_appsrcs,
        audio_appsrcs,
        playlist: RwLock::new(super::state::Playlist {
            files: initial_playlist.clone(),
            current_index: 0,
        }),
        is_paused: AtomicBool::new(false),
        loop_playlist: AtomicBool::new(loop_playlist),
        block_id: block_id.to_string(),
        flow_id,
        control: std::sync::Mutex::new(()),
        switch_generation: std::sync::atomic::AtomicU64::new(0),
        source_ready: std::sync::Mutex::new(true),
        source_ready_cv: std::sync::Condvar::new(),
        starting: std::sync::Mutex::new(None),
        starting_cv: std::sync::Condvar::new(),
        video_slots: MediaPlayerState::free_slots(num_video_slots),
        audio_slots: MediaPlayerState::free_slots(num_audio_slots),
        decode,
        sync,
        media_path: media_path.clone(),
        timing: Arc::new(Timing::new(playout_delay_ms)),
        main_pipeline: gst::glib::WeakRef::new(),
        bus_watch: std::sync::Mutex::new(None),
    });

    // --- Resolve initial URI ---
    let initial_uri = initial_playlist
        .first()
        .map(|f| normalize_uri(f, &media_path));

    if let Some(ref uri) = initial_uri {
        info!("Media Player {}: Initial URI: {}", instance_id, uri);
    }

    // --- Create internal pipeline ---
    let internal_pipeline = if decode {
        bridge::create_decode_pipeline(instance_id, &state, initial_uri.as_deref(), decoder)?
    } else {
        bridge::create_passthrough_pipeline(instance_id, &state, initial_uri.as_deref())?
    };

    // Store internal pipeline in state
    if let Ok(mut guard) = state.internal_pipeline.write() {
        *guard = Some(internal_pipeline.clone());
    }

    // Register in global registry
    let registry_key = MediaPlayerKey {
        flow_id,
        block_id: block_id.to_string(),
    };
    MEDIA_PLAYER_REGISTRY.register(registry_key, Arc::clone(&state));

    // --- Bus message handler (called when main pipeline starts) ---
    // Starts the internal pipeline, sets up its bus watch, and starts position polling.
    let state_for_handler = Arc::clone(&state);
    let block_id_for_handler = block_id.to_string();
    let internal_pipeline_for_handler = internal_pipeline.clone();

    let bus_message_handler: BusMessageConnectFn = Box::new(
        move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
            connect_main_pipeline_handler(
                bus,
                flow_id,
                events,
                Arc::clone(&state_for_handler),
                block_id_for_handler.clone(),
                position_update_interval_ms,
                internal_pipeline_for_handler.clone(),
            )
        },
    );

    Ok(BlockBuildResult {
        elements,
        internal_links,
        bus_message_handler: Some(bus_message_handler),
        pad_properties: HashMap::new(),
    })
}

/// Handler called when the main pipeline's bus is connected.
///
/// Starts the internal pipeline, watches its bus for EOS/errors, and starts
/// the position polling timer.
fn connect_main_pipeline_handler(
    main_bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    state: Arc<MediaPlayerState>,
    block_id: String,
    position_update_interval_ms: u64,
    internal_pipeline: gst::Pipeline,
) -> gst::glib::SignalHandlerId {
    info!(
        "Media Player {}: Starting internal pipeline and position timer",
        block_id
    );

    // Store main pipeline reference for timestamp offset computation in the bridge.
    // The appsrc lives in the main pipeline, so we traverse up from it to find the pipeline.
    if let Some(appsrc) = state
        .video_appsrcs
        .first()
        .or_else(|| state.audio_appsrcs.first())
    {
        let mut current: Option<gst::Object> =
            Some(appsrc.clone().upcast::<gst::Element>().upcast());
        while let Some(obj) = current {
            if let Some(pipeline) = obj.downcast_ref::<gst::Pipeline>() {
                state.main_pipeline.set(Some(pipeline));
                info!("Media Player {}: Main pipeline reference set", block_id);
                break;
            }
            current = obj.parent();
        }
    }

    // Watch internal pipeline bus for EOS, errors, state changes. Before the
    // start below: an error or EOS in it must reach the handlers.
    bridge::watch_internal_bus(
        &internal_pipeline,
        Arc::clone(&state),
        flow_id,
        block_id.clone(),
        events.clone(),
    );

    // The internal pipeline runs on the flow's clock and base time, which the
    // flow only has once it plays. Start it now if it does, or when it does.
    state.start_with_flow();

    // Start position polling timer
    let events_for_timer = events;
    let state_for_timer = Arc::clone(&state);
    let block_id_for_timer = block_id.clone();
    let timer_instance_id = state.instance_id;

    let registry_key = MediaPlayerKey {
        flow_id,
        block_id: block_id_for_timer.clone(),
    };

    gst::glib::timeout_add(
        std::time::Duration::from_millis(position_update_interval_ms),
        move || {
            let is_current_instance = MEDIA_PLAYER_REGISTRY
                .get(&registry_key)
                .map(|s| s.instance_id == timer_instance_id)
                .unwrap_or(false);

            if !is_current_instance {
                debug!(
                    "Media Player {}: Instance {} no longer current, stopping position timer",
                    block_id_for_timer, timer_instance_id
                );
                return gst::glib::ControlFlow::Break;
            }

            let position = state_for_timer.position().unwrap_or(0);
            let duration = state_for_timer.duration().unwrap_or(0);
            let current_index = state_for_timer.current_index();
            let total_files = state_for_timer.playlist_len();

            events_for_timer.broadcast(StromEvent::MediaPlayerPosition {
                flow_id,
                block_id: block_id_for_timer.clone(),
                position_ns: position,
                duration_ns: duration,
                current_file_index: current_index,
                total_files,
            });

            gst::glib::ControlFlow::Continue
        },
    );

    // Start the internal pipeline when the flow reaches PLAYING. A weak ref
    // only: the main bus outlives the state. The start waits for the control
    // lock and for the internal pipeline to settle, which must not hold up
    // the main context this is dispatched on, so it runs on a thread.
    let state_weak = Arc::downgrade(&state);
    main_bus.connect_message(Some("state-changed"), move |_bus, msg| {
        let gst::MessageView::StateChanged(change) = msg.view() else {
            return;
        };
        if change.current() != gst::State::Playing
            || !msg.src().is_some_and(|s| s.is::<gst::Pipeline>())
        {
            return;
        }
        let Some(state) = state_weak.upgrade() else {
            return;
        };
        // A pause the user asked for holds when the flow starts playing.
        // `start_with_flow` checks again under the control lock.
        if state.is_paused.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("media-player-start".to_string())
            .spawn(move || state.start_with_flow());
        if let Err(e) = spawned {
            tracing::error!("Media Player: Failed to spawn start thread: {}", e);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad_names(properties: &[(&str, u64)]) -> Vec<String> {
        let props: HashMap<String, PropertyValue> = properties
            .iter()
            .map(|(k, v)| (k.to_string(), PropertyValue::UInt(*v)))
            .collect();
        MediaPlayerBuilder
            .get_external_pads(&props)
            .unwrap()
            .outputs
            .into_iter()
            .map(|p| p.name)
            .collect()
    }

    #[test]
    fn one_video_and_one_audio_output_unless_asked_for_more() {
        // The names flows already link to.
        assert_eq!(pad_names(&[]), ["video_out", "audio_out"]);
    }

    #[test]
    fn each_extra_track_gets_its_own_output() {
        assert_eq!(
            pad_names(&[("num_audio_tracks", 3)]),
            ["video_out", "audio_out", "audio_out_1", "audio_out_2"]
        );
        assert_eq!(pad_names(&[("num_video_tracks", 0)]), ["audio_out"]);
        assert_eq!(
            pad_names(&[("num_audio_tracks", 99)]).len(),
            1 + 8,
            "capped at 8"
        );
    }

    #[test]
    fn every_output_pad_has_an_element_behind_it() {
        let props: HashMap<String, PropertyValue> = [
            ("num_video_tracks".to_string(), PropertyValue::UInt(2)),
            ("num_audio_tracks".to_string(), PropertyValue::UInt(2)),
        ]
        .into();
        let _ = gst::init();
        let built = MediaPlayerBuilder
            .build(
                "mp",
                &props,
                &crate::blocks::BlockBuildContext::new(Vec::new(), "all".to_string()),
            )
            .unwrap();
        let ids: Vec<&str> = built.elements.iter().map(|(id, _)| id.as_str()).collect();
        for pad in MediaPlayerBuilder
            .get_external_pads(&props)
            .unwrap()
            .outputs
        {
            let id = format!("mp:{}", pad.internal_element_id);
            assert!(ids.contains(&id.as_str()), "{} has no element", id);
        }
    }

    /// A Media Player built by the real builder, its elements in a flow of
    /// their own (`video_out` into a fakesink), and its flow handler connected
    /// the way `start_flow` connects it. The handler's signal watches, on the
    /// flow's bus and on the internal one, are attached to `ctx`, so nothing
    /// they do happens until the test iterates it.
    struct Rig {
        main: gst::Pipeline,
        player: Arc<MediaPlayerState>,
        internal: gst::Pipeline,
        ctx: gst::glib::MainContext,
        key: MediaPlayerKey,
    }

    impl Rig {
        fn new(playlist: &[String]) -> Self {
            let _ = gst::init();
            let flow_id = Uuid::new_v4();
            let props: HashMap<String, PropertyValue> = [
                (
                    "playlist".to_string(),
                    PropertyValue::String(serde_json::to_string(playlist).unwrap()),
                ),
                ("decode".to_string(), PropertyValue::Bool(true)),
                ("loop_playlist".to_string(), PropertyValue::Bool(false)),
                ("num_audio_tracks".to_string(), PropertyValue::UInt(0)),
                (
                    "_flow_id".to_string(),
                    PropertyValue::String(flow_id.to_string()),
                ),
            ]
            .into();
            let built = MediaPlayerBuilder
                .build(
                    "mp",
                    &props,
                    &crate::blocks::BlockBuildContext::new(Vec::new(), "all".to_string()),
                )
                .unwrap();

            // A clock the internal pipeline would never pick on its own.
            let main = gst::Pipeline::new();
            main.use_clock(Some(
                &gst::glib::Object::builder::<gst::SystemClock>()
                    .property("clock-type", gst::ClockType::Realtime)
                    .build(),
            ));
            for (_, element) in &built.elements {
                main.add(element).unwrap();
            }
            let sink = gst::ElementFactory::make("fakesink")
                .property("async", false)
                .build()
                .unwrap();
            main.add(&sink).unwrap();
            let by_id = |id: &str| {
                built
                    .elements
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, e)| e.clone())
                    .unwrap()
            };
            gst::Element::link_many([
                &by_id("mp:appsrc_video"),
                &by_id("mp:queue_video"),
                &by_id("mp:video_out"),
                &sink,
            ])
            .unwrap();

            let ctx = gst::glib::MainContext::new();
            let bus = main.bus().unwrap();
            let handler = built.bus_message_handler.unwrap();
            ctx.with_thread_default(|| {
                bus.add_signal_watch();
                handler(&bus, flow_id, EventBroadcaster::default());
            })
            .unwrap();

            let key = MediaPlayerKey {
                flow_id,
                block_id: "mp".to_string(),
            };
            let player = MEDIA_PLAYER_REGISTRY.get(&key).unwrap();
            let internal = player.internal_pipeline.read().unwrap().clone().unwrap();
            Rig {
                main,
                player,
                internal,
                ctx,
                key,
            }
        }

        /// Dispatch what is pending on the rig's context until `done`, or fail.
        fn run_until(&self, what: &str, done: impl Fn() -> bool) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out: {} (flow {:?}, internal {:?})",
                    what,
                    self.main.state(gst::ClockTime::ZERO),
                    self.internal.state(gst::ClockTime::ZERO)
                );
                if !self.ctx.iteration(false) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }

        /// Start the flow and wait until the player follows it to PLAYING.
        fn start_flow(&self) {
            self.main.set_state(gst::State::Playing).unwrap();
            let _ = self.main.state(gst::ClockTime::from_seconds(5));
            self.run_until("the internal pipeline plays", || {
                self.internal.current_state() == gst::State::Playing
            });
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            MEDIA_PLAYER_REGISTRY.unregister(&self.key);
            let _ = self.main.set_state(gst::State::Null);
            if let Some(bus) = self.main.bus() {
                bus.remove_signal_watch();
            }
        }
    }

    /// `frames` of raw video at 25 fps in Matroska, so no encoder or decoder
    /// is needed: matroskamux and videotestsrc are in gstreamer1.0-plugins-good
    /// and -base, installed in CI.
    fn write_clip(dir: &std::path::Path, name: &str, frames: u32) -> String {
        let _ = gst::init();
        let path = dir.join(name);
        let writer = gst::parse::launch(&format!(
            "videotestsrc num-buffers={} ! video/x-raw,format=I420,width=64,height=48,framerate=25/1 \
             ! matroskamux ! filesink name=out",
            frames
        ))
        .unwrap();
        writer
            .downcast_ref::<gst::Bin>()
            .and_then(|bin| bin.by_name("out"))
            .unwrap()
            .set_property("location", &path);
        writer.set_state(gst::State::Playing).unwrap();
        writer
            .bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::from_seconds(20), &[gst::MessageType::Eos])
            .expect("writing the clip finishes");
        writer.set_state(gst::State::Null).unwrap();
        super::super::file_uri(&path)
    }

    /// A playlist jump before the flow plays used to start the internal
    /// pipeline then and there, on its own clock and base time, and the
    /// flow's PLAYING handler leaves a pipeline that already plays alone. It
    /// then ran on the wrong clock for good, so the bridge's running times
    /// meant nothing in the flow.
    #[test]
    fn a_jump_before_the_flow_plays_still_runs_on_the_flows_clock() {
        let dir = tempfile::tempdir().unwrap();
        let clips = [
            write_clip(dir.path(), "a.mkv", 250),
            write_clip(dir.path(), "b.mkv", 250),
        ];
        let rig = Rig::new(&clips);

        rig.player.goto(1).unwrap();
        rig.start_flow();

        assert_eq!(
            rig.internal.clock(),
            rig.main.clock(),
            "the internal pipeline does not run on the flow's clock"
        );
        assert_eq!(
            rig.internal.base_time(),
            rig.main.base_time(),
            "the internal pipeline does not run on the flow's base time"
        );
    }

    /// A pause the user asked for before the flow plays must hold when it
    /// does: the flow's PLAYING handler used to start the player regardless.
    #[test]
    fn a_pause_before_the_flow_plays_holds_when_it_does() {
        let dir = tempfile::tempdir().unwrap();
        let clips = [write_clip(dir.path(), "a.mkv", 250)];
        let rig = Rig::new(&clips);

        rig.player.goto(0).unwrap();
        rig.player.pause().unwrap();
        rig.main.set_state(gst::State::Playing).unwrap();
        let _ = rig.main.state(gst::ClockTime::from_seconds(5));
        // Deliver the flow's state changes to the player's handler.
        while rig.ctx.iteration(false) {}

        assert_ne!(
            rig.internal.current_state(),
            gst::State::Playing,
            "the flow starting overrode the user's pause"
        );
        assert_ne!(rig.internal.pending_state(), gst::State::Playing);
    }

    /// An EOS the old file posted just before a jump must not advance past
    /// the file jumped to. It used to be judged when the signal watch got to
    /// it, by a flag that the first of two overlapping jumps had already
    /// cleared, so it advanced the playlist one past the target.
    #[test]
    fn an_eos_from_before_concurrent_jumps_does_not_advance_past_them() {
        let dir = tempfile::tempdir().unwrap();
        let clips = [
            write_clip(dir.path(), "a.mkv", 250),
            write_clip(dir.path(), "b.mkv", 250),
            write_clip(dir.path(), "c.mkv", 250),
        ];
        let rig = Rig::new(&clips);
        rig.start_flow();

        // The old file's EOS, still queued for the signal watch when the
        // jumps run.
        let generation = rig.player.eos_generation().unwrap();
        rig.internal
            .bus()
            .unwrap()
            .post(gst::message::Eos::builder().src(&rig.internal).build())
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let jumps: Vec<_> = (0..2)
            .map(|_| {
                let player = Arc::clone(&rig.player);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    player.goto(1)
                })
            })
            .collect();
        for jump in jumps {
            jump.join().unwrap().unwrap();
        }
        while rig.ctx.iteration(false) {}

        // The EOS's own thread may not have run yet; judge an EOS of that
        // generation here, as that thread does.
        assert_eq!(
            rig.player.advance_after_eos(generation),
            Ok(false),
            "an EOS from before the jumps would advance the playlist"
        );
        assert_eq!(
            rig.player.current_index(),
            1,
            "a stale EOS advanced the playlist past the file jumped to"
        );
    }
}
