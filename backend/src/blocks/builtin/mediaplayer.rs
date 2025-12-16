//! Media player block for file playback with playlist support.
//!
//! Uses uridecodebin for file decoding with dynamic pad handling.
//! Supports play, pause, seek, and playlist navigation.

use crate::blocks::{BlockBuildError, BlockBuildResult, BlockBuilder, BusMessageConnectFn};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use strom_types::{block::*, FlowId, MediaType, PropertyValue, StromEvent};
use tracing::{debug, error, info};
use uuid::Uuid;

/// Global registry of media player instances for API access.
pub static MEDIA_PLAYER_REGISTRY: LazyLock<MediaPlayerRegistry> =
    LazyLock::new(MediaPlayerRegistry::new);

/// Registry key for looking up media player instances.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct MediaPlayerKey {
    pub flow_id: FlowId,
    pub block_id: String,
}

/// Runtime state for a media player instance.
pub struct MediaPlayerState {
    /// Unique instance ID (to detect stale timers after restart)
    pub instance_id: Uuid,
    /// Weak reference to the uridecodebin element (uses GLib's ref counting)
    pub uridecodebin: gst::glib::WeakRef<gst::Element>,
    /// Weak reference to the pipeline (for seeking) - set when bus handler connects
    pub pipeline: RwLock<gst::glib::WeakRef<gst::Pipeline>>,
    /// Current playlist of file URIs
    pub playlist: RwLock<Vec<String>>,
    /// Current file index
    pub current_index: AtomicUsize,
    /// Whether playback is paused
    pub is_paused: AtomicBool,
    /// Whether to loop the playlist
    pub loop_playlist: AtomicBool,
    /// Block ID for event broadcasting
    pub block_id: String,
    /// Flow ID for event broadcasting
    pub flow_id: FlowId,
    /// Whether video pad has been linked (reset on file switch)
    pub video_linked: AtomicBool,
    /// Whether audio pad has been linked (reset on file switch)
    pub audio_linked: AtomicBool,
}

impl MediaPlayerState {
    /// Get the current file URI, if any.
    pub fn current_file(&self) -> Option<String> {
        let playlist = self.playlist.read().ok()?;
        let index = self.current_index.load(Ordering::SeqCst);
        playlist.get(index).cloned()
    }

    /// Get the number of files in the playlist.
    pub fn playlist_len(&self) -> usize {
        self.playlist.read().map(|p| p.len()).unwrap_or(0)
    }

    /// Set the playlist.
    pub fn set_playlist(&self, files: Vec<String>) {
        if let Ok(mut playlist) = self.playlist.write() {
            *playlist = files;
        }
    }

    /// Go to a specific file index.
    pub fn goto(&self, index: usize) -> Result<(), String> {
        let playlist = self.playlist.read().map_err(|e| e.to_string())?;
        if index >= playlist.len() {
            return Err(format!(
                "Index {} out of range (playlist has {} files)",
                index,
                playlist.len()
            ));
        }
        drop(playlist);

        self.current_index.store(index, Ordering::SeqCst);
        self.load_current_file()
    }

    /// Go to the next file.
    pub fn next(&self) -> Result<(), String> {
        let playlist_len = self.playlist_len();
        if playlist_len == 0 {
            return Err("Playlist is empty".to_string());
        }

        let current = self.current_index.load(Ordering::SeqCst);
        let next = if current + 1 >= playlist_len {
            if self.loop_playlist.load(Ordering::SeqCst) {
                0
            } else {
                return Err("Already at last file".to_string());
            }
        } else {
            current + 1
        };

        self.current_index.store(next, Ordering::SeqCst);
        self.load_current_file()
    }

