//! Background segment uploader for the TAMS Output block.
//!
//! The segmenter ([`crate::tams::segmenter`]) writes complete, GOP-aligned
//! segment files to a temp directory. Each finished file, together with its
//! timerange, is handed to this task over a bounded channel. All HTTP and disk reads
//! happen here — never on the streaming thread — so the pipeline is never blocked by
//! the gateway or S3.
//!
//! Reliability:
//! - each segment is uploaded with a few short inline attempts (backoff +
//!   jitter) before being **parked** on disk for later; while the gateway keeps
//!   failing, new segments are parked at once, so an outage of any length costs
//!   no time per segment and the channel does not fill;
//! - a segment that finds the channel full is parked by the producer; one that
//!   cannot be parked (the disk is full) is dropped with an error;
//! - a parked segment keeps its bytes plus a `.meta` sidecar holding its TAI
//!   timerange and the gateway and TAMS flow it belongs to, so a background
//!   recovery sweep — this run or a *later* one, after a full process restart —
//!   can re-register it once the gateway/storage recovers; a segment parked for
//!   another gateway or flow (the block's settings changed) is kept but never
//!   sent to this one;
//! - the sidecar is written to a temporary file and renamed, so the sweep never
//!   reads one half written;
//! - each parked segment that fails waits longer before it is tried again, and
//!   the sweep moves on to the next one meanwhile, so one segment that always
//!   fails never holds back the others;
//! - what is parked can be capped in count and bytes per flow directory
//!   ([`ParkedLimits`], off unless set); past the cap the oldest segments are
//!   deleted, with an error event. Every segment is parked through
//!   [`crate::tams::park::Parking::park`], which also caps all parked segments
//!   of the server and keeps free space on the volume;
//! - a retry reuses the storage object an earlier try allocated (and uploaded
//!   to), so retries do not leave unused objects behind;
//! - a successfully registered segment (and its sidecar) is deleted;
//! - a definitively rejected segment (non-retryable 4xx) is dropped with an error,
//!   since retrying identical bytes can never succeed; but a 404 for the flow
//!   creates the flow again, and 401/403 park the segment until the credentials
//!   are fixed;
//! - a parked or retried segment is first looked up on the gateway, so one whose
//!   registration went through without an answer is not registered twice;
//! - segment files carry the id of their run ([`RunGuard`]), so a restarted flow
//!   never writes over a file an earlier run parked;
//! - when the flow stops, a stop drain sends EOS through the muxer so the current
//!   file is complete (see [`crate::tams::segmenter::Segmenter::drain`]), but no next fragment
//!   rotates it into a `FragmentReady`, so the last fragment is flushed here when
//!   the channel closes — see [`TailSlot`].

use crate::events::EventBroadcaster;
use crate::tams::client::{CollectionItem, FlowSpec, TamsClient};
use gstreamer as gst;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use strom_types::{FlowId, StromEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Flow metadata detected from caps at runtime, read when the flow is created.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DetectedEssence {
    /// Codec label (e.g. `video/h264`); overrides the spec's fallback when set.
    pub codec: Option<String>,
    /// TAMS `essence_parameters`, merged over the spec's.
    pub essence_parameters: serde_json::Map<String, serde_json::Value>,
    /// Codec and parameters per track of a muxed flow (`video`, `audio-0`,
    /// ...), for the member flows it collects (`FlowSpec::collected`).
    pub tracks: BTreeMap<String, TrackEssence>,
}

/// What caps say about one track of a muxed flow.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TrackEssence {
    /// TAMS codec label, e.g. `audio/aac`.
    pub codec: String,
    /// TAMS `essence_parameters` of the track's member flow.
    pub essence_parameters: serde_json::Map<String, serde_json::Value>,
}

/// [`DetectedEssence`] shared between the caps probe and the uploader.
pub type SharedDetected = Arc<Mutex<DetectedEssence>>;

/// A finished segment file ready to be uploaded and registered.
#[derive(Debug)]
pub struct FragmentReady {
    /// Path to the complete segment file in the temp directory.
    pub path: PathBuf,
    /// Segment start on the flow timeline, in nanoseconds (TAI).
    pub start_ns: u64,
    /// Segment end (exclusive) on the flow timeline, in nanoseconds (TAI).
    pub end_ns: u64,
}

/// The currently-open (not-yet-rotated) fragment. The segmenter has opened this
/// file but it only becomes a `FragmentReady` when the *next* fragment opens. On
/// pipeline stop there is no "next" fragment, so the uploader flushes whatever is
/// here when its channel closes.
pub struct TailFragment {
    pub path: PathBuf,
    pub start_ns: u64,
    /// Running time of the fragment's first buffer (`start_ns` on the timeline).
    pub start_running_time_ns: u64,
}

/// Buffers a [`PadMediaEnd`] remembers between entering the muxer's pad and
/// being consumed by it. An aggregator muxer queues far fewer.
const PAD_RING: usize = 64;

/// Where the media written by a muxer through one sink pad ends, as running
/// time.
///
/// A buffer probe on the pad reports each buffer as it enters
/// ([`Self::add_buffer`]), with the running time from the pad's segment
/// ([`Self::set_segment`]). When the muxer also reports each buffer it consumed
/// ([`Self::consume`], from an aggregator pad's `buffer-consumed` signal; its
/// queue is first in, first out), only consumed buffers count: what is still
/// queued when the pipeline stops is never written. The consumed buffer's own
/// timestamps are not used, since some muxers (mpegtsmux) rewrite them to
/// running time first.
///
/// Updated per buffer, so it holds only atomics. The uploader reads it after
/// the pipeline has stopped.
#[derive(Debug)]
pub struct PadMediaEnd {
    seg_start_ns: AtomicU64,
    seg_base_ns: AtomicU64,
    /// The pad's segment is a forward TIME segment at rate 1.0.
    seg_ok: AtomicBool,
    /// End running time of entered buffer `n` at `ring[n % PAD_RING]`; 0 = not
    /// tracked (untracked segment).
    ring: [AtomicU64; PAD_RING],
    entered: AtomicU64,
    consumed: AtomicU64,
    counts_consumed: AtomicBool,
}

impl Default for PadMediaEnd {
    fn default() -> Self {
        Self {
            seg_start_ns: AtomicU64::new(0),
            seg_base_ns: AtomicU64::new(0),
            seg_ok: AtomicBool::new(false),
            ring: std::array::from_fn(|_| AtomicU64::new(0)),
            entered: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            counts_consumed: AtomicBool::new(false),
        }
    }
}

impl PadMediaEnd {
    /// Count only buffers reported by [`Self::consume`].
    pub fn count_consumed(&self) {
        self.counts_consumed.store(true, Ordering::Relaxed);
    }

    /// A new segment event on the pad. Running time of a buffer in a forward,
    /// rate 1.0 segment is `pts - start + base`; any other segment is not tracked.
    pub fn set_segment(&self, segment: &gst::Segment) {
        let ok = segment
            .downcast_ref::<gst::format::Time>()
            .filter(|seg| seg.rate() == 1.0)
            .map(|seg| {
                let start = seg.start().map_or(0, |t| t.nseconds());
                let base = seg.base().map_or(0, |t| t.nseconds());
                self.seg_start_ns.store(start, Ordering::Relaxed);
                self.seg_base_ns.store(base, Ordering::Relaxed);
            })
            .is_some();
        self.seg_ok.store(ok, Ordering::Relaxed);
    }

    /// A buffer with this PTS (`None`: none) and duration entered the pad.
    pub fn add_buffer(&self, pts_ns: Option<u64>, duration_ns: u64) {
        let start = self.seg_start_ns.load(Ordering::Relaxed);
        let end = match pts_ns {
            Some(pts) if self.seg_ok.load(Ordering::Relaxed) && pts >= start => {
                pts - start + self.seg_base_ns.load(Ordering::Relaxed) + duration_ns
            }
            _ => 0,
        };
        let n = self.entered.load(Ordering::Relaxed);
        self.ring[n as usize % PAD_RING].store(end, Ordering::Relaxed);
        self.entered.store(n + 1, Ordering::Release);
    }

    /// The muxer consumed the oldest buffer still queued on the pad.
    pub fn consume(&self) {
        self.consumed.fetch_add(1, Ordering::AcqRel);
    }

    /// Latest end of the buffers written (consumed, or entered when the muxer
    /// does not report consumption) that are still in the ring.
    fn end_running_time_ns(&self) -> Option<u64> {
        let entered = self.entered.load(Ordering::Acquire);
        let upto = if self.counts_consumed.load(Ordering::Relaxed) {
            self.consumed.load(Ordering::Acquire).min(entered)
        } else {
            entered
        };
        // Ring slots still hold entries from `entered - PAD_RING` on; with more
        // than that queued, the written end is unknown.
        let from = entered.saturating_sub(PAD_RING as u64);
        (from..upto)
            .map(|n| self.ring[n as usize % PAD_RING].load(Ordering::Relaxed))
            .max()
            .filter(|&end| end > 0)
    }
}

/// The [`PadMediaEnd`] of every sink pad of one fragment muxer. Locked only
/// when a pad is added and when the uploader reads it.
pub type MediaEnd = Arc<Mutex<Vec<Arc<PadMediaEnd>>>>;

/// Latest end running time over all pads, `None` when no pad saw a buffer.
pub fn media_end_running_time_ns(end: &MediaEnd) -> Option<u64> {
    let pads = end.lock().unwrap_or_else(|e| e.into_inner());
    pads.iter().filter_map(|p| p.end_running_time_ns()).max()
}

/// Timerange end of the last fragment of a run, which no next fragment closes:
/// where its media ends, or `start + nominal_ns` when that is unknown (no buffer
/// seen, or an untracked segment).
fn tail_end_ns(
    tail: &TailFragment,
    media_end_running_time_ns: Option<u64>,
    nominal_ns: u64,
) -> u64 {
    match media_end_running_time_ns {
        Some(end) if end > tail.start_running_time_ns => tail
            .start_ns
            .saturating_add(end - tail.start_running_time_ns),
        _ => tail.start_ns.saturating_add(nominal_ns.max(1)),
    }
}

/// The shortest hole between where a fragment's media ends and where the
/// next fragment starts that ends the fragment at its media: a stall of the
/// input, not a frame or two missing at the split (or a buffer the media-end
/// tracking missed).
pub const STALL_NS: u64 = 200_000_000;

