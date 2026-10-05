//! WHIP session manager for per-client whipserversrc elements.
//!
//! Each WHIP client session gets its own isolated GStreamer pipeline with a
//! whipserversrc. Media is bridged to the main pipeline via appsink→appsrc,
//! where each session is assigned to a numbered slot with independent output chains.
//!
//! Dead sessions (ICE disconnect, pipeline error) are automatically cleaned up
//! via a background task that receives cleanup requests through an mpsc channel.

use crate::blocks::DynamicWebrtcbinStore;
use crate::gst::keyframe_request::VideoDamage;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use strom_types::block::StreamMode;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// One of a slot's `decodebin` elements, in the main pipeline.
#[derive(Clone)]
pub struct SlotDecodebin {
    /// Weak: the pipeline owns the element.
    pub element: gst::glib::WeakRef<gst::Element>,
    /// Give each new session on the slot a fresh decode chain instead of the
    /// one the previous session left behind. See `restart_decodebin`.
    pub restart_on_reuse: bool,
}

/// Configuration for a WHIP endpoint, registered at pipeline start.
///
/// Stores everything needed to create a new whipserversrc for each session,
/// including per-slot appsrc references for the media bridge.
pub struct WhipEndpointConfig {
    pub instance_id: String,
    pub endpoint_id: String,
    pub mode: StreamMode,
    pub stun_server: Option<String>,
    pub turn_server: Option<String>,
    pub ice_transport_policy: String,
    /// Weak ref to the pipeline
    pub pipeline_weak: gst::glib::WeakRef<gst::Pipeline>,
    /// Whether to decode RTP to raw media (true) or pass through RTP (false)
    pub decode: bool,
    /// Per-slot flag, set once the main pipeline's `decodebin` has exposed a
    /// video pad for that slot — i.e. video is genuinely being decoded.
    ///
    /// A session asks the publisher for a keyframe until this flips, because
    /// without the parameter sets that travel with a keyframe the depayloader
    /// can never produce an access unit. See `gst::keyframe_request`.
    pub video_decoding: Arc<Vec<AtomicBool>>,
    /// Per-slot flag, set by the slot's decode chain when a running session's
    /// video has lost data that only a keyframe can repair. The session asks
    /// the publisher for one; see `gst::keyframe_request::VideoDamage`.
    pub video_damage: Arc<Vec<VideoDamage>>,
    /// Jitterbuffer latency in milliseconds for the per-session webrtcbin.
    pub jitterbuffer_latency_ms: u32,
    /// Whether whipserversrc should request retransmission (NACK) of lost
    /// packets from the publisher.
    pub do_retransmission: bool,
    /// Whether the per-session rtpbin's jitterbuffers drop queued packets that
    /// exceed `jitterbuffer_latency_ms` rather than holding them.
    pub drop_on_latency: bool,
    /// Shared dynamic webrtcbin store for ICE policy tracking
    pub dynamic_webrtcbin_store: DynamicWebrtcbinStore,
    /// Maximum video bitrate hint for Chrome (kbps). Injected into the SDP
    /// answer as x-google-max-bitrate so Chrome's encoder ramps up accordingly.
    pub max_video_bitrate_kbps: u32,
    /// Maximum number of simultaneous client slots
    pub max_sessions: usize,
    /// Per-slot audio appsrc elements (main pipeline side, created at build time)
    pub slot_audio_appsrcs: Vec<gst_app::AppSrc>,
    /// Per-slot video appsrc elements (main pipeline side, created at build time)
    pub slot_video_appsrcs: Vec<gst_app::AppSrc>,
    /// Per-slot `decodebin` elements (main pipeline side, created at build
    /// time), indexed by slot. Locked while the slot has no publisher so it
    /// cannot hold the pipeline short of PLAYING; `allocate_slot` unlocks them.
    /// Empty when the endpoint runs with `decode=false`.
    pub slot_decodebins: Vec<Vec<SlotDecodebin>>,
    /// Per-slot stamps of the media coming out of that slot's chains, written
    /// by a pad probe on each output tee. The session sitting in a slot borrows
    /// them; see `SessionActivity`.
    pub slot_output: Vec<Arc<SlotOutput>>,
    /// Per-slot work queued on the slot's inputs; see `SlotInputWork`.
    pub slot_input_work: Vec<SlotInputs>,
    /// Slot assignments: slot index → Option<resource_id>
    /// Protected by RwLock for concurrent access from HTTP handlers.
    pub slot_assignments: Arc<RwLock<Vec<Option<String>>>>,
}

/// One of a slot's two inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotMedium {
    Audio,
    Video,
}

/// Drop what the slot's appsrc still holds. See `SlotInputWork`.
const INPUT_FLUSH: u8 = 1;
/// Give the input's decoder a fresh decode chain. See `restart_decodebin`.
const INPUT_RESTART: u8 = 2;

/// Changes to one of a slot's inputs — its appsrc and the `decodebin` it
/// feeds — that must wait until no buffer is in flight on the appsrc's src pad.
///
/// They are queued here and run from a single IDLE probe on that pad, so a
/// release's flush and a claim's decoder restart never run concurrently and
/// always run in the order they were asked for. `pending` holds the work,
/// `armed` says a probe is installed that will pick it up.
#[derive(Default)]
pub struct SlotInputWork {
    pending: AtomicU8,
    armed: AtomicBool,
}

/// A slot's `SlotInputWork`, one per input.
#[derive(Default)]
pub struct SlotInputs {
    pub audio: Arc<SlotInputWork>,
    pub video: Arc<SlotInputWork>,
}

impl SlotInputWork {
    /// Queue `ops` on the input fed by `appsrc`. `restart` is the slot's
    /// decodebins that `INPUT_RESTART` restarts; it only touches one fed by
    /// this appsrc.
    fn schedule(
        self: &Arc<Self>,
        appsrc: &gst_app::AppSrc,
        restart: Vec<gst::glib::WeakRef<gst::Element>>,
        slot: usize,
        ops: u8,
    ) {
        self.pending.fetch_or(ops, Ordering::SeqCst);
        if self.armed.swap(true, Ordering::SeqCst) {
            // The installed probe runs it.
            return;
        }
        let Some(src) = appsrc.static_pad("src") else {
            self.armed.store(false, Ordering::SeqCst);
            return;
        };
        // Weak: the pad belongs to the appsrc, and a strong ref in its own
        // probe would keep it alive forever.
        let appsrc_weak = appsrc.downgrade();
        let work = self.clone();
        src.add_probe(gst::PadProbeType::IDLE, move |src, _| {
            let appsrc = appsrc_weak.upgrade();
            loop {
                let ops = work.pending.swap(0, Ordering::SeqCst);
                if ops & INPUT_FLUSH != 0 {
                    if let Some(appsrc) = &appsrc {
                        flush_slot_appsrc(appsrc, src, slot);
                    }
                }
                if ops & INPUT_RESTART != 0 {
                    for decodebin in restart.iter().filter_map(|d| d.upgrade()) {
                        restart_decodebin(&decodebin, src, slot);
                    }
                }
                work.armed.store(false, Ordering::SeqCst);
                // Work queued while this ran, by a caller that saw the probe
                // armed, is still ours unless a new probe has been armed for it.
                if work.pending.load(Ordering::SeqCst) == 0
                    || work.armed.swap(true, Ordering::SeqCst)
                {
                    break;
                }
            }
            gst::PadProbeReturn::Remove
        });
    }
}

/// Drop every buffer a slot's appsrc still holds from the session that left.
///
/// When the slot's chain falls behind, the appsrc queues the publisher's media,
/// up to `APPSRC_MAX_TIME`. Left there, it drains into the next session on the
/// slot: through the decoder restarted for that session, as its first output,
/// with the previous session's timestamps.
///
/// Runs from the input's IDLE probe, so no buffer is in flight. A flush-start
/// first: it wakes the appsrc's streaming thread, which holds the stream lock
/// while it waits for data and would block the flush-stop. The flush events
/// stop at the appsrc's own pad: past it, they would flush the slot's decoder
/// and every consumer of the flow downstream of the slot's tee.
///
/// Done when the slot is released, not when the next session claims it. The
/// flush-stop restarts the appsrc's task, which renegotiates with the last
/// caps it had; here that reaches the decoder that already has them, while at
/// the claim it would configure the fresh decoder for the previous session's
/// codec before the new publisher's caps arrive.
fn flush_slot_appsrc(appsrc: &gst_app::AppSrc, src: &gst::Pad, slot: usize) {
    let queued = appsrc.current_level_buffers();
    let contain = src.add_probe(gst::PadProbeType::EVENT_FLUSH, |_, _| {
        gst::PadProbeReturn::Drop
    });
    appsrc.send_event(gst::event::FlushStart::new());
    appsrc.send_event(gst::event::FlushStop::new(false));
    if let Some(contain) = contain {
        src.remove_probe(contain);
    }
    debug!(
        "WhipEndpointConfig: Flushed {} for slot {} ({} buffer(s) queued)",
        appsrc.name(),
        slot,
        queued
    );
}

/// Replace a running `decodebin`'s decode chain with a fresh one, as on a slot
/// no session has used yet.
///
/// A kept video decoder carries the previous session's reference frames and
/// reorder queue, and its configuration from the first session's caps. On
/// macOS, VideoToolbox decodes a later Safari session slowly or not at all.
///
/// Runs from the input's IDLE probe on `src`, so no buffer is in flight. NULL
/// drops the plugged elements and the source pad; the pad-added handler links
/// the new one. The relink re-sends the sticky events (stream-start, caps,
/// segment) that the sink pad lost in NULL.
fn restart_decodebin(decodebin: &gst::Element, src: &gst::Pad, slot: usize) {
    let Some(sink) = decodebin.static_pad("sink") else {
        return;
    };
    if sink.peer().as_ref() != Some(src) {
        return;
    }
    let _ = src.unlink(&sink);
    if let Err(e) = decodebin.set_state(gst::State::Null) {
        warn!(
            "WhipEndpointConfig: Failed to stop {} for slot {}: {}",
            decodebin.name(),
            slot,
            e
        );
    }
    if let Err(e) = src.link(&sink) {
        warn!(
            "WhipEndpointConfig: Failed to relink {} for slot {}: {:?}",
            decodebin.name(),
            slot,
            e
        );
    }
    // The link asks the appsrc to renegotiate. When this runs on the appsrc's
    // own streaming thread, its next loop would do that with the previous
    // session's caps and configure the fresh decoder for them before the new
    // publisher's arrive. The appsrc negotiates each new publisher's caps from
    // its samples anyway.
    let _ = src.check_reconfigure();
    match decodebin.sync_state_with_parent() {
        Ok(()) => debug!(
            "WhipEndpointConfig: Restarted {} for slot {}",
            decodebin.name(),
            slot
        ),
        Err(e) => warn!(
            "WhipEndpointConfig: Failed to sync {} with pipeline state: {}",
            decodebin.name(),
            e
        ),
    }
}