    /// Go to the previous file.
    pub fn previous(&self) -> Result<(), String> {
        let playlist_len = self.playlist_len();
        if playlist_len == 0 {
            return Err("Playlist is empty".to_string());
        }

        let current = self.current_index.load(Ordering::SeqCst);
        let prev = if current == 0 {
            if self.loop_playlist.load(Ordering::SeqCst) {
                playlist_len - 1
            } else {
                return Err("Already at first file".to_string());
            }
        } else {
            current - 1
        };

        self.current_index.store(prev, Ordering::SeqCst);
        self.load_current_file()
    }

    /// Load the current file into the uridecodebin.
    fn load_current_file(&self) -> Result<(), String> {
        let file_path = self.current_file().ok_or("No file to load")?;
        let uridecodebin = self
            .uridecodebin
            .upgrade()
            .ok_or("uridecodebin no longer exists")?;

        // Convert relative paths to absolute file:// URIs
        let uri = if file_path.starts_with("file://")
            || file_path.starts_with("http://")
            || file_path.starts_with("https://")
        {
            file_path
        } else {
            // Relative path - convert to absolute
            let path = std::path::Path::new(&file_path);
            if let Ok(abs_path) = path.canonicalize() {
                format!("file://{}", abs_path.display())
            } else {
                format!("file://{}", file_path)
            }
        };

        info!("Loading file: {}", uri);

        // Get the pipeline to flush and restart
        let pipeline = self.get_pipeline().ok_or("Pipeline no longer exists")?;

        // Reset linked flags so new pads get linked
        self.video_linked.store(false, Ordering::SeqCst);
        self.audio_linked.store(false, Ordering::SeqCst);

        // Set pipeline to READY to flush the old stream
        pipeline
            .set_state(gst::State::Ready)
            .map_err(|e| format!("Failed to set state to Ready: {:?}", e))?;

        // Set the new URI on uridecodebin
        uridecodebin.set_property("uri", &uri);

        // Start playing again
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("Failed to set state to Playing: {:?}", e))?;

        self.is_paused.store(false, Ordering::SeqCst);

        Ok(())
    }

    /// Set the pipeline reference (called when bus handler connects).
    pub fn set_pipeline(&self, pipeline: &gst::Pipeline) {
        if let Ok(p) = self.pipeline.write() {
            p.set(Some(pipeline));
            info!("Media Player {}: Pipeline reference set", self.block_id);
        }
    }

    /// Helper to get pipeline reference without holding lock during GStreamer operations.
    fn get_pipeline(&self) -> Option<gst::Pipeline> {
        let pipeline_guard = self.pipeline.read().ok()?;
        pipeline_guard.upgrade()
        // Lock is dropped here before returning
    }

    /// Play the media.
    pub fn play(&self) -> Result<(), String> {
        let pipeline = self.get_pipeline().ok_or("Pipeline no longer exists")?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("Failed to set state to Playing: {:?}", e))?;
        self.is_paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Pause the media.
    pub fn pause(&self) -> Result<(), String> {
        let pipeline = self.get_pipeline().ok_or("Pipeline no longer exists")?;
        pipeline
            .set_state(gst::State::Paused)
            .map_err(|e| format!("Failed to set state to Paused: {:?}", e))?;
        self.is_paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Seek to a position in nanoseconds.
    ///
    /// Pauses pipeline, seeks on pipeline, then resumes to avoid buffer burst.
    pub fn seek(&self, position_ns: u64) -> Result<(), String> {
        let pipeline = self.get_pipeline().ok_or("Pipeline no longer exists")?;

        let was_playing = !self.is_paused.load(Ordering::SeqCst);
        info!(
            "Seeking to {} ns (pause-seek-play on pipeline, was_playing={})",
            position_ns, was_playing
        );

        let pipeline_clone = pipeline.clone();
        std::thread::spawn(move || {
            // Pause pipeline first
            if was_playing {
                if let Err(e) = pipeline_clone.set_state(gst::State::Paused) {
                    error!("Failed to pause before seek: {:?}", e);
                    return;
                }
                // Wait for pause to complete
                let _ = pipeline_clone.state(gst::ClockTime::from_mseconds(100));
            }

            // FLUSH seek on pipeline - properly coordinates all elements
            match pipeline_clone.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_nseconds(position_ns),
            ) {
                Ok(_) => {
                    info!("Seek completed successfully");
                    // Resume playing
                    if was_playing {
                        if let Err(e) = pipeline_clone.set_state(gst::State::Playing) {
                            error!("Failed to resume after seek: {:?}", e);
                        } else {
                            info!("Resumed playing after seek");
                        }
                    }
                }
                Err(e) => error!("Seek failed: {:?}", e),
            }
        });

        Ok(())
    }

    /// Get current position in nanoseconds.
    pub fn position(&self) -> Option<u64> {
        let pipeline = self.get_pipeline()?;
        pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.nseconds())
    }

    /// Get duration in nanoseconds.
    pub fn duration(&self) -> Option<u64> {
        let pipeline = self.get_pipeline()?;
        pipeline
            .query_duration::<gst::ClockTime>()
            .map(|t| t.nseconds())
    }

    /// Get the current playback state as a string.
    pub fn state_string(&self) -> String {
        if self.is_paused.load(Ordering::SeqCst) {
            "paused".to_string()
        } else if self.playlist_len() == 0 {
            "stopped".to_string()
        } else {
            "playing".to_string()
        }
    }
}

