//! Media player runtime state, global registry, and lifecycle methods.

use super::normalize_uri;
use super::timing::Timing;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, RwLock};
use strom_types::FlowId;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// How long a control call waits for the source to finish setting up the
/// stream it was last given (see [`MediaPlayerState::settle`]). A file is
/// done in milliseconds; a slow network source carries on after this.
const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Block ID for event broadcasting
    pub block_id: String,
    /// Flow ID for event broadcasting
    pub flow_id: FlowId,
    /// Held across every control call that changes the internal pipeline's
    /// state or position (file switch, play, pause, seek, shutdown), so one
    /// never starts while another is still under way. Never taken on a
    /// streaming thread: the EOS handler hands its work to a thread of its
    /// own (see `bridge::watch_internal_bus`).
    pub control: Mutex<()>,
    /// Counts file switches, seqlock style: odd while a switch is in
    /// progress, even otherwise. An EOS posted while it is odd, or under an
    /// earlier value, is the old file's and must not advance the playlist.
    pub switch_generation: AtomicU64,
    /// False from when the internal pipeline starts a new stream until its
    /// source has exposed a pad (or failed): until then the source may still
    /// be setting itself up on its streaming thread. See [`Self::settle`].
    pub source_ready: Mutex<bool>,
    /// Signalled when `source_ready` turns true.
    pub source_ready_cv: Condvar,
    /// The thread taking the internal pipeline up from READY, while it does.
    /// See [`MediaPlayerState::hold_typefind_while_starting`].
    pub starting: Mutex<Option<std::thread::ThreadId>>,
    /// Signalled when `starting` turns `None`.
    pub starting_cv: Condvar,
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
    /// Stinger clip source behaviour; off for an ordinary player.
    pub stinger: StingerPlayback,
}

/// How a stinger clip source differs from an ordinary player: it starts
/// parked on its first frame, never loops or moves on at the end of a clip,
/// and a take says when its first frame goes on air.
#[derive(Default)]
pub struct StingerPlayback {
    /// Declared a stinger clip source.
    pub enabled: bool,
    /// Set when the cued clip's first frame waits in its clocksync.
    pub park: Arc<ParkFlags>,
    /// The playlist entry the internal pipeline holds. A playlist edit can
    /// put another file at the parked index; this is what tells a cue that
    /// the parked frame is not the new entry's.
    pub loaded_file: Mutex<Option<String>>,
    /// The one-shot probe waiting for the next parked frame (see
    /// [`StingerPlayback::arm_park_probe`]), with the parked-frame count it
    /// was armed at. Taken once per cue or bridge chain, never by the probe.
    pub(super) park_probe: Mutex<Option<(gst::glib::WeakRef<gst::Pad>, gst::PadProbeId, u64)>>,
}

impl StingerPlayback {
    /// Name of the clocksync the clip's first video stream is parked in.
    pub fn park_clocksync_name(block_id: &str) -> String {
        format!("{}_clocksync_video", block_id)
    }

    /// Catch the next buffer to enter `clocksync_sink`, the video clocksync:
    /// it is the frame that parks there. The probe records it in
    /// [`StingerPlayback::park`] and removes itself, so the clip plays with no
    /// probe on its path. A cue arms one per rewind; the bridge arms one per
    /// new chain, which is how a freshly loaded clip's first frame is caught.
    ///
    /// A previous probe that has not fired yet (a cue that timed out, or the
    /// bridge's probe on a chain a cue now rewinds) is removed first. One that
    /// has fired took itself off; nothing flows while a cue arms, so it cannot
    /// be firing right now.
    pub fn arm_park_probe(&self, clocksync_sink: &gst::Pad) {
        let mut slot = self.park_probe.lock().unwrap_or_else(|p| p.into_inner());
        let seen = self.park.seen.load(Ordering::Acquire);
        if let Some((pad, id, armed_at)) = slot.take() {
            if armed_at == seen {
                if let Some(pad) = pad.upgrade() {
                    pad.remove_probe(id);
                }
            }
        }
        let park = Arc::clone(&self.park);
        let id = clocksync_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() {
                park.on_parked_frame(buffer.pts());
            }
            gst::PadProbeReturn::Remove
        });
        *slot = id.map(|id| (clocksync_sink.downgrade(), id, seen));
    }
}

