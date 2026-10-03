//! Media player runtime state, global registry, and lifecycle methods.

use super::normalize_uri;
use super::timing::Timing;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use strom_types::FlowId;
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

/// Playlist with current index, protected by a single lock for consistency.
pub struct Playlist {
    pub files: Vec<String>,
    pub current_index: usize,
}

/// Runtime state for a media player instance.
pub struct MediaPlayerState {
    /// Unique instance ID (to detect stale timers after restart)
    pub instance_id: Uuid,
    /// Weak reference to the source element (uridecodebin or urisourcebin) in the internal pipeline
    pub source_element: gst::glib::WeakRef<gst::Element>,
    /// The isolated internal pipeline (owned by this block)
    pub internal_pipeline: RwLock<Option<gst::Pipeline>>,
    /// Video appsrcs in the main pipeline, one per video track slot (bridge targets).
    /// Slot 0 feeds `video_out`.
    pub video_appsrcs: Vec<gst_app::AppSrc>,
    /// Audio appsrcs in the main pipeline, one per audio track slot (bridge targets).
    /// Slot 0 feeds `audio_out`.
    pub audio_appsrcs: Vec<gst_app::AppSrc>,
    /// Playlist and current index (single lock for atomicity)
    pub playlist: RwLock<Playlist>,
    /// Whether playback is paused
    pub is_paused: AtomicBool,
    /// Whether to loop the playlist
    pub loop_playlist: AtomicBool,
    /// True while the player is parked on its first frame, ready to fire as a
    /// stinger with no decode latency. Cleared as soon as it plays.
    ///
    /// Armed start is ~0.5 ms and independent of resolution, because the frame
    /// is already decoded and `play()` is only a state change. Firing without
    /// arming — straight after a clip switch — costs up to a full frame at 4K,
    /// which is the whole reason this state is tracked rather than assumed.
    pub stinger_armed: AtomicBool,
    /// Block ID for event broadcasting
    pub block_id: String,
    /// Flow ID for event broadcasting
    pub flow_id: FlowId,
    /// True while load_current_file() is in progress — bus watch should ignore EOS.
    pub switching_file: AtomicBool,
    /// Which source pad holds each video slot, by pad name; `None` is free.
    /// A slot is freed when its pad goes away - an HLS variant switch replaces
    /// every stream pad - so the next pad of that kind takes over the same
    /// output. Reset on file switch.
    pub video_slots: Mutex<Vec<Option<String>>>,
    /// Which source pad holds each audio slot, as for `video_slots`.
    pub audio_slots: Mutex<Vec<Option<String>>>,
    /// Whether to decode streams (true) or pass through encoded (false)
    pub decode: bool,
    /// Whether clocksync pacing is enabled
    pub sync: bool,
    /// Configured media files directory (for resolving relative playlist paths)
    pub media_path: std::path::PathBuf,
    /// When the bridge lets buffers go and what it stamps them with, shared
    /// by every stream so audio and video stay together. Reset on file
    /// switch, seek and resume.
    pub timing: Arc<Timing>,
    /// Weak reference to the main pipeline (for computing running time in the bridge).
    pub main_pipeline: gst::glib::WeakRef<gst::Pipeline>,
    /// Handler id of the signal watch on the internal pipeline's bus.
    ///
    /// The watch's closure owns an `Arc<MediaPlayerState>`, and the bus owns the
    /// closure, and this state owns the pipeline the bus belongs to. That is a
    /// cycle: while the handler stays connected, nothing can ever drop this
    /// state, so [`Drop`] never runs and the internal pipeline keeps decoding —
    /// holding its file descriptors — for the life of the process. Keeping the
    /// id is what lets [`MediaPlayerState::shutdown`] break it.
    pub bus_watch: Mutex<Option<gst::glib::SignalHandlerId>>,
}

impl MediaPlayerState {
    /// `n` free slots.
    pub fn free_slots(n: usize) -> Mutex<Vec<Option<String>>> {
        Mutex::new(vec![None; n])
    }