/// Timerange end of a fragment that the next fragment closes, given the end
/// `planned_end_ns` (TAI) the timeline gives it. If the input stalled before
/// the split, so that the next fragment's first buffer (running time
/// `next_running_time_ns`) comes more than [`STALL_NS`] (or `contiguous_ns`)
/// after where this fragment's media ends, it ends at its media: the stall
/// stays a hole in TAMS rather than a segment that claims seconds of media it
/// does not hold (seen live: a 6.248 s timerange over 0.9 s of video).
pub fn closed_end_ns(
    fragment: &TailFragment,
    planned_end_ns: u64,
    media_end_running_time_ns: Option<u64>,
    next_running_time_ns: u64,
    contiguous_ns: u64,
) -> u64 {
    match media_end_running_time_ns {
        Some(end)
            if end > fragment.start_running_time_ns
                && next_running_time_ns > end.saturating_add(contiguous_ns.max(STALL_NS)) =>
        {
            let media = fragment
                .start_ns
                .saturating_add(end - fragment.start_running_time_ns);
            planned_end_ns.min(media)
        }
        _ => planned_end_ns,
    }
}

/// Shared slot holding the current [`TailFragment`], written by the location signal
/// and read by the uploader on shutdown.
pub type TailSlot = Arc<Mutex<Option<TailFragment>>>;

pub fn new_tail_slot() -> TailSlot {
    Arc::new(Mutex::new(None))
}

/// Bounded channel capacity. When it is full, the producer parks the segment on
/// disk (see [`park_on_disk`]) instead of blocking the pipeline; the recovery
/// sweep uploads it later.
pub const CHANNEL_CAPACITY: usize = 256;

/// Upload/registration attempts for a fresh segment before it is parked on disk
/// for the recovery sweep. Few and short (1 s, 2 s backoff): a segment that waits
/// longer holds back every segment behind it.
const LIVE_ATTEMPTS: u32 = 3;

/// Attempts for a parked segment per turn. The sweep comes back to it.
const RECOVERY_ATTEMPTS: u32 = 1;

/// Upper bound on a single backoff step. The exponential 1,2,4,8,16,32s schedule is
/// clamped here so one attempt never stalls for minutes.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often the uploader looks for segments parked on disk (failed, queued past
/// a full channel, or left by a previous run), and, while the gateway is failing,
/// tries one of them to see whether it is back.
const RECOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Suffix for the per-segment sidecar that persists a parked segment's timerange.
const SIDECAR_SUFFIX: &str = ".meta";

/// Suffix of the temporary file a sidecar is written to before it is renamed
/// into place.
const SIDECAR_TMP_SUFFIX: &str = ".tmp";

/// Longest wait before a parked segment that keeps failing is tried again.
const PARKED_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Failed tries after which a parked segment is reported as stuck (once).
const PARKED_STUCK_FAILURES: u32 = 5;

/// How often refused credentials are reported while they stay refused.
const AUTH_REPORT_INTERVAL: Duration = Duration::from_secs(300);

/// Prefix of every segment file name.
const SEGMENT_PREFIX: &str = "seg_";

/// Create the fragment channel for one flow.
pub fn channel() -> (mpsc::Sender<FragmentReady>, mpsc::Receiver<FragmentReady>) {
    mpsc::channel(CHANNEL_CAPACITY)
}

/// The gateway and TAMS flow a parked segment is meant for, stored in its
/// sidecar. The recovery sweep only uploads segments parked for its own target,
/// so changing a block's gateway or flow id never sends old segments to the
/// new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkTarget {
    /// Gateway URL without credentials, query or fragment.
    pub gateway: String,
    /// TAMS flow id.
    pub flow_id: String,
}

impl ParkTarget {
    pub fn new(gateway_url: &str, flow_id: &str) -> Self {
        Self {
            gateway: crate::client_auth::redact_url(gateway_url)
                .trim_end_matches('/')
                .to_string(),
            flow_id: flow_id.to_string(),
        }
    }
}

/// Cap on what one flow directory keeps parked. Past it, the oldest parked
/// segments are deleted.
///
/// The flow directories live under `STROM_TAMS_PARK_DIR` (default
/// `<data_dir>/tams-park`), one per TAMS flow; see `crate::tams::park`, which
/// also caps all parked segments of the server together. That cap deletes the
/// oldest of the whole server, so every flow keeps the same window back in
/// time; a per-flow cap would only shorten it, so there is none by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedLimits {
    pub max_bytes: u64,
    pub max_segments: usize,
}

impl ParkedLimits {
    /// `STROM_TAMS_PARKED_MAX_MB` and `STROM_TAMS_PARKED_MAX_SEGMENTS`; one
    /// that is not set does not cap.
    pub fn from_env() -> Self {
        fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
            std::env::var(name).ok()?.trim().parse().ok()
        }
        Self::new(
            env("STROM_TAMS_PARKED_MAX_MB"),
            env("STROM_TAMS_PARKED_MAX_SEGMENTS"),
        )
    }

    /// A cap of `max_mb` MiB and `max_segments` segments; `None` does not cap.
    pub fn new(max_mb: Option<u64>, max_segments: Option<usize>) -> Self {
        Self {
            max_bytes: max_mb.map_or(u64::MAX, |mb| mb.saturating_mul(1024 * 1024)),
            max_segments: max_segments.unwrap_or(usize::MAX),
        }
    }
}

// --- Runs ----------------------------------------------------------------------

/// Run ids whose uploader is still running in this process. Their segment files
/// are never cleared as orphans: the files may still be written or uploaded.
static LIVE_RUNS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

/// One run of a TAMS Output (one pipeline start). Every segment file of the run
/// carries its id (see [`segment_file_name`]), so a restarted flow never writes
/// over files that an earlier run parked, and the recovery sweep never pairs one
/// run's bytes with another run's timerange. Live while the guard exists; the
/// uploader holds it until it has stopped.
pub struct RunGuard {
    id: String,
}

impl RunGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        LIVE_RUNS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Start a new run with an id unique across runs and process restarts (wall
/// clock nanoseconds plus a counter).
pub fn new_run() -> RunGuard {
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("{:x}{:x}", nanos, n);
    LIVE_RUNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone());
    RunGuard { id }
}

/// Segment file name: `seg_<run>_<fragment>.<ext>`.
pub fn segment_file_name(run: &str, fragment_id: u32, ext: &str) -> String {
    format!("{}{}_{:05}.{}", SEGMENT_PREFIX, run, fragment_id, ext)
}

/// The run id in a segment file name, `None` for a name without one (written
/// before files carried it).
fn run_of(name: &str) -> Option<&str> {
    let (run, _) = name.strip_prefix(SEGMENT_PREFIX)?.split_once('_')?;
    Some(run)
}