/// Global registry for media player instances.
pub struct MediaPlayerRegistry {
    players: RwLock<HashMap<MediaPlayerKey, Arc<MediaPlayerState>>>,
}

impl MediaPlayerRegistry {
    pub fn new() -> Self {
        Self {
            players: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(&self, key: MediaPlayerKey, state: Arc<MediaPlayerState>) {
        if let Ok(mut players) = self.players.write() {
            players.insert(key, state);
        }
    }

    pub fn unregister(&self, key: &MediaPlayerKey) {
        if let Ok(mut players) = self.players.write() {
            players.remove(key);
        }
    }

    pub fn get(&self, key: &MediaPlayerKey) -> Option<Arc<MediaPlayerState>> {
        self.players.read().ok()?.get(key).cloned()
    }

    pub fn contains(&self, key: &MediaPlayerKey) -> bool {
        self.players
            .read()
            .ok()
            .map(|p| p.contains_key(key))
            .unwrap_or(false)
    }
}

impl Default for MediaPlayerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Media player block builder.
pub struct MediaPlayerBuilder;

impl BlockBuilder for MediaPlayerBuilder {
    fn build(
        &self,
        instance_id: &str,
        properties: &HashMap<String, PropertyValue>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        info!("Building Media Player block instance: {}", instance_id);

        // Get properties
        let loop_playlist = properties
            .get("loop_playlist")
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

        // Get injected flow_id (as string, parse to UUID)
        let flow_id: FlowId = properties
            .get("_flow_id")
            .and_then(|v| match v {
                PropertyValue::String(s) => Uuid::parse_str(s).ok(),
                _ => None,
            })
            .unwrap_or_else(Uuid::nil);

        // Block ID is the instance ID
        let block_id = instance_id.to_string();

        // Create element IDs
        let uridecodebin_id = format!("{}:uridecodebin", instance_id);
        let videoconvert_id = format!("{}:videoconvert", instance_id);
        let videoscale_id = format!("{}:videoscale", instance_id);
        let audioconvert_id = format!("{}:audioconvert", instance_id);
        let audioresample_id = format!("{}:audioresample", instance_id);

        // Create uridecodebin - handles file decoding with dynamic pads
        let uridecodebin = gst::ElementFactory::make("uridecodebin")
            .name(&uridecodebin_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("uridecodebin: {}", e)))?;

        // Create video processing chain
        let videoconvert = gst::ElementFactory::make("videoconvert")
            .name(&videoconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoconvert: {}", e)))?;

        let videoscale = gst::ElementFactory::make("videoscale")
            .name(&videoscale_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("videoscale: {}", e)))?;

        // Create audio processing chain
        let audioconvert = gst::ElementFactory::make("audioconvert")
            .name(&audioconvert_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioconvert: {}", e)))?;

        let audioresample = gst::ElementFactory::make("audioresample")
            .name(&audioresample_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audioresample: {}", e)))?;

        // Read playlist from properties (stored as JSON string)
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

        // Create shared state for the media player
        let instance_id = Uuid::new_v4();
        let uridecodebin_weak = gst::glib::WeakRef::new();
        uridecodebin_weak.set(Some(&uridecodebin));
        let state = Arc::new(MediaPlayerState {
            instance_id,
            uridecodebin: uridecodebin_weak,
            pipeline: RwLock::new(gst::glib::WeakRef::new()), // Will be set when bus handler connects
            playlist: RwLock::new(initial_playlist.clone()),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(loop_playlist),
            block_id: block_id.clone(),
            flow_id,
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
        });

        // If we have an initial playlist, set the first URI
        if !initial_playlist.is_empty() {
            if let Some(first_file) = initial_playlist.first() {
                // Convert relative paths to absolute file:// URIs
                let uri = if first_file.starts_with("file://")
                    || first_file.starts_with("http://")
                    || first_file.starts_with("https://")
                {
                    first_file.clone()
                } else {
                    // Relative path - convert to absolute
                    let path = std::path::Path::new(first_file);
                    if let Ok(abs_path) = path.canonicalize() {
                        format!("file://{}", abs_path.display())
                    } else {
                        format!("file://{}", first_file)
                    }
                };
                info!("Media Player {}: Setting initial URI: {}", instance_id, uri);
                uridecodebin.set_property("uri", &uri);
            }
        }

        // Register in global registry
        let registry_key = MediaPlayerKey {
            flow_id,
            block_id: block_id.clone(),
        };
        MEDIA_PLAYER_REGISTRY.register(registry_key, Arc::clone(&state));

        // Setup pad-added callback for dynamic pads from uridecodebin
        let videoconvert_weak = videoconvert.downgrade();
        let audioconvert_weak = audioconvert.downgrade();
        let instance_id_owned = instance_id.to_string();
        let state_for_pad_added = Arc::clone(&state);

        uridecodebin.connect_pad_added(move |_src, pad| {
            let pad_name = pad.name();
            debug!("Media Player: New pad added: {}", pad_name);

            // Get the caps to determine media type
            let caps = pad.current_caps().or_else(|| Some(pad.query_caps(None)));
            if let Some(caps) = caps {
                if let Some(structure) = caps.structure(0) {
                    let caps_name = structure.name();
                    debug!("Media Player: Pad {} has caps: {}", pad_name, caps_name);

                    if caps_name.starts_with("video/")
                        && !state_for_pad_added.video_linked.load(Ordering::SeqCst)
                    {
                        // Link video pad to videoconvert
                        if let Some(videoconvert) = videoconvert_weak.upgrade() {
                            if let Some(sink_pad) = videoconvert.static_pad("sink") {
                                match pad.link(&sink_pad) {
                                    Ok(_) => {
                                        info!(
                                            "Media Player {}: Linked video pad to videoconvert",
                                            instance_id_owned
                                        );
                                        state_for_pad_added
                                            .video_linked
                                            .store(true, Ordering::SeqCst);
                                    }
                                    Err(e) => {
                                        error!("Media Player: Failed to link video pad: {:?}", e);
                                    }
                                }
                            }
                        }
                    } else if caps_name.starts_with("audio/")
                        && !state_for_pad_added.audio_linked.load(Ordering::SeqCst)
                    {
                        // Link audio pad to audioconvert
                        if let Some(audioconvert) = audioconvert_weak.upgrade() {
                            if let Some(sink_pad) = audioconvert.static_pad("sink") {
                                match pad.link(&sink_pad) {
                                    Ok(_) => {
                                        info!(
                                            "Media Player {}: Linked audio pad to audioconvert",
                                            instance_id_owned
                                        );
                                        state_for_pad_added
                                            .audio_linked
                                            .store(true, Ordering::SeqCst);
                                    }
                                    Err(e) => {
                                        error!("Media Player: Failed to link audio pad: {:?}", e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        // Create bus message handler for position updates and EOS handling
        let state_for_handler = Arc::clone(&state);
        let block_id_for_handler = block_id.clone();
        let bus_message_handler: BusMessageConnectFn = Box::new(
            move |bus: &gst::Bus, flow_id: FlowId, events: EventBroadcaster| {
                connect_media_player_handler(
                    bus,
                    flow_id,
                    events,
                    state_for_handler,
                    block_id_for_handler,
                    position_update_interval_ms,
                )
            },
        );

        // Internal links: videoconvert -> videoscale, audioconvert -> audioresample
        let internal_links = vec![
            (
                strom_types::element::ElementPadRef::pad(&videoconvert_id, "src"),
                strom_types::element::ElementPadRef::pad(&videoscale_id, "sink"),
            ),
            (
                strom_types::element::ElementPadRef::pad(&audioconvert_id, "src"),
                strom_types::element::ElementPadRef::pad(&audioresample_id, "sink"),
            ),
        ];

        Ok(BlockBuildResult {
            elements: vec![
                (uridecodebin_id, uridecodebin),
                (videoconvert_id, videoconvert),
                (videoscale_id, videoscale),
                (audioconvert_id, audioconvert),
                (audioresample_id, audioresample),
            ],
            internal_links,
            bus_message_handler: Some(bus_message_handler),
            pad_properties: HashMap::new(),
        })
    }
}

/// Connect bus message handler for the media player.
fn connect_media_player_handler(
    bus: &gst::Bus,
    flow_id: FlowId,
    events: EventBroadcaster,
    state: Arc<MediaPlayerState>,
    block_id: String,
    position_update_interval_ms: u64,
) -> gst::glib::SignalHandlerId {
    use gst::prelude::*;
    use gst::MessageView;

    debug!("Media Player {}: Connecting bus message handler", block_id);

    // Try to get pipeline from uridecodebin element (it should be in the pipeline now)
    if let Some(uridecodebin) = state.uridecodebin.upgrade() {
        // Traverse up to find the pipeline
        let mut current: Option<gst::Object> = Some(gst::Element::clone(&uridecodebin).upcast());
        while let Some(obj) = current {
            if let Some(pipeline) = obj.downcast_ref::<gst::Pipeline>() {
                state.set_pipeline(pipeline);
                info!(
                    "Media Player {}: Pipeline reference set from uridecodebin",
                    block_id
                );
                break;
            }
            current = obj.parent();
        }
    }

    // Enable signal watch
    bus.add_signal_watch();

    // Start position polling timer
    let events_clone = events.clone();
    let state_clone = Arc::clone(&state);
    let block_id_clone = block_id.clone();
    let timer_instance_id = state.instance_id;

    // Use glib timeout to poll position periodically
    // Check if block is still in registry AND same instance to know when to stop
    let registry_key = MediaPlayerKey {
        flow_id,
        block_id: block_id_clone.clone(),
    };

    gst::glib::timeout_add(
        std::time::Duration::from_millis(position_update_interval_ms),
        move || {
            // Check if this instance is still the registered one (stops stale timers after restart)
            let is_current_instance = MEDIA_PLAYER_REGISTRY
                .get(&registry_key)
                .map(|s| s.instance_id == timer_instance_id)
                .unwrap_or(false);

            if !is_current_instance {
                debug!(
                    "Media Player {}: Instance {} no longer current, stopping position timer",
                    block_id_clone, timer_instance_id
                );
                return gst::glib::ControlFlow::Break;
            }

            let position = state_clone.position().unwrap_or(0);
            let duration = state_clone.duration().unwrap_or(0);
            let current_index = state_clone.current_index.load(Ordering::SeqCst);
            let total_files = state_clone.playlist_len();

            events_clone.broadcast(StromEvent::MediaPlayerPosition {
                flow_id,
                block_id: block_id_clone.clone(),
                position_ns: position,
                duration_ns: duration,
                current_file_index: current_index,
                total_files,
            });

            gst::glib::ControlFlow::Continue
        },
    );

    // Connect to bus messages for EOS and state changes
    let state_for_bus = Arc::clone(&state);
    let block_id_for_bus = block_id.clone();

    bus.connect_message(None, move |_bus, msg| {
        match msg.view() {
            MessageView::Eos(_) => {
                info!("Media Player {}: End of stream", block_id_for_bus);

                // Try to advance to next file
                match state_for_bus.next() {
                    Ok(_) => {
                        info!("Media Player {}: Advanced to next file", block_id_for_bus);
                    }
                    Err(e) => {
                        info!("Media Player {}: End of playlist: {}", block_id_for_bus, e);
                        // Broadcast stopped state
                        events.broadcast(StromEvent::MediaPlayerStateChanged {
                            flow_id,
                            block_id: block_id_for_bus.clone(),
                            state: "stopped".to_string(),
                            current_file: None,
                        });
                    }
                }
            }
            MessageView::StateChanged(state_msg) => {
                // Only handle messages from the pipeline itself (check type, not name)
                let is_pipeline = msg
                    .src()
                    .map(|s| s.type_() == gst::Pipeline::static_type())
                    .unwrap_or(false);
                if is_pipeline {
                    let new_state = state_msg.current();
                    let state_str = match new_state {
                        gst::State::Playing => "playing",
                        gst::State::Paused => "paused",
                        gst::State::Ready => "stopped",
                        gst::State::Null => "stopped",
                        _ => "unknown",
                    };

                    events.broadcast(StromEvent::MediaPlayerStateChanged {
                        flow_id,
                        block_id: block_id.clone(),
                        state: state_str.to_string(),
                        current_file: state_for_bus.current_file(),
                    });
                }
            }
            MessageView::Error(err) => {
                error!(
                    "Media Player {}: Error: {} ({:?})",
                    block_id,
                    err.error(),
                    err.debug()
                );
            }
            _ => {}
        }
    })
}

/// Get metadata for Media Player blocks (for UI/API).
pub fn get_blocks() -> Vec<BlockDefinition> {
    vec![media_player_definition()]
}

/// Get Media Player block definition (metadata only).
fn media_player_definition() -> BlockDefinition {
    BlockDefinition {
        id: "builtin.media_player".to_string(),
        name: "Media Player".to_string(),
        description: "Plays video and audio files with playlist support. Connect video_out and audio_out to Inter Output blocks for streaming.".to_string(),
        category: "Inputs".to_string(),
        exposed_properties: vec![
            ExposedProperty {
                name: "loop_playlist".to_string(),
                label: "Loop Playlist".to_string(),
                description: "Loop back to the first file when reaching the end of the playlist"
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(true)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "loop_playlist".to_string(),
                    transform: None,
                },
            },
            ExposedProperty {
                name: "position_update_interval".to_string(),
                label: "Position Update Interval (ms)".to_string(),
                description: "How often to broadcast position updates (lower = more responsive)"
                    .to_string(),
                property_type: PropertyType::Int,
                default_value: Some(PropertyValue::Int(200)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "position_update_interval".to_string(),
                    transform: None,
                },
            },
        ],
        external_pads: ExternalPads {
            inputs: vec![],
            outputs: vec![
                ExternalPad {
                    name: "video_out".to_string(),
                    media_type: MediaType::Video,
                    internal_element_id: "videoscale".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audioresample".to_string(),
                    internal_pad_name: "src".to_string(),
                },
            ],
        },
        built_in: true,
        ui_metadata: Some(BlockUIMetadata {
            icon: None, // Avoiding emoji as per guidelines
            width: Some(3.0),
            height: Some(2.5),
            ..Default::default()
        }),
    }
}