    /// Run `internal` on the flow's clock and base time, so a running time
    /// means the same in both pipelines (see `timing`). Without a start time
    /// the internal pipeline keeps that base time through pause and resume.
    /// Returns false when the flow is not playing yet.
    pub fn follow_main_clock(&self, internal: &gst::Pipeline) -> bool {
        let Some(main) = self.main_pipeline.upgrade() else {
            return false;
        };
        let (Some(clock), Some(base)) = (main.clock(), main.base_time()) else {
            return false;
        };
        internal.use_clock(Some(&clock));
        internal.set_start_time(gst::ClockTime::NONE);
        internal.set_base_time(base);
        true
    }

    /// Free every slot, for a new file.
    pub fn free_all_slots(&self) {
        for slots in [&self.video_slots, &self.audio_slots] {
            let mut slots = slots.lock().unwrap_or_else(|p| p.into_inner());
            slots.iter_mut().for_each(|s| *s = None);
        }
    }

    /// Free the slot `pad_name` holds, if any.
    pub fn free_slot_of(&self, pad_name: &str) {
        for slots in [&self.video_slots, &self.audio_slots] {
            let mut slots = slots.lock().unwrap_or_else(|p| p.into_inner());
            for slot in slots.iter_mut() {
                if slot.as_deref() == Some(pad_name) {
                    *slot = None;
                }
            }
        }
    }