/// Remove orphan segment files from `temp_dir`: `seg_*` files with no `.meta`
/// sidecar whose run is not live. Their timerange is lost, so they cannot be
/// registered (a fragment mid-write when the process was killed, or one already
/// uploaded but not yet deleted). Parked segments (with a sidecar) are kept for
/// the recovery sweep, also across a restart, and so are the files of a run
/// whose uploader still runs (a previous run of a restarted flow flushing its
/// last segment). Returns how many files were removed.
pub fn clear_orphans(temp_dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return 0;
    };
    let live = LIVE_RUNS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with(SEGMENT_PREFIX) || name.ends_with(SIDECAR_SUFFIX) {
            continue;
        }
        if run_of(name).is_some_and(|run| live.contains(run)) || sidecar_path(&path).exists() {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Parked segments some uploader of this process is uploading right now. Two
/// runs of one flow (a restart while the old uploader still drains) sweep the
/// same directory; a claim keeps them from registering one segment twice.
static CLAIMED: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

/// A claim on a parked segment, released on drop.
pub(crate) struct Claim(PathBuf);

impl Claim {
    pub(crate) fn take(path: &Path) -> Option<Self> {
        let mut claimed = CLAIMED.lock().unwrap_or_else(|e| e.into_inner());
        claimed
            .insert(path.to_path_buf())
            .then(|| Claim(path.to_path_buf()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

// --- Uploader task ---------------------------------------------------------------

/// Spawn the uploader task for one TAMS flow.
///
/// `content_type` is the MIME of the segment files (`video/mp4` or `video/mp2t`).
/// `temp_dir` is the per-flow directory of segment files, scanned by the recovery
/// sweep. The TAMS flow is created lazily on the first segment, so an idle block
/// never touches the gateway. `detected_codec` is filled in by the caps probe once
/// the encoded format is known (codec, e.g. `video/h264`, and video essence parameters); by the time the first segment
/// completes, caps have always arrived, so the stored codec reflects the real
/// essence. `flow_spec.codec` is only a fallback. `tail` is flushed on shutdown
/// with a timerange that ends where its media ends (`media_end`), or one
/// `tail_segment_ns` after its start when that is unknown. `run` stays live
/// until the task ends.
#[allow(clippy::too_many_arguments)]
pub fn spawn_uploader(
    client: TamsClient,
    mut flow_spec: FlowSpec,
    detected_codec: SharedDetected,
    content_type: String,
    temp_dir: PathBuf,
    strom_flow_id: FlowId,
    block_id: String,
    events: EventBroadcaster,
    mut rx: mpsc::Receiver<FragmentReady>,
    tail: TailSlot,
    media_end: MediaEnd,
    tail_segment_ns: u64,
    run: RunGuard,
) {
    tokio::spawn(async move {
        let _run = run;
        let tams_flow_id = flow_spec.flow_id.clone();
        let mut registered: Option<DetectedEssence> = None;
        let mut segments_registered: u64 = 0;

        info!(
            "TAMS {}: uploader started for flow {} ({})",
            block_id, tams_flow_id, flow_spec.format
        );

        let mut ctx = UploadCtx {
            client: &client,
            registered: &mut registered,
            flow_spec: &mut flow_spec,
            detected_codec: &detected_codec,
            content_type: &content_type,
            events: &events,
            strom_flow_id,
            block_id: &block_id,
            segments_registered: &mut segments_registered,
            failing: false,
            objects: HashMap::new(),
            auth_reported_at: None,
        };
        let mut parked = ParkedState::new(
            temp_dir.clone(),
            ParkTarget::new(client.base_url(), &tams_flow_id),
            ParkedLimits::from_env(),
        );

        // Anything a previous run parked on disk is uploaded between new
        // fragments, oldest first.
        let mut backlog = parked.refresh(&mut ctx).await;
        log_backlog(&ctx, &backlog);

        let mut sweep = tokio::time::interval(RECOVERY_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        sweep.tick().await; // consume the immediate first tick (we just looked)

        // New fragments come first, so a backlog never holds the live edge back
        // or fills the channel. Parked segments go one at a time in between,
        // and not at all while the gateway is failing (one is tried at each
        // sweep to see whether it is back). The backlog holds only segments
        // that are due: one that failed waits out its backoff, and the oldest
        // due segment after it is tried instead.
        loop {
            tokio::select! {
                biased;
                maybe = rx.recv() => match maybe {
                    Some(frag) => {
                        process_fragment(&mut ctx, frag).await;
                    }
                    None => break, // channel closed: pipeline tearing down
                },
                _ = sweep.tick() => {
                    backlog = parked.refresh(&mut ctx).await;
                    if ctx.failing {
                        match backlog.pop_front() {
                            Some(p) => parked.try_one(&mut ctx, p).await,
                            // Nothing due to probe with: let the next fragment try.
                            None => ctx.set_failing(false),
                        }
                    }
                }
                _ = std::future::ready(()), if !ctx.failing && !backlog.is_empty() => {
                    if let Some(p) = backlog.pop_front() {
                        parked.try_one(&mut ctx, p).await;
                    }
                }
            }
        }

        // Channel closed: the pipeline is tearing down. The segmenter has finalized
        // the last file by now, but it was never rotated into a FragmentReady, so
        // flush it as the final segment.
        if let Some(tf) = tail.lock().ok().and_then(|mut t| t.take()) {
            if tokio::fs::try_exists(&tf.path).await.unwrap_or(false) {
                debug!("TAMS {}: flushing final segment on shutdown", block_id);
                let end_ns =
                    tail_end_ns(&tf, media_end_running_time_ns(&media_end), tail_segment_ns);
                let frag = FragmentReady {
                    path: tf.path,
                    start_ns: tf.start_ns,
                    end_ns,
                };
                process_fragment(&mut ctx, frag).await;
            }
        }

        // Drain what is parked, so a graceful stop after a brief outage doesn't
        // leave recoverable segments behind. Stops at the first failure; the
        // next run picks up the rest.
        ctx.failing = false;
        let mut backlog = parked.refresh(&mut ctx).await;
        while !ctx.failing {
            let Some(p) = backlog.pop_front() else { break };
            parked.try_one(&mut ctx, p).await;
        }
        if !backlog.is_empty() {
            warn!(
                "TAMS {}: {} segment(s) left parked in {} for the next run",
                block_id,
                backlog.len(),
                temp_dir.display()
            );
        }

        info!(
            "TAMS {}: uploader stopped for flow {}",
            block_id, tams_flow_id
        );
    });
}

/// Shared mutable state for one uploader task, threaded through the fragment and
/// recovery paths (avoids a dozen-argument function each).
struct UploadCtx<'a> {
    client: &'a TamsClient,
    /// What the flow was last registered with (`None`: not yet, or it has to be
    /// created again).
    registered: &'a mut Option<DetectedEssence>,
    flow_spec: &'a mut FlowSpec,
    detected_codec: &'a SharedDetected,
    content_type: &'a str,
    events: &'a EventBroadcaster,
    strom_flow_id: FlowId,
    block_id: &'a str,
    segments_registered: &'a mut u64,
    /// The gateway or storage failed the last segment (network, 5xx, auth).
    /// New segments are parked at once, without the inline backoff, so an
    /// outage costs no time per segment and the channel never fills.
    failing: bool,
    /// Storage objects allocated for parked segments whose upload or
    /// registration failed, reused when the segment is tried again.
    objects: HashMap<PathBuf, Allocation>,
    /// When refused credentials were last reported (error log and event);
    /// `None` once a segment goes through again.
    auth_reported_at: Option<Instant>,
}

impl UploadCtx<'_> {
    fn set_failing(&mut self, failing: bool) {
        if failing == self.failing {
            return;
        }
        self.failing = failing;
        if failing {
            warn!(
                "TAMS {}: gateway failing; parking segments on disk until it recovers \
                 (checked every {} s)",
                self.block_id,
                RECOVERY_SWEEP_INTERVAL.as_secs()
            );
        } else {
            self.auth_reported_at = None;
            info!(
                "TAMS {}: gateway back; uploading parked segments",
                self.block_id
            );
        }
    }

    /// Whether refused credentials should be reported now: the first time,
    /// then every [`AUTH_REPORT_INTERVAL`] while they stay refused.
    fn should_report_auth(&mut self) -> bool {
        let now = Instant::now();
        if self
            .auth_reported_at
            .is_some_and(|at| now.duration_since(at) < AUTH_REPORT_INTERVAL)
        {
            return false;
        }
        self.auth_reported_at = Some(now);
        true
    }
}

/// A storage object allocated for one segment, kept across tries so a retry
/// does not allocate (and leave unused) a new one.
#[derive(Debug, Clone)]
struct Allocation {
    object: crate::tams::client::AllocatedObject,
    /// The segment's bytes are in the object already; only the registration
    /// is left.
    uploaded: bool,
}

/// Failed tries of one parked segment.
#[derive(Debug, Clone, Copy)]
struct Retry {
    failures: u32,
    next: Instant,
}

/// Wait before a parked segment that failed `failures` times is tried again:
/// two sweeps, doubling per failure, up to [`PARKED_MAX_BACKOFF`]. At least two
/// sweeps, so the next sweep always moves on to another segment.
fn parked_backoff(failures: u32) -> Duration {
    let sweeps = 1u32 << failures.clamp(1, 8);
    (RECOVERY_SWEEP_INTERVAL * sweeps).min(PARKED_MAX_BACKOFF)
}

/// The uploader's view of what is parked in its directory: which segments
/// are its own, which are due, and the cap on what is kept.
struct ParkedState {
    temp_dir: PathBuf,
    target: ParkTarget,
    limits: ParkedLimits,
    retries: HashMap<PathBuf, Retry>,
    /// Segments parked for another gateway or flow, last reported.
    foreign_reported: usize,
}

impl ParkedState {
    fn new(temp_dir: PathBuf, target: ParkTarget, limits: ParkedLimits) -> Self {
        Self {
            temp_dir,
            target,
            limits,
            retries: HashMap::new(),
            foreign_reported: 0,
        }
    }

    /// Scan the directory: enforce the cap, forget state of segments that are
    /// gone, and return this target's segments that are due, oldest first.
    async fn refresh(&mut self, ctx: &mut UploadCtx<'_>) -> VecDeque<Parked> {
        let scanned_at = Instant::now();
        let mut all = collect_parked(&self.temp_dir).await;
        enforce_parked_limits(ctx, &self.limits, &mut all).await;
        crate::tams::park::parking().sync_dir(
            &self.temp_dir,
            scanned_at,
            all.iter().map(|p| (p.path.clone(), p.start_ns, p.bytes)),
        );
        let present: HashSet<&Path> = all.iter().map(|p| p.path.as_path()).collect();
        self.retries
            .retain(|path, _| present.contains(path.as_path()));
        ctx.objects
            .retain(|path, _| present.contains(path.as_path()));

        let (own, foreign): (VecDeque<Parked>, VecDeque<Parked>) = all
            .into_iter()
            .partition(|p| p.target.as_ref().is_none_or(|t| *t == self.target));
        if foreign.len() != self.foreign_reported {
            self.foreign_reported = foreign.len();
            if !foreign.is_empty() {
                warn!(
                    "TAMS {}: {} segment(s) in {} were parked for another gateway or flow; \
                     they are kept but not uploaded to {} flow {}",
                    ctx.block_id,
                    foreign.len(),
                    self.temp_dir.display(),
                    self.target.gateway,
                    self.target.flow_id
                );
            }
        }
        let now = Instant::now();
        own.into_iter()
            .filter(|p| self.retries.get(&p.path).is_none_or(|r| r.next <= now))
            .collect()
    }

    /// Try one parked segment again and note how it went: a segment that is
    /// parked again waits out a growing backoff.
    async fn try_one(&mut self, ctx: &mut UploadCtx<'_>, p: Parked) {
        let path = p.path.clone();
        match recover_one(ctx, p).await {
            Some(Outcome::Parked) => {
                let retry = self.retries.entry(path.clone()).or_insert(Retry {
                    failures: 0,
                    next: Instant::now(),
                });
                retry.failures += 1;
                retry.next = Instant::now() + parked_backoff(retry.failures);
                if retry.failures == PARKED_STUCK_FAILURES {
                    warn!(
                        "TAMS {}: parked segment {} failed {} times; trying the others \
                         first, and it again every {} s at most",
                        ctx.block_id,
                        path.display(),
                        retry.failures,
                        PARKED_MAX_BACKOFF.as_secs()
                    );
                }
            }
            Some(_) => {
                self.retries.remove(&path);
            }
            None => {}
        }
    }
}

/// Delete the oldest parked segments until what is parked fits `limits`, with
/// an error event. A segment another uploader is busy with is left alone.
async fn enforce_parked_limits(
    ctx: &UploadCtx<'_>,
    limits: &ParkedLimits,
    parked: &mut VecDeque<Parked>,
) {
    let mut count = parked.len();
    let mut bytes: u64 = parked.iter().map(|p| p.bytes).sum();
    if count <= limits.max_segments && bytes <= limits.max_bytes {
        return;
    }
    let (mut dropped, mut dropped_bytes) = (0usize, 0u64);
    let mut kept = VecDeque::with_capacity(parked.len());
    while let Some(p) = parked.pop_front() {
        let over = count > limits.max_segments || bytes > limits.max_bytes;
        if over {
            if let Some(_claim) = Claim::take(&p.path) {
                discard_segment(&p.path).await;
                count -= 1;
                bytes = bytes.saturating_sub(p.bytes);
                dropped += 1;
                dropped_bytes += p.bytes;
                continue;
            }
        }
        kept.push_back(p);
    }
    *parked = kept;
    if dropped > 0 {
        let msg = format!(
            "parked segments over the cap ({} segments, {} MiB): deleted the {} oldest \
             ({} MiB); they are lost from the store",
            limits.max_segments,
            limits.max_bytes / (1024 * 1024),
            dropped,
            dropped_bytes / (1024 * 1024)
        );
        warn!("TAMS {}: {}", ctx.block_id, msg);
        emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
    }
}

/// Ensure the flow exists on the gateway, preferring the codec detected from caps
/// over the fallback. The flow is registered again whenever the detected codec or
/// essence parameters change (caps that arrive late or renegotiate, a frame rate
/// estimated from timestamps), so it never stays registered with what was known
/// at the first segment, and when it was found gone (see [`Failure::FlowGone`]).
/// Returns whether the flow is present.
async fn ensure_flow(ctx: &mut UploadCtx<'_>, attempts: u32) -> bool {
    let detected = ctx
        .detected_codec
        .lock()
        .map(|d| d.clone())
        .unwrap_or_default();
    if ctx.registered.as_ref() == Some(&detected) {
        return true;
    }
    let exists = ctx.registered.is_some();
    if let Some(codec) = &detected.codec {
        ctx.flow_spec.codec = Some(codec.clone());
    }
    for (k, v) in &detected.essence_parameters {
        ctx.flow_spec
            .essence_parameters
            .insert(k.clone(), v.clone());
    }
    if !ctx.flow_spec.collected.is_empty() {
        ctx.flow_spec.flow_collection = register_collected(ctx, &detected, attempts).await;
    }
    match retry(attempts, || ctx.client.ensure_flow(ctx.flow_spec)).await {
        Ok(()) => {
            info!(
                "TAMS {}: flow {} {} (codec {:?}, essence_parameters {})",
                ctx.block_id,
                ctx.flow_spec.flow_id,
                if exists { "updated" } else { "created" },
                ctx.flow_spec.codec,
                serde_json::Value::Object(ctx.flow_spec.essence_parameters.clone())
            );
            let params = &ctx.flow_spec.essence_parameters;
            if params.contains_key("frame_width") && !params.contains_key("frame_rate") {
                warn!(
                    "TAMS {}: flow {} has no frame_rate yet",
                    ctx.block_id, ctx.flow_spec.flow_id
                );
            }
            *ctx.registered = Some(detected);
            true
        }
        Err(e) if exists => {
            // The flow is there; the update is tried again at the next segment.
            warn!(
                "TAMS {}: flow {} not updated yet: {:#}",
                ctx.block_id, ctx.flow_spec.flow_id, e
            );
            true
        }
        Err(e) => {
            warn!(
                "TAMS {}: flow {} not created yet: {:#}",
                ctx.block_id, ctx.flow_spec.flow_id, e
            );
            false
        }
    }
}

/// Register the member flows a muxed flow collects (one per track whose
/// essence is known), before the flow itself, since a collection lists flows
/// that are already registered. Returns the collection items of the members
/// that were registered: those not yet known, or that failed, are left out
/// and tried again when the essence changes or the flow is registered again.
async fn register_collected(
    ctx: &UploadCtx<'_>,
    detected: &DetectedEssence,
    attempts: u32,
) -> Vec<CollectionItem> {
    let mut items = Vec::new();
    for member in &ctx.flow_spec.collected {
        let Some(track) = detected.tracks.get(&member.track) else {
            continue;
        };
        let mut spec = member.spec.clone();
        spec.codec = Some(track.codec.clone());
        if member.inherit_parameters {
            spec.essence_parameters = detected.essence_parameters.clone();
        }
        for (k, v) in &track.essence_parameters {
            spec.essence_parameters.insert(k.clone(), v.clone());
        }
        let mut mapping = member.container_mapping.clone();
        // All channels of the track belong to the member.
        if let Some(n) = track
            .essence_parameters
            .get("channels")
            .and_then(|c| c.as_u64())
            .filter(|n| *n > 0)
        {
            mapping.insert(
                "audio_track".into(),
                serde_json::json!({ "channel_range": format!("0_{}", n - 1) }),
            );
        }
        spec.container_mapping = mapping.clone();
        match retry(attempts, || ctx.client.ensure_flow(&spec)).await {
            Ok(()) => {
                info!(
                    "TAMS {}: member flow {} ({}) registered: codec {}, essence_parameters {}",
                    ctx.block_id,
                    spec.flow_id,
                    member.role,
                    track.codec,
                    serde_json::Value::Object(spec.essence_parameters.clone())
                );
                items.push(CollectionItem {
                    id: spec.flow_id.clone(),
                    role: member.role.clone(),
                    container_mapping: mapping,
                });
            }
            Err(e) => warn!(
                "TAMS {}: member flow {} ({}) not registered yet: {:#}",
                ctx.block_id, spec.flow_id, member.role, e
            ),
        }
    }
    items
}

/// What became of one segment.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Registered,
    /// Kept on disk with a sidecar for the recovery sweep.
    Parked,
    /// Rejected for good, or could not be parked; deleted.
    Dropped,
    /// Its file could not be read; left to the caller.
    Unreadable,
}

/// Why an upload failed, and so what to do with the segment.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The gateway does not know the flow (404): it was deleted behind our
    /// back. Create it again and retry.
    FlowGone,
    /// 401/403: the credentials are wrong or expired. Someone has to fix them;
    /// the segment is parked until then, not thrown away.
    Auth,
    /// Any other 4xx: the request itself is wrong and will fail the same way.
    Rejected,
    /// Network error, timeout, 5xx, 408, 429.
    Transient,
}

fn classify(e: &anyhow::Error) -> Failure {
    let Some(h) = e.downcast_ref::<crate::tams::client::HttpStatusError>() else {
        return Failure::Transient;
    };
    // The presigned PUT goes to storage, not the gateway: a 404 there is no
    // missing flow, and a 403 there is an expired signature, which a new
    // allocation fixes like an auth problem would.
    let gateway = !h.context.starts_with("presigned");
    match h.status.as_u16() {
        404 if gateway => Failure::FlowGone,
        401 | 403 => Failure::Auth,
        _ if !h.is_retryable() => Failure::Rejected,
        _ => Failure::Transient,
    }
}

/// Upload and register one segment, making `attempts` tries with backoff. On a
/// transient or auth failure the segment is parked on disk for the recovery
/// sweep; on a definitive rejection it is dropped with an error. With
/// `maybe_registered` (a parked segment, whose earlier try may have succeeded
/// without us hearing it), the gateway is asked first whether it already has
/// the segment; every retry asks too.
async fn upload_segment(
    ctx: &mut UploadCtx<'_>,
    frag: &FragmentReady,
    attempts: u32,
    mut maybe_registered: bool,
) -> Outcome {
    let bytes = match tokio::fs::read(&frag.path).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("reading segment {}: {}", frag.path.display(), e);
            error!("TAMS {}: {}", ctx.block_id, msg);
            emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
            return Outcome::Unreadable;
        }
    };
    debug!(
        "TAMS {}: uploading segment {} ({} bytes) to flow {}",
        ctx.block_id,
        frag.path.display(),
        bytes.len(),
        ctx.flow_spec.flow_id
    );

    let mut recreated = false;
    // The object an earlier try of this segment allocated, if any.
    let slot = Mutex::new(ctx.objects.remove(&frag.path));
    loop {
        if !ensure_flow(ctx, attempts).await {
            ctx.set_failing(true);
            return park_kept(ctx, frag, slot, "flow not created").await;
        }
        let tams_flow_id = ctx.flow_spec.flow_id.clone();
        let client = ctx.client;
        let content_type = ctx.content_type;
        let mut check = maybe_registered;
        let result = retry(attempts, || {
            let c = check;
            check = true;
            upload_one(client, &tams_flow_id, content_type, frag, &bytes, c, &slot)
        })
        .await;
        let e = match result {
            Ok((object_id, timerange)) => {
                on_registered(ctx, &frag.path, object_id, timerange).await;
                ctx.set_failing(false);
                return Outcome::Registered;
            }
            Err(e) => e,
        };
        match classify(&e) {
            Failure::FlowGone if !recreated => {
                warn!(
                    "TAMS {}: flow {} is gone from the gateway ({:#}); creating it again",
                    ctx.block_id, tams_flow_id, e
                );
                *ctx.registered = None;
                recreated = true;
                // A new flow has none of our segments, nor their objects.
                maybe_registered = false;
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
                ctx.objects.clear();
            }
            Failure::Auth => {
                let msg = format!(
                    "gateway refused the credentials, segment parked until they are fixed: {:#}",
                    e
                );
                // Reported once, then every few minutes: the sweep tries again
                // every few seconds while they stay refused.
                if ctx.should_report_auth() {
                    error!("TAMS {}: {}", ctx.block_id, msg);
                    emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
                } else {
                    debug!("TAMS {}: {}", ctx.block_id, msg);
                }
                ctx.set_failing(true);
                return park_kept(ctx, frag, slot, "credentials refused").await;
            }
            Failure::Rejected => {
                // A register whose answer was lost but that went through comes
                // back as a rejection (overlap): it is not lost.
                if let Ok(Some(object_id)) = registered_object(client, &tams_flow_id, frag).await {
                    let timerange =
                        crate::tams::client::format_timerange(frag.start_ns, frag.end_ns);
                    on_registered(ctx, &frag.path, object_id, timerange).await;
                    return Outcome::Registered;
                }
                // Identical bytes will be rejected the same way forever — give up.
                let msg = format!(
                    "segment rejected ({} bytes), discarding: {:#}",
                    bytes.len(),
                    e
                );
                error!("TAMS {}: {}", ctx.block_id, msg);
                emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
                discard_segment(&frag.path).await;
                return Outcome::Dropped;
            }
            Failure::FlowGone | Failure::Transient => {
                if !ctx.failing {
                    emit_error(
                        ctx.events,
                        ctx.strom_flow_id,
                        ctx.block_id,
                        format!("segment upload failed, parked for retry: {:#}", e),
                    );
                }
                ctx.set_failing(true);
                return park_kept(ctx, frag, slot, &format!("{:#}", e)).await;
            }
        }
    }
}

