//! Background segment uploader for the TAMS Output block.
//!
//! A `splitmuxsink` writes complete, GOP-aligned segment files to a temp directory
//! on the GStreamer streaming thread. Each finished file, together with its
//! timerange, is handed to this task over a bounded channel. All HTTP and disk reads
//! happen here — never on the streaming thread — so the pipeline is never blocked by
//! the gateway or S3.
//!
//! Reliability:
//! - each segment is uploaded with a few inline attempts (exponential backoff +
//!   jitter) before being **parked** on disk for later;
//! - a parked segment keeps its bytes plus a `.meta` sidecar holding its TAI
//!   timerange, so a background recovery sweep — this run or a *later* one, after a
//!   full process restart — can re-register it once the gateway/storage recovers;
//! - a successfully registered segment (and its sidecar) is deleted;
//! - a definitively rejected segment (non-retryable 4xx) is dropped with an error,
//!   since retrying identical bytes can never succeed;
//! - when the flow stops, a stop drain sends EOS through the muxer so the current
//!   file is complete (see [`crate::tams::stop_drain`]), but no next fragment
//!   rotates it into a `FragmentReady`, so the last fragment is flushed here when
//!   the channel closes — see [`TailSlot`].

use crate::events::EventBroadcaster;
use crate::tams::client::{FlowSpec, TamsClient};
use gstreamer as gst;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom_types::{FlowId, StromEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Flow metadata detected from caps at runtime, read when the flow is created.
#[derive(Debug, Default, Clone)]
pub struct DetectedEssence {
    /// Codec label (e.g. `video/h264`); overrides the spec's fallback when set.
    pub codec: Option<String>,
    /// TAMS `essence_parameters`, merged over the spec's.
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

/// The currently-open (not-yet-rotated) fragment. `splitmuxsink` has opened this
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
fn media_end_running_time_ns(end: &MediaEnd) -> Option<u64> {
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

/// Shared slot holding the current [`TailFragment`], written by the location signal
/// and read by the uploader on shutdown.
pub type TailSlot = Arc<Mutex<Option<TailFragment>>>;

pub fn new_tail_slot() -> TailSlot {
    Arc::new(Mutex::new(None))
}

/// Bounded channel capacity. On sustained backpressure the producer drops
/// fragments (with a warning) rather than blocking the pipeline.
pub const CHANNEL_CAPACITY: usize = 256;

/// Inline upload/registration attempts per segment before it is parked on disk for
/// the background recovery sweep.
const MAX_ATTEMPTS: u32 = 6;

/// Upper bound on a single backoff step. The exponential 1,2,4,8,16,32s schedule is
/// clamped here so one attempt never stalls for minutes.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often the uploader re-attempts segments parked on disk (failed inline, or
/// left by a previous run). Self-heals once a busy/over-quota gateway recovers.
const RECOVERY_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Suffix for the per-segment sidecar that persists a parked segment's timerange.
const SIDECAR_SUFFIX: &str = ".meta";

/// Create the fragment channel for one flow.
pub fn channel() -> (mpsc::Sender<FragmentReady>, mpsc::Receiver<FragmentReady>) {
    mpsc::channel(CHANNEL_CAPACITY)
}

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
/// `tail_segment_ns` after its start when that is unknown.
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
) {
    tokio::spawn(async move {
        let tams_flow_id = flow_spec.flow_id.clone();
        let mut flow_created = false;
        let mut segments_registered: u64 = 0;

        info!(
            "TAMS {}: uploader started for flow {} ({})",
            block_id, tams_flow_id, flow_spec.format
        );

        let mut ctx = UploadCtx {
            client: &client,
            flow_created: &mut flow_created,
            flow_spec: &mut flow_spec,
            detected_codec: &detected_codec,
            content_type: &content_type,
            events: &events,
            strom_flow_id,
            block_id: &block_id,
            segments_registered: &mut segments_registered,
        };

        // Pick up anything a previous run parked on disk before taking new fragments.
        recovery_sweep(&mut ctx, &temp_dir).await;

        let mut sweep = tokio::time::interval(RECOVERY_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        sweep.tick().await; // consume the immediate first tick (we just swept)

        loop {
            tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(frag) => process_fragment(&mut ctx, frag).await,
                    None => break, // channel closed: pipeline tearing down
                },
                _ = sweep.tick() => recovery_sweep(&mut ctx, &temp_dir).await,
            }
        }

        // Channel closed: the pipeline is tearing down. splitmuxsink has finalized
        // the last file by now, but it was never rotated into a FragmentReady, so
        // flush it as the final segment.
        if let Some(tf) = tail.lock().ok().and_then(|mut t| t.take()) {
            if tf.path.exists() {
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

        // One last attempt to drain whatever is parked, so a graceful stop after a
        // brief outage doesn't leave recoverable segments behind.
        recovery_sweep(&mut ctx, &temp_dir).await;

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
    flow_created: &'a mut bool,
    flow_spec: &'a mut FlowSpec,
    detected_codec: &'a SharedDetected,
    content_type: &'a str,
    events: &'a EventBroadcaster,
    strom_flow_id: FlowId,
    block_id: &'a str,
    segments_registered: &'a mut u64,
}

/// Ensure the flow exists on the gateway (once), preferring the codec detected from
/// caps over the fallback. Returns whether the flow is now present.
async fn ensure_flow(ctx: &mut UploadCtx<'_>) -> bool {
    if *ctx.flow_created {
        return true;
    }
    if let Ok(detected) = ctx.detected_codec.lock() {
        if let Some(codec) = &detected.codec {
            ctx.flow_spec.codec = Some(codec.clone());
        }
        for (k, v) in &detected.essence_parameters {
            ctx.flow_spec
                .essence_parameters
                .insert(k.clone(), v.clone());
        }
    }
    match retry(MAX_ATTEMPTS, || ctx.client.ensure_flow(ctx.flow_spec)).await {
        Ok(()) => {
            *ctx.flow_created = true;
            info!(
                "TAMS {}: flow {} created",
                ctx.block_id, ctx.flow_spec.flow_id
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

/// Upload and register one freshly-rotated fragment. On a transient failure the
/// segment is parked on disk (bytes + `.meta` sidecar) for the recovery sweep; on a
/// definitive (non-retryable) rejection it is dropped with an error.
async fn process_fragment(ctx: &mut UploadCtx<'_>, frag: FragmentReady) {
    // Park first if the flow can't be created — the sweep will retry both.
    if !ensure_flow(ctx).await {
        park_segment(
            ctx,
            &frag.path,
            frag.start_ns,
            frag.end_ns,
            "flow not created",
        );
        return;
    }

    let tams_flow_id = ctx.flow_spec.flow_id.clone();
    let bytes = match tokio::fs::read(&frag.path).await {
        Ok(b) => b,
        Err(e) => {
            // A read error here is local and unlikely to fix itself; report it but
            // leave the file in place rather than deleting unverified data.
            let msg = format!("reading segment {}: {}", frag.path.display(), e);
            error!("TAMS {}: {}; keeping file", ctx.block_id, msg);
            emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
            return;
        }
    };

    debug!(
        "TAMS {}: uploading segment {} ({} bytes) to flow {}",
        ctx.block_id,
        frag.path.display(),
        bytes.len(),
        tams_flow_id
    );

    match retry(MAX_ATTEMPTS, || {
        upload_one(ctx.client, &tams_flow_id, ctx.content_type, &frag, &bytes)
    })
    .await
    {
        Ok((object_id, timerange)) => on_registered(ctx, &frag.path, object_id, timerange),
        Err(e) if !is_retryable(&e) => {
            // Identical bytes will be rejected the same way forever — give up.
            let msg = format!(
                "segment rejected ({} bytes), discarding: {:#}",
                bytes.len(),
                e
            );
            error!("TAMS {}: {}", ctx.block_id, msg);
            emit_error(ctx.events, ctx.strom_flow_id, ctx.block_id, msg);
            discard_segment(&frag.path);
        }
        Err(e) => {
            // Transient (5xx / network / timeout): park for the recovery sweep.
            park_segment(
                ctx,
                &frag.path,
                frag.start_ns,
                frag.end_ns,
                &format!("{:#}", e),
            );
            emit_error(
                ctx.events,
                ctx.strom_flow_id,
                ctx.block_id,
                format!("segment upload failed, parked for retry: {:#}", e),
            );
        }
    }
}

/// Re-attempt every segment parked on disk in `temp_dir` (failed inline, or left by
/// a previous run). Each parked segment is a file plus a `.meta` sidecar holding its
/// timerange. Successful ones are registered + deleted; non-retryable ones dropped.
async fn recovery_sweep(ctx: &mut UploadCtx<'_>, temp_dir: &Path) {
    let sidecars = match collect_parked(temp_dir) {
        Some(v) if !v.is_empty() => v,
        _ => return,
    };

    if !ensure_flow(ctx).await {
        // Gateway still unreachable; leave everything parked for the next sweep.
        return;
    }

    info!(
        "TAMS {}: recovering {} parked segment(s) for flow {}",
        ctx.block_id,
        sidecars.len(),
        ctx.flow_spec.flow_id
    );
    let tams_flow_id = ctx.flow_spec.flow_id.clone();
    let mut recovered = 0u64;
    for (segment, _sidecar, start_ns, end_ns) in sidecars {
        let bytes = match tokio::fs::read(&segment).await {
            Ok(b) => b,
            Err(_) => {
                // Sidecar without readable bytes is useless — drop the pair.
                discard_segment(&segment);
                continue;
            }
        };
        let frag = FragmentReady {
            path: segment.clone(),
            start_ns,
            end_ns,
        };
        match retry(MAX_ATTEMPTS, || {
            upload_one(ctx.client, &tams_flow_id, ctx.content_type, &frag, &bytes)
        })
        .await
        {
            Ok((object_id, timerange)) => {
                on_registered(ctx, &segment, object_id, timerange);
                recovered += 1;
            }
            Err(e) if !is_retryable(&e) => {
                error!(
                    "TAMS {}: parked segment {} rejected, discarding: {:#}",
                    ctx.block_id,
                    segment.display(),
                    e
                );
                discard_segment(&segment);
            }
            Err(_) => {
                // Still failing: stop the sweep early and keep the rest parked (the
                // sidecar stays on disk), so a sustained outage doesn't hammer the
                // gateway with the whole backlog at once.
                debug!(
                    "TAMS {}: gateway still failing, leaving remaining segments parked",
                    ctx.block_id
                );
                break;
            }
        }
    }
    if recovered > 0 {
        info!(
            "TAMS {}: recovered {} parked segment(s) for flow {}",
            ctx.block_id, recovered, tams_flow_id
        );
    }
}

/// Record a successful registration: emit the event, bump the counter, delete the
/// segment file and any sidecar.
fn on_registered(ctx: &mut UploadCtx<'_>, segment: &Path, object_id: String, timerange: String) {
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
    discard_segment(segment);
}

/// Park a segment on disk for the recovery sweep: keep its bytes and write a `.meta`
/// sidecar with the timerange so it can be re-registered later (even after a restart).
fn park_segment(ctx: &UploadCtx<'_>, segment: &Path, start_ns: u64, end_ns: u64, reason: &str) {
    write_sidecar(segment, start_ns, end_ns);
    debug!(
        "TAMS {}: parked segment {} for retry ({})",
        ctx.block_id,
        segment.display(),
        reason
    );
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

/// Allocate, upload and register a single fragment (bytes already read by the
/// caller). Returns the registered `(object_id, timerange)` on success.
async fn upload_one(
    client: &TamsClient,
    tams_flow_id: &str,
    content_type: &str,
    frag: &FragmentReady,
    bytes: &[u8],
) -> anyhow::Result<(String, String)> {
    let obj = client.allocate_object(tams_flow_id, content_type).await?;
    client
        .upload_object(&obj.put_url, &obj.content_type, bytes.to_vec())
        .await?;
    let timerange = crate::tams::client::format_timerange(frag.start_ns, frag.end_ns);
    client
        .register_segment(tams_flow_id, &obj.object_id, &timerange)
        .await?;

    debug!(
        "TAMS: registered segment {} {} on flow {}",
        obj.object_id, timerange, tams_flow_id
    );
    Ok((obj.object_id, timerange))
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
fn sidecar_path(segment: &Path) -> PathBuf {
    let mut s = segment.as_os_str().to_owned();
    s.push(SIDECAR_SUFFIX);
    PathBuf::from(s)
}

/// Persist a parked segment's TAI timerange next to its bytes.
fn write_sidecar(segment: &Path, start_ns: u64, end_ns: u64) {
    let path = sidecar_path(segment);
    if let Err(e) = std::fs::write(&path, format!("{} {}", start_ns, end_ns)) {
        warn!(
            "TAMS: could not write recovery sidecar {}: {}",
            path.display(),
            e
        );
    }
}

/// Parse a sidecar's `"<start_ns> <end_ns>"` contents.
fn read_sidecar(path: &Path) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string(path).ok()?;
    let mut it = s.split_whitespace();
    let start = it.next()?.parse().ok()?;
    let end = it.next()?.parse().ok()?;
    Some((start, end))
}

/// Scan `temp_dir` for parked segments (a `.meta` sidecar plus its segment file),
/// returning `(segment, sidecar, start_ns, end_ns)`. A sidecar whose segment is gone
/// is cleaned up here. Returns `None` only if the directory can't be read.
fn collect_parked(temp_dir: &Path) -> Option<Vec<(PathBuf, PathBuf, u64, u64)>> {
    let entries = std::fs::read_dir(temp_dir).ok()?;
    let mut parked = Vec::new();
    for entry in entries.flatten() {
        let sidecar = entry.path();
        let name = sidecar
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.ends_with(SIDECAR_SUFFIX) {
            continue;
        }
        // Strip the trailing ".meta" to get the segment path.
        let segment = PathBuf::from(
            sidecar
                .as_os_str()
                .to_string_lossy()
                .strip_suffix(SIDECAR_SUFFIX)
                .unwrap_or_default(),
        );
        match read_sidecar(&sidecar) {
            Some((start, end)) if segment.exists() => {
                parked.push((segment, sidecar, start, end));
            }
            _ => {
                // Orphan/corrupt sidecar (no segment, or unparseable): clean it up.
                let _ = std::fs::remove_file(&sidecar);
            }
        }
    }
    // Stable order so segments recover roughly in capture order.
    parked.sort_by_key(|a| a.2);
    Some(parked)
}

/// Delete a segment file and its sidecar (if any). Used on success and on giving up.
fn discard_segment(segment: &Path) {
    if let Err(e) = std::fs::remove_file(segment) {
        if segment.exists() {
            warn!(
                "TAMS: could not remove segment {}: {}",
                segment.display(),
                e
            );
        }
    }
    let sidecar = sidecar_path(segment);
    if sidecar.exists() {
        let _ = std::fs::remove_file(sidecar);
    }
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
        let seg = PathBuf::from("/tmp/x/seg_00007.ts");
        assert_eq!(
            sidecar_path(&seg),
            PathBuf::from("/tmp/x/seg_00007.ts.meta")
        );
    }

    #[test]
    fn sidecar_round_trips_and_collect_finds_it() {
        let dir = std::env::temp_dir().join("strom-tams-uploader-sidecar-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let seg = dir.join("seg_00001.ts");
        std::fs::write(&seg, b"segment-bytes").unwrap();

        write_sidecar(&seg, 12_000_000_000, 14_000_000_000);
        assert_eq!(
            read_sidecar(&sidecar_path(&seg)),
            Some((12_000_000_000, 14_000_000_000))
        );

        let parked = collect_parked(&dir).unwrap();
        assert_eq!(parked.len(), 1);
        let (found_seg, _, start, end) = &parked[0];
        assert_eq!(found_seg, &seg);
        assert_eq!((*start, *end), (12_000_000_000, 14_000_000_000));

        // discard removes both the segment and its sidecar.
        discard_segment(&seg);
        assert!(!seg.exists());
        assert!(!sidecar_path(&seg).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_parked_cleans_orphan_sidecar() {
        let dir = std::env::temp_dir().join("strom-tams-uploader-orphan-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Sidecar with no matching segment file.
        let orphan = dir.join("seg_00002.ts.meta");
        std::fs::write(&orphan, b"1 2").unwrap();

        let parked = collect_parked(&dir).unwrap();
        assert!(parked.is_empty());
        assert!(!orphan.exists(), "orphan sidecar should be cleaned up");
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
}
