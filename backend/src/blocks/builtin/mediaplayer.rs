//! Media player block for file playback with playlist support.
//!
//! Uses uridecodebin for file decoding or parsebin for passthrough of encoded streams.
//! Supports play, pause, seek, and playlist navigation.

use crate::blocks::{
    BlockBuildContext, BlockBuildError, BlockBuildResult, BlockBuilder, BusMessageConnectFn,
};
use crate::events::EventBroadcaster;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use strom_types::{block::*, FlowId, MediaType, PropertyValue, StromEvent};
use tracing::{debug, error, info};
use uuid::Uuid;

/// Normalize a file path to a proper URI.
///
/// Converts relative paths to absolute file:// URIs.
/// Passes through URIs that already have a scheme (file://, http://, https://).
fn normalize_uri(path: &str) -> String {
    if path.starts_with("file://") || path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        // Convert to absolute path
        let file_path = std::path::Path::new(path);

        // Try canonicalize first (requires file to exist)
        if let Ok(abs_path) = file_path.canonicalize() {
            return format!("file://{}", abs_path.display());
        }

        // If canonicalize fails, construct absolute path manually
        if file_path.is_relative() {
            if let Ok(cwd) = std::env::current_dir() {
                let abs_path = cwd.join(file_path);
                // Try to canonicalize parent directory at least
                if let Some(parent) = abs_path.parent() {
                    if let Ok(canonical_parent) = parent.canonicalize() {
                        if let Some(filename) = abs_path.file_name() {
                            let final_path = canonical_parent.join(filename);
                            return format!("file://{}", final_path.display());
                        }
                    }
                }
                // Fallback: just use joined path
                return format!("file://{}", abs_path.display());
            }
        }

        // Last resort: assume it's already absolute
        format!("file://{}", path)
    }
}

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
    /// Weak reference to the source element (uridecodebin or urisourcebin)
    pub source_element: gst::glib::WeakRef<gst::Element>,
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
    /// Whether to decode streams (true) or pass through encoded (false)
    pub decode: bool,
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

    /// Load the current file into the source element.
    fn load_current_file(&self) -> Result<(), String> {
        let file_path = self.current_file().ok_or("No file to load")?;
        let source_element = self
            .source_element
            .upgrade()
            .ok_or("Source element no longer exists")?;

        let uri = normalize_uri(&file_path);
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

        // Set the new URI on source element
        source_element.set_property("uri", &uri);

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
    /// Performs a flush seek on the pipeline for proper downstream handling.
    /// Uses SEGMENT seek to reset running time, which is needed for live sinks.
    pub fn seek(&self, position_ns: u64) -> Result<(), String> {
        let secs = position_ns / 1_000_000_000;
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        let secs_rem = secs % 60;
        info!(
            "Seeking to {} ns ({:02}:{:02}:{:02})",
            position_ns, hours, mins, secs_rem
        );

        // Seek on source element (urisourcebin/uridecodebin) for file playback
        let source = self
            .source_element
            .upgrade()
            .ok_or("Source element no longer exists")?;

        // For seeking with live sinks (sync=true), we need to handle the base time
        // After a flush seek, timestamps change but running time doesn't automatically adjust
        let pipeline_guard = self.pipeline.read().unwrap();
        let pipeline = pipeline_guard.upgrade();

        let seek_result = source.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_nseconds(position_ns),
        );

        match seek_result {
            Ok(_) => {
                info!("Seek completed to {} ns", position_ns);

                // Reset pipeline base time so running time aligns with new position
                // This is critical for live sinks like srtsink with sync=true
                if let Some(ref pipe) = pipeline {
                    // Set start time to NONE and reset base time to current clock time
                    // This makes running time restart from 0 after the seek
                    pipe.set_start_time(gst::ClockTime::NONE);
                    if let Some(clock) = pipe.clock() {
                        let clock_time = clock.time();
                        pipe.set_base_time(clock_time);
                        debug!("Reset pipeline base time to {:?} after seek", clock_time);
                    }
                }

                Ok(())
            }
            Err(e) => {
                error!("Seek failed: {:?}", e);
                Err(format!("Seek failed: {:?}", e))
            }
        }
    }

    /// Get current position in nanoseconds.
    pub fn position(&self) -> Option<u64> {
        // Query position from source element (playbin) rather than pipeline
        // to avoid potential issues with pipeline-level queries
        if let Some(source) = self.source_element.upgrade() {
            if let Some(position) = source.query_position::<gst::ClockTime>() {
                return Some(position.nseconds());
            }
        }

        // Fallback to pipeline query
        let pipeline = self.get_pipeline()?;
        pipeline
            .query_position::<gst::ClockTime>()
            .map(|t| t.nseconds())
    }

    /// Get duration in nanoseconds.
    pub fn duration(&self) -> Option<u64> {
        // Try pipeline query first
        if let Some(pipeline) = self.get_pipeline() {
            if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
                return Some(duration.nseconds());
            }
        }

        // Fallback: try querying the source element directly
        // This can help if dynamic linking isn't complete yet
        if let Some(source) = self.source_element.upgrade() {
            if let Some(duration) = source.query_duration::<gst::ClockTime>() {
                return Some(duration.nseconds());
            }
        }

        None
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
        _ctx: &BlockBuildContext,
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

        let decode = properties
            .get("decode")
            .and_then(|v| match v {
                PropertyValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false); // Default: passthrough (no decoding)

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

        info!(
            "Media Player {}: decode={} ({})",
            instance_id,
            decode,
            if decode {
                "decoding to raw"
            } else {
                "passthrough encoded"
            }
        );

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

        // Build elements based on decode mode
        if decode {
            // DECODE MODE: uridecodebin → videoconvert/audioconvert → videoscale/audioresample
            MediaPlayerBuilder::build_decode_mode(
                instance_id,
                &block_id,
                flow_id,
                loop_playlist,
                decode,
                position_update_interval_ms,
                initial_playlist,
            )
        } else {
            // PASSTHROUGH MODE: urisourcebin → parsebin → encoded output
            MediaPlayerBuilder::build_passthrough_mode(
                instance_id,
                &block_id,
                flow_id,
                loop_playlist,
                decode,
                position_update_interval_ms,
                initial_playlist,
            )
        }
    }
}