/// Park a segment that failed, keeping the object allocated for it (in
/// `slot`) for its next try.
async fn park_kept(
    ctx: &mut UploadCtx<'_>,
    frag: &FragmentReady,
    slot: Mutex<Option<Allocation>>,
    reason: &str,
) -> Outcome {
    let outcome = park_segment(ctx, frag, reason).await;
    if outcome == Outcome::Parked {
        if let Some(a) = slot.into_inner().unwrap_or_else(|e| e.into_inner()) {
            ctx.objects.insert(frag.path.clone(), a);
        }
    }
    outcome
}

/// Upload and register one freshly-rotated fragment, or park it at once while
/// the gateway is failing.
async fn process_fragment(ctx: &mut UploadCtx<'_>, frag: FragmentReady) -> Outcome {
    if ctx.failing {
        return park_segment(ctx, &frag, "gateway failing").await;
    }
    // An unreadable fresh file is a local problem, unlikely to fix itself; it
    // is reported and left in place rather than deleting unverified data.
    upload_segment(ctx, &frag, LIVE_ATTEMPTS, false).await
}

/// A segment parked on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parked {
    path: PathBuf,
    start_ns: u64,
    end_ns: u64,
    /// Size of the segment file.
    bytes: u64,
    /// Where it is meant to go; `None` for a sidecar written before sidecars
    /// carried it.
    target: Option<ParkTarget>,
}

/// Try one parked segment again. Skipped (`None`) when another uploader has
/// it, or it is gone already.
async fn recover_one(ctx: &mut UploadCtx<'_>, p: Parked) -> Option<Outcome> {
    let _claim = Claim::take(&p.path)?;
    if !tokio::fs::try_exists(&p.path).await.unwrap_or(false) {
        return None;
    }
    let frag = FragmentReady {
        path: p.path,
        start_ns: p.start_ns,
        end_ns: p.end_ns,
    };
    let outcome = upload_segment(ctx, &frag, RECOVERY_ATTEMPTS, true).await;
    match outcome {
        Outcome::Registered => info!(
            "TAMS {}: recovered parked segment {}",
            ctx.block_id,
            frag.path.display()
        ),
        // Sidecar without readable bytes is useless — drop the pair.
        Outcome::Unreadable => discard_segment(&frag.path).await,
        Outcome::Parked | Outcome::Dropped => {}
    }
    Some(outcome)
}