impl WhipEndpointConfig {
    /// Allocate a free slot for a new session.
    /// Returns the slot index, or None if all slots are occupied.
    pub fn allocate_slot(&self, resource_id: &str) -> Option<usize> {
        let allocated = {
            let mut slots = self.slot_assignments.write().unwrap();
            let mut allocated = None;
            for (i, slot) in slots.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(resource_id.to_string());
                    info!(
                        "WhipEndpointConfig: Allocated slot {} for session '{}'",
                        i, resource_id
                    );
                    allocated = Some(i);
                    break;
                }
            }
            allocated
        };

        // A publisher is on its way, so the slot's decode chain can join the
        // pipeline's state changes. The SDP exchange is well ahead of the first
        // RTP packet: ICE and DTLS still have to complete before media arrives.
        if let Some(slot) = allocated {
            self.activate_slot_decoders(slot);
        }
        allocated
    }

    /// Bring a slot's `decodebin` elements into the running pipeline.
    ///
    /// They are built with their state locked (see `prepare_idle_decodebin` in
    /// the WHIP block builder). On a slot reused by a later session, a
    /// decodebin marked `restart_on_reuse` is restarted; any other is re-synced.
    fn activate_slot_decoders(&self, slot: usize) {
        let Some(decodebins) = self.slot_decodebins.get(slot) else {
            return;
        };
        for slot_decodebin in decodebins {
            let Some(decodebin) = slot_decodebin.element.upgrade() else {
                // Pipeline already torn down.
                continue;
            };
            decodebin.set_locked_state(false);
            // NULL means no session has used the slot since the flow started.
            if slot_decodebin.restart_on_reuse && decodebin.current_state() != gst::State::Null {
                if let Some(medium) = self.input_feeding(slot, &decodebin) {
                    self.schedule_input_work(slot, medium, INPUT_RESTART);
                }
                continue;
            }
            if let Err(e) = decodebin.sync_state_with_parent() {
                warn!(
                    "WhipEndpointConfig: Failed to sync {} with pipeline state: {}",
                    decodebin.name(),
                    e
                );
            } else {
                debug!(
                    "WhipEndpointConfig: Activated {} for slot {}",
                    decodebin.name(),
                    slot
                );
            }
        }
    }

    /// Release a slot when a session disconnects.
    ///
    /// `holder` is the id the slot was claimed under: the temporary id passed
    /// to `allocate_slot`, or the resource_id after `rename_slot_holder`. The
    /// slot is only freed if it is still held by `holder`, so a late release
    /// from a session that no longer owns the slot (one left over from an
    /// earlier run of the flow, say) cannot free a slot a live publisher holds.
    /// Returns whether the slot was released.
    ///
    /// A released slot's appsrcs are flushed, so media the session left queued
    /// cannot drain into the next one; see `flush_slot_appsrc`. Queued while
    /// the slot is still held, so it runs before the restart a later claim
    /// queues.
    pub fn release_slot(&self, slot: usize, holder: &str) -> bool {
        let mut slots = self.slot_assignments.write().unwrap();
        match slots.get_mut(slot) {
            Some(entry) if entry.as_deref() == Some(holder) => {
                self.schedule_input_work(slot, SlotMedium::Audio, INPUT_FLUSH);
                self.schedule_input_work(slot, SlotMedium::Video, INPUT_FLUSH);
                *entry = None;
                info!(
                    "WhipEndpointConfig: Released slot {} (was session '{}')",
                    slot, holder
                );
                true
            }
            Some(entry) => {
                warn!(
                    "WhipEndpointConfig: Not releasing slot {} for session '{}': it is held by '{}'",
                    slot,
                    holder,
                    entry.as_deref().unwrap_or("nobody")
                );
                false
            }
            None => false,
        }
    }

    /// Queue `ops` on one of a slot's inputs; see `SlotInputWork`.
    fn schedule_input_work(&self, slot: usize, medium: SlotMedium, ops: u8) {
        let (appsrcs, work) = match medium {
            SlotMedium::Audio => (
                &self.slot_audio_appsrcs,
                self.slot_input_work.get(slot).map(|w| &w.audio),
            ),
            SlotMedium::Video => (
                &self.slot_video_appsrcs,
                self.slot_input_work.get(slot).map(|w| &w.video),
            ),
        };
        let (Some(appsrc), Some(work)) = (appsrcs.get(slot), work) else {
            // No such input: the endpoint does not carry this medium.
            return;
        };
        let restart = self
            .slot_decodebins
            .get(slot)
            .into_iter()
            .flatten()
            .filter(|d| d.restart_on_reuse)
            .map(|d| d.element.clone())
            .collect();
        work.schedule(appsrc, restart, slot, ops);
    }

    /// Which of the slot's inputs feeds `decodebin`, if either does.
    fn input_feeding(&self, slot: usize, decodebin: &gst::Element) -> Option<SlotMedium> {
        let upstream = decodebin.static_pad("sink")?.peer()?;
        let feeds = |appsrcs: &[gst_app::AppSrc]| {
            appsrcs
                .get(slot)
                .and_then(|appsrc| appsrc.static_pad("src"))
                .is_some_and(|src| src == upstream)
        };
        if feeds(&self.slot_video_appsrcs) {
            Some(SlotMedium::Video)
        } else if feeds(&self.slot_audio_appsrcs) {
            Some(SlotMedium::Audio)
        } else {
            None
        }
    }

    /// Move a slot from the temporary id it was claimed under to the session's
    /// real resource_id, once the WHIP answer has revealed it. Only renames a
    /// slot still held by `from`. Returns whether it did.
    pub fn rename_slot_holder(&self, slot: usize, from: &str, to: &str) -> bool {
        let mut slots = self.slot_assignments.write().unwrap();
        match slots.get_mut(slot) {
            Some(entry) if entry.as_deref() == Some(from) => {
                *entry = Some(to.to_string());
                true
            }
            _ => false,
        }
    }
}

/// A config with no pipeline behind it, for tests that exercise slot and
/// session bookkeeping only.
#[cfg(test)]
impl WhipEndpointConfig {
    pub(crate) fn for_tests(endpoint_id: &str, max_sessions: usize) -> Self {
        WhipEndpointConfig {
            instance_id: "whip-input".to_string(),
            endpoint_id: endpoint_id.to_string(),
            mode: StreamMode::AudioVideo,
            stun_server: None,
            turn_server: None,
            ice_transport_policy: "all".to_string(),
            pipeline_weak: Default::default(),
            decode: true,
            video_decoding: Arc::new((0..max_sessions).map(|_| AtomicBool::new(false)).collect()),
            video_damage: Arc::new((0..max_sessions).map(|_| VideoDamage::default()).collect()),
            jitterbuffer_latency_ms: 200,
            do_retransmission: true,
            drop_on_latency: true,
            dynamic_webrtcbin_store: Arc::new(Mutex::new(HashMap::new())),
            max_video_bitrate_kbps: 4000,
            max_sessions,
            slot_audio_appsrcs: Vec::new(),
            slot_video_appsrcs: Vec::new(),
            slot_decodebins: vec![Vec::new(); max_sessions],
            slot_output: (0..max_sessions)
                .map(|_| Arc::new(SlotOutput::new(Instant::now())))
                .collect(),
            slot_input_work: (0..max_sessions).map(|_| SlotInputs::default()).collect(),
            slot_assignments: Arc::new(RwLock::new(vec![None; max_sessions])),
        }
    }
}

/// Request to clean up a dead WHIP session.
///
/// Sent from GStreamer callbacks (ICE state, bus watch) via the cleanup channel.
/// Uses `port` as the session identifier since it's known at session creation time,
/// before the resource_id is assigned.
pub struct SessionCleanupRequest {
    /// The internal port uniquely identifying the session
    pub port: u16,
    /// Why the session is being cleaned up
    pub reason: String,
}

/// One stream of buffers, reduced to when its first and most recent buffer went
/// past, as milliseconds from a fixed epoch.
///
/// Written from GStreamer's data path — an appsink callback, a pad probe — so
/// `touch` is one clock read (`Instant` takes the vDSO fast path) plus relaxed
/// atomics: no lock, no allocation, no formatting. 0 means "nothing yet", so a
/// buffer landing inside the first millisecond counts as 1.
pub struct ActivityStamp {
    epoch: Instant,
    first_ms: AtomicU64,
    last_ms: AtomicU64,
    /// The first buffer of the current run: one that came after a gap of at
    /// least `ARRIVAL_PAUSE`, or the very first. Only meaningful on a stamp
    /// with a single writer; see `since_run_start`.
    run_ms: AtomicU64,
}

/// A gap in a stream at least this long ends a run of buffers; see
/// `ActivityStamp::since_run_start`.
///
/// Longer than the gaps a stream that is still flowing leaves: Opus DTX sends
/// a packet every 400 ms through silence. A stream sparser than this (video
/// below about 1 fps) starts a new run on every buffer, so it is never judged
/// on its own; see `medium_stall`.
pub(crate) const ARRIVAL_PAUSE: Duration = Duration::from_secs(1);

impl ActivityStamp {
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            first_ms: AtomicU64::new(0),
            last_ms: AtomicU64::new(0),
            run_ms: AtomicU64::new(0),
        }
    }

    /// Stamp a buffer. Per-buffer hot path; see the type comment.
    pub fn touch(&self) {
        let ms = (self.epoch.elapsed().as_millis() as u64).max(1);
        // A load and a store, not a swap: no read-modify-write on the hot
        // path. Two writers can race here and misplace `run_ms`, which only
        // stamps with a single writer are ever asked about.
        let previous = self.last_ms.load(Ordering::Relaxed);
        self.last_ms.store(ms, Ordering::Relaxed);
        if previous == 0 || ms.saturating_sub(previous) >= ARRIVAL_PAUSE.as_millis() as u64 {
            self.run_ms.store(ms, Ordering::Relaxed);
        }
        // Only the very first buffer writes `first_ms`; every later one pays a
        // relaxed load and a branch that predicts perfectly.
        if self.first_ms.load(Ordering::Relaxed) == 0 {
            let _ = self
                .first_ms
                .compare_exchange(0, ms, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// Forget everything seen so far. Used when a new session claims a slot
    /// whose output chain outlives individual sessions.
    pub fn reset(&self) {
        self.first_ms.store(0, Ordering::Relaxed);
        self.last_ms.store(0, Ordering::Relaxed);
        self.run_ms.store(0, Ordering::Relaxed);
    }

    /// Milliseconds from `epoch` at which the most recent buffer went past,
    /// 0 if none has. Only meaningful compared against another reading of the
    /// same stamp — a value that changed between two of them is a stream that
    /// is still moving.
    pub fn last(&self) -> u64 {
        self.last_ms.load(Ordering::Relaxed)
    }

    /// Time since the most recent buffer, `None` if none has gone past.
    pub fn since_last(&self) -> Option<Duration> {
        self.since(self.last_ms.load(Ordering::Relaxed))
    }

    /// Time since the first buffer, `None` if none has gone past.
    pub fn since_first(&self) -> Option<Duration> {
        self.since(self.first_ms.load(Ordering::Relaxed))
    }

    /// Time since the first buffer of the current run, `None` if none has
    /// gone past. A stream that pauses for `ARRIVAL_PAUSE` or longer starts a
    /// new run when it resumes, so this says how long it has been flowing
    /// without a break.
    pub fn since_run_start(&self) -> Option<Duration> {
        self.since(self.run_ms.load(Ordering::Relaxed))
    }

    /// A stamp that already looks as if its first buffer went past
    /// `since_first` ago and its most recent one `since_last` ago, so a test can
    /// stand a session up mid-life without waiting out real seconds.
    #[cfg(test)]
    pub fn backdated(since_first: Duration, since_last: Duration) -> Self {
        assert!(
            since_last <= since_first,
            "the first buffer cannot be newer than the last"
        );
        // 1 ms of headroom keeps `first_ms` clear of the 0 that means "nothing
        // yet", exactly as `touch` does.
        let stamp = Self::new(Instant::now() - since_first - Duration::from_millis(1));
        stamp.first_ms.store(1, Ordering::Relaxed);
        stamp.run_ms.store(1, Ordering::Relaxed);
        stamp.last_ms.store(
            (since_first - since_last).as_millis() as u64 + 1,
            Ordering::Relaxed,
        );
        stamp
    }

    fn since(&self, ms: u64) -> Option<Duration> {
        if ms == 0 {
            return None;
        }
        Some(
            self.epoch
                .elapsed()
                .saturating_sub(Duration::from_millis(ms)),
        )
    }
}

/// What comes out of one slot's chains, one stamp per medium.
///
/// Each stamp is written by a probe on that medium's output tee. They are kept
/// apart because a slot can lose one medium and keep the other: a video decoder
/// that wedges while audio keeps flowing would otherwise look live forever.
pub struct SlotOutput {
    pub audio: Arc<ActivityStamp>,
    pub video: Arc<ActivityStamp>,
}

impl SlotOutput {
    pub fn new(epoch: Instant) -> Self {
        Self {
            audio: Arc::new(ActivityStamp::new(epoch)),
            video: Arc::new(ActivityStamp::new(epoch)),
        }
    }

    /// A slot whose audio stamp is `audio` and whose video has produced nothing.
    #[cfg(test)]
    pub fn with_audio(audio: ActivityStamp) -> Self {
        Self {
            audio: Arc::new(audio),
            video: Arc::new(ActivityStamp::new(Instant::now())),
        }
    }

    /// Forget both media; see `ActivityStamp::reset`.
    pub fn reset(&self) {
        self.audio.reset();
        self.video.reset();
    }

    /// A counter that changes whenever either medium produces a buffer. Only
    /// meaningful compared against another reading of it.
    pub fn last(&self) -> u64 {
        self.audio.last().wrapping_add(self.video.last())
    }

    /// Time since either medium last produced a buffer, `None` if neither has.
    pub fn since_last(&self) -> Option<Duration> {
        match (self.audio.since_last(), self.video.since_last()) {
            (Some(a), Some(v)) => Some(a.min(v)),
            (a, v) => a.or(v),
        }
    }
}

/// How long a slot's video may stay frozen while its frames keep arriving
/// before it counts against the session.
///
/// A damaged stream is repaired with a keyframe, and the session asks the
/// publisher for one once a second, up to ten times; see
/// `keyframe_request::RecoveryPolicy`. A seat must not be displaced while that
/// repair still has a chance. Past it, the decoder is wedged and the seat
/// shows a frozen picture for as long as its audio flows.
pub(crate) const VIDEO_REPAIR_WINDOW: Duration = Duration::from_secs(10);

/// How long one medium has kept arriving at a session while nothing of it came
/// out of the slot's chain, `None` if it must not be judged.
///
/// `None` when the medium never arrived (the publisher did not negotiate it,
/// or has not sent it yet) or only started arriving less than `DECODE_GRACE`
/// ago. Otherwise the time from the first buffer in that nothing came out
/// after to the last one in:
/// - A medium the publisher stopped sending (a camera turned off, a screen
///   share of a window nobody touches) stops adding to it.
/// - A medium that resumes after a pause of `ARRIVAL_PAUSE` or more is judged
///   from when it resumed, not from the last buffer that came out before the
///   pause: the pause was the publisher's, not a stall.
/// - Output stamped before this medium's grace ran out (the previous
///   occupant's trailing frames) is no older than the grace.
fn medium_stall(ingress: &ActivityStamp, output: &ActivityStamp) -> Option<Duration> {
    let ingress_idle = ingress.since_last()?;
    let past_grace = ingress.since_first()?.checked_sub(DECODE_GRACE)?;
    let run = ingress.since_run_start()?;
    let output_idle = output
        .since_last()
        .map_or(past_grace, |idle| idle.min(past_grace));
    Some(output_idle.min(run).saturating_sub(ingress_idle))
}

/// Which of a session's two stamps stopped moving; see `SessionActivity::idle`.
///
/// The two are different faults with different suspects, and a reap message that
/// says only "no usable media" gives an operator no way to tell a participant
/// whose laptop closed from a blocked consumer inside their own flow that is
/// costing every publisher its seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallSide {
    /// Nothing is arriving from the publisher any more. The suspect is the
    /// client or its transport.
    Ingress,
    /// Media is still arriving, but nothing usable is leaving the slot's chain.
    /// The suspect is inside the flow: a decoder that never got its keyframe, or
    /// a consumer downstream of the slot's tee blocking and backing pressure up.
    Output,
    /// One medium is still arriving, but nothing of it is leaving the slot's
    /// chain, whatever the other medium does. Named so a reap of a seat whose
    /// audio still played says it was the video.
    MediumOutput(&'static str),
}

impl std::fmt::Display for StallSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StallSide::Ingress => f.write_str("nothing arriving from the publisher"),
            StallSide::Output => {
                f.write_str("still receiving, nothing usable leaving the slot's chain")
            }
            StallSide::MediumOutput(medium) => write!(
                f,
                "still receiving {medium}, none of it leaving the slot's chain"
            ),
        }
    }
}