/// What the one-shot park probe found: how many frames have parked, so a
/// cue can tell when its clip's first frame is there, and the PTS of the
/// latest, so a take knows where the parked frame is in the clip. Shared
/// with the probe, so the probe holds no player state.
pub struct ParkFlags {
    /// Frames parked so far: one per armed probe that fired, not one per
    /// buffer. Nothing counts the buffers of a playing clip.
    pub seen: AtomicU64,
    /// The cued clip's first frame waits in the clocksync.
    pub parked: AtomicBool,
    /// PTS of the latest parked frame, [`ParkFlags::NO_PTS`] for none.
    parked_pts: AtomicU64,
}

impl Default for ParkFlags {
    fn default() -> Self {
        Self {
            seen: AtomicU64::new(0),
            parked: AtomicBool::new(false),
            parked_pts: AtomicU64::new(Self::NO_PTS),
        }
    }
}

impl ParkFlags {
    const NO_PTS: u64 = u64::MAX;

    /// Called by the one-shot park probe for the frame that parks: two
    /// atomic writes.
    pub fn on_parked_frame(&self, pts: Option<gst::ClockTime>) {
        self.parked_pts.store(
            pts.map_or(Self::NO_PTS, |p| p.nseconds()),
            Ordering::Relaxed,
        );
        self.seen.fetch_add(1, Ordering::Release);
    }

    /// PTS of the latest frame to park in the video clocksync.
    pub fn parked_pts(&self) -> Option<gst::ClockTime> {
        Some(self.parked_pts.load(Ordering::Acquire))
            .filter(|&p| p != Self::NO_PTS)
            .map(gst::ClockTime::from_nseconds)
    }
}