fn log_backlog(ctx: &UploadCtx<'_>, backlog: &VecDeque<Parked>) {
    if !backlog.is_empty() {
        info!(
            "TAMS {}: {} parked segment(s) to upload for flow {}",
            ctx.block_id,
            backlog.len(),
            ctx.flow_spec.flow_id
        );
    }
}

/// Record a successful registration: emit the event, bump the counter, delete the
/// segment file and any sidecar.
async fn on_registered(
    ctx: &mut UploadCtx<'_>,
    segment: &Path,
    object_id: String,
    timerange: String,
) {
    ctx.events.broadcast(StromEvent::TamsSegmentRegistered {
        flow_id: ctx.strom_flow_id,
        block_id: ctx.block_id.to_string(),
        tams_flow_id: ctx.flow_spec.flow_id.clone(),
        object_id,
        timerange,
    });
    *ctx.segments_registered += 1;
    // Surface progress at info so a healthy uploader is visible without debug
    // logging: the first segment, then every 30th thereafter.
    if *ctx.segments_registered == 1 || ctx.segments_registered.is_multiple_of(30) {
        info!(
            "TAMS {}: {} segments registered on flow {}",
            ctx.block_id, *ctx.segments_registered, ctx.flow_spec.flow_id
        );
    }
    discard_segment(segment).await;
}

/// Park a segment on disk for the recovery sweep: keep its bytes and write a `.meta`
/// sidecar with the timerange so it can be re-registered later (even after a restart).
/// When it cannot be parked (over the server's cap or short of free space even
/// after deleting older parked segments, or the sidecar cannot be written), the
/// segment cannot be recovered: it is deleted, to free the space, with an error.
async fn park_segment(ctx: &UploadCtx<'_>, frag: &FragmentReady, reason: &str) -> Outcome {
    let target = ParkTarget::new(ctx.client.base_url(), &ctx.flow_spec.flow_id);
    let frag_owned = FragmentReady {
        path: frag.path.clone(),
        start_ns: frag.start_ns,
        end_ns: frag.end_ns,
    };
    let written = tokio::task::spawn_blocking(move || park_on_disk(&frag_owned, &target))
        .await
        .unwrap_or_else(|e| {
            Err(crate::tams::park::ParkError::Io(std::io::Error::other(
                e.to_string(),
            )))
        });
    if let Err(e) = written {
        let msg = format!(
            "could not park segment {} ({}), dropping it: {}",
            frag.path.display(),
            reason,
            e
        );
        error!("TAMS {}: {}", ctx.block_id, msg);
        emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
        discard_segment(&frag.path).await;
        return Outcome::Dropped;
    }
    debug!(
        "TAMS {}: parked segment {} for retry ({})",
        ctx.block_id,
        frag.path.display(),
        reason
    );
    Outcome::Parked
}

/// Park a finished segment on disk from outside the uploader (the streaming
/// side, when the uploader's channel is full or closed): write its sidecar so
/// the recovery sweep of this run or a later one uploads it to `target`.
/// Blocking: a small write and a sync to disk, and older parked segments may
/// be deleted to make room (see [`crate::tams::park::Parking::park`]); from a
/// streaming thread, call it through [`crate::tams::park::park_off_thread`].
/// On an error (over the server's cap or short of free space, or the disk is
/// full) the caller drops the segment.
pub fn park_on_disk(
    frag: &FragmentReady,
    target: &ParkTarget,
) -> Result<(), crate::tams::park::ParkError> {
    crate::tams::park::parking().park(
        &frag.path,
        frag.start_ns,
        &sidecar_contents(frag.start_ns, frag.end_ns, target),
    )
}

/// Run an async operation up to `attempts` times, with jittered exponential backoff.
async fn retry<T, F, Fut>(attempts: u32, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut last_err = None;
    for attempt in 0..attempts {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                // A non-retryable HTTP status (e.g. 413 Payload Too Large, 401
                // Unauthorized) will fail identically on every attempt — give up
                // now rather than sleeping through the whole backoff schedule.
                // (A 401 from a stale OSC SAT has already been retried once with
                // a new SAT inside `TamsClient`; one that still fails is real.)
                if !is_retryable(&e) {
                    return Err(e);
                }
                if attempt + 1 < attempts {
                    tokio::time::sleep(backoff_with_jitter(attempt)).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("operation failed")))
}

/// Exponential backoff (1,2,4,8,16,32s) clamped to [`MAX_BACKOFF`], plus up to ~1s of
/// jitter so many uploaders don't retry in lockstep against an already-stressed
/// gateway (thundering herd).
fn backoff_with_jitter(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(5)).min(MAX_BACKOFF);
    base + Duration::from_millis(jitter_ms(1000))
}

/// Cheap, dependency-free jitter in `0..=max_ms`, seeded from the wall-clock subsec
/// nanos. Not cryptographic — only needs to de-correlate concurrent retriers.
fn jitter_ms(max_ms: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    u64::from(nanos) % (max_ms + 1)
}

/// Whether an error is worth retrying. A gateway/storage HTTP 4xx (except
/// 408/429) cannot succeed on retry; everything else (network errors, 5xx) can.
fn is_retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::tams::client::HttpStatusError>()
        .map(|h| h.is_retryable())
        .unwrap_or(true)
}

/// The object of a segment the flow already has with exactly this timerange,
/// if any. A store that cannot answer the query (a 4xx other than 404) is taken
/// to have none.
async fn registered_object(
    client: &TamsClient,
    tams_flow_id: &str,
    frag: &FragmentReady,
) -> anyhow::Result<Option<String>> {
    let timerange = crate::tams::client::format_timerange(frag.start_ns, frag.end_ns);
    let query = crate::tams::client::SegmentQuery {
        timerange: Some(timerange),
        limit: Some(10),
        reverse_order: false,
    };
    let listed = match client.list_segments(tams_flow_id, &query, 1).await {
        Ok(l) => l,
        Err(e) if classify(&e) == Failure::Rejected => return Ok(None),
        Err(e) => return Err(e),
    };
    let want = (frag.start_ns as i64, frag.end_ns as i64);
    Ok(listed
        .into_iter()
        .find(|s| {
            strom_types::tams::parse_timerange(&s.timerange).and_then(|t| t.half_open())
                == Some(want)
        })
        .map(|s| s.object_id))
}

/// Allocate, upload and register a single fragment (bytes already read by the
/// caller). Returns the registered `(object_id, timerange)` on success. With
/// `check_existing`, a segment the flow already has with this timerange (an
/// earlier try registered it, but its answer was lost) counts as success
/// instead of being registered a second time.
///
/// `slot` holds the object an earlier try allocated: it is used again rather
/// than allocating a new one, and its upload is skipped when the bytes are in
/// it already. Storage refusing the presigned PUT (an expired signature)
/// clears it, so the next try allocates afresh.
async fn upload_one(
    client: &TamsClient,
    tams_flow_id: &str,
    content_type: &str,
    frag: &FragmentReady,
    bytes: &[u8],
    check_existing: bool,
    slot: &Mutex<Option<Allocation>>,
) -> anyhow::Result<(String, String)> {
    let lock = || slot.lock().unwrap_or_else(|e| e.into_inner());
    let timerange = crate::tams::client::format_timerange(frag.start_ns, frag.end_ns);
    if check_existing {
        if let Some(object_id) = registered_object(client, tams_flow_id, frag).await? {
            debug!(
                "TAMS: segment {} already registered on flow {} as {}",
                timerange, tams_flow_id, object_id
            );
            return Ok((object_id, timerange));
        }
    }
    let cached = lock().clone();
    let alloc = match cached {
        Some(a) => a,
        None => {
            let object = client.allocate_object(tams_flow_id, content_type).await?;
            let a = Allocation {
                object,
                uploaded: false,
            };
            *lock() = Some(a.clone());
            a
        }
    };
    let obj = &alloc.object;
    if !alloc.uploaded {
        if let Err(e) = client
            .upload_object(&obj.put_url, &obj.content_type, bytes.to_vec())
            .await
        {
            if classify(&e) != Failure::Transient {
                *lock() = None;
            }
            return Err(e);
        }
        if let Some(a) = lock().as_mut() {
            a.uploaded = true;
        }
    }
    client
        .register_segment(tams_flow_id, &obj.object_id, &timerange)
        .await?;

    debug!(
        "TAMS: registered segment {} {} on flow {}",
        obj.object_id, timerange, tams_flow_id
    );
    Ok((alloc.object.object_id, timerange))
}

fn emit_error(events: &EventBroadcaster, flow_id: FlowId, block_id: &str, error: String) {
    events.broadcast(StromEvent::TamsError {
        flow_id,
        block_id: block_id.to_string(),
        error,
    });
}

// --- Parked-segment sidecars -------------------------------------------------

/// Sidecar path for a segment: `seg_00001.ts` -> `seg_00001.ts.meta`.
pub(crate) fn sidecar_path(segment: &Path) -> PathBuf {
    let mut s = segment.as_os_str().to_owned();
    s.push(SIDECAR_SUFFIX);
    PathBuf::from(s)
}

/// Temporary file a sidecar is written to before it is renamed into place.
pub(crate) fn sidecar_tmp_path(segment: &Path) -> PathBuf {
    let mut s = sidecar_path(segment).into_os_string();
    s.push(SIDECAR_TMP_SUFFIX);
    PathBuf::from(s)
}

/// Sidecar contents: the timerange and the target, as JSON.
fn sidecar_contents(start_ns: u64, end_ns: u64, target: &ParkTarget) -> String {
    serde_json::json!({
        "start_ns": start_ns,
        "end_ns": end_ns,
        "gateway": target.gateway,
        "flow_id": target.flow_id,
    })
    .to_string()
}

/// Parse a sidecar: the JSON of [`sidecar_contents`], or `"<start_ns> <end_ns>"`
/// as written before sidecars carried their target.
pub(crate) fn parse_sidecar(s: &str) -> Option<(u64, u64, Option<ParkTarget>)> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        let start = v.get("start_ns")?.as_u64()?;
        let end = v.get("end_ns")?.as_u64()?;
        let target = ParkTarget {
            gateway: v.get("gateway")?.as_str()?.to_string(),
            flow_id: v.get("flow_id")?.as_str()?.to_string(),
        };
        return Some((start, end, Some(target)));
    }
    let mut it = s.split_whitespace();
    let start = it.next()?.parse().ok()?;
    let end = it.next()?.parse().ok()?;
    it.next().is_none().then_some((start, end, None))
}