/// Liveness of one WHIP session, in the only terms that matter to the slot it
/// occupies: is it still producing media the flow can use?
///
/// Two kinds of stamp decide it, because arriving bytes are not the same thing
/// as usable media:
///
/// - `ingress` is stamped by the session pipeline's appsink, once per buffer
///   that crosses the appsink→appsrc bridge. It says the publisher is still
///   sending, and nothing more. `ingress_audio` and `ingress_video` count the
///   same arrivals per medium.
/// - `output` is stamped by a pad probe on each of the slot's output tees, in
///   the *main* pipeline, downstream of the slot's `decodebin`. It says frames
///   are coming out the far end and reaching the flow's consumers.
///
/// A seat can sit at the first without the second indefinitely — a decoder that
/// never gets the keyframe it needs, or a downstream consumer that blocks and
/// backs pressure up through the slot's tee — and by `ingress` alone it looks
/// perfectly healthy while producing nothing. The same holds for one medium
/// while the other flows: a video decoder that wedges leaves a frozen picture
/// behind audio that keeps playing, so each medium is judged on its own too.
///
/// Read by the session's inactivity watchdog and by
/// `allocate_slot_or_take_over`.
pub struct SessionActivity {
    ingress: ActivityStamp,
    /// The same arrivals, counting audio buffers only. Shares `ingress`'s epoch.
    /// Non-zero means this session negotiated an audio stream and it produced
    /// something, which is what makes `TAKEOVER_IDLE_THRESHOLD` a sound bound;
    /// see `has_delivered_audio`.
    ingress_audio: ActivityStamp,
    /// The same arrivals, counting video buffers only. Shares `ingress`'s epoch.
    ingress_video: ActivityStamp,
    /// Shared with the slot, not owned by the session: the slot's output chain
    /// is built once, at flow build time, and outlives the sessions that pass
    /// through it. `SessionActivity::new` resets it so one session never
    /// inherits its predecessor's liveness.
    output: Arc<SlotOutput>,
}

impl SessionActivity {
    /// `epoch` is session start; `output` is the stamps belonging to the slot
    /// this session was assigned.
    pub fn new(epoch: Instant, output: Arc<SlotOutput>) -> Self {
        // This session has to prove for itself that its media comes out of the
        // slot's chain. Frames already in flight from the previous occupant can
        // still stamp it; `idle` holds the decode grace so they cannot count
        // against it.
        output.reset();
        Self {
            ingress: ActivityStamp::new(epoch),
            ingress_audio: ActivityStamp::new(epoch),
            ingress_video: ActivityStamp::new(epoch),
            output,
        }
    }

    /// Assemble a session from stamps a test has already positioned in time.
    /// Skips the reset `new` does, which would wipe them. The session counts
    /// as carrying audio from now on, so it is in displacement range while its
    /// audio is still inside its own decode grace, and it has carried no video.
    #[cfg(test)]
    pub fn from_stamps(ingress: ActivityStamp, output: Arc<SlotOutput>) -> Self {
        let ingress_audio = ActivityStamp::new(Instant::now());
        ingress_audio.touch();
        Self {
            ingress,
            ingress_audio,
            ingress_video: ActivityStamp::new(Instant::now()),
            output,
        }
    }

    /// Assemble a session that has never carried audio, so nothing about it can
    /// be judged on a short silence; see `has_delivered_audio`.
    #[cfg(test)]
    pub fn video_only_from_stamps(ingress: ActivityStamp, output: Arc<SlotOutput>) -> Self {
        Self {
            ingress,
            ingress_audio: ActivityStamp::new(Instant::now()),
            ingress_video: ActivityStamp::new(Instant::now()),
            output,
        }
    }

    /// Assemble a session from every stamp, each already positioned in time.
    #[cfg(test)]
    pub fn from_media_stamps(
        ingress: ActivityStamp,
        ingress_audio: ActivityStamp,
        ingress_video: ActivityStamp,
        output: Arc<SlotOutput>,
    ) -> Self {
        Self {
            ingress,
            ingress_audio,
            ingress_video,
            output,
        }
    }

    /// Stamp the arrival of a buffer from the publisher. Called from the
    /// appsink callback, so this is the per-buffer hot path.
    pub fn touch_ingress(&self, is_audio: bool) {
        self.ingress.touch();
        if is_audio {
            self.ingress_audio.touch();
        } else {
            self.ingress_video.touch();
        }
    }

    /// Whether this session has ever carried an audio buffer.
    ///
    /// Only an audio-bearing session can be judged dead on a couple of seconds
    /// of silence, because only audio has a cadence tight enough for a gap that
    /// short to mean anything: ~20 ms per packet, against a video stream whose
    /// interval is whatever the publisher's encoder decided to emit. A
    /// video-only publisher of static content — a screen share of a window
    /// nobody is touching — legitimately goes seconds between frames on a
    /// perfectly healthy transport, and is indistinguishable from a dead one by
    /// any media-based signal. Such a session is left to the inactivity
    /// watchdog; see `allocate_slot_or_take_over`.
    ///
    /// Read on the arriving side, not the slot's output: the question is what
    /// the publisher negotiated.
    pub fn has_delivered_audio(&self) -> bool {
        self.ingress_audio.last() != 0
    }

    /// The slot's output counter, for comparing two readings a poll apart. A
    /// value that changed is a session that is still producing something;
    /// whether it produces every medium it sends is for `idle` to say.
    pub fn last_usable(&self) -> u64 {
        self.output.last()
    }

    /// Time since this session last produced usable media, or `None` if it must
    /// not be judged yet.
    ///
    /// `None` means one of two states that must both be left alone: nothing has
    /// arrived at all (still negotiating — ICE through a TURN relay is slow, and
    /// evicting it lets two clients take turns throwing each other off before
    /// either sends media), or the first buffers arrived less than
    /// `DECODE_GRACE` ago and the decoder may still be waiting for the keyframe
    /// that carries H.264's parameter sets.
    ///
    /// The grace holds even if the output stamps have moved. After a takeover
    /// the displaced session's frames already inside the slot's chain still
    /// cross the tee after `new` reset the stamps, and those must not start the
    /// clock on a newcomer whose own decoder has not produced anything yet.
    /// What the predecessor left queued in the slot's appsrc never gets that
    /// far: releasing the slot flushes it.
    ///
    /// Otherwise it is the stalest of:
    /// - `ingress` against the slot's output as a whole: a session is usable
    ///   only while both move, and a publisher going away freezes `ingress`
    ///   first while a stall below the decoder freezes the output first.
    /// - Each medium the publisher sends, judged on its own by `medium_stall`:
    ///   how long it kept arriving while nothing of it came out. Video is
    ///   allowed `VIDEO_REPAIR_WINDOW` first, the time keyframe repair takes to
    ///   give up. Without this a seat whose video died holds its slot for as
    ///   long as its audio flows.
    ///
    /// The `StallSide` that comes with it is for the reap log only and never
    /// changes the duration.
    pub fn idle(&self) -> Option<(Duration, StallSide)> {
        let ingress_idle = self.ingress.since_last()?;
        let past_grace = self.ingress.since_first()?.checked_sub(DECODE_GRACE)?;

        let output_idle = match self.output.since_last() {
            Some(idle) => idle,
            // Media is arriving but nothing has come out of the decode chain.
            // Past the grace this is the failure the output stamp exists to
            // catch, and the session has been useless since the grace ran out.
            None => past_grace,
        };

        // A publisher that goes away freezes `ingress` while the last buffers
        // are still draining through the slot, so it is the staler one and
        // anything close to a tie belongs to it. Only `output` being clearly
        // staler means media is still arriving, which is the case worth naming.
        let side = if output_idle > ingress_idle + STALL_SIDE_MARGIN {
            StallSide::Output
        } else {
            StallSide::Ingress
        };
        let mut idle = (ingress_idle.max(output_idle), side);

        // A medium that keeps arriving while nothing of it comes out is a stall
        // inside the flow, whatever the other medium is doing.
        let media = [
            (
                &self.ingress_audio,
                &self.output.audio,
                Duration::ZERO,
                "audio",
            ),
            (
                &self.ingress_video,
                &self.output.video,
                VIDEO_REPAIR_WINDOW,
                "video",
            ),
        ];
        for (ingress, output, allowance, medium) in media {
            if let Some(stall) = medium_stall(ingress, output) {
                let counted = stall.saturating_sub(allowance);
                if counted > idle.0 {
                    idle = (counted, StallSide::MediumOutput(medium));
                }
            }
        }
        Some(idle)
    }
}

