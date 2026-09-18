//! Media player block builder — creates main pipeline elements and internal pipeline.

use super::bridge;
use super::normalize_uri;
use super::state::{MediaPlayerKey, MediaPlayerState, MEDIA_PLAYER_REGISTRY};
use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, BusMessageConnectFn,
    APPSRC_MAX_BYTES_AUDIO, APPSRC_MAX_BYTES_VIDEO, APPSRC_MAX_TIME,
};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, RwLock};
use strom_types::element::ElementPadRef;
use strom_types::{FlowId, PropertyValue, StromEvent};
use tracing::{debug, info};
use uuid::Uuid;

/// Media player block builder.
pub struct MediaPlayerBuilder;

impl BlockBuilder for MediaPlayerBuilder {
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
            "Media Player {}: decode={}, sync={} ({})",
            instance_id,
            decode,
            sync,
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

        build_media_player(
            instance_id,
            &block_id,
            flow_id,
            loop_playlist,
            decode,
            sync,
            position_update_interval_ms,
            initial_playlist,
            media_path,
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
    position_update_interval_ms: u64,
    initial_playlist: Vec<String>,
    media_path: std::path::PathBuf,
) -> Result<BlockBuildResult, BlockBuildError> {
    // --- Main pipeline elements: appsrc → identity ---
    let appsrc_video_id = format!("{}:appsrc_video", instance_id);
    let video_out_id = format!("{}:video_out", instance_id);
    let appsrc_audio_id = format!("{}:appsrc_audio", instance_id);
    let audio_out_id = format!("{}:audio_out", instance_id);

    let appsrc_video = gst_app::AppSrc::builder()
        .name(&appsrc_video_id)
        .format(gst::Format::Time)
        .is_live(true)
        .automatic_eos(false)
        .max_bytes(APPSRC_MAX_BYTES_VIDEO)
        .max_time(APPSRC_MAX_TIME)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();

    let appsrc_audio = gst_app::AppSrc::builder()
        .name(&appsrc_audio_id)
        .format(gst::Format::Time)
        .is_live(true)
        .automatic_eos(false)
        .max_bytes(APPSRC_MAX_BYTES_AUDIO)
        .max_time(APPSRC_MAX_TIME)
        .leaky_type(gst_app::AppLeakyType::Upstream)
        .build();

    let video_out = gst::ElementFactory::make("identity")
        .name(&video_out_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("video_out: {}", e)))?;

    let audio_out = gst::ElementFactory::make("identity")
        .name(&audio_out_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("audio_out: {}", e)))?;

    // --- Create shared state ---
    let player_instance_id = Uuid::new_v4();
    let source_element_weak = gst::glib::WeakRef::new();
    let ts_offset = Arc::new(AtomicI64::new(i64::MIN));
    let state = Arc::new(MediaPlayerState {
        instance_id: player_instance_id,
        source_element: source_element_weak,
        internal_pipeline: RwLock::new(None),
        video_appsrc: Some(appsrc_video.clone()),
        audio_appsrc: Some(appsrc_audio.clone()),
        playlist: RwLock::new(super::state::Playlist {
            files: initial_playlist.clone(),
            current_index: 0,
        }),
        is_paused: AtomicBool::new(false),
        loop_playlist: AtomicBool::new(loop_playlist),
        // Armed only once a stinger trigger parks it; a player is not on
        // frame 0 merely because it was built.
        stinger_armed: AtomicBool::new(false),
        block_id: block_id.to_string(),
        flow_id,
        switching_file: AtomicBool::new(false),
        video_linked: AtomicBool::new(false),
        audio_linked: AtomicBool::new(false),
        decode,
        sync,
        media_path: media_path.clone(),
        ts_offset,
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

    // --- Queues between appsrc and identity for buffering ---
    // Without queues, downstream sinks (e.g. srtsink) warn about insufficient
    // buffering for their processing deadline.
    let queue_video_id = format!("{}:queue_video", instance_id);
    let queue_audio_id = format!("{}:queue_audio", instance_id);

    let queue_video = gst::ElementFactory::make("queue")
        .name(&queue_video_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue_video: {}", e)))?;

    let queue_audio = gst::ElementFactory::make("queue")
        .name(&queue_audio_id)
        .build()
        .map_err(|e| BlockBuildError::ElementCreation(format!("queue_audio: {}", e)))?;

    // --- Static links: appsrc → queue → identity ---
    let internal_links = vec![
        (
            ElementPadRef::pad(&appsrc_video_id, "src"),
            ElementPadRef::pad(&queue_video_id, "sink"),
        ),
        (
            ElementPadRef::pad(&queue_video_id, "src"),
            ElementPadRef::pad(&video_out_id, "sink"),
        ),
        (
            ElementPadRef::pad(&appsrc_audio_id, "src"),
            ElementPadRef::pad(&queue_audio_id, "sink"),
        ),
        (
            ElementPadRef::pad(&queue_audio_id, "src"),
            ElementPadRef::pad(&audio_out_id, "sink"),
        ),
    ];

    Ok(BlockBuildResult {
        elements: vec![
            (appsrc_video_id, appsrc_video.upcast()),
            (queue_video_id, queue_video),
            (video_out_id, video_out),
            (appsrc_audio_id, appsrc_audio.upcast()),
            (queue_audio_id, queue_audio),
            (audio_out_id, audio_out),
        ],
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
    if let Some(ref appsrc) = state.video_appsrc {
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

    // Start the internal pipeline
    if let Err(e) = internal_pipeline.set_state(gst::State::Playing) {
        tracing::error!(
            "Media Player {}: Failed to start internal pipeline: {:?}",
            block_id,
            e
        );
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

    // Return a no-op handler on the main bus — all real work is on the internal bus.
    // We must return a valid SignalHandlerId per BusMessageConnectFn contract.
    main_bus.connect_message(None, |_bus, _msg| {})
}