impl MediaPlayerBuilder {
    /// Build media player in decode mode (raw video/audio output).
    ///
    /// Uses uridecodebin which decodes to raw video/audio.
    /// Output goes through identity elements for consistent pad naming.
    fn build_decode_mode(
        instance_id: &str,
        block_id: &str,
        flow_id: FlowId,
        loop_playlist: bool,
        decode: bool,
        position_update_interval_ms: u64,
        initial_playlist: Vec<String>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        // Create element IDs - use consistent names for external pad references
        let uridecodebin_id = format!("{}:uridecodebin", instance_id);
        let video_out_id = format!("{}:video_out", instance_id);
        let audio_out_id = format!("{}:audio_out", instance_id);

        // Create uridecodebin - handles file decoding with dynamic pads
        let uridecodebin = gst::ElementFactory::make("uridecodebin")
            .name(&uridecodebin_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("uridecodebin: {}", e)))?;

        // Create identity elements for output (provides consistent named pads)
        let video_out = gst::ElementFactory::make("identity")
            .name(&video_out_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("video_out: {}", e)))?;

        let audio_out = gst::ElementFactory::make("identity")
            .name(&audio_out_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audio_out: {}", e)))?;

        // Create shared state for the media player
        let player_instance_id = Uuid::new_v4();
        let source_element_weak = gst::glib::WeakRef::new();
        source_element_weak.set(Some(&uridecodebin));
        let state = Arc::new(MediaPlayerState {
            instance_id: player_instance_id,
            source_element: source_element_weak,
            pipeline: RwLock::new(gst::glib::WeakRef::new()),
            playlist: RwLock::new(initial_playlist.clone()),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(loop_playlist),
            block_id: block_id.to_string(),
            flow_id,
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode,
        });

        // If we have an initial playlist, set the first URI
        if !initial_playlist.is_empty() {
            if let Some(first_file) = initial_playlist.first() {
                let uri = normalize_uri(first_file);
                info!("Media Player {}: Setting initial URI: {}", instance_id, uri);
                uridecodebin.set_property("uri", &uri);
            }
        }

        // Register in global registry
        let registry_key = MediaPlayerKey {
            flow_id,
            block_id: block_id.to_string(),
        };
        MEDIA_PLAYER_REGISTRY.register(registry_key, Arc::clone(&state));

        // Setup pad-added callback for dynamic pads from uridecodebin
        let video_out_weak = video_out.downgrade();
        let audio_out_weak = audio_out.downgrade();
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
                        if let Some(video_out) = video_out_weak.upgrade() {
                            if let Some(sink_pad) = video_out.static_pad("sink") {
                                match pad.link(&sink_pad) {
                                    Ok(_) => {
                                        info!(
                                            "Media Player {}: Linked video pad",
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
                        if let Some(audio_out) = audio_out_weak.upgrade() {
                            if let Some(sink_pad) = audio_out.static_pad("sink") {
                                match pad.link(&sink_pad) {
                                    Ok(_) => {
                                        info!(
                                            "Media Player {}: Linked audio pad",
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
        let block_id_for_handler = block_id.to_string();
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

        // No internal links needed - uridecodebin links dynamically to output elements
        let internal_links = vec![];

        Ok(BlockBuildResult {
            elements: vec![
                (uridecodebin_id, uridecodebin),
                (video_out_id, video_out),
                (audio_out_id, audio_out),
            ],
            internal_links,
            bus_message_handler: Some(bus_message_handler),
            pad_properties: HashMap::new(),
        })
    }

    /// Build media player in passthrough mode (encoded video/audio output).
    ///
    /// Uses urisourcebin which demuxes and parses streams internally.
    /// Output pads carry the original encoded streams (e.g., H.264, AAC).
    ///
    /// Both video and audio outputs use identity - downstream blocks handle any
    /// required parsing (e.g., MPEGTSSRT block dynamically inserts appropriate parsers).
    fn build_passthrough_mode(
        instance_id: &str,
        block_id: &str,
        flow_id: FlowId,
        loop_playlist: bool,
        decode: bool,
        position_update_interval_ms: u64,
        initial_playlist: Vec<String>,
    ) -> Result<BlockBuildResult, BlockBuildError> {
        // Create element IDs - use consistent names for external pad references
        let urisourcebin_id = format!("{}:urisourcebin", instance_id);
        let video_out_id = format!("{}:video_out", instance_id);
        let audio_out_id = format!("{}:audio_out", instance_id);

        // Create urisourcebin - reads, demuxes, and parses streams (no decoding)
        // parse-streams=true enables demuxing and outputs parsed elementary streams
        let urisourcebin = gst::ElementFactory::make("urisourcebin")
            .name(&urisourcebin_id)
            .property("parse-streams", true)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("urisourcebin: {}", e)))?;

        // Use identity for video output - downstream blocks handle any required parsing.
        // The MPEGTSSRT block dynamically inserts the appropriate parser (h264parse/h265parse)
        // based on the actual codec detected in the stream.
        let video_out = gst::ElementFactory::make("identity")
            .name(&video_out_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("video_out identity: {}", e)))?;

        // Use identity for audio output - audio codecs don't have the same format conversion issues
        let audio_out = gst::ElementFactory::make("identity")
            .name(&audio_out_id)
            .build()
            .map_err(|e| BlockBuildError::ElementCreation(format!("audio_out: {}", e)))?;

        // Create shared state for the media player
        let player_instance_id = Uuid::new_v4();
        let source_element_weak = gst::glib::WeakRef::new();
        source_element_weak.set(Some(&urisourcebin));
        let state = Arc::new(MediaPlayerState {
            instance_id: player_instance_id,
            source_element: source_element_weak,
            pipeline: RwLock::new(gst::glib::WeakRef::new()),
            playlist: RwLock::new(initial_playlist.clone()),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(loop_playlist),
            block_id: block_id.to_string(),
            flow_id,
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode,
        });

        // If we have an initial playlist, set the first URI
        if !initial_playlist.is_empty() {
            if let Some(first_file) = initial_playlist.first() {
                let uri = normalize_uri(first_file);
                info!("Media Player {}: Setting initial URI: {}", instance_id, uri);
                urisourcebin.set_property("uri", &uri);
            }
        }

        // Register in global registry
        let registry_key = MediaPlayerKey {
            flow_id,
            block_id: block_id.to_string(),
        };
        MEDIA_PLAYER_REGISTRY.register(registry_key, Arc::clone(&state));

        // Setup pad-added callback for urisourcebin -> output elements
        // urisourcebin outputs one pad per elementary stream (video, audio, etc.)
        // IMPORTANT: We must link immediately before the flow engine's pad-added handler
        // creates autotees for unlinked pads.
        let video_out_weak = video_out.downgrade();
        let audio_out_weak = audio_out.downgrade();
        let instance_id_for_source = instance_id.to_string();
        let state_for_pad_added = Arc::clone(&state);

        urisourcebin.connect_pad_added(move |_src, pad| {
            let pad_name = pad.name();
            debug!(
                "Media Player {}: urisourcebin pad added: {}",
                instance_id_for_source, pad_name
            );

            // Only handle src pads
            if pad.direction() != gst::PadDirection::Src {
                return;
            }

            // Try to get caps - first current_caps, then query_caps with filter
            let caps = pad.current_caps().or_else(|| {
                // Query caps with no filter - this should give us the actual caps
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
                instance_id_for_source, pad_name, caps_name
            );

            // Determine media type from caps
            let is_video = caps_name
                .as_ref()
                .map(|n| n.starts_with("video/"))
                .unwrap_or(false);
            let is_audio = caps_name
                .as_ref()
                .map(|n| n.starts_with("audio/"))
                .unwrap_or(false);

            // Try to link video
            if is_video && !state_for_pad_added.video_linked.load(Ordering::SeqCst) {
                if let Some(video_out) = video_out_weak.upgrade() {
                    if let Some(sink_pad) = video_out.static_pad("sink") {
                        // Log caps for debugging
                        let src_caps = pad.query_caps(None);
                        let sink_caps = sink_pad.query_caps(None);
                        debug!(
                            "Media Player {}: Attempting video link - src caps: {:?}, sink accepts: {:?}",
                            instance_id_for_source, src_caps.to_string(), sink_caps.to_string()
                        );

                        match pad.link(&sink_pad) {
                            Ok(_) => {
                                info!(
                                    "Media Player {}: Linked video pad {} ({})",
                                    instance_id_for_source,
                                    pad_name,
                                    caps_name.as_deref().unwrap_or("unknown")
                                );
                                state_for_pad_added
                                    .video_linked
                                    .store(true, Ordering::SeqCst);
                                return;
                            }
                            Err(e) => {
                                error!(
                                    "Media Player {}: Failed to link video pad {}: {:?} (src_caps={}, sink_caps={})",
                                    instance_id_for_source, pad_name, e, src_caps.to_string(), sink_caps.to_string()
                                );
                            }
                        }
                    }
                }
            }
            // Try to link audio
            else if is_audio && !state_for_pad_added.audio_linked.load(Ordering::SeqCst) {
                if let Some(audio_out) = audio_out_weak.upgrade() {
                    if let Some(sink_pad) = audio_out.static_pad("sink") {
                        match pad.link(&sink_pad) {
                            Ok(_) => {
                                info!(
                                    "Media Player {}: Linked audio pad {} ({})",
                                    instance_id_for_source,
                                    pad_name,
                                    caps_name.as_deref().unwrap_or("unknown")
                                );
                                state_for_pad_added
                                    .audio_linked
                                    .store(true, Ordering::SeqCst);
                                return;
                            }
                            Err(e) => {
                                error!(
                                    "Media Player {}: Failed to link audio pad {}: {:?}",
                                    instance_id_for_source, pad_name, e
                                );
                            }
                        }
                    }
                }
            }
            // Caps not determined - try video first (since identity accepts any caps),
            // if that's already linked, try audio
            else if !is_video && !is_audio {
                debug!(
                    "Media Player {}: Pad {} media type unknown, trying heuristic linking",
                    instance_id_for_source, pad_name
                );

                // Try video first if not already linked
                if !state_for_pad_added.video_linked.load(Ordering::SeqCst) {
                    if let Some(video_out) = video_out_weak.upgrade() {
                        if let Some(sink_pad) = video_out.static_pad("sink") {
                            match pad.link(&sink_pad) {
                                Ok(_) => {
                                    info!(
                                        "Media Player {}: Linked unknown pad {} to video_out (heuristic)",
                                        instance_id_for_source, pad_name
                                    );
                                    state_for_pad_added
                                        .video_linked
                                        .store(true, Ordering::SeqCst);
                                    return;
                                }
                                Err(_) => {
                                    // Video link failed, will try audio next
                                }
                            }
                        }
                    }
                }

                // Try audio if video already linked or failed
                if !state_for_pad_added.audio_linked.load(Ordering::SeqCst) {
                    if let Some(audio_out) = audio_out_weak.upgrade() {
                        if let Some(sink_pad) = audio_out.static_pad("sink") {
                            match pad.link(&sink_pad) {
                                Ok(_) => {
                                    info!(
                                        "Media Player {}: Linked unknown pad {} to audio_out (heuristic)",
                                        instance_id_for_source, pad_name
                                    );
                                    state_for_pad_added
                                        .audio_linked
                                        .store(true, Ordering::SeqCst);
                                    return;
                                }
                                Err(e) => {
                                    debug!(
                                        "Media Player {}: Heuristic link to audio_out also failed: {:?}",
                                        instance_id_for_source, e
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // If we get here without linking, log it (the flow engine will create an autotee)
            if !state_for_pad_added.video_linked.load(Ordering::SeqCst)
                || !state_for_pad_added.audio_linked.load(Ordering::SeqCst)
            {
                debug!(
                    "Media Player {}: Pad {} not linked (video_linked={}, audio_linked={})",
                    instance_id_for_source,
                    pad_name,
                    state_for_pad_added.video_linked.load(Ordering::SeqCst),
                    state_for_pad_added.audio_linked.load(Ordering::SeqCst)
                );
            }
        });

        // Create bus message handler
        let state_for_handler = Arc::clone(&state);
        let block_id_for_handler = block_id.to_string();
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

        // No static internal links needed - urisourcebin links dynamically to output elements
        let internal_links = vec![];

        Ok(BlockBuildResult {
            elements: vec![
                (urisourcebin_id, urisourcebin),
                (video_out_id, video_out),
                (audio_out_id, audio_out),
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

    // Try to get pipeline from source element (it should be in the pipeline now)
    if let Some(source_element) = state.source_element.upgrade() {
        // Traverse up to find the pipeline
        let mut current: Option<gst::Object> = Some(gst::Element::clone(&source_element).upcast());
        while let Some(obj) = current {
            if let Some(pipeline) = obj.downcast_ref::<gst::Pipeline>() {
                state.set_pipeline(pipeline);
                info!(
                    "Media Player {}: Pipeline reference set from source element",
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
                name: "decode".to_string(),
                label: "Decode".to_string(),
                description: "Decode to raw video/audio (true) or pass through encoded streams (false). Passthrough is more efficient for transcoding."
                    .to_string(),
                property_type: PropertyType::Bool,
                default_value: Some(PropertyValue::Bool(false)),
                mapping: PropertyMapping {
                    element_id: "_block".to_string(),
                    property_name: "decode".to_string(),
                    transform: None,
                },
            },
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
                    internal_element_id: "video_out".to_string(),
                    internal_pad_name: "src".to_string(),
                },
                ExternalPad {
                    name: "audio_out".to_string(),
                    media_type: MediaType::Audio,
                    internal_element_id: "audio_out".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_uri_file_scheme() {
        // URIs with file:// scheme should pass through unchanged
        let uri = "file:///path/to/video.mp4";
        assert_eq!(normalize_uri(uri), uri);
    }

    #[test]
    fn test_normalize_uri_http_scheme() {
        // HTTP URIs should pass through unchanged
        let uri = "http://example.com/video.mp4";
        assert_eq!(normalize_uri(uri), uri);
    }

    #[test]
    fn test_normalize_uri_https_scheme() {
        // HTTPS URIs should pass through unchanged
        let uri = "https://example.com/video.mp4";
        assert_eq!(normalize_uri(uri), uri);
    }

    #[test]
    fn test_normalize_uri_relative_path() {
        // Relative paths should get file:// prefix
        let path = "video.mp4";
        let result = normalize_uri(path);
        assert!(result.starts_with("file://"), "Should have file:// prefix");
        assert!(result.ends_with("video.mp4"), "Should end with filename");
    }

    #[test]
    fn test_normalize_uri_absolute_path() {
        // Absolute paths that don't start with file:// should get the prefix
        #[cfg(not(target_os = "windows"))]
        {
            let path = "/tmp/video.mp4";
            let result = normalize_uri(path);
            assert_eq!(result, "file:///tmp/video.mp4");
        }
        #[cfg(target_os = "windows")]
        {
            let path = "C:\\temp\\video.mp4";
            let result = normalize_uri(path);
            assert!(
                result.starts_with("file:///"),
                "Windows path should get file:/// prefix"
            );
            assert!(result.contains("video.mp4"), "Should contain filename");
        }
    }

    #[test]
    fn test_media_player_registry_basic() {
        let registry = MediaPlayerRegistry::new();

        let key = MediaPlayerKey {
            flow_id: Uuid::new_v4(),
            block_id: "test_block".to_string(),
        };

        // Initially should not contain the key
        assert!(!registry.contains(&key));
        assert!(registry.get(&key).is_none());

        // Create a minimal state for testing (without GStreamer elements)
        let weak_ref = gst::glib::WeakRef::new();
        let state = Arc::new(MediaPlayerState {
            instance_id: Uuid::new_v4(),
            source_element: weak_ref,
            pipeline: RwLock::new(gst::glib::WeakRef::new()),
            playlist: RwLock::new(vec!["file1.mp4".to_string(), "file2.mp4".to_string()]),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(true),
            block_id: "test_block".to_string(),
            flow_id: key.flow_id,
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode: false,
        });

        // Register and verify
        registry.register(key.clone(), Arc::clone(&state));
        assert!(registry.contains(&key));
        assert!(registry.get(&key).is_some());

        // Unregister and verify
        registry.unregister(&key);
        assert!(!registry.contains(&key));
        assert!(registry.get(&key).is_none());
    }

    #[test]
    fn test_media_player_state_playlist() {
        let weak_ref = gst::glib::WeakRef::new();
        let state = MediaPlayerState {
            instance_id: Uuid::new_v4(),
            source_element: weak_ref,
            pipeline: RwLock::new(gst::glib::WeakRef::new()),
            playlist: RwLock::new(vec![]),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(true),
            block_id: "test".to_string(),
            flow_id: Uuid::new_v4(),
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode: false,
        };

        // Initially empty
        assert_eq!(state.playlist_len(), 0);
        assert!(state.current_file().is_none());

        // Set playlist
        state.set_playlist(vec![
            "file1.mp4".to_string(),
            "file2.mp4".to_string(),
            "file3.mp4".to_string(),
        ]);
        assert_eq!(state.playlist_len(), 3);
        assert_eq!(state.current_file(), Some("file1.mp4".to_string()));
    }

    #[test]
    fn test_media_player_state_string() {
        let weak_ref = gst::glib::WeakRef::new();
        let state = MediaPlayerState {
            instance_id: Uuid::new_v4(),
            source_element: weak_ref,
            pipeline: RwLock::new(gst::glib::WeakRef::new()),
            playlist: RwLock::new(vec![]),
            current_index: AtomicUsize::new(0),
            is_paused: AtomicBool::new(false),
            loop_playlist: AtomicBool::new(true),
            block_id: "test".to_string(),
            flow_id: Uuid::new_v4(),
            video_linked: AtomicBool::new(false),
            audio_linked: AtomicBool::new(false),
            decode: false,
        };

        // Empty playlist = stopped
        assert_eq!(state.state_string(), "stopped");

        // With playlist, not paused = playing
        state.set_playlist(vec!["file.mp4".to_string()]);
        assert_eq!(state.state_string(), "playing");

        // Paused = paused
        state.is_paused.store(true, Ordering::SeqCst);
        assert_eq!(state.state_string(), "paused");
    }

    #[test]
    fn test_media_player_definition() {
        let def = media_player_definition();

        assert_eq!(def.id, "builtin.media_player");
        assert_eq!(def.name, "Media Player");
        assert_eq!(def.category, "Inputs");
        assert!(def.built_in);

        // Check exposed properties
        assert_eq!(def.exposed_properties.len(), 3);

        let decode_prop = def.exposed_properties.iter().find(|p| p.name == "decode");
        assert!(decode_prop.is_some());
        let decode_prop = decode_prop.unwrap();
        assert!(matches!(decode_prop.property_type, PropertyType::Bool));
        assert!(matches!(
            decode_prop.default_value,
            Some(PropertyValue::Bool(false))
        ));

        let loop_prop = def
            .exposed_properties
            .iter()
            .find(|p| p.name == "loop_playlist");
        assert!(loop_prop.is_some());

        // Check external pads
        assert_eq!(def.external_pads.inputs.len(), 0);
        assert_eq!(def.external_pads.outputs.len(), 2);

        let video_pad = def
            .external_pads
            .outputs
            .iter()
            .find(|p| p.name == "video_out");
        assert!(video_pad.is_some());

        let audio_pad = def
            .external_pads
            .outputs
            .iter()
            .find(|p| p.name == "audio_out");
        assert!(audio_pad.is_some());
    }
}