/// An active WHIP session (one whipserversrc element per client).
/// Each session runs in its own GStreamer pipeline to isolate NiceAgent instances.
struct WhipSession {
    /// Internal port where this session's whipserversrc is listening
    port: u16,
    /// The whipserversrc element for this session
    element: gst::Element,
    /// The isolated pipeline for this session's whipserversrc
    session_pipeline: gst::Pipeline,
    /// The endpoint this session belongs to
    endpoint_id: String,
    /// The slot index assigned to this session
    slot: usize,
    /// Set once the session is finished, by whichever path tears it down.
    /// Stops the session's inactivity watchdog thread and suppresses duplicate
    /// cleanup requests. Shared with the callbacks in `whip.rs`.
    cleanup_sent: Arc<AtomicBool>,
    /// When this session last delivered media. Shared with the session's appsink
    /// callbacks; see `SessionActivity`.
    activity: Arc<SessionActivity>,
}

/// One live session and the pipeline it runs in, for callers that need to
/// inspect a seat's receive path from outside the session manager.
pub struct WhipSessionPipeline {
    /// The resource_id the session was registered under
    pub resource_id: String,
    /// The slot this session occupies on its endpoint
    pub slot: usize,
    /// The session's own pipeline
    pub pipeline: gst::Pipeline,
}

/// A freshly created WHIP session, handed to `register_session`.
pub struct NewWhipSession {
    /// The resource_id assigned by the internal whipserversrc signaller
    pub resource_id: String,
    /// Internal port where this session's whipserversrc is listening
    pub port: u16,
    /// The whipserversrc element for this session
    pub element: gst::Element,
    /// The isolated pipeline for this session's whipserversrc
    pub session_pipeline: gst::Pipeline,
    /// The endpoint this session belongs to
    pub endpoint_id: String,
    /// The slot index assigned to this session
    pub slot: usize,
    /// The endpoint config `slot` was allocated from. `register_session`
    /// refuses the session unless this is still the config registered for
    /// `endpoint_id`: a POST still in flight when its flow stopped must not
    /// join the flow's next run under the same endpoint_id.
    pub config: Arc<WhipEndpointConfig>,
    /// Shared with the session's own callbacks; see `WhipSession::cleanup_sent`.
    pub cleanup_sent: Arc<AtomicBool>,
    /// Shared with the session's appsink callbacks; see `SessionActivity`.
    pub activity: Arc<SessionActivity>,
}

/// Manages WHIP sessions across all endpoints.
///
/// Thread-safe: uses RwLock for the sessions map and read-only Arc for endpoint configs.
pub struct WhipSessionManager {
    /// endpoint_id -> config (registered at pipeline start, immutable after that)
    endpoints: RwLock<HashMap<String, Arc<WhipEndpointConfig>>>,
    /// resource_id -> session (created/removed dynamically as clients connect/disconnect)
    sessions: RwLock<HashMap<String, WhipSession>>,
    /// Channel sender for cleanup requests from GStreamer callbacks
    cleanup_tx: mpsc::UnboundedSender<SessionCleanupRequest>,
    /// Channel receiver — taken once when starting the cleanup task
    cleanup_rx: Mutex<Option<mpsc::UnboundedReceiver<SessionCleanupRequest>>>,
    /// Ports for sessions that died before register_session was called, with the
    /// time they were marked. register_session checks this map and skips
    /// registration if the port is present and the mark has not expired.
    ///
    /// Marks expire after `PENDING_CLEANUP_TTL`: session ports come from the OS
    /// ephemeral range and are recycled, so a mark that is never claimed must not
    /// poison a later, unrelated session that happens to be given the same port.
    pending_cleanup_ports: Mutex<HashMap<u16, Instant>>,
}

/// How long a pending-cleanup mark stays valid. The window it has to cover is the
/// gap between a session dying and `register_session` running for it, which is
/// sub-second in practice.
const PENDING_CLEANUP_TTL: Duration = Duration::from_secs(30);

/// How long a session may receive media without any of it coming out of its
/// slot's chain before it counts as producing nothing.
///
/// Some delay is normal: H.264 cannot be decoded until a keyframe brings its
/// parameter sets, and `decodebin` has to autoplug a decoder first. Measured
/// from the session's *first* buffer, so a session that spent a minute
/// negotiating still gets the full grace once media starts. It has to stay under
/// the watchdog's `INACTIVITY_TIMEOUT` for the watchdog to reap a session that
/// never decodes at all, which `whip.rs` asserts at compile time.
pub(crate) const DECODE_GRACE: Duration = Duration::from_secs(5);

/// How much staler `output` must be than `ingress` before a reap is blamed on
/// the flow rather than on the publisher; see `StallSide`. It moves the label
/// only, never the idle duration.
///
/// The two stamps keep separate epochs and store whole milliseconds, so
/// simultaneous events can read a millisecond apart either way, and a publisher
/// going away freezes both within one buffer of each other. The fault this
/// margin exists to name holds `ingress` live for seconds.
const STALL_SIDE_MARGIN: Duration = Duration::from_millis(100);

/// How long to wait for a session pipeline that returns ASYNC from its
/// transition to NULL. Bounded because the wait runs on a blocking thread.
const TEARDOWN_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);

/// How long a session must have gone without media before a new client is
/// allowed to take its slot.
///
/// A connected WebRTC publisher delivers audio every ~20 ms and video every
/// ~33 ms, so two seconds of nothing means the transport is gone, not that the
/// network had a bad moment. The threshold has to stay well under the session
/// watchdog's own inactivity timeout, otherwise the watchdog frees the slot
/// first and takeover buys nothing.
const TAKEOVER_IDLE_THRESHOLD: Duration = Duration::from_secs(2);

/// How long a POST may be held while the sitting session is judged. Long enough
/// for a dead session to cross `TAKEOVER_IDLE_THRESHOLD` with polls to spare; the
/// reasoning is on `allocate_slot_or_take_over`.
const TAKEOVER_WAIT: Duration = Duration::from_secs(3);

/// How often the takeover wait re-reads the slots and the sitting session's
/// buffer counter. A live publisher stamps that counter every audio packet
/// (~20 ms) and every video frame (~33 ms), so one poll is plenty to catch it
/// moving.
const TAKEOVER_POLL: Duration = Duration::from_millis(100);

/// The least-live session on an endpoint: the one a new client would displace.
struct IdlestSession {
    resource_id: String,
    port: u16,
    /// Time since it last produced usable media and which stamp froze, `None`
    /// if it must not be judged yet; see `SessionActivity::idle`.
    idle: Option<(Duration, StallSide)>,
    /// Its slot's output counter, for comparison against the previous poll.
    last_usable: u64,
    /// Another path is already tearing it down, so its slot is about to free.
    dying: bool,
    /// Whether it has ever delivered audio; see `SessionActivity::has_delivered_audio`.
    has_audio: bool,
    cleanup_sent: Arc<AtomicBool>,
}

impl WhipSessionManager {
    pub fn new() -> Self {
        let (cleanup_tx, cleanup_rx) = mpsc::unbounded_channel();
        Self {
            endpoints: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            cleanup_tx,
            cleanup_rx: Mutex::new(Some(cleanup_rx)),
            pending_cleanup_ports: Mutex::new(HashMap::new()),
        }
    }

    /// Get a clone of the cleanup channel sender.
    /// Pass this to `create_whipserversrc_for_session` so GStreamer callbacks can
    /// send cleanup requests.
    pub fn cleanup_sender(&self) -> mpsc::UnboundedSender<SessionCleanupRequest> {
        self.cleanup_tx.clone()
    }