/// Scan `temp_dir` for parked segments (a `.meta` sidecar plus its segment file),
/// oldest first. A sidecar whose segment is gone, or that cannot be parsed, is
/// cleaned up here. Empty when the directory can't be read.
async fn collect_parked(temp_dir: &Path) -> VecDeque<Parked> {
    let mut parked = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(temp_dir).await else {
        return VecDeque::new();
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let sidecar = entry.path();
        let Some(segment) = sidecar
            .to_str()
            .and_then(|s| s.strip_suffix(SIDECAR_SUFFIX))
            .map(PathBuf::from)
        else {
            continue;
        };
        let range = tokio::fs::read_to_string(&sidecar)
            .await
            .ok()
            .and_then(|s| parse_sidecar(&s));
        let size = tokio::fs::metadata(&segment).await.ok().map(|m| m.len());
        match (range, size) {
            (Some((start_ns, end_ns, target)), Some(bytes)) => {
                parked.push(Parked {
                    path: segment,
                    start_ns,
                    end_ns,
                    bytes,
                    target,
                });
            }
            _ => {
                // Orphan/corrupt sidecar (no segment, or unparseable): clean it
                // up, unless another uploader is busy with its segment.
                if let Some(_claim) = Claim::take(&segment) {
                    let _ = tokio::fs::remove_file(&sidecar).await;
                }
            }
        }
    }
    // Stable order so segments recover roughly in capture order.
    parked.sort_by_key(|p| p.start_ns);
    parked.into()
}