/// How long a cue waits for the clip's first frame.
pub const CUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

    /// Take the control lock. A panic in another control call must not leave
    /// the player uncontrollable, so a poisoned lock is taken over.
    fn lock_control(&self) -> std::sync::MutexGuard<'_, ()> {
        self.control.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A new stream is starting: its source sets itself up from here on.
    fn expect_source(&self) {
        *self.source_ready.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }

    /// The source has set itself up, or given up trying. Called from the
    /// source's pad-added handlers and from the bus on an error or EOS.
    pub fn mark_source_ready(&self) {
        let mut ready = self.source_ready.lock().unwrap_or_else(|p| p.into_inner());
        if !*ready {
            *ready = true;
            self.source_ready_cv.notify_all();
        }
    }

    /// Wait until the internal pipeline can take another state change.
    ///
    /// Changing its state while the source is still setting up a new stream
    /// deadlocks uridecodebin3 and urisourcebin (issue #963): urisourcebin's
    /// typefind thread adds a parsebin and then brings it to its own state,
    /// and a state change that walks into that parsebin first waits for the
    /// typefind thread's stream lock, while the typefind thread waits for the
    /// parsebin's state lock. The pipeline's state is no guide: it can report
    /// a change complete while typefinding is still under way. A pad on the
    /// source is: no source exposes one before its typefinding is done.
    ///
    /// Waiting for the pipeline to preroll as well is not needed, and costs
    /// the whole playout delay on every call: the bridge's clocksync holds the
    /// first buffer back for that long.
    ///
    /// Only sources built on urisourcebin wait (uridecodebin3, and urisourcebin
    /// itself in passthrough mode). Classic uridecodebin has not shown the
    /// deadlock, and its calls do not wait.
    ///
    /// Known cost: callers hold the control lock while this waits, up to
    /// `SETTLE_TIMEOUT`, so a slow network source queues every other control
    /// call for the player behind it.
    fn settle(&self) {
        let uses_urisourcebin = self
            .source_element
            .upgrade()
            .and_then(|source| source.factory())
            .is_some_and(|factory| factory.name() != "uridecodebin");
        if !uses_urisourcebin {
            return;
        }
        let ready = self.source_ready.lock().unwrap_or_else(|p| p.into_inner());
        let (mut ready, waited) = self
            .source_ready_cv
            .wait_timeout_while(ready, SETTLE_TIMEOUT, |ready| !*ready)
            .unwrap_or_else(|p| p.into_inner());
        if waited.timed_out() {
            warn!(
                "Media Player {}: source exposed no stream within {:?}, carrying on",
                self.block_id, SETTLE_TIMEOUT
            );
            // Wait once per stream, not once per call: a source that sets up
            // nothing (no stream it can select, a stalled server) would
            // otherwise cost every later call the full timeout.
            *ready = true;
        }
    }

    /// Hold back every typefind in `source` that finds a type while a state
    /// change takes the internal pipeline up from READY, until that change has
    /// returned (see [`Self::up_from_ready`]).
    ///
    /// Going from READY to PAUSED, urisourcebin creates its source and a
    /// typefind and starts the typefind straight away, inside that one state
    /// change. Typefind can find the type before the change is through. It
    /// then adds a parsebin on its own thread and brings it to its state,
    /// while the state change, which picks up the new parsebin as it walks
    /// urisourcebin's children, does the same from the other side: each waits
    /// for a lock the other holds, and the pipeline deadlocks (issue #963).
    /// [`Self::settle`] cannot cover this, as the race is with the state change
    /// that started the source. A pad probe cannot hold typefind back either:
    /// in pull mode it calls its source's getrange function directly.
    ///
    /// This handler is connected to `have-type` as each typefind is added,
    /// before urisourcebin connects its own, so it runs first.
    pub fn hold_typefind_while_starting(source: &gst::Element, state: &Arc<Self>) {
        let state = Arc::downgrade(state);
        source.connect("deep-element-added", false, move |args| {
            let element = args.get(2)?.get::<gst::Element>().ok()?;
            if element.factory().is_none_or(|f| f.name() != "typefind") {
                return None;
            }
            let state = state.clone();
            element.connect("have-type", false, move |_| {
                if let Some(player) = state.upgrade() {
                    player.wait_until_started();
                }
                None
            });
            None
        });
    }

    /// Wait until no state change is taking the internal pipeline up from
    /// READY, unless that change is running on this thread.
    ///
    /// Bounded: should that state change ever wait for the typefind thread
    /// (a source that fails and is torn down on the spot), waiting here for
    /// good would deadlock it.
    fn wait_until_started(&self) {
        let me = std::thread::current().id();
        let starting = self.starting.lock().unwrap_or_else(|p| p.into_inner());
        let (_starting, waited) = self
            .starting_cv
            .wait_timeout_while(starting, SETTLE_TIMEOUT, |thread| {
                thread.is_some_and(|thread| thread != me)
            })
            .unwrap_or_else(|p| p.into_inner());
        if waited.timed_out() {
            warn!(
                "Media Player {}: state change still under way after {:?}, typefind carries on",
                self.block_id, SETTLE_TIMEOUT
            );
        }
    }

    /// Change `pipeline`, at READY or below, to `target`, holding back the
    /// source's typefinding until the change has returned (see
    /// [`Self::hold_typefind_while_starting`]).
    fn up_from_ready(
        &self,
        pipeline: &gst::Pipeline,
        target: gst::State,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        *self.starting.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(std::thread::current().id());
        let result = pipeline.set_state(target);
        *self.starting.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.starting_cv.notify_all();
        result
    }

    /// Go to a specific file index.
    pub fn goto(&self, index: usize) -> Result<(), String> {
        let _control = self.lock_control();
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
        let _control = self.lock_control();
        self.next_locked()
    }

    fn next_locked(&self) -> Result<(), String> {
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

    /// The switch generation an EOS posted now belongs to, or `None` when a
    /// switch is in progress and the EOS is the old file's.
    pub fn eos_generation(&self) -> Option<u64> {
        let generation = self.switch_generation.load(Ordering::SeqCst);
        generation.is_multiple_of(2).then_some(generation)
    }

    /// Advance on an EOS that was posted under `generation`. Returns
    /// `Ok(false)` without touching anything when a file switch has happened
    /// since (that EOS ended a file that is no longer playing), when the user
    /// has paused or stopped since, or when the player has been shut down.
    pub fn advance_after_eos(&self, generation: u64) -> Result<bool, String> {
        // A stinger clip plays once per take; the take re-cues it.
        if self.stinger.enabled {
            return Ok(false);
        }
        let _control = self.lock_control();
        if self.switch_generation.load(Ordering::SeqCst) != generation
            || self.is_paused.load(Ordering::SeqCst)
            || self
                .internal_pipeline
                .read()
                .map_or(true, |guard| guard.is_none())
        {
            return Ok(false);
        }
        self.next_locked().map(|()| true)
    }

    /// Go to the previous file.
    pub fn previous(&self) -> Result<(), String> {
        let _control = self.lock_control();
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
    /// The caller holds the control lock.
    ///
    /// Removes dynamically-created elements (clocksync, appsink) from the previous
    /// file, then sets the new URI and restarts. The pad-added callback will recreate
    /// the clocksync→appsink chain for the new file's pads.
    fn load_current_file(&self) -> Result<(), String> {
        self.load_current_file_and(true)
    }

    /// Load the current file, then play it, or with `play` false only bring
    /// it up to its first frame (paused).
    fn load_current_file_and(&self, play: bool) -> Result<(), String> {
        // Odd from here: an EOS posted now is the old file's.
        let switching = self.switch_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let result = self.load_current_file_inner(play);
        // `load_current_file_inner` ends the switch once the old stream is
        // gone; this ends it on a failure before that.
        let _ = self.switch_generation.compare_exchange(
            switching,
            switching + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        result
    }

    fn load_current_file_inner(&self, play: bool) -> Result<(), String> {
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

        // Set internal pipeline to READY to flush the old stream
        self.settle();
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
        *self
            .stinger
            .loaded_file
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(file_path);

        // The old stream went at READY: an EOS posted from here on is the new
        // file's, a clip so short it ends while starting included, and must
        // advance the playlist.
        self.switch_generation.fetch_add(1, Ordering::SeqCst);

        if !play {
            self.is_paused.store(true, Ordering::SeqCst);
            self.expect_source();
            pipeline.set_state(gst::State::Paused).map_err(|e| {
                self.mark_source_ready();
                error!("Failed to pause new file: {:?}", e);
                "Failed to load file paused".to_string()
            })?;
            return Ok(());
        }
        self.is_paused.store(false, Ordering::SeqCst);
        self.start(pipeline, "Failed to start playback")
    }

    /// Start `pipeline` on the flow's clock and base time. Before the flow
    /// plays it has neither, so the pipeline only prerolls, and the flow's
    /// PLAYING handler starts it on the right clock then
    /// ([`MediaPlayerState::start_with_flow`]). Going to PLAYING on its own
    /// clock instead would keep that clock and base time for good: that
    /// handler leaves a pipeline that is already playing alone.
    ///
    /// From READY the way up stops at PAUSED until the source has set up the
    /// new stream (see [`MediaPlayerState::settle`]): one change straight to
    /// PLAYING makes its PAUSED-to-PLAYING pass while the source is still
    /// doing so, and deadlocks.
    fn start(&self, pipeline: &gst::Pipeline, failure: &str) -> Result<(), String> {
        let fail = |e: gst::StateChangeError| {
            error!("{}: {:?}", failure, e);
            // Nothing is setting up now, so nothing to wait for.
            self.mark_source_ready();
            failure.to_string()
        };
        if is_stopped(pipeline) {
            self.expect_source();
            self.up_from_ready(pipeline, gst::State::Paused)
                .map_err(fail)?;
            self.settle();
        }
        if !self.follow_main_clock(pipeline) {
            debug!(
                "Media Player {}: flow not playing yet, prerolling until it does",
                self.block_id
            );
            return Ok(());
        }
        pipeline.set_state(gst::State::Playing).map_err(fail)?;
        Ok(())
    }

    /// Start the internal pipeline on the flow's clock, now that the flow
    /// plays, unless the user paused it. Does nothing while the flow has no
    /// clock yet, or when the internal pipeline already plays.
    pub fn start_with_flow(&self) {
        let _control = self.lock_control();
        if self.is_paused.load(Ordering::SeqCst) {
            return;
        }
        let Ok(guard) = self.internal_pipeline.read() else {
            return;
        };
        let Some(pipeline) = guard.as_ref() else {
            return;
        };
        self.settle();
        if pipeline.current_state() == gst::State::Playing || !self.follow_main_clock(pipeline) {
            return;
        }
        let _ = self.start(pipeline, "Failed to start internal pipeline");
    }

    /// Play the media.
    pub fn play(&self) -> Result<(), String> {
        let _control = self.lock_control();
        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Lock error: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        self.settle();
        // Reset timestamp offset so the bridge recomputes from the first buffer
        // after resume — prevents accumulated drift from pause duration.
        self.timing.reset(Some(pipeline), &self.main_pipeline);
        self.is_paused.store(false, Ordering::SeqCst);
        self.start(pipeline, "Failed to resume playback")
    }

    /// Pause the media.
    pub fn pause(&self) -> Result<(), String> {
        let _control = self.lock_control();
        self.pause_locked()
    }

    fn pause_locked(&self) -> Result<(), String> {
        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Lock error: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        self.settle();
        let result = if is_stopped(pipeline) {
            // Pausing a pipeline that never started starts its source.
            self.expect_source();
            self.up_from_ready(pipeline, gst::State::Paused)
        } else {
            pipeline.set_state(gst::State::Paused)
        };
        result.map_err(|e| {
            self.mark_source_ready();
            error!("Failed to pause playback: {:?}", e);
            "Failed to pause playback".to_string()
        })?;
        self.is_paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop playback: pause and seek to the beginning.
    pub fn stop(&self) -> Result<(), String> {
        let _control = self.lock_control();
        self.pause_locked()?;
        self.seek_locked(0)?;
        Ok(())
    }

    /// Load playlist entry `index` and park it on its first frame, ready for
    /// a stinger take. Returns how long that took, or right away when it is
    /// already parked there.
    ///
    /// Parked means the first decoded frame waits inside its clocksync, with
    /// the internal pipeline paused: a take then only lets it go.
    pub fn cue(&self, index: usize) -> Result<std::time::Duration, String> {
        let started = std::time::Instant::now();
        let before: u64;
        {
            let _control = self.lock_control();
            if self.is_parked_on(index) {
                return Ok(std::time::Duration::ZERO);
            }
            let switch_file = {
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
                let switch = pl.current_index != index || !self.holds_file(&pl.files[index]);
                pl.current_index = index;
                switch
            };
            // A cue only runs while no clip plays (a take refuses to cue
            // over itself), so the next frame to park after this count is
            // the loaded or rewound clip's first frame.
            let park = &self.stinger.park;
            park.parked.store(false, Ordering::Release);
            before = park.seen.load(Ordering::Acquire);
            let stopped = self
                .internal_pipeline
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(is_stopped))
                .unwrap_or(true);
            let result = if switch_file || stopped {
                // Loading removes the bridge chains; the bridge arms the park
                // probe on the new video chain it builds.
                self.load_current_file_and(false)
            } else {
                // The rewound first frame enters the chain already there.
                // Before the chain exists, the bridge arms it when it builds it.
                if let Some(sink) = self.park_clocksync_sink() {
                    self.stinger.arm_park_probe(&sink);
                }
                self.pause_locked().and_then(|()| self.seek_locked(0))
            };
            result?;
        }
        // Outside the control lock: a take or a stop may come in meanwhile,
        // and the frame arrives on the internal pipeline's own thread.
        while self.stinger.park.seen.load(Ordering::Acquire) == before {
            if started.elapsed() > CUE_TIMEOUT {
                return Err(format!(
                    "clip {} decoded no frame within {:?}",
                    index, CUE_TIMEOUT
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        self.stinger.park.parked.store(true, Ordering::Release);
        self.restart_stopped_outputs();
        self.warm_up_consumer();
        Ok(started.elapsed())
    }

    /// Restart an output whose streaming stopped on an error. A consumer that
    /// refused the last clip's format (`not-negotiated`) stops the output's
    /// `appsrc`, which then sends EOS downstream, and it would stay stopped,
    /// every later clip queued and never sent, until the flow restarts. The
    /// cue of the next clip flushes it instead, which clears the EOS and
    /// starts it again; that clip then negotiates afresh. The flush is local
    /// to this output's branches: a mixer takes a flush on one sink pad
    /// without passing it on. A healthy output is left alone; this player
    /// never ends its outputs itself, so one at EOS has stopped.
    fn restart_stopped_outputs(&self) {
        for appsrc in self.video_appsrcs.iter().chain(&self.audio_appsrcs) {
            let Some(src) = appsrc.static_pad("src") else {
                continue;
            };
            let stopped = !matches!(
                src.last_flow_result(),
                Ok(_) | Err(gst::FlowError::Flushing)
            );
            if !stopped {
                continue;
            }
            info!(
                "Media Player {}: {} stopped on {:?}; restarting it for the cued clip",
                self.block_id,
                appsrc.name(),
                src.last_flow_result()
            );
            appsrc.send_event(gst::event::FlushStart::new());
            appsrc.send_event(gst::event::FlushStop::new(false));
        }
    }

    /// Send the consumer one blank frame in the cued clip's format, so that
    /// whatever it sets up for a new format (a GL upload, a shader compiled
    /// for the new size) happens now and not on the take's first frames. A
    /// vision mixer keeps its stinger pads hidden between takes, so the frame
    /// never shows. Skipped while the flow is not playing.
    fn warm_up_consumer(&self) {
        let Some(appsrc) = self.video_appsrcs.first() else {
            return;
        };
        let caps = self.internal_pipeline.read().ok().and_then(|guard| {
            let pipeline = guard.as_ref()?;
            super::timing::clocksyncs(pipeline)
                .into_iter()
                .filter_map(|c| c.static_pad("sink")?.current_caps())
                .find(|c| c.structure(0).is_some_and(|s| s.name() == "video/x-raw"))
        });
        let Some(caps) = caps else {
            return;
        };
        let Ok(info) = gstreamer_video::VideoInfo::from_caps(&caps) else {
            return;
        };
        let Some(now) = self
            .main_pipeline
            .upgrade()
            .and_then(|p| p.current_running_time())
        else {
            return;
        };
        let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; info.size()]);
        if let Some(b) = buffer.get_mut() {
            b.set_pts(now);
        }
        let sample = gst::Sample::builder().buffer(&buffer).caps(&caps).build();
        if let Err(e) = appsrc.push_sample(&sample) {
            debug!(
                "Media Player {}: stinger warm-up frame not taken: {:?}",
                self.block_id, e
            );
        }
    }

    /// Sink pad of the video clocksync a stinger clip parks in, when the
    /// internal pipeline has built it.
    fn park_clocksync_sink(&self) -> Option<gst::Pad> {
        let guard = self.internal_pipeline.read().ok()?;
        guard
            .as_ref()?
            .by_name(&StingerPlayback::park_clocksync_name(&self.block_id))?
            .static_pad("sink")
    }

    /// Whether playlist entry `index` is loaded and parked on its first frame.
    /// The loaded file is compared, not only the index: a playlist edit can
    /// leave another file at the parked index.
    pub fn is_parked_on(&self, index: usize) -> bool {
        if !self.stinger.park.parked.load(Ordering::Acquire)
            || !self.is_paused.load(Ordering::SeqCst)
        {
            return false;
        }
        let Ok(pl) = self.playlist.read() else {
            return false;
        };
        pl.current_index == index && pl.files.get(index).is_some_and(|f| self.holds_file(f))
    }

    /// Whether the internal pipeline holds playlist entry `file`.
    fn holds_file(&self, file: &str) -> bool {
        self.stinger
            .loaded_file
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_deref()
            == Some(file)
    }

    /// Play the parked clip so that its first frame lands at main-pipeline
    /// running time `start_at`, and each later one its own spacing after it.
    ///
    /// The parked frame is let go at once and stamped `start_at`; the rest
    /// leave their clocksync [`super::timing::STINGER_RELEASE_AHEAD_MS`]
    /// before they are due.
    pub fn play_at(&self, start_at: i64) -> Result<(), String> {
        let _control = self.lock_control();
        let pipeline_guard = self
            .internal_pipeline
            .read()
            .map_err(|e| format!("Lock error: {}", e))?;
        let pipeline = pipeline_guard
            .as_ref()
            .ok_or("Internal pipeline not created")?;
        self.settle();
        // The parked frame's running time pins the start to video, whichever
        // stream reaches the bridge first. Read before the clocksyncs are let
        // go, while the clocksync's segment is still the parked frame's. Not
        // parked, the first buffer to arrive takes the pin.
        let first_video = if self.stinger.park.parked.load(Ordering::Acquire) {
            self.parked_video_running_time(pipeline)
        } else {
            None
        };
        self.timing.reset(Some(pipeline), &self.main_pipeline);
        self.timing.pin_start(start_at, first_video);
        // The parked frame waits inside its clocksync and is synced against
        // whatever offset the clocksync holds when it is let go; an hour back
        // makes it due already. The first frame after it takes the real one.
        for clocksync in super::timing::clocksyncs(pipeline) {
            clocksync.set_property("ts-offset", -3_600_000_000_000i64);
        }
        self.stinger.park.parked.store(false, Ordering::Release);
        self.is_paused.store(false, Ordering::SeqCst);
        self.start(pipeline, "Failed to start stinger clip")
    }

    /// Running time of the frame parked in the video clocksync, from its PTS
    /// and the clocksync's segment: the same running time the bridge places
    /// it by.
    fn parked_video_running_time(&self, pipeline: &gst::Pipeline) -> Option<i64> {
        let pts = self.stinger.park.parked_pts()?;
        pipeline
            .by_name(&StingerPlayback::park_clocksync_name(&self.block_id))?
            .static_pad("sink")?
            .sticky_event::<gst::event::Segment>(0)?
            .segment()
            .downcast_ref::<gst::ClockTime>()?
            .to_running_time(pts)
            .map(|rt| rt.nseconds() as i64)
    }

    /// Seek to a position in nanoseconds.
    ///
    /// Seeks on the internal pipeline's source element. The timestamp offset is reset
    /// so the bridge recomputes it from the first buffer at the new position.
    pub fn seek(&self, position_ns: u64) -> Result<(), String> {
        let _control = self.lock_control();
        self.seek_locked(position_ns)
    }

    fn seek_locked(&self, position_ns: u64) -> Result<(), String> {
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
            self.settle();
            self.timing.reset(guard.as_ref(), &self.main_pipeline);
        }

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
        // Wait out a control call in progress, then for the source to finish
        // setting up: going to NULL while it does deadlocks like any other
        // state change (see `settle`).
        let _control = self.lock_control();
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
        self.settle();
        let _ = pipeline.set_state(gst::State::Null);
    }
}

/// Whether `pipeline` is at READY or below with no change under way, so that
/// taking it up starts its source on a new stream. One already on its way up
/// from READY has started its source, and `source_ready` covers it.
fn is_stopped(pipeline: &gst::Pipeline) -> bool {
    pipeline.current_state() <= gst::State::Ready
        && pipeline.pending_state() == gst::State::VoidPending
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