    /// Start the background cleanup task.
    ///
    /// Receives cleanup requests from GStreamer callbacks and tears down dead sessions.
    /// Must be called once after the WhipSessionManager is created (from a tokio context).
    pub fn start_cleanup_task(self: &Arc<Self>) {
        let rx = self
            .cleanup_rx
            .lock()
            .unwrap()
            .take()
            .expect("start_cleanup_task called more than once");

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            Self::run_cleanup_loop(manager, rx).await;
        });
        info!("WhipSessionManager: Cleanup task started");
    }

    async fn run_cleanup_loop(
        manager: Arc<Self>,
        mut rx: mpsc::UnboundedReceiver<SessionCleanupRequest>,
    ) {
        while let Some(req) = rx.recv().await {
            info!(
                "WhipSessionManager: Auto-cleanup request for port {} (reason: {})",
                req.port, req.reason
            );

            // Try to find and remove the session by port
            let removed = manager.remove_session_by_port(req.port);

            match removed {
                Some((resource_id, element, session_pipeline, endpoint_id, _port, slot)) => {
                    // Release the slot
                    let webrtcbin_store =
                        if let Some(config) = manager.get_endpoint_config(&endpoint_id) {
                            config.release_slot(slot, &resource_id);
                            Some((
                                config.dynamic_webrtcbin_store.clone(),
                                config.instance_id.clone(),
                            ))
                        } else {
                            None
                        };

                    // Tear down session pipeline on a blocking thread.
                    // Keep element alive until after pipeline reaches NULL.
                    tokio::task::spawn_blocking(move || {
                        Self::teardown_session_pipeline(&session_pipeline);
                        drop(element);
                        // Remove stale webrtcbin entries so frontend stops showing dead stats
                        if let Some((store, block_id)) = webrtcbin_store {
                            Self::cleanup_dynamic_webrtcbin_store(&store, &block_id);
                        }
                    });

                    info!(
                        "WhipSessionManager: Auto-cleaned session '{}' for endpoint '{}' (slot {}, reason: {})",
                        resource_id, endpoint_id, slot, req.reason
                    );
                }
                None => {
                    // Session not registered yet (ICE failed before register_session).
                    // Mark port as pending cleanup so register_session skips it.
                    // The mark expires after PENDING_CLEANUP_TTL so it cannot poison
                    // a later session that is handed the same recycled port.
                    let mut pending = manager.pending_cleanup_ports.lock().unwrap();
                    pending.retain(|_, marked| marked.elapsed() < PENDING_CLEANUP_TTL);
                    pending.insert(req.port, Instant::now());
                    warn!(
                        "WhipSessionManager: Session on port {} not found, marked for pending cleanup (reason: {})",
                        req.port, req.reason
                    );
                }
            }
        }
        debug!("WhipSessionManager: Cleanup task exiting (channel closed)");
    }

    /// Register an endpoint configuration (called once per WHIP Input block at pipeline start).
    pub fn register_endpoint(&self, endpoint_id: String, config: WhipEndpointConfig) {
        info!(
            "WhipSessionManager: Registering endpoint '{}' (instance: {}, mode: {:?}, max_sessions: {})",
            endpoint_id, config.instance_id, config.mode, config.max_sessions
        );
        let mut endpoints = self.endpoints.write().unwrap();
        endpoints.insert(endpoint_id, Arc::new(config));
    }

    /// Get the endpoint configuration for creating new sessions.
    pub fn get_endpoint_config(&self, endpoint_id: &str) -> Option<Arc<WhipEndpointConfig>> {
        let endpoints = self.endpoints.read().unwrap();
        endpoints.get(endpoint_id).cloned()
    }

    /// Whether `config` is the one registered for its endpoint right now, as
    /// opposed to one left over from an earlier run of the flow.
    fn is_current_config(&self, config: &WhipEndpointConfig) -> bool {
        self.endpoints
            .read()
            .unwrap()
            .get(&config.endpoint_id)
            .is_some_and(|current| std::ptr::eq(Arc::as_ptr(current), config))
    }

    /// Register a new session after a whipserversrc has been created.
    ///
    /// If the session's port is in the pending_cleanup_ports set (ICE failed before
    /// registration), the session is immediately torn down instead of being registered.
    /// Returns true if registered, false if immediately cleaned up.
    ///
    /// The session is also refused when its endpoint is no longer registered, or
    /// has been registered again with a different config since the session's
    /// slot was allocated. That is a POST that was still in flight while its
    /// flow stopped: its slot belongs to a config nobody uses any more, and
    /// registering it under the flow's next run would let its watchdog release
    /// that run's slot of the same index from under a live publisher.
    pub fn register_session(&self, session: NewWhipSession) -> bool {
        let NewWhipSession {
            resource_id,
            port,
            element,
            session_pipeline,
            endpoint_id,
            slot,
            config,
            cleanup_sent,
            activity,
        } = session;

        let refuse = |why: &str| {
            // Nothing will tear this session down later, so stop its watchdog here.
            cleanup_sent.store(true, Ordering::SeqCst);
            warn!(
                "WhipSessionManager: Session '{}' on port {} for endpoint '{}' {}, tearing down immediately",
                resource_id, port, endpoint_id, why
            );
            // Release the slot on the config it was allocated from, never on
            // whatever is registered under the endpoint_id now.
            config.release_slot(slot, &resource_id);
            let pipeline = session_pipeline.clone();
            let element = element.clone();
            std::thread::spawn(move || {
                Self::teardown_session_pipeline(&pipeline);
                drop(element);
            });
            false
        };

        // Check if this port was marked for cleanup before we could register it.
        // The mark is taken under the lock; the refusal runs after it is dropped.
        let died_before_registration = {
            let mut pending = self.pending_cleanup_ports.lock().unwrap();
            pending.retain(|_, marked| marked.elapsed() < PENDING_CLEANUP_TTL);
            pending.remove(&port).is_some()
        };
        if died_before_registration {
            return refuse("died before registration");
        }

        // Held until the session is in the map, so `unregister_endpoint` either
        // runs first (and the session is refused here) or after (and sweeps it).
        let endpoints = self.endpoints.read().unwrap();
        match endpoints.get(&endpoint_id) {
            Some(current) if Arc::ptr_eq(current, &config) => {}
            Some(_) => {
                drop(endpoints);
                return refuse("belongs to an earlier run of its endpoint");
            }
            None => {
                drop(endpoints);
                return refuse("outlived its endpoint");
            }
        }

        info!(
            "WhipSessionManager: Registering session '{}' on port {} for endpoint '{}' (slot {})",
            resource_id, port, endpoint_id, slot
        );
        let mut sessions = self.sessions.write().unwrap();
        sessions.insert(
            resource_id,
            WhipSession {
                port,
                element,
                session_pipeline,
                endpoint_id,
                slot,
                cleanup_sent,
                activity,
            },
        );
        true
    }

    /// Allocate a slot for a new client, displacing a session that is no longer
    /// producing usable media if the endpoint is full.
    ///
    /// Two ways a seat stops being worth its slot: the publisher dies without
    /// sending a WHIP DELETE (network loss, the common case for a real
    /// participant), or its media keeps arriving while nothing usable comes out
    /// the far end — a decoder that never gets its keyframe, or a consumer
    /// downstream of the slot's tee that blocks and backs pressure up the chain.
    /// `SessionActivity` covers both.
    ///
    /// When all slots are taken, this watches the sitting session for up to
    /// `TAKEOVER_WAIT` instead of refusing outright. A session whose idle time
    /// is past `TAKEOVER_IDLE_THRESHOLD` is displaced at once; that includes a
    /// seat that still produces audio while its video has been frozen past
    /// `VIDEO_REPAIR_WINDOW`, see `SessionActivity::idle`. Otherwise a session
    /// whose output counter is still moving is producing for real and is never
    /// touched: the new client gets its 503 as soon as the counter is seen to
    /// move, which is a poll interval, not a wait. A counter frozen past
    /// `TAKEOVER_IDLE_THRESHOLD` means the seat is dead, and the session is
    /// handed to the ordinary cleanup path so the new client can take the slot
    /// it releases.
    ///
    /// Both cases start out looking identical — a reconnect that lands 300 ms
    /// after the drop sees the same near-zero idle time as a healthy stream —
    /// which is why the decision is made on the counter moving rather than on a
    /// single reading of it.
    ///
    /// Only a session that has delivered audio is ever displaced. Audio is what
    /// makes `TAKEOVER_IDLE_THRESHOLD` mean anything; a video-only session can
    /// sit that long between frames while its publisher is healthy, so it is
    /// left to the inactivity watchdog and the new client gets a 503 at once. The
    /// residual case is a session whose audio stopped for good — a muted or
    /// failed microphone — while its video continues at gaps wider than the
    /// threshold: that one can still be displaced.
    pub async fn allocate_slot_or_take_over(
        &self,
        config: &WhipEndpointConfig,
        resource_id: &str,
    ) -> Option<usize> {
        let deadline = Instant::now() + TAKEOVER_WAIT;
        // The candidate's output counter as of the previous poll, so this poll
        // can tell whether it moved.
        let mut previous: Option<(String, u64)> = None;

        loop {
            if let Some(slot) = config.allocate_slot(resource_id) {
                return Some(slot);
            }

            // A POST that fetched its config before the flow stopped must not
            // judge the sessions of the flow's next run, which share the
            // endpoint_id: displacing one would cost a live run a session to
            // seat a POST that `register_session` will refuse anyway.
            if !self.is_current_config(config) {
                return None;
            }

            // Full. Nothing registered on this endpoint means the slots are held
            // by sessions still being set up: there is nothing to displace.
            let candidate = self.idlest_session(&config.endpoint_id)?;

            if candidate.dying {
                // Another path is already tearing it down. Wait for the slot it
                // is about to release rather than asking for cleanup twice.
            } else if !candidate.has_audio {
                // Sessions with audio sort first, so no session on this endpoint
                // can be displaced. One that starts delivering audio while we
                // wait moves its counter, which refuses too, so answer now.
                return None;
            } else if let Some((idle, side)) = candidate
                .idle
                .filter(|(idle, _)| *idle >= TAKEOVER_IDLE_THRESHOLD)
            {
                // Win the flag every other teardown path uses, so the session is
                // cleaned up exactly once and its watchdog thread stops. The
                // cleanup task is what releases the slot; the next poll takes it.
                if !candidate.cleanup_sent.swap(true, Ordering::SeqCst) {
                    let idle_ms = idle.as_millis();
                    warn!(
                        "WhipSessionManager: Displacing session '{}' on port {} ({} ms without usable media: {}) so a new client can take its slot on endpoint '{}'",
                        candidate.resource_id, candidate.port, idle_ms, side, config.endpoint_id
                    );
                    let _ = self.cleanup_tx.send(SessionCleanupRequest {
                        port: candidate.port,
                        reason: format!(
                            "displaced by a new client after {} ms without usable media: {}",
                            idle_ms, side
                        ),
                    });
                }
            } else if previous.as_ref().is_some_and(|(id, last)| {
                *id == candidate.resource_id && *last != candidate.last_usable
            }) {
                // Its counter moved while we watched: media is still coming out
                // of that slot and the endpoint is genuinely full.
                return None;
            }

            if Instant::now() + TAKEOVER_POLL >= deadline {
                return None;
            }
            previous = Some((candidate.resource_id, candidate.last_usable));
            tokio::time::sleep(TAKEOVER_POLL).await;
        }
    }

    /// The session on an endpoint that has gone longest without producing usable
    /// media — the one a new client would displace. `None` if the endpoint has
    /// no registered session at all.
    ///
    /// A session already being torn down sorts first, then sessions that have
    /// delivered audio, so a sparse video-only session cannot hide a dead one
    /// behind its longer idle time.
    ///
    /// A session `SessionActivity::idle` refuses to judge sorts last and is
    /// never displaced: it is still negotiating (ICE through a TURN relay can be
    /// slow), or still inside `DECODE_GRACE`. Evicting it would let two clients
    /// take turns throwing each other off before either ever produces media.
    fn idlest_session(&self, endpoint_id: &str) -> Option<IdlestSession> {
        let sessions = self.sessions.read().unwrap();
        sessions
            .iter()
            .filter(|(_, s)| s.endpoint_id == endpoint_id)
            .map(|(resource_id, s)| IdlestSession {
                resource_id: resource_id.clone(),
                port: s.port,
                idle: s.activity.idle(),
                last_usable: s.activity.last_usable(),
                dying: s.cleanup_sent.load(Ordering::SeqCst),
                has_audio: s.activity.has_delivered_audio(),
                cleanup_sent: s.cleanup_sent.clone(),
            })
            .max_by_key(|c| (c.dying, c.has_audio, c.idle.map(|(idle, _)| idle)))
    }

    /// The live sessions on an endpoint, with the pipeline each one runs in.
    ///
    /// A WHIP session's whipserversrc lives in its own pipeline, not the
    /// flow's, so inspecting a seat's receive path needs that pipeline. Sorted
    /// by slot so repeated polls return a stable order.
    pub fn sessions_for_endpoint(&self, endpoint_id: &str) -> Vec<WhipSessionPipeline> {
        let sessions = self.sessions.read().unwrap();
        let mut found: Vec<WhipSessionPipeline> = sessions
            .iter()
            .filter(|(_, s)| s.endpoint_id == endpoint_id)
            .map(|(resource_id, s)| WhipSessionPipeline {
                resource_id: resource_id.clone(),
                slot: s.slot,
                pipeline: s.session_pipeline.clone(),
            })
            .collect();
        found.sort_by_key(|s| s.slot);
        found
    }

    /// Look up the port for a session by resource_id.
    pub fn get_session_port(&self, resource_id: &str) -> Option<u16> {
        let sessions = self.sessions.read().unwrap();
        sessions.get(resource_id).map(|s| s.port)
    }

    /// Look up the port for a session, also returning the endpoint_id.
    pub fn get_session_info(&self, resource_id: &str) -> Option<(u16, String)> {
        let sessions = self.sessions.read().unwrap();
        sessions
            .get(resource_id)
            .map(|s| (s.port, s.endpoint_id.clone()))
    }

    /// Remove a session and return (element, session_pipeline, endpoint_id, port, slot) for teardown.
    pub fn remove_session(
        &self,
        resource_id: &str,
    ) -> Option<(gst::Element, gst::Pipeline, String, u16, usize)> {
        let mut sessions = self.sessions.write().unwrap();
        sessions.remove(resource_id).map(|s| {
            s.cleanup_sent.store(true, Ordering::SeqCst);
            (s.element, s.session_pipeline, s.endpoint_id, s.port, s.slot)
        })
    }

    /// Remove a session by its internal port (reverse lookup for auto-cleanup).
    /// Returns (resource_id, element, session_pipeline, endpoint_id, port, slot).
    fn remove_session_by_port(
        &self,
        port: u16,
    ) -> Option<(String, gst::Element, gst::Pipeline, String, u16, usize)> {
        let mut sessions = self.sessions.write().unwrap();
        let resource_id = sessions
            .iter()
            .find(|(_, s)| s.port == port)
            .map(|(k, _)| k.clone());

        if let Some(rid) = resource_id {
            sessions.remove(&rid).map(|s| {
                s.cleanup_sent.store(true, Ordering::SeqCst);
                (
                    rid,
                    s.element,
                    s.session_pipeline,
                    s.endpoint_id,
                    s.port,
                    s.slot,
                )
            })
        } else {
            None
        }
    }

    /// Remove all sessions for a given endpoint (called during pipeline stop).
    /// Returns (session_pipeline, element) pairs for teardown. The element must be
    /// kept alive until after the pipeline reaches NULL state.
    pub fn remove_all_sessions(&self, endpoint_id: &str) -> Vec<(gst::Pipeline, gst::Element)> {
        let mut sessions = self.sessions.write().unwrap();
        let resource_ids: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.endpoint_id == endpoint_id)
            .map(|(k, _)| k.clone())
            .collect();

        let mut result = Vec::new();
        for resource_id in &resource_ids {
            if let Some(session) = sessions.remove(resource_id) {
                info!(
                    "WhipSessionManager: Removing session '{}' for endpoint '{}'",
                    resource_id, endpoint_id
                );
                session.cleanup_sent.store(true, Ordering::SeqCst);
                result.push((session.session_pipeline, session.element));
            }
        }
        result
    }

    /// Unregister an endpoint (called during pipeline stop, after
    /// `remove_all_sessions`).
    ///
    /// Returns any session that registered between `remove_all_sessions` and
    /// this call (a POST that was still in flight), removed and ready for
    /// teardown the same way. Once this returns, `register_session` refuses
    /// every session allocated from the endpoint's config.
    pub fn unregister_endpoint(&self, endpoint_id: &str) -> Vec<(gst::Pipeline, gst::Element)> {
        info!(
            "WhipSessionManager: Unregistering endpoint '{}'",
            endpoint_id
        );
        self.endpoints.write().unwrap().remove(endpoint_id);
        self.remove_all_sessions(endpoint_id)
    }

    /// List all registered endpoint IDs.
    pub fn list_endpoints(&self) -> Vec<String> {
        let endpoints = self.endpoints.read().unwrap();
        endpoints.keys().cloned().collect()
    }

    /// Remove stale entries from the dynamic webrtcbin store for a block.
    ///
    /// After a session pipeline is set to NULL, its webrtcbin elements are dead
    /// but still referenced in the store (used for WebRTC stats in the frontend).
    /// This removes entries where the element is in NULL state.
    pub fn cleanup_dynamic_webrtcbin_store(store: &DynamicWebrtcbinStore, block_id: &str) {
        if let Ok(mut store) = store.lock() {
            if let Some(entries) = store.get_mut(block_id) {
                let before = entries.len();
                entries.retain(|(_, elem)| {
                    let (_, state, _) = elem.state(gst::ClockTime::ZERO);
                    state != gst::State::Null
                });
                let removed = before - entries.len();
                if removed > 0 {
                    debug!(
                        "WhipSessionManager: Removed {} stale webrtcbin entries for block '{}'",
                        removed, block_id
                    );
                }
            }
        }
    }

    /// Teardown a session's isolated pipeline.
    pub fn teardown_session_pipeline(session_pipeline: &gst::Pipeline) {
        let name = session_pipeline.name().to_string();
        debug!(
            "WhipSessionManager: Tearing down session pipeline '{}'",
            name
        );

        match session_pipeline.set_state(gst::State::Null) {
            // Nothing in a session pipeline goes async on the way down today,
            // but an element that did would have the pipeline dropped out from
            // under a transition still in flight, leaving its children to be
            // disposed above NULL.
            Ok(gst::StateChangeSuccess::Async) => {
                let (result, current, _) = session_pipeline.state(TEARDOWN_TIMEOUT);
                if result.is_err() || current != gst::State::Null {
                    warn!(
                        "WhipSessionManager: Session pipeline {} did not reach Null within {:?} (current: {:?})",
                        name, TEARDOWN_TIMEOUT, current
                    );
                }
            }
            Ok(_) => {}
            Err(e) => warn!(
                "WhipSessionManager: Failed to set session pipeline {} to Null: {:?}",
                name, e
            ),
        }
    }
}