/// Delete a segment file and its sidecar (if any). Used on success and on giving up.
async fn discard_segment(segment: &Path) {
    crate::tams::park::parking().forget(segment);
    if let Err(e) = tokio::fs::remove_file(segment).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                "TAMS: could not remove segment {}: {}",
                segment.display(),
                e
            );
        }
    }
    let _ = tokio::fs::remove_file(sidecar_path(segment)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time_segment(start_ns: u64, base_ns: u64) -> gst::Segment {
        let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
        seg.set_start(gst::ClockTime::from_nseconds(start_ns));
        seg.set_base(gst::ClockTime::from_nseconds(base_ns));
        seg.upcast()
    }

    /// The last fragment of a run ends where its last frame ends, not one full
    /// segment after its start.
    #[test]
    fn tail_ends_at_the_last_frame() {
        gst::init().unwrap();
        const FRAME: u64 = 20_000_000;
        let pad = Arc::new(PadMediaEnd::default());
        // PTS 1000 s + running time, as after an encoder with a PTS offset.
        pad.set_segment(&time_segment(1_000_000_000_000, 0));
        let first_rt = 40 * 1_000_000_000;
        for n in 0..7 {
            pad.add_buffer(Some(1_000_000_000_000 + first_rt + n * FRAME), FRAME);
        }
        let end: MediaEnd = Arc::new(Mutex::new(vec![pad]));
        let tail = TailFragment {
            path: PathBuf::new(),
            start_ns: 1_800_000_000_000_000_000,
            start_running_time_ns: first_rt,
        };
        let rt = media_end_running_time_ns(&end);
        assert_eq!(rt, Some(first_rt + 7 * FRAME));
        assert_eq!(
            tail_end_ns(&tail, rt, 1_000_000_000),
            tail.start_ns + 7 * FRAME
        );
    }

    /// With a muxer that reports consumed buffers, buffers still queued in it
    /// when the pipeline stops are not counted: they are never written.
    #[test]
    fn only_consumed_buffers_count() {
        gst::init().unwrap();
        let pad = PadMediaEnd::default();
        pad.count_consumed();
        pad.set_segment(&time_segment(1_000, 0));
        assert_eq!(pad.end_running_time_ns(), None);
        for n in 0..5 {
            pad.add_buffer(Some(1_000 + n * 20), 20);
        }
        assert_eq!(pad.end_running_time_ns(), None, "nothing consumed yet");
        for _ in 0..4 {
            pad.consume();
        }
        // Buffers 0..=3 written, buffer 4 still queued.
        assert_eq!(pad.end_running_time_ns(), Some(80));
        // More queued than the ring holds: unknown.
        for n in 5..(5 + PAD_RING as u64) {
            pad.add_buffer(Some(1_000 + n * 20), 20);
        }
        assert_eq!(pad.end_running_time_ns(), None);
    }

    #[test]
    fn a_fragment_before_a_stall_ends_at_its_media() {
        let frag = TailFragment {
            path: PathBuf::new(),
            start_ns: 10_000_000_000,
            start_running_time_ns: 1_000_000_000,
        };
        let frame = 21_000_000;
        // 0.9 s of media, then 5.3 s without input: ends at its media.
        assert_eq!(
            closed_end_ns(
                &frag,
                16_200_000_000,
                Some(1_900_000_000),
                7_200_000_000,
                frame
            ),
            10_900_000_000
        );
        // Contiguous, or a frame or two missing at the split: as planned.
        assert_eq!(
            closed_end_ns(
                &frag,
                11_000_000_000,
                Some(2_000_000_000),
                2_000_000_000,
                frame
            ),
            11_000_000_000
        );
        assert_eq!(
            closed_end_ns(
                &frag,
                11_000_000_000,
                Some(1_940_000_000),
                2_000_000_000,
                frame
            ),
            11_000_000_000
        );
        // Media end unknown, stale, or past the next start (running time went
        // back): as planned.
        for media in [None, Some(900_000_000), Some(8_000_000_000)] {
            assert_eq!(
                closed_end_ns(&frag, 16_200_000_000, media, 7_200_000_000, frame),
                16_200_000_000
            );
        }
        // Never later than planned (a backward jump cut it short).
        assert_eq!(
            closed_end_ns(
                &frag,
                10_500_000_000,
                Some(1_900_000_000),
                7_200_000_000,
                frame
            ),
            10_500_000_000
        );
    }

    #[test]
    fn tail_without_media_end_falls_back_to_nominal() {
        gst::init().unwrap();
        let tail = TailFragment {
            path: PathBuf::new(),
            start_ns: 5_000,
            start_running_time_ns: 100,
        };
        assert_eq!(tail_end_ns(&tail, None, 1_000), 6_000);
        // Media that ends at or before the start (stale) is not trusted.
        assert_eq!(tail_end_ns(&tail, Some(100), 1_000), 6_000);
        // A pad that saw no buffer reports nothing.
        let pad = PadMediaEnd::default();
        pad.set_segment(&time_segment(0, 0));
        assert_eq!(pad.end_running_time_ns(), None);
        pad.add_buffer(Some(500), 20);
        assert_eq!(pad.end_running_time_ns(), Some(520));
        // Several pads: the latest end wins.
        let a = Arc::new(PadMediaEnd::default());
        let b = Arc::new(PadMediaEnd::default());
        a.set_segment(&time_segment(0, 0));
        b.set_segment(&time_segment(0, 1_000));
        a.add_buffer(Some(300), 20);
        b.add_buffer(Some(10), 10);
        assert_eq!(
            media_end_running_time_ns(&Arc::new(Mutex::new(vec![a, b]))),
            Some(1_020)
        );
    }

    #[test]
    fn sidecar_path_appends_meta() {
        let seg = PathBuf::from("/tmp/x/seg_r1_00007.ts");
        assert_eq!(
            sidecar_path(&seg),
            PathBuf::from("/tmp/x/seg_r1_00007.ts.meta")
        );
    }

    #[tokio::test]
    async fn parked_segment_is_collected_and_discarded() {
        let dir =
            std::env::temp_dir().join(format!("strom-tams-uploader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let seg = dir.join("seg_r1_00001.ts");
        std::fs::write(&seg, b"segment-bytes").unwrap();
        let frag = FragmentReady {
            path: seg.clone(),
            start_ns: 12_000_000_000,
            end_ns: 14_000_000_000,
        };
        let target = ParkTarget::new("http://gw", "flow-1");
        park_on_disk(&frag, &target).unwrap();

        let parked = collect_parked(&dir).await;
        assert_eq!(
            parked,
            VecDeque::from([Parked {
                path: seg.clone(),
                start_ns: 12_000_000_000,
                end_ns: 14_000_000_000,
                bytes: 13,
                target: Some(target),
            }])
        );

        // discard removes both the segment and its sidecar.
        discard_segment(&seg).await;
        assert!(!seg.exists());
        assert!(!sidecar_path(&seg).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn collect_parked_cleans_orphan_sidecar() {
        let dir =
            std::env::temp_dir().join(format!("strom-tams-uploader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Sidecar with no matching segment file.
        let orphan = dir.join("seg_r1_00002.ts.meta");
        std::fs::write(&orphan, b"1 2").unwrap();

        assert!(collect_parked(&dir).await.is_empty());
        assert!(!orphan.exists(), "orphan sidecar should be cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two runs of one block never share a segment file name, and clearing
    /// orphans at the start of a run keeps parked segments and the files of a
    /// run that is still live. With shared names, a restart would truncate
    /// parked bytes, and recovery would upload the new run's file under the
    /// old run's timerange.
    #[test]
    fn runs_have_their_own_segment_files() {
        let dir =
            std::env::temp_dir().join(format!("strom-tams-uploader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = new_run();
        let new = new_run();
        assert_ne!(old.id(), new.id());
        let old_parked = dir.join(segment_file_name(old.id(), 0, "ts"));
        let new_first = dir.join(segment_file_name(new.id(), 0, "ts"));
        assert_ne!(old_parked, new_first);
        assert_eq!(
            run_of(&segment_file_name(old.id(), 7, "ts")),
            Some(old.id())
        );
        assert_eq!(run_of("seg_00001.ts"), None);

        std::fs::write(&old_parked, b"old").unwrap();
        park_on_disk(
            &FragmentReady {
                path: old_parked.clone(),
                start_ns: 1,
                end_ns: 2,
            },
            &ParkTarget::new("http://gw", "flow-1"),
        )
        .unwrap();
        let old_unparked = dir.join(segment_file_name(old.id(), 1, "ts"));
        std::fs::write(&old_unparked, b"old, not parked").unwrap();
        let legacy = dir.join("seg_00003.ts");
        std::fs::write(&legacy, b"legacy").unwrap();
        std::fs::write(&new_first, b"new").unwrap();

        // The old run's uploader still runs: only the legacy orphan goes.
        assert_eq!(clear_orphans(&dir), 1);
        assert!(!legacy.exists());
        assert!(old_unparked.exists() && old_parked.exists() && new_first.exists());

        // The old run is over: its unparked file is an orphan, its parked one is kept.
        drop(old);
        assert_eq!(clear_orphans(&dir), 1);
        assert!(!old_unparked.exists());
        assert!(old_parked.exists() && new_first.exists());
        assert_eq!(std::fs::read(&old_parked).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backoff_is_clamped_and_jittered() {
        // A high attempt index would overflow the exponential; it must clamp to
        // MAX_BACKOFF and add at most ~1s of jitter.
        let d = backoff_with_jitter(10);
        assert!(d >= MAX_BACKOFF);
        assert!(d <= MAX_BACKOFF + Duration::from_millis(1000));
        // Early attempts follow the exponential (1s) plus jitter.
        let d0 = backoff_with_jitter(0);
        assert!(d0 >= Duration::from_secs(1));
        assert!(d0 <= Duration::from_secs(1) + Duration::from_millis(1000));
    }

    /// A gateway that answers every request with 201 and keeps the bodies of
    /// the flow PUTs it got.
    async fn fake_gateway() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let puts = Arc::new(Mutex::new(Vec::new()));
        let seen = puts.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut request_line = String::new();
                        if stream.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let mut len = 0usize;
                        loop {
                            let mut line = String::new();
                            stream.read_line(&mut line).await.unwrap();
                            let line = line.trim_end();
                            if line.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = line.split_once(':') {
                                if k.eq_ignore_ascii_case("content-length") {
                                    len = v.trim().parse().unwrap();
                                }
                            }
                        }
                        let mut body = vec![0; len];
                        stream.read_exact(&mut body).await.unwrap();
                        if request_line.starts_with("PUT /flows/") {
                            seen.lock()
                                .unwrap()
                                .push(serde_json::from_slice(&body).unwrap());
                        }
                        stream
                            .get_mut()
                            .write_all(b"HTTP/1.1 201 Created\r\ncontent-length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    }
                });
            }
        });
        (format!("http://{}", addr), puts)
    }

    /// The flow is registered again when the detected essence changes (a frame
    /// rate that is only known after the first segment), and not otherwise.
    #[tokio::test]
    async fn flow_is_updated_when_the_essence_changes() {
        let (url, puts) = fake_gateway().await;
        let client = TamsClient::new(url, crate::client_auth::AuthMethod::None).unwrap();
        let mut spec = FlowSpec {
            flow_id: "flow-1".into(),
            source_id: "src-1".into(),
            format: crate::tams::client::FORMAT_VIDEO.into(),
            codec: Some("video/h264".into()),
            container: Some("video/mp2t".into()),
            label: None,
            description: None,
            tags: vec![],
            flow_collection: vec![],
            collected: vec![],
            container_mapping: Default::default(),
            essence_parameters: Default::default(),
        };
        let detected: SharedDetected = Arc::default();
        detected
            .lock()
            .unwrap()
            .essence_parameters
            .insert("frame_width".into(), 1280.into());
        let events = EventBroadcaster::default();
        let mut registered = None;
        let mut segments = 0;
        let mut ctx = UploadCtx {
            client: &client,
            registered: &mut registered,
            flow_spec: &mut spec,
            detected_codec: &detected,
            content_type: "video/mp2t",
            events: &events,
            strom_flow_id: FlowId::new_v4(),
            block_id: "t",
            segments_registered: &mut segments,
            failing: false,
            objects: HashMap::new(),
            auth_reported_at: None,
        };
        assert!(ensure_flow(&mut ctx, 1).await);
        assert!(ensure_flow(&mut ctx, 1).await);
        assert_eq!(puts.lock().unwrap().len(), 1);

        detected.lock().unwrap().essence_parameters.insert(
            "frame_rate".into(),
            serde_json::json!({ "numerator": 30, "denominator": 1 }),
        );
        assert!(ensure_flow(&mut ctx, 1).await);
        assert!(ensure_flow(&mut ctx, 1).await);
        let puts = puts.lock().unwrap();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[0]["essence_parameters"]["frame_width"], 1280);
        assert!(puts[0]["essence_parameters"].get("frame_rate").is_none());
        assert_eq!(puts[1]["essence_parameters"]["frame_rate"]["numerator"], 30);
        assert_eq!(puts[1]["essence_parameters"]["frame_width"], 1280);
    }

    /// A TAMS gateway stub whose answers the test sets: the status of each
    /// storage allocation and registration (taken in order, then 2xx), and the
    /// segments the flow already has.
    #[derive(Clone, Default)]
    struct Stub {
        base: Arc<Mutex<String>>,
        storage: Arc<Mutex<VecDeque<u16>>>,
        register: Arc<Mutex<VecDeque<u16>>>,
        listed: Arc<Mutex<Vec<serde_json::Value>>>,
        flow_puts: Arc<AtomicU64>,
        allocations: Arc<AtomicU64>,
        registrations: Arc<Mutex<Vec<String>>>,
    }

    async fn start_stub(stub: Stub) -> String {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{get, post, put};
        use axum::{Json, Router};
        fn status(q: &Mutex<VecDeque<u16>>) -> Option<StatusCode> {
            q.lock()
                .unwrap()
                .pop_front()
                .map(|c| StatusCode::from_u16(c).unwrap())
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        *stub.base.lock().unwrap() = base.clone();
        let app = Router::new()
            .route(
                "/flows/{id}",
                put(|State(s): State<Stub>| async move {
                    s.flow_puts.fetch_add(1, Ordering::SeqCst);
                    StatusCode::CREATED
                }),
            )
            .route(
                "/flows/{id}/storage",
                post(|State(s): State<Stub>| async move {
                    s.allocations.fetch_add(1, Ordering::SeqCst);
                    if let Some(code) = status(&s.storage) {
                        return (code, Json(serde_json::json!({}))).into_response();
                    }
                    let base = s.base.lock().unwrap().clone();
                    Json(serde_json::json!({ "media_objects": [{
                        "object_id": "obj-new",
                        "put_url": { "url": format!("{}/upload", base), "content-type": "video/mp2t" }
                    }]}))
                    .into_response()
                }),
            )
            .route("/upload", put(|| async { StatusCode::OK }))
            .route(
                "/flows/{id}/segments",
                get(|State(s): State<Stub>| async move {
                    Json(serde_json::Value::Array(s.listed.lock().unwrap().clone()))
                })
                .post(
                    |State(s): State<Stub>, Json(body): Json<serde_json::Value>| async move {
                        if let Some(code) = status(&s.register) {
                            return code;
                        }
                        let tr = body["timerange"].as_str().unwrap_or_default().to_string();
                        s.registrations.lock().unwrap().push(tr);
                        StatusCode::CREATED
                    },
                ),
            )
            .with_state(stub);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }
    fn spec() -> FlowSpec {
        FlowSpec {
            flow_id: "flow-1".into(),
            source_id: "src-1".into(),
            format: crate::tams::client::FORMAT_VIDEO.into(),
            codec: Some("video/h264".into()),
            container: Some("video/mp2t".into()),
            label: None,
            description: None,
            tags: vec![],
            flow_collection: vec![],
            collected: vec![],
            container_mapping: Default::default(),
            essence_parameters: Default::default(),
        }
    }

    /// Upload one segment file through [`upload_segment`] against `stub`, with
    /// the flow already registered. Returns the outcome, whether the uploader
    /// is now failing, and the segment file.
    async fn upload_with(stub: &Stub, maybe_registered: bool) -> (Outcome, bool, PathBuf) {
        upload_as(stub, maybe_registered, None).await
    }

    /// As [`upload_with`]; with `Some(failing)`, as a fresh fragment of an
    /// uploader in that state.
    async fn upload_as(
        stub: &Stub,
        maybe_registered: bool,
        fresh: Option<bool>,
    ) -> (Outcome, bool, PathBuf) {
        let url = start_stub(stub.clone()).await;
        let client = TamsClient::new(url, crate::client_auth::AuthMethod::None).unwrap();
        let mut spec = spec();
        let detected: SharedDetected = Arc::default();
        let events = EventBroadcaster::default();
        let mut registered = Some(DetectedEssence::default());
        let mut segments = 0;
        let mut ctx = UploadCtx {
            client: &client,
            registered: &mut registered,
            flow_spec: &mut spec,
            detected_codec: &detected,
            content_type: "video/mp2t",
            events: &events,
            strom_flow_id: FlowId::new_v4(),
            block_id: "t",
            segments_registered: &mut segments,
            failing: fresh.unwrap_or(false),
            objects: HashMap::new(),
            auth_reported_at: None,
        };
        let dir =
            std::env::temp_dir().join(format!("strom-tams-uploader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seg_r1_00000.ts");
        std::fs::write(&path, b"bytes").unwrap();
        let frag = FragmentReady {
            path: path.clone(),
            start_ns: 10_000_000_000,
            end_ns: 12_000_000_000,
        };
        let outcome = match fresh {
            Some(_) => process_fragment(&mut ctx, frag).await,
            None => upload_segment(&mut ctx, &frag, 1, maybe_registered).await,
        };
        (outcome, ctx.failing, path)
    }

    /// While the gateway is failing, a fresh segment is parked at once, without
    /// a request or backoff, so a long outage does not back up the channel.
    /// Retrying each one inline (about 61 s per segment) would fill it in
    /// minutes and then drop segments. A healthy uploader uploads it.
    #[tokio::test]
    async fn failing_gateway_parks_fresh_segments_at_once() {
        let stub = Stub::default();
        let started = std::time::Instant::now();
        let (outcome, failing, path) = upload_as(&stub, false, Some(true)).await;
        assert_eq!(outcome, Outcome::Parked);
        assert!(failing);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(stub.allocations.load(Ordering::SeqCst), 0);
        assert!(path.exists() && sidecar_path(&path).exists());

        let stub = Stub::default();
        let (outcome, failing, _) = upload_as(&stub, false, Some(false)).await;
        assert_eq!(outcome, Outcome::Registered);
        assert!(!failing);
    }

    /// A flow deleted on the gateway (404) is created again and the segment
    /// registered, instead of every later segment being thrown away.
    #[tokio::test]
    async fn missing_flow_is_created_again() {
        let stub = Stub::default();
        stub.storage.lock().unwrap().push_back(404);
        let (outcome, failing, path) = upload_with(&stub, false).await;
        assert_eq!(outcome, Outcome::Registered);
        assert!(!failing);
        assert_eq!(
            stub.flow_puts.load(Ordering::SeqCst),
            1,
            "flow not re-created"
        );
        assert_eq!(stub.registrations.lock().unwrap().len(), 1);
        assert!(!path.exists());
    }

    /// Refused credentials park the segment (bytes and sidecar kept) until
    /// they are fixed; they do not discard it.
    #[tokio::test]
    async fn refused_credentials_park_the_segment() {
        for code in [401, 403] {
            let stub = Stub::default();
            stub.storage.lock().unwrap().push_back(code);
            let (outcome, failing, path) = upload_with(&stub, false).await;
            assert_eq!(outcome, Outcome::Parked, "{}", code);
            assert!(failing);
            assert!(path.exists() && sidecar_path(&path).exists());
        }
    }

    /// A parked segment the gateway already has (its registration went
    /// through, but the answer was lost) is not registered a second time.
    #[tokio::test]
    async fn already_registered_segment_is_not_registered_again() {
        let stub = Stub::default();
        stub.listed.lock().unwrap().push(serde_json::json!({
            "object_id": "obj-old",
            "timerange": "[10:0_12:0)",
        }));
        let (outcome, _, path) = upload_with(&stub, true).await;
        assert_eq!(outcome, Outcome::Registered);
        assert_eq!(stub.allocations.load(Ordering::SeqCst), 0);
        assert!(stub.registrations.lock().unwrap().is_empty());
        assert!(!path.exists());
    }

    /// A registration rejected because the segment is there already (its
    /// first try went through unheard) counts as registered, not discarded.
    #[tokio::test]
    async fn rejected_registration_of_a_present_segment_counts_as_registered() {
        let stub = Stub::default();
        stub.register.lock().unwrap().push_back(400);
        stub.listed.lock().unwrap().push(serde_json::json!({
            "object_id": "obj-old",
            "timerange": "[10:0_12:0)",
        }));
        let (outcome, _, _) = upload_with(&stub, false).await;
        assert_eq!(outcome, Outcome::Registered);
        // Without it, the rejection discards the segment.
        let stub = Stub::default();
        stub.register.lock().unwrap().push_back(400);
        let (outcome, _, path) = upload_with(&stub, false).await;
        assert_eq!(outcome, Outcome::Dropped);
        assert!(!path.exists());
    }

    /// What an [`UploadCtx`] borrows, against a gateway at `url`.
    struct Fixture {
        client: TamsClient,
        spec: FlowSpec,
        detected: SharedDetected,
        events: EventBroadcaster,
        registered: Option<DetectedEssence>,
        segments: u64,
    }

    impl Fixture {
        fn new(url: String) -> Self {
            Self {
                client: TamsClient::new(url, crate::client_auth::AuthMethod::None).unwrap(),
                spec: spec(),
                detected: Arc::default(),
                events: EventBroadcaster::default(),
                registered: Some(DetectedEssence::default()),
                segments: 0,
            }
        }

        fn ctx(&mut self) -> UploadCtx<'_> {
            UploadCtx {
                client: &self.client,
                registered: &mut self.registered,
                flow_spec: &mut self.spec,
                detected_codec: &self.detected,
                content_type: "video/mp2t",
                events: &self.events,
                strom_flow_id: FlowId::new_v4(),
                block_id: "t",
                segments_registered: &mut self.segments,
                failing: false,
                objects: HashMap::new(),
                auth_reported_at: None,
            }
        }

        fn target(&self) -> ParkTarget {
            ParkTarget::new(self.client.base_url(), &self.spec.flow_id)
        }
    }

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("strom-tams-uploader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Park a segment file of `len` bytes starting at `start_s` seconds.
    fn park(dir: &Path, n: u32, start_s: u64, len: usize, target: &ParkTarget) -> PathBuf {
        let path = dir.join(segment_file_name("r1", n, "ts"));
        std::fs::write(&path, vec![0u8; len]).unwrap();
        let frag = FragmentReady {
            path: path.clone(),
            start_ns: start_s * 1_000_000_000,
            end_ns: (start_s + 1) * 1_000_000_000,
        };
        park_on_disk(&frag, target).unwrap();
        path
    }

    fn no_limits() -> ParkedLimits {
        ParkedLimits {
            max_bytes: u64::MAX,
            max_segments: usize::MAX,
        }
    }

    /// No per-flow cap unless one is set.
    #[test]
    fn tams_per_flow_cap_is_off_unless_set() {
        assert_eq!(ParkedLimits::new(None, None), no_limits());
        assert_eq!(
            ParkedLimits::new(Some(2), Some(7)),
            ParkedLimits {
                max_bytes: 2 * 1024 * 1024,
                max_segments: 7,
            }
        );
    }

    /// A parked segment that fails waits out a backoff, and the sweep moves on
    /// to the next one instead of trying the same oldest segment forever.
    #[tokio::test]
    async fn a_failing_parked_segment_does_not_block_the_others() {
        let stub = Stub::default();
        stub.storage.lock().unwrap().push_back(500);
        let mut f = Fixture::new(start_stub(stub.clone()).await);
        let dir = temp_dir();
        let target = f.target();
        let old = park(&dir, 0, 10, 5, &target);
        let new = park(&dir, 1, 11, 5, &target);
        let mut ctx = f.ctx();
        let mut state = ParkedState::new(dir.clone(), target, no_limits());

        let mut backlog = state.refresh(&mut ctx).await;
        assert_eq!(backlog.len(), 2);
        state.try_one(&mut ctx, backlog.pop_front().unwrap()).await;
        assert!(ctx.failing && old.exists());

        // The next sweep probes the other segment, which goes through.
        let mut backlog = state.refresh(&mut ctx).await;
        assert_eq!(backlog.len(), 1);
        assert_eq!(backlog[0].path, new);
        state.try_one(&mut ctx, backlog.pop_front().unwrap()).await;
        assert!(!ctx.failing && !new.exists());
        assert!(state.refresh(&mut ctx).await.is_empty(), "old one waits");

        assert!(parked_backoff(1) >= RECOVERY_SWEEP_INTERVAL * 2);
        assert_eq!(parked_backoff(30), PARKED_MAX_BACKOFF);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past the cap in count or bytes, the oldest parked segments are deleted
    /// and an error event is sent.
    #[tokio::test]
    async fn parked_segments_are_capped() {
        let mut f = Fixture::new("http://127.0.0.1:9".into());
        let mut events = f.events.subscribe();
        let dir = temp_dir();
        let target = f.target();
        let paths: Vec<_> = (0..4)
            .map(|n| park(&dir, n, 10 + n as u64, 100, &target))
            .collect();
        let mut ctx = f.ctx();

        let limits = ParkedLimits {
            max_bytes: u64::MAX,
            max_segments: 3,
        };
        let backlog = ParkedState::new(dir.clone(), target.clone(), limits)
            .refresh(&mut ctx)
            .await;
        assert_eq!(backlog.len(), 3);
        assert!(!paths[0].exists() && !sidecar_path(&paths[0]).exists());
        assert!(paths[1..].iter().all(|p| p.exists()));
        assert!(matches!(
            events.try_recv(),
            Ok(StromEvent::TamsError { .. })
        ));

        let limits = ParkedLimits {
            max_bytes: 150,
            max_segments: usize::MAX,
        };
        let backlog = ParkedState::new(dir.clone(), target, limits)
            .refresh(&mut ctx)
            .await;
        assert_eq!(backlog.len(), 1);
        assert_eq!(backlog[0].path, paths[3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment parked for another gateway or flow stays on disk but is not
    /// uploaded here; one from before sidecars named their target still is.
    #[tokio::test]
    async fn segments_parked_for_another_target_are_kept_back() {
        let mut f = Fixture::new("http://127.0.0.1:9".into());
        let dir = temp_dir();
        let target = f.target();
        let own = park(&dir, 0, 10, 5, &target);
        let other_flow = park(
            &dir,
            1,
            11,
            5,
            &ParkTarget::new("http://127.0.0.1:9", "flow-2"),
        );
        let other_gw = park(
            &dir,
            2,
            12,
            5,
            &ParkTarget::new("http://elsewhere", "flow-1"),
        );
        let legacy = dir.join(segment_file_name("r1", 3, "ts"));
        std::fs::write(&legacy, b"x").unwrap();
        std::fs::write(sidecar_path(&legacy), "13000000000 14000000000").unwrap();
        let mut ctx = f.ctx();

        let backlog = ParkedState::new(dir.clone(), target, no_limits())
            .refresh(&mut ctx)
            .await;
        let paths: Vec<_> = backlog.iter().map(|p| p.path.clone()).collect();
        assert_eq!(paths, vec![own, legacy]);
        assert!(other_flow.exists() && sidecar_path(&other_flow).exists());
        assert!(other_gw.exists() && sidecar_path(&other_gw).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sidecar is written through a temporary file, holds the target, and
    /// the old two-number form still parses.
    #[test]
    fn sidecar_is_written_whole_with_its_target() {
        let dir = temp_dir();
        let path = dir.join("seg_r1_00000.ts");
        std::fs::write(&path, b"x").unwrap();
        let target = ParkTarget::new("https://user:pw@gw.example/tams/?token=s", "flow-1");
        assert_eq!(target.gateway, "https://gw.example/tams");
        park_on_disk(
            &FragmentReady {
                path: path.clone(),
                start_ns: 1,
                end_ns: 2,
            },
            &target,
        )
        .unwrap();
        assert!(!sidecar_tmp_path(&path).exists());
        let contents = std::fs::read_to_string(sidecar_path(&path)).unwrap();
        assert!(!contents.contains("pw") && !contents.contains("token"));
        assert_eq!(parse_sidecar(&contents), Some((1, 2, Some(target))));
        assert_eq!(parse_sidecar("3 4"), Some((3, 4, None)));
        assert_eq!(parse_sidecar("3"), None);
        assert_eq!(parse_sidecar("{\"start_ns\":3}"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A retry after a failed registration reuses the object already allocated
    /// and uploaded to, instead of allocating a new one per try.
    #[tokio::test]
    async fn a_retry_reuses_the_allocated_object() {
        let stub = Stub::default();
        stub.register.lock().unwrap().push_back(500);
        let mut f = Fixture::new(start_stub(stub.clone()).await);
        let dir = temp_dir();
        let path = dir.join("seg_r1_00000.ts");
        std::fs::write(&path, b"bytes").unwrap();
        let frag = FragmentReady {
            path: path.clone(),
            start_ns: 10_000_000_000,
            end_ns: 12_000_000_000,
        };
        let mut ctx = f.ctx();
        assert_eq!(
            upload_segment(&mut ctx, &frag, 1, false).await,
            Outcome::Parked
        );
        assert!(ctx.objects.get(&path).is_some_and(|a| a.uploaded));
        assert_eq!(
            upload_segment(&mut ctx, &frag, 1, true).await,
            Outcome::Registered
        );
        assert_eq!(stub.allocations.load(Ordering::SeqCst), 1);
        assert_eq!(stub.registrations.lock().unwrap().len(), 1);
        assert!(ctx.objects.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Refused credentials are reported once, then only every few minutes,
    /// not at every sweep; a success resets that.
    #[tokio::test]
    async fn refused_credentials_are_reported_once_per_interval() {
        let mut f = Fixture::new("http://127.0.0.1:9".into());
        let mut ctx = f.ctx();
        assert!(ctx.should_report_auth());
        assert!(!ctx.should_report_auth());
        ctx.auth_reported_at = Some(Instant::now() - AUTH_REPORT_INTERVAL);
        assert!(ctx.should_report_auth());
        ctx.failing = true;
        ctx.set_failing(false);
        assert!(ctx.should_report_auth());
    }
}
