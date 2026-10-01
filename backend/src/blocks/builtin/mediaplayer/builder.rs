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
            "Media Player {}: decode={}, sync={}, playout_delay={} ms ({})",
            instance_id,
            decode,
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
        switching_file: AtomicBool::new(false),
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
        bridge::create_decode_pipeline(instance_id, &state, initial_uri.as_deref())?
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

    // The internal pipeline runs on the flow's clock and base time, which the
    // flow only has once it plays. Start it now if it does, or when it does.
    if state.follow_main_clock(&internal_pipeline) {
        start_internal(&internal_pipeline, &block_id);
    }

    // Watch internal pipeline bus for EOS, errors, state changes
    bridge::watch_internal_bus(
        &internal_pipeline,
        Arc::clone(&state),
        flow_id,
        block_id.clone(),
        events.clone(),
    );

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

    // Start the internal pipeline when the flow reaches PLAYING. Weak refs
    // only: the main bus outlives neither.
    let state_weak = Arc::downgrade(&state);
    let internal_weak = internal_pipeline.downgrade();
    main_bus.connect_message(Some("state-changed"), move |_bus, msg| {
        let gst::MessageView::StateChanged(change) = msg.view() else {
            return;
        };
        if change.current() != gst::State::Playing
            || !msg.src().is_some_and(|s| s.is::<gst::Pipeline>())
        {
            return;
        }
        let (Some(state), Some(internal)) = (state_weak.upgrade(), internal_weak.upgrade()) else {
            return;
        };
        if internal.current_state() != gst::State::Playing
            && internal.pending_state() != gst::State::Playing
            && state.follow_main_clock(&internal)
        {
            start_internal(&internal, &state.block_id);
        }
    })
}

fn start_internal(internal: &gst::Pipeline, block_id: &str) {
    if let Err(e) = internal.set_state(gst::State::Playing) {
        tracing::error!(
            "Media Player {}: Failed to start internal pipeline: {:?}",
            block_id,
            e
        );
    }
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
}