impl Default for WhipSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_session() -> (gst::Element, gst::Pipeline, Arc<AtomicBool>) {
        let _ = gst::init();
        let element = gst::ElementFactory::make("fakesrc")
            .build()
            .expect("fakesrc is part of gstreamer core");
        let pipeline = gst::Pipeline::new();
        (element, pipeline, Arc::new(AtomicBool::new(false)))
    }

    /// A slot output whose audio stamp is `audio`; its video has produced
    /// nothing.
    fn out(audio: ActivityStamp) -> Arc<SlotOutput> {
        Arc::new(SlotOutput::with_audio(audio))
    }

    /// How long a fixture session has been running before the test looks at it.
    /// Comfortably past `DECODE_GRACE`, so a session that has produced nothing
    /// usable in that time is genuinely broken rather than still starting up.
    const RUNNING_FOR: Duration = Duration::from_secs(60);

    /// Tick `stamp` every 20 ms until `stop` is set — the rate the session
    /// appsink and the slot's output probe stamp a running publisher at.
    fn tick_until_stopped(stop: Arc<AtomicBool>, stamp: impl Fn() + Send + 'static) {
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                stamp();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    /// A publisher whose transport is gone: nothing has arrived and nothing has
    /// come out of its slot for `idle`. `idle` of zero is the case that matters
    /// — a client reconnecting the instant its publisher died looks, at that
    /// moment, exactly like a healthy one.
    fn dead_publisher(idle: Duration) -> Arc<SessionActivity> {
        let ran_for = RUNNING_FOR + idle;
        Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(ran_for, idle),
            out(ActivityStamp::backdated(ran_for, idle)),
        ))
    }

    /// A publisher that is still sending and whose media still comes out of its
    /// slot: both stamps tick, the way the appsink callback and the tee probe do
    /// for a running session. The caller sets `stop` to end it.
    fn live_publisher(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let output = out(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            output.clone(),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop.clone(), move || ingress.touch_ingress(true));
        tick_until_stopped(stop, move || output.audio.touch());
        activity
    }

    /// A healthy video-only publisher of static content, mid-gap: its last frame
    /// arrived `gap` ago, came out of its slot, and the next one is not due yet.
    /// Never carried audio, so nothing about it can be judged on a two-second
    /// silence.
    fn sparse_video_publisher(gap: Duration) -> Arc<SessionActivity> {
        let ran_for = RUNNING_FOR + gap;
        Arc::new(SessionActivity::video_only_from_stamps(
            ActivityStamp::backdated(ran_for, gap),
            out(ActivityStamp::backdated(ran_for, gap)),
        ))
    }

    /// A seat that has received RTP for a minute while nothing has come out of
    /// its slot's chain — the decoder never got a usable
    /// keyframe, or a consumer below the slot's tee is blocking it. Judged on
    /// arriving bytes alone this seat looks perfectly healthy.
    fn receiving_but_never_usable(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            Arc::new(SlotOutput::new(Instant::now())),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress(true));
        activity
    }

    /// A seat that decoded, then froze `stalled_for` ago, while RTP keeps
    /// arriving.
    fn receiving_but_stalled(stop: Arc<AtomicBool>, stalled_for: Duration) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            out(ActivityStamp::backdated(RUNNING_FOR, stalled_for)),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress(true));
        activity
    }

    /// Media has only just started arriving and nothing has decoded yet. Normal:
    /// H.264 cannot be decoded until a keyframe brings its parameter sets.
    fn still_prerolling(stop: Arc<AtomicBool>) -> Arc<SessionActivity> {
        let activity = Arc::new(SessionActivity::from_stamps(
            ActivityStamp::backdated(Duration::from_millis(200), Duration::ZERO),
            Arc::new(SlotOutput::new(Instant::now())),
        ));
        let ingress = activity.clone();
        tick_until_stopped(stop, move || ingress.touch_ingress(true));
        activity
    }

    /// A session that is still negotiating: connected, but nothing has arrived.
    fn no_media_yet() -> Arc<SessionActivity> {
        Arc::new(SessionActivity::new(
            Instant::now(),
            Arc::new(SlotOutput::new(Instant::now())),
        ))
    }

    /// A seat sending both media for a minute, both still arriving. Audio keeps
    /// coming out of the slot; `audio_out_idle` and `video_out_idle` say how
    /// long ago each medium last did, and `video_in_idle` how long ago its
    /// video last arrived (zero: still arriving). The caller sets `stop` to end
    /// the arrivals and the audio output.
    fn audio_and_video(
        stop: Arc<AtomicBool>,
        audio_out_idle: Duration,
        video_in_idle: Duration,
        video_out_idle: Duration,
    ) -> Arc<SessionActivity> {
        let output = Arc::new(SlotOutput {
            audio: Arc::new(ActivityStamp::backdated(RUNNING_FOR, audio_out_idle)),
            video: Arc::new(ActivityStamp::backdated(RUNNING_FOR, video_out_idle)),
        });
        let activity = Arc::new(SessionActivity::from_media_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            ActivityStamp::backdated(RUNNING_FOR, video_in_idle),
            output.clone(),
        ));
        let publisher = activity.clone();
        let video_arriving = video_in_idle.is_zero();
        tick_until_stopped(stop.clone(), move || {
            publisher.touch_ingress(true);
            if video_arriving {
                publisher.touch_ingress(false);
            }
        });
        if audio_out_idle.is_zero() {
            tick_until_stopped(stop, move || output.audio.touch());
        }
        activity
    }

    fn endpoint_config(max_sessions: usize) -> WhipEndpointConfig {
        WhipEndpointConfig::for_tests("endpoint", max_sessions)
    }

    /// A manager with one single-slot endpoint whose only slot is held by a
    /// registered session with the given liveness — i.e. a full endpoint.
    fn full_endpoint(
        resource_id: &str,
        port: u16,
        activity: Arc<SessionActivity>,
    ) -> (
        Arc<WhipSessionManager>,
        Arc<WhipEndpointConfig>,
        Arc<AtomicBool>,
    ) {
        let manager = Arc::new(WhipSessionManager::new());
        manager.start_cleanup_task();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        let config = manager
            .get_endpoint_config("endpoint")
            .expect("endpoint was just registered");

        assert_eq!(
            config.allocate_slot(resource_id),
            Some(0),
            "the endpoint starts with its only slot free"
        );
        let cleanup_sent = register_with_activity(&manager, resource_id, port, activity);
        assert_eq!(
            config.allocate_slot("someone-else"),
            None,
            "the endpoint is now full"
        );
        (manager, config, cleanup_sent)
    }

    /// A session's `cleanup_sent` flag is the only way to stop its inactivity
    /// watchdog thread. Every path that removes a session must set it, or the
    /// watchdog outlives the session and asks for cleanup of a port that is gone.
    fn register(manager: &WhipSessionManager, resource_id: &str, port: u16) -> Arc<AtomicBool> {
        register_with_activity(manager, resource_id, port, dead_publisher(Duration::ZERO))
    }

    fn register_with_activity(
        manager: &WhipSessionManager,
        resource_id: &str,
        port: u16,
        activity: Arc<SessionActivity>,
    ) -> Arc<AtomicBool> {
        register_in_slot(manager, resource_id, port, 0, activity)
    }

    /// The config registered for "endpoint" on `manager`, registering a
    /// single-slot one first if there is none.
    fn registered_endpoint(manager: &WhipSessionManager) -> Arc<WhipEndpointConfig> {
        if let Some(config) = manager.get_endpoint_config("endpoint") {
            return config;
        }
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        manager
            .get_endpoint_config("endpoint")
            .expect("endpoint was just registered")
    }

    fn register_in_slot(
        manager: &WhipSessionManager,
        resource_id: &str,
        port: u16,
        slot: usize,
        activity: Arc<SessionActivity>,
    ) -> Arc<AtomicBool> {
        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: resource_id.to_string(),
            port,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot,
            config: registered_endpoint(manager),
            cleanup_sent: cleanup_sent.clone(),
            activity,
        });
        assert!(registered, "session should register");
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a freshly registered session is not finished"
        );
        cleanup_sent
    }

    #[test]
    fn remove_session_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-a", 40001);

        assert!(manager.remove_session("resource-a").is_some());

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "remove_session (WHIP DELETE) must stop the session's watchdog"
        );
    }

    #[test]
    fn remove_session_by_port_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-b", 40002);

        assert!(manager.remove_session_by_port(40002).is_some());

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "auto-cleanup by port must stop the session's watchdog"
        );
    }

    #[test]
    fn remove_all_sessions_stops_the_watchdog() {
        let manager = WhipSessionManager::new();
        let cleanup_sent = register(&manager, "resource-c", 40003);

        assert_eq!(manager.remove_all_sessions("endpoint").len(), 1);

        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "flow stop must stop the session's watchdog"
        );
    }

    /// A pending-cleanup mark that is never claimed must not survive: session ports
    /// come from the OS ephemeral range and are recycled, so a stale mark would
    /// destroy a later, unrelated session that is handed the same port.
    #[test]
    fn expired_pending_cleanup_mark_does_not_reject_a_recycled_port() {
        let manager = WhipSessionManager::new();
        {
            let mut pending = manager.pending_cleanup_ports.lock().unwrap();
            pending.insert(40004, Instant::now() - PENDING_CLEANUP_TTL * 2);
        }

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "resource-d".to_string(),
            port: 40004,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            config: registered_endpoint(&manager),
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(
            registered,
            "an expired pending-cleanup mark must not poison a recycled port"
        );
        assert!(!cleanup_sent.load(Ordering::SeqCst));
    }

    /// The mark must still do its job inside the TTL: a session that died before
    /// registration is torn down rather than registered.
    #[test]
    fn fresh_pending_cleanup_mark_still_rejects_the_session() {
        let manager = WhipSessionManager::new();
        {
            let mut pending = manager.pending_cleanup_ports.lock().unwrap();
            pending.insert(40005, Instant::now());
        }

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "resource-e".to_string(),
            port: 40005,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            config: registered_endpoint(&manager),
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(!registered, "a fresh mark must still reject the session");
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "a rejected session's watchdog must be stopped too"
        );
    }

    /// The bug this guards: a publisher that dies without sending a WHIP DELETE
    /// (network loss) leaves its slot occupied, and the rejoining client is
    /// refused with 503 until the inactivity watchdog reclaims the slot seconds
    /// later. A session whose media has stopped must give its slot up instead.
    #[tokio::test]
    async fn a_dead_session_gives_its_slot_to_a_new_client() {
        let (manager, config, cleanup_sent) = full_endpoint(
            "dead-session",
            40010,
            dead_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(0),
            "the rejoining client must get the dead session's slot"
        );
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "the displaced session's watchdog must be stopped"
        );
        assert!(
            manager.get_session_port("dead-session").is_none(),
            "the displaced session must be torn down by the ordinary cleanup path"
        );
        assert_eq!(
            config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("rejoining-client"),
            "the slot must be assigned to the new client, not left free"
        );
    }

    /// The case a single reading of the idle time gets wrong: the publisher is
    /// SIGKILLed and its client reconnects a few hundred milliseconds later,
    /// while the dead session still looks freshly fed. It is the counter staying
    /// frozen, not its value, that gives the session away.
    #[tokio::test]
    async fn a_session_that_died_moments_before_the_post_is_still_displaced() {
        let (manager, config, cleanup_sent) =
            full_endpoint("just-died", 40013, dead_publisher(Duration::ZERO));

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(0),
            "a client reconnecting straight after the drop must still get the slot"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(
            started.elapsed() >= TAKEOVER_IDLE_THRESHOLD,
            "the session must not be displaced before it has been quiet long enough"
        );
    }

    /// The risk in takeover: a second participant must not be able to evict a
    /// publisher that is streaming fine. Its buffer counter is moving, so that
    /// client still gets 503, and gets it in a poll interval rather than after
    /// the full takeover wait.
    #[tokio::test]
    async fn a_live_session_is_never_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) =
            full_endpoint("live-session", 40011, live_publisher(stop.clone()));

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, None, "a second client must still be refused");
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a session that is delivering media must not be torn down"
        );
        assert!(
            manager.get_session_port("live-session").is_some(),
            "the live session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a live publisher must be recognised from its moving counter, not \
             waited out: took {:?}",
            started.elapsed()
        );
    }

    /// THE BUG. A seat keeps receiving RTP while nothing usable ever comes out
    /// of its slot — the decoder never got a keyframe it could use, or a
    /// consumer below the slot's tee is blocking the chain. Its arriving-bytes
    /// counter moves the whole time, so liveness measured there says "healthy"
    /// and every rejoining client is refused for as long as the seat sits there.
    #[tokio::test]
    async fn a_session_receiving_media_it_never_decodes_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "receiving-nothing-usable",
            40014,
            receiving_but_never_usable(stop.clone()),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot,
            Some(0),
            "a seat producing nothing usable must give its slot up, however much RTP it receives"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager
                .get_session_port("receiving-nothing-usable")
                .is_none(),
            "the displaced session must be torn down by the ordinary cleanup path"
        );
    }

    /// The same seat by the other route: it decoded fine and then froze, which is
    /// what tee backpressure from a stuck consumer does to it. RTP keeps
    /// arriving either way.
    #[tokio::test]
    async fn a_session_whose_output_has_stalled_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "stalled-output",
            40015,
            receiving_but_stalled(stop.clone(), TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, Some(0), "a stalled seat must give its slot up");
        assert!(cleanup_sent.load(Ordering::SeqCst));
    }

    /// The risk in judging a seat by its decoded output: media arrives before it
    /// can be decoded, because H.264 carries its parameter sets with a keyframe
    /// and `decodebin` has a decoder to autoplug first. A session inside that
    /// window has produced nothing usable yet and must still be left alone.
    #[tokio::test]
    async fn a_session_still_waiting_for_its_first_decoded_frame_is_not_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) =
            full_endpoint("prerolling", 40016, still_prerolling(stop.clone()));

        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(slot, None, "a prerolling session must keep its slot");
        assert!(!cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("prerolling").is_some(),
            "the prerolling session must still be registered"
        );
    }

    /// The output stamp belongs to the slot, which outlives the sessions passing
    /// through it. A new session must not be judged on frames its predecessor
    /// produced: inheriting a stale one makes the newcomer look stalled the
    /// moment its own media starts arriving, and the next client evicts it.
    #[test]
    fn a_new_session_does_not_inherit_the_slots_previous_liveness() {
        let slot_output = out(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        assert!(
            slot_output.last() != 0,
            "the previous occupant left the slot's stamp set"
        );

        let session = SessionActivity::new(Instant::now(), slot_output.clone());
        session.touch_ingress(true);

        assert_eq!(
            slot_output.last(),
            0,
            "claiming a slot must clear what the previous session left on it"
        );
        assert_eq!(
            session.idle(),
            None,
            "a session whose own media has only just started must not be judged yet"
        );
    }

    /// After a takeover, frames the displaced session had already pushed into
    /// the slot's chain can cross the tee after the newcomer reset the stamp.
    /// They must not stand in for the newcomer's own first decoded frame: they
    /// stop within moments, and judging on them would let the next client evict
    /// a session whose decoder has not had its keyframe yet.
    #[test]
    fn a_predecessors_trailing_frames_do_not_cut_the_decode_grace_short() {
        let slot_output = out(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO));
        let session = SessionActivity::from_stamps(
            ActivityStamp::backdated(Duration::from_millis(200), Duration::ZERO),
            slot_output.clone(),
        );
        slot_output.reset();
        // The predecessor's tail crosses the tee, then the newcomer's media arrives.
        slot_output.audio.touch();
        std::thread::sleep(Duration::from_millis(50));
        session.touch_ingress(true);

        assert_eq!(
            session.idle(),
            None,
            "a session inside its decode grace must not be judged on its predecessor's frames"
        );
    }

    /// Both faults reap the seat, but they have different suspects, and the reap
    /// log is the only place an operator sees which one it was: a participant
    /// who went away, or a consumer inside their own flow that is blocking the
    /// slot's chain and will do the same to whoever reconnects.
    #[test]
    fn a_reaped_session_names_which_side_of_the_chain_stopped() {
        // Arrivals stopped first and the buffers already in the slot's chain
        // drained out after them, which is the order a publisher going away
        // always produces.
        let publisher_gone = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2),
            out(ActivityStamp::backdated(
                RUNNING_FOR,
                RUNNING_FOR / 2 - Duration::from_secs(1),
            )),
        );
        assert_eq!(
            publisher_gone.idle().map(|(_, side)| side),
            Some(StallSide::Ingress),
            "nothing arriving is the publisher's fault, not the flow's"
        );

        // Still receiving; the slot's chain stopped producing a while ago.
        let stuck_consumer = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            out(ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2)),
        );
        assert_eq!(
            stuck_consumer.idle().map(|(_, side)| side),
            Some(StallSide::Output),
            "a seat that still receives must be reaped against the slot's output"
        );

        // The stamps hold whole milliseconds against separate epochs, so a
        // publisher drop that freezes both at once can read either way round.
        // Anything inside the margin must not be reported as a stuck consumer,
        // and the label must not change how long the session counts as idle.
        let near_tie_output = out(ActivityStamp::backdated(
            RUNNING_FOR,
            RUNNING_FOR / 2 + STALL_SIDE_MARGIN / 2,
        ));
        let near_tie = SessionActivity::from_stamps(
            ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2),
            near_tie_output.clone(),
        );
        let output_idle = near_tie_output.since_last().unwrap();
        let (idle, side) = near_tie.idle().unwrap();
        assert_eq!(
            side,
            StallSide::Ingress,
            "stamp noise inside the margin must not move the blame to the flow"
        );
        assert!(
            idle >= output_idle,
            "idle must be the staler stamp whichever side is named: {idle:?} < {output_idle:?}"
        );
    }

    /// A session that has not produced a buffer yet may just be slow to
    /// negotiate (ICE through a TURN relay). Displacing it would let two clients
    /// take turns evicting each other before either ever sends media.
    #[tokio::test]
    async fn a_session_that_has_not_delivered_media_yet_is_not_displaced() {
        let (manager, config, cleanup_sent) = full_endpoint("negotiating", 40012, no_media_yet());

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;

        assert_eq!(slot, None, "a negotiating session must keep its slot");
        assert!(!cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("negotiating").is_some(),
            "the negotiating session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a session that cannot be displaced must not hold the POST: took {:?}",
            started.elapsed()
        );
    }

    /// A video-only publisher of static content goes seconds between frames on a
    /// healthy transport, so its frozen counter says nothing about whether it is
    /// still there.
    #[tokio::test]
    async fn a_sparse_video_only_session_is_not_displaced() {
        let (manager, config, cleanup_sent) = full_endpoint(
            "screenshare",
            40013,
            sparse_video_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let started = Instant::now();
        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;

        assert_eq!(
            slot, None,
            "a video-only session must keep its slot however wide its frame gap"
        );
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "a healthy publisher must not be torn down"
        );
        assert!(
            manager.get_session_port("screenshare").is_some(),
            "the video-only session must still be registered"
        );
        assert!(
            started.elapsed() < TAKEOVER_IDLE_THRESHOLD,
            "a session that cannot be displaced must not hold the POST: took {:?}",
            started.elapsed()
        );
    }

    /// THE BUG for video. The slot's video decoder wedged and keyframe repair
    /// gave up, while the publisher's audio and video both keep arriving and
    /// audio keeps coming out of the slot. Judged on one output stamp shared by
    /// both media, audio keeps the seat live forever: viewers see a frozen
    /// picture and a rejoining client gets 503.
    #[tokio::test]
    async fn a_session_whose_video_died_while_its_audio_flows_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "frozen-video",
            40030,
            audio_and_video(
                stop.clone(),
                Duration::ZERO,
                Duration::ZERO,
                VIDEO_REPAIR_WINDOW + TAKEOVER_IDLE_THRESHOLD * 2,
            ),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot,
            Some(0),
            "a seat whose video stopped coming out must give its slot up, however live its audio"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(
            manager.get_session_port("frozen-video").is_none(),
            "the displaced session must be torn down by the ordinary cleanup path"
        );
    }

    /// The same for audio: its chain stopped while audio keeps arriving and
    /// video keeps coming out.
    #[tokio::test]
    async fn a_session_whose_audio_died_while_its_video_flows_is_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let output = Arc::new(SlotOutput {
            audio: Arc::new(ActivityStamp::backdated(
                RUNNING_FOR,
                TAKEOVER_IDLE_THRESHOLD * 2,
            )),
            video: Arc::new(ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO)),
        });
        let activity = Arc::new(SessionActivity::from_media_stamps(
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO),
            output.clone(),
        ));
        let publisher = activity.clone();
        tick_until_stopped(stop.clone(), move || {
            publisher.touch_ingress(true);
            publisher.touch_ingress(false);
        });
        tick_until_stopped(stop.clone(), move || output.video.touch());
        let (manager, config, cleanup_sent) = full_endpoint("silent-audio", 40031, activity);

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot,
            Some(0),
            "a seat whose audio chain died must give its slot up"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
    }

    /// The risk in judging video on its own: a damaged stream is repaired with
    /// a keyframe, which can take seconds. A seat whose video froze inside the
    /// repair window is still recovering and keeps its slot.
    #[tokio::test]
    async fn a_session_whose_video_is_still_being_repaired_is_not_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let (manager, config, cleanup_sent) = full_endpoint(
            "repairing-video",
            40032,
            audio_and_video(
                stop.clone(),
                Duration::ZERO,
                Duration::ZERO,
                VIDEO_REPAIR_WINDOW / 2,
            ),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot, None,
            "a seat inside keyframe repair must keep its slot"
        );
        assert!(!cleanup_sent.load(Ordering::SeqCst));
    }

    /// A publisher that stopped sending video (camera off, or a screen share
    /// of a window nobody touches) while its audio plays on: nothing of its
    /// video comes out because nothing of it arrives. That is no stall.
    #[tokio::test]
    async fn a_session_that_stopped_sending_video_is_not_displaced() {
        let stop = Arc::new(AtomicBool::new(false));
        let quiet = VIDEO_REPAIR_WINDOW + TAKEOVER_IDLE_THRESHOLD * 4;
        let (manager, config, cleanup_sent) = full_endpoint(
            "camera-off",
            40033,
            audio_and_video(stop.clone(), Duration::ZERO, quiet, quiet),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "second-client")
            .await;
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            slot, None,
            "a live audio seat with no video to show keeps its slot"
        );
        assert!(!cleanup_sent.load(Ordering::SeqCst));
    }

    /// The watchdog reads `idle` too, so it reaps the same seat, and names the
    /// flow rather than the publisher. The repair window comes off the top.
    #[test]
    fn frozen_video_counts_as_idle_once_repair_has_had_its_chance() {
        let frozen_for = VIDEO_REPAIR_WINDOW + Duration::from_secs(20);
        let stop = Arc::new(AtomicBool::new(false));
        let activity = audio_and_video(stop.clone(), Duration::ZERO, Duration::ZERO, frozen_for);
        let (idle, side) = activity.idle().expect("past the decode grace");
        stop.store(true, Ordering::SeqCst);

        assert_eq!(
            side,
            StallSide::MediumOutput("video"),
            "the publisher is still sending, and it is the video that stalled"
        );
        assert!(
            idle >= Duration::from_secs(19) && idle <= Duration::from_secs(21),
            "idle must be the freeze less the repair window: {idle:?}"
        );
    }

    /// A camera turned back on after a long pause, or a mic unmuted: the
    /// medium resumes while the last buffer of it out of the slot is as old as
    /// the pause. The pause was the publisher's, so it must not count as a
    /// stall, not even for the moment before the decoder's first new frame.
    #[test]
    fn a_medium_that_resumes_after_a_pause_is_not_stalled() {
        let paused = VIDEO_REPAIR_WINDOW + Duration::from_secs(30);
        for medium in ["audio", "video"] {
            let ingress = ActivityStamp::backdated(RUNNING_FOR, paused);
            ingress.touch();
            let output = ActivityStamp::backdated(RUNNING_FOR, paused);
            assert_eq!(
                medium_stall(&ingress, &output).map(|stall| stall < ARRIVAL_PAUSE),
                Some(true),
                "{medium} that just resumed after {paused:?} is not stalled"
            );
        }
    }

    /// A medium that keeps arriving without a break is judged from the last
    /// buffer out, however long ago that was.
    #[test]
    fn a_medium_arriving_without_a_break_is_judged_from_its_last_output() {
        let ingress = ActivityStamp::backdated(RUNNING_FOR, Duration::ZERO);
        ingress.touch();
        let output = ActivityStamp::backdated(RUNNING_FOR, Duration::from_secs(20));
        let stall = medium_stall(&ingress, &output).expect("past the grace");
        assert!(
            stall >= Duration::from_secs(19) && stall <= Duration::from_secs(21),
            "{stall:?}"
        );
    }

    /// Output stamped long before this medium first arrived (the previous
    /// occupant's trailing frames, or the other half of a session that sent
    /// audio first) is not this medium's stall. It counts from the end of the
    /// medium's own decode grace at the most.
    #[test]
    fn output_older_than_the_mediums_grace_is_not_held_against_it() {
        let started = DECODE_GRACE + Duration::from_secs(1);
        let ingress = ActivityStamp::backdated(started, Duration::ZERO);
        let output = ActivityStamp::backdated(RUNNING_FOR, RUNNING_FOR / 2);
        let stall = medium_stall(&ingress, &output).expect("past the grace");
        assert!(
            stall <= Duration::from_millis(1100),
            "a medium {started:?} old cannot have stalled for {stall:?}"
        );
    }

    /// The window has to outlast keyframe repair, or a seat is judged dead
    /// while the session is still asking its publisher for the keyframe that
    /// would revive it.
    #[test]
    fn the_video_repair_window_outlasts_keyframe_repair() {
        let policy = crate::gst::keyframe_request::RecoveryPolicy::default();
        assert!(VIDEO_REPAIR_WINDOW >= policy.interval * policy.attempts);
    }

    /// With more than one slot, the idlest session is not necessarily the one
    /// that can be displaced. A video-only seat that has been quiet longer must
    /// not shield a dead audio-bearing seat from takeover.
    #[tokio::test]
    async fn a_dead_session_is_displaced_past_a_quieter_video_only_one() {
        let manager = Arc::new(WhipSessionManager::new());
        manager.start_cleanup_task();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(2));
        let config = manager
            .get_endpoint_config("endpoint")
            .expect("endpoint was just registered");

        assert_eq!(config.allocate_slot("screenshare"), Some(0));
        let screenshare_cleanup = register_in_slot(
            &manager,
            "screenshare",
            40014,
            0,
            sparse_video_publisher(TAKEOVER_IDLE_THRESHOLD * 3),
        );
        assert_eq!(config.allocate_slot("dead-session"), Some(1));
        let dead_cleanup = register_in_slot(
            &manager,
            "dead-session",
            40015,
            1,
            dead_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );

        let slot = manager
            .allocate_slot_or_take_over(&config, "rejoining-client")
            .await;

        assert_eq!(
            slot,
            Some(1),
            "the rejoining client must get the dead session's slot"
        );
        assert!(dead_cleanup.load(Ordering::SeqCst));
        assert!(
            !screenshare_cleanup.load(Ordering::SeqCst),
            "the video-only session must not be torn down"
        );
    }

    /// The bug this guards: a session left over from an earlier run of the
    /// flow is reaped by its watchdog and releases "its" slot by index on the
    /// endpoint's current config, freeing a slot a live publisher holds. The
    /// next POST then takes it and two sessions push into one appsrc. A release
    /// must only free a slot still held by the session releasing it.
    #[test]
    fn releasing_a_slot_held_by_another_session_leaves_it_held() {
        let config = endpoint_config(1);
        assert_eq!(config.allocate_slot("live-publisher"), Some(0));

        assert!(
            !config.release_slot(0, "orphan"),
            "a session that does not hold the slot must not release it"
        );
        assert_eq!(
            config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("live-publisher"),
            "the live publisher must keep its slot"
        );
        assert_eq!(
            config.allocate_slot("next-client"),
            None,
            "the slot must not be handed to a second publisher"
        );

        assert!(
            config.release_slot(0, "live-publisher"),
            "the holder itself can still release the slot"
        );
        assert_eq!(config.slot_assignments.read().unwrap()[0], None);
    }

    /// A POST still in flight when its flow stopped must not register under
    /// the flow's next run, which re-registers the same endpoint_id with a new
    /// config. Registered, the orphan's watchdog would later release the new
    /// run's slot of the same index from under a live publisher.
    #[test]
    fn a_session_from_an_earlier_run_of_its_endpoint_is_refused() {
        let manager = WhipSessionManager::new();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        let old_config = manager.get_endpoint_config("endpoint").unwrap();
        assert_eq!(old_config.allocate_slot("orphan"), Some(0));

        // The flow stops while the orphan's POST is still in flight...
        assert!(manager.remove_all_sessions("endpoint").is_empty());
        assert!(manager.unregister_endpoint("endpoint").is_empty());
        // ...and starts again, and a live publisher takes slot 0.
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        let new_config = manager.get_endpoint_config("endpoint").unwrap();
        assert_eq!(new_config.allocate_slot("live-publisher"), Some(0));

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "orphan".to_string(),
            port: 40020,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            config: old_config.clone(),
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(
            !registered,
            "a session allocated from an earlier config must be refused"
        );
        assert!(
            cleanup_sent.load(Ordering::SeqCst),
            "a refused session's watchdog must be stopped"
        );
        assert!(manager.get_session_port("orphan").is_none());
        assert_eq!(
            new_config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("live-publisher"),
            "the live publisher must keep its slot"
        );
        assert_eq!(
            old_config.slot_assignments.read().unwrap()[0],
            None,
            "the orphan's slot is released on the config it came from"
        );
    }

    /// A POST that fetched its config before the flow stopped, and is still in
    /// the takeover wait when the flow starts again, sees the old config full
    /// (stopping a flow does not free its slots). It must not displace a
    /// session of the new run, which shares the endpoint_id.
    #[tokio::test]
    async fn a_post_from_an_earlier_run_does_not_displace_a_new_runs_session() {
        let (manager, new_config, cleanup_sent) = full_endpoint(
            "new-run-session",
            40023,
            dead_publisher(TAKEOVER_IDLE_THRESHOLD * 2),
        );
        let old_config = endpoint_config(1);
        assert_eq!(old_config.allocate_slot("old-run-session"), Some(0));

        let slot = manager
            .allocate_slot_or_take_over(&old_config, "stale-post")
            .await;

        assert_eq!(slot, None, "a stale POST gets no slot");
        assert!(
            !cleanup_sent.load(Ordering::SeqCst),
            "the new run's session must not be displaced by a stale POST"
        );
        assert!(manager.get_session_port("new-run-session").is_some());
        assert_eq!(
            new_config.slot_assignments.read().unwrap()[0].as_deref(),
            Some("new-run-session")
        );
    }

    /// A POST still in flight when its flow stopped, and the flow has not
    /// started again.
    #[test]
    fn a_session_whose_endpoint_is_gone_is_refused() {
        let manager = WhipSessionManager::new();
        manager.register_endpoint("endpoint".to_string(), endpoint_config(1));
        let config = manager.get_endpoint_config("endpoint").unwrap();
        assert_eq!(config.allocate_slot("orphan"), Some(0));
        assert!(manager.unregister_endpoint("endpoint").is_empty());

        let (element, pipeline, cleanup_sent) = dummy_session();
        let registered = manager.register_session(NewWhipSession {
            resource_id: "orphan".to_string(),
            port: 40021,
            element,
            session_pipeline: pipeline,
            endpoint_id: "endpoint".to_string(),
            slot: 0,
            config,
            cleanup_sent: cleanup_sent.clone(),
            activity: dead_publisher(Duration::ZERO),
        });

        assert!(!registered, "a session must not outlive its endpoint");
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(manager.get_session_port("orphan").is_none());
    }

    /// Flow stop removes the endpoint's sessions, tears them down (which takes
    /// a while), then unregisters the endpoint. A POST that registers in
    /// between must be handed back by `unregister_endpoint` for teardown, not
    /// left registered against an endpoint that no longer exists.
    #[test]
    fn a_session_registered_during_flow_stop_is_swept_on_unregister() {
        let manager = WhipSessionManager::new();
        assert!(manager.remove_all_sessions("endpoint").is_empty());
        let cleanup_sent = register(&manager, "late-session", 40022);

        assert_eq!(
            manager.unregister_endpoint("endpoint").len(),
            1,
            "the late session must be handed back for teardown"
        );
        assert!(cleanup_sent.load(Ordering::SeqCst));
        assert!(manager.get_session_port("late-session").is_none());
    }
}