    /// Lift the limits of every video appsrc that waits for room, until the
    /// returned guard drops.
    ///
    /// A stinger clip's video appsrc waits instead of dropping (see
    /// `build_media_player`), which parks the internal pipeline's streaming
    /// thread in `push_sample` for as long as the flow takes nothing from it.
    /// Pausing, seeking, switching file and stopping all wait for that
    /// thread, so without this they never return while the clip's consumer
    /// is stalled or its output is wired to nothing.
    fn lift_waiting_limits(&self) -> LiftedLimits<'_> {
        let restore = self
            .video_appsrcs
            .iter()
            .filter(|src| src.property::<bool>("block"))
            .map(|src| {
                let limits = (src, src.max_bytes(), src.max_time());
                // Zero is no limit. max-time first: changing max-bytes is
                // what wakes a push waiting for room.
                src.set_max_time(gst::ClockTime::ZERO);
                src.set_max_bytes(0);
                limits
            })
            .collect();
        LiftedLimits { restore }
    }

    /// Get the current file URI, if any.
    pub fn current_file(&self) -> Option<String> {
        let pl = self.playlist.read().ok()?;
        pl.files.get(pl.current_index).cloned()
    }

    /// Get the playlist length.
    pub fn playlist_len(&self) -> usize {
        self.playlist.read().map(|pl| pl.files.len()).unwrap_or(0)
    }

    /// Get the current file index.
    pub fn current_index(&self) -> usize {
        self.playlist.read().map(|pl| pl.current_index).unwrap_or(0)
    }

    /// Get playlist snapshot (files list).
    pub fn playlist_files(&self) -> Vec<String> {
        self.playlist
            .read()
            .map(|pl| pl.files.clone())
            .unwrap_or_default()
    }

    /// Set the playlist, clamping current index to remain valid.
    pub fn set_playlist(&self, files: Vec<String>) {
        if let Ok(mut pl) = self.playlist.write() {
            if !files.is_empty() && pl.current_index >= files.len() {
                pl.current_index = 0;
            }
            pl.files = files;
        }
    }

    /// Go to a specific file index.
    pub fn goto(&self, index: usize) -> Result<(), String> {
        {
            let mut pl = self
                .playlist
                .write()
                .map_err(|e| format!("Lock error: {}", e))?;
            if index >= pl.files.len() {
                return Err(format!(
                    "Index {} out of range (playlist has {} files)",
                    index,
                    pl.files.len()
                ));
            }
            pl.current_index = index;
        }
        self.load_current_file()
    }

    /// Advance to the next file.
    pub fn next(&self) -> Result<(), String> {
        {
            let mut pl = self
                .playlist
                .write()
                .map_err(|e| format!("Lock error: {}", e))?;
            if pl.files.is_empty() {
                return Err("Playlist is empty".to_string());
            }
            let next = pl.current_index + 1;
            if next >= pl.files.len() {
                if self.loop_playlist.load(Ordering::SeqCst) {
                    pl.current_index = 0;
                } else {
                    return Err("End of playlist".to_string());
                }
            } else {
                pl.current_index = next;
            }
        }
        self.load_current_file()
    }

    /// Go to the previous file.
    pub fn previous(&self) -> Result<(), String> {
        {
            let mut pl = self
                .playlist
                .write()
                .map_err(|e| format!("Lock error: {}", e))?;
            if pl.files.is_empty() {
                return Err("Playlist is empty".to_string());
            }
            if pl.current_index == 0 {
                if self.loop_playlist.load(Ordering::SeqCst) {
                    pl.current_index = pl.files.len() - 1;
                } else {
                    return Err("Already at start of playlist".to_string());
                }
            } else {
                pl.current_index -= 1;
            }
        }
        self.load_current_file()
    }

    /// Load the current file into the internal pipeline's source element.
    ///
    /// Removes dynamically-created elements (clocksync, appsink) from the previous
    /// file, then sets the new URI and restarts. The pad-added callback will recreate
    /// the clocksync→appsink chain for the new file's pads.
    fn load_current_file(&self) -> Result<(), String> {
        self.switching_file.store(true, Ordering::SeqCst);
        let result = self.load_current_file_inner();
        self.switching_file.store(false, Ordering::SeqCst);
        result
    }

    fn load_current_file_inner(&self) -> Result<(), String> {
        let file_path = self.current_file().ok_or("No file to load")?;
        let source_element = self
            .source_element
            .upgrade()
            .ok_or("Source element no longer exists")?;

        let uri = normalize_uri(&file_path, &self.media_path);
        info!("Loading file: {}", uri);

        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Failed to lock internal pipeline: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        let _lifted = self.lift_waiting_limits();

        // Set internal pipeline to READY to flush the old stream
        pipeline.set_state(gst::State::Ready).map_err(|e| {
            error!("Failed to set internal pipeline to Ready: {:?}", e);
            "Failed to prepare pipeline for file switch".to_string()
        })?;

        // Remove dynamically-created clocksync and appsink elements from previous file.
        // The source element (uridecodebin/urisourcebin) stays — only the bridge chain
        // is recreated by pad-added when the new file starts.
        let dynamic_elements: Vec<gst::Element> = pipeline
            .iterate_elements()
            .into_iter()
            .flatten()
            .filter(|e| e.name() != source_element.name())
            .collect();
        for elem in &dynamic_elements {
            let _ = elem.set_state(gst::State::Null);
        }
        for elem in &dynamic_elements {
            let _ = pipeline.remove(elem);
        }

        // Reset linked flags and timestamp offset so new pads get linked
        // and the bridge recomputes the offset from the first buffer
        self.free_all_slots();
        self.timing.reset(None, &self.main_pipeline);

        // Set the new URI on source element
        source_element.set_property("uri", &uri);

        // Start playing again
        self.follow_main_clock(pipeline);
        pipeline.set_state(gst::State::Playing).map_err(|e| {
            error!("Failed to start internal pipeline: {:?}", e);
            "Failed to start playback".to_string()
        })?;

        self.is_paused.store(false, Ordering::SeqCst);

        Ok(())
    }

    /// Main-pipeline running time of this clip's position zero, once the bridge
    /// has taken its baseline since the last play or seek.
    ///
    /// A play or seek starts the clip at running time zero, so clip time C
    /// reaches the mixer at stream time `offset + C`, which is how a stinger
    /// works out which output frame carries its cut point. `None` until the
    /// first buffer sets it.
    pub fn stream_offset_ns(&self) -> Option<i64> {
        self.timing.stream_offset()
    }

    /// Play the media.
    pub fn play(&self) -> Result<(), String> {
        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Lock error: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        // Reset timestamp offset so the bridge recomputes from the first buffer
        // after resume — prevents accumulated drift from pause duration.
        self.timing.reset(Some(pipeline), &self.main_pipeline);
        self.follow_main_clock(pipeline);
        pipeline.set_state(gst::State::Playing).map_err(|e| {
            error!("Failed to resume playback: {:?}", e);
            "Failed to resume playback".to_string()
        })?;
        self.is_paused.store(false, Ordering::SeqCst);
        self.stinger_armed.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Pause the media.
    pub fn pause(&self) -> Result<(), String> {
        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Lock error: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        let _lifted = self.lift_waiting_limits();
        pipeline.set_state(gst::State::Paused).map_err(|e| {
            error!("Failed to pause playback: {:?}", e);
            "Failed to pause playback".to_string()
        })?;
        self.is_paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop playback: pause and seek to the beginning.
    pub fn stop(&self) -> Result<(), String> {
        self.pause()?;
        self.seek(0)?;
        Ok(())
    }

    /// Park this player on its first frame so a stinger can fire from it
    /// without paying decode latency, and stop it looping.
    ///
    /// A stinger plays once per trigger, so looping is forced off here rather
    /// than left to the block's `loop_playlist` property: a stinger clip that
    /// restarts would put a second copy on air behind the program.
    pub fn arm_stinger(&self) -> Result<(), String> {
        self.loop_playlist.store(false, Ordering::SeqCst);
        self.stop()?;
        self.stinger_armed.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Whether this player is parked on its first frame.
    pub fn is_stinger_armed(&self) -> bool {
        self.stinger_armed.load(Ordering::SeqCst)
    }

    /// Seek to a position in nanoseconds.
    ///
    /// Seeks on the internal pipeline's source element. The timestamp offset is reset
    /// so the bridge recomputes it from the first buffer at the new position.
    pub fn seek(&self, position_ns: u64) -> Result<(), String> {
        let secs = position_ns / 1_000_000_000;
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        let secs_rem = secs % 60;
        info!(
            "Seeking to {} ns ({:02}:{:02}:{:02})",
            position_ns, hours, mins, secs_rem
        );

        let source = self
            .source_element
            .upgrade()
            .ok_or("Source element no longer exists")?;

        // Reset timestamp offset so the bridge recomputes from the first buffer
        // after the seek — the file PTS jumps but main pipeline running time doesn't.
        if let Ok(guard) = self.internal_pipeline.read() {
            self.timing.reset(guard.as_ref(), &self.main_pipeline);
        }

        let _lifted = self.lift_waiting_limits();
        let seek_result = source.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_nseconds(position_ns),
        );

        match seek_result {
            Ok(_) => {
                info!("Seek completed to {} ns", position_ns);
                Ok(())
            }
            Err(e) => {
                error!("Seek failed: {:?}", e);
                Err("Seek failed".to_string())
            }
        }
    }

    /// Get current position in nanoseconds.
    pub fn position(&self) -> Option<u64> {
        if let Some(source) = self.source_element.upgrade() {
            if let Some(position) = source.query_position::<gst::ClockTime>() {
                return Some(position.nseconds());
            }
            debug!(
                "Media Player {}: Source element position query failed",
                self.block_id
            );
        }

        // Fallback: query internal pipeline
        if let Ok(guard) = self.internal_pipeline.read() {
            if let Some(ref pipeline) = *guard {
                if let Some(position) = pipeline.query_position::<gst::ClockTime>() {
                    return Some(position.nseconds());
                }
                debug!(
                    "Media Player {}: Internal pipeline position query also failed",
                    self.block_id
                );
            }
        }
        None
    }

    /// Get duration in nanoseconds.
    pub fn duration(&self) -> Option<u64> {
        // Try internal pipeline first
        if let Ok(guard) = self.internal_pipeline.read() {
            if let Some(ref pipeline) = *guard {
                if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
                    return Some(duration.nseconds());
                }
            }
        }

        // Fallback: try querying the source element directly
        if let Some(source) = self.source_element.upgrade() {
            if let Some(duration) = source.query_duration::<gst::ClockTime>() {
                return Some(duration.nseconds());
            }
        }

        None
    }

    /// Get the current playback state.
    pub fn state(&self) -> strom_types::mediaplayer::PlayerState {
        use strom_types::mediaplayer::PlayerState;
        if self.is_paused.load(Ordering::SeqCst) {
            PlayerState::Paused
        } else if self.playlist_len() == 0 {
            PlayerState::Stopped
        } else {
            PlayerState::Playing
        }
    }

    /// Release the internal pipeline and everything that keeps this state alive.
    ///
    /// Dropping the registry's `Arc` is not enough on its own: the signal watch
    /// on the internal bus holds another one, so `Drop` never runs and the
    /// pipeline decodes on, once per start, for the life of the process.
    /// Disconnecting that handler is what makes the drop reachable.
    ///
    /// The pipeline is taken out of the lock before it is stopped, so the bus
    /// callback — which reads the same lock — cannot be blocked by a
    /// `set_state(Null)` that is itself waiting for the callback's thread.
    pub fn shutdown(&self) {
        let pipeline = match self.internal_pipeline.write() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        let Some(pipeline) = pipeline else {
            return;
        };

        // Only remove the watch this state added. An unconditional
        // `remove_signal_watch()` on a bus that has none is a GStreamer
        // CRITICAL, and removing someone else's would silence their handler.
        let watch = match self.bus_watch.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let (Some(handler), Some(bus)) = (watch, pipeline.bus()) {
            bus.disconnect(handler);
            bus.remove_signal_watch();
        }

        debug!(
            "Media Player {}: Stopping internal pipeline on shutdown",
            self.block_id
        );
        let _lifted = self.lift_waiting_limits();
        let _ = pipeline.set_state(gst::State::Null);
    }
}

/// The limits [`MediaPlayerState::lift_waiting_limits`] lifted, put back on drop.
struct LiftedLimits<'a> {
    restore: Vec<(&'a gst_app::AppSrc, u64, gst::ClockTime)>,
}

impl Drop for LiftedLimits<'_> {
    fn drop(&mut self) {
        for (src, max_bytes, max_time) in &self.restore {
            src.set_max_time(*max_time);
            src.set_max_bytes(*max_bytes);
        }
    }
}

/// Fallback for a state that was dropped without going through
/// [`MediaPlayerState::shutdown`] — a build that failed before the block was
/// registered, say. Teardown goes through the registry, which shuts the state
/// down explicitly and leaves nothing for this to do.
impl Drop for MediaPlayerState {
    fn drop(&mut self) {
        if let Ok(guard) = self.internal_pipeline.read() {
            if let Some(ref pipeline) = *guard {
                debug!(
                    "Media Player {}: Stopping internal pipeline on drop",
                    self.block_id
                );
                let _lifted = self.lift_waiting_limits();
                let _ = pipeline.set_state(gst::State::Null);
            }
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
        // Take the entry out under the lock, then shut it down without holding
        // it: `shutdown()` stops a pipeline, which can block.
        let removed = self.players.write().ok().and_then(|mut p| p.remove(key));
        if let Some(state) = removed {
            state.shutdown();
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

    /// Remove all media player entries for a given flow.
    pub fn unregister_flow(&self, flow_id: &FlowId) {
        let removed: Vec<Arc<MediaPlayerState>> = match self.players.write() {
            Ok(mut players) => {
                let keys: Vec<MediaPlayerKey> = players
                    .keys()
                    .filter(|k| k.flow_id == *flow_id)
                    .cloned()
                    .collect();
                keys.iter().filter_map(|k| players.remove(k)).collect()
            }
            Err(_) => return,
        };

        if removed.is_empty() {
            return;
        }

        info!(
            "Unregistered {} media player(s) for flow {}",
            removed.len(),
            flow_id
        );
        // Outside the lock: `shutdown()` stops a pipeline, which can block.
        for state in removed {
            state.shutdown();
        }
    }
}

impl Default for MediaPlayerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
