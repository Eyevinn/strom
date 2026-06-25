//! Background segment uploader for the TAMS Output block.
//!
//! A `splitmuxsink` writes complete, GOP-aligned segment files to a temp directory
//! on the GStreamer streaming thread. Each finished file, together with its
//! timerange, is handed to this task over a bounded channel. All HTTP and disk reads
//! happen here — never on the streaming thread — so the pipeline is never blocked by
//! the gateway or S3.
//!
//! Reliability:
//! - uploads retry with backoff before giving up;
//! - a segment that ultimately fails is **kept** on disk (not deleted) and a
//!   `TamsError` event is emitted, so nothing is lost silently;
//! - a successfully registered segment is deleted (verified uploaded);
//! - when the pipeline stops, `splitmuxsink` finalizes the current file but Strom
//!   sends no EOS, so the last (not-yet-rotated) fragment is flushed here when the
//!   channel closes — see [`TailSlot`].

use crate::events::EventBroadcaster;
use crate::tams::client::{FlowSpec, TamsClient};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use strom_types::{FlowId, StromEvent};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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

/// Maximum upload/registration attempts before a segment is kept for manual recovery.
const MAX_ATTEMPTS: u32 = 3;

/// Create the fragment channel for one flow.
pub fn channel() -> (mpsc::Sender<FragmentReady>, mpsc::Receiver<FragmentReady>) {
    mpsc::channel(CHANNEL_CAPACITY)
}

/// Spawn the uploader task for one TAMS flow.
///
/// `content_type` is the MIME of the segment files (`video/mp4` or `video/mp2t`).
/// The TAMS flow is created lazily on the first segment, so an idle block never
/// touches the gateway. `detected_codec` is filled in by the caps probe once the
/// encoded format is known (e.g. `video/h264`); by the time the first segment
/// completes, caps have always arrived, so the stored codec reflects the real
/// essence. `flow_spec.codec` is only a fallback. `tail` is flushed on shutdown,
/// using `tail_segment_ns` as the (nominal) duration of the final fragment.
#[allow(clippy::too_many_arguments)]
pub fn spawn_uploader(
    client: TamsClient,
    mut flow_spec: FlowSpec,
    detected_codec: Arc<Mutex<Option<String>>>,
    content_type: String,
    strom_flow_id: FlowId,
    block_id: String,
    events: EventBroadcaster,
    mut rx: mpsc::Receiver<FragmentReady>,
    tail: TailSlot,
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

        while let Some(frag) = rx.recv().await {
            process_fragment(
                &client,
                &mut flow_created,
                &mut flow_spec,
                &detected_codec,
                &content_type,
                &events,
                strom_flow_id,
                &block_id,
                &mut segments_registered,
                frag,
            )
            .await;
        }

        // Channel closed: the pipeline is tearing down. splitmuxsink has finalized
        // the last file by now, but it was never rotated into a FragmentReady, so
        // flush it as the final segment.
        if let Some(tf) = tail.lock().ok().and_then(|mut t| t.take()) {
            if tf.path.exists() {
                debug!("TAMS {}: flushing final segment on shutdown", block_id);
                let frag = FragmentReady {
                    path: tf.path,
                    start_ns: tf.start_ns,
                    end_ns: tf.start_ns.saturating_add(tail_segment_ns),
                };
                process_fragment(
                    &client,
                    &mut flow_created,
                    &mut flow_spec,
                    &detected_codec,
                    &content_type,
                    &events,
                    strom_flow_id,
                    &block_id,
                    &mut segments_registered,
                    frag,
                )
                .await;
            }
        }

        info!(
            "TAMS {}: uploader stopped for flow {}",
            block_id, tams_flow_id
        );
    });
}

/// Ensure the flow exists (once), then upload and register one fragment, retrying
/// transient failures. On success the file is deleted; on definitive failure it is
/// kept on disk and a `TamsError` event is emitted.
#[allow(clippy::too_many_arguments)]
async fn process_fragment(
    client: &TamsClient,
    flow_created: &mut bool,
    flow_spec: &mut FlowSpec,
    detected_codec: &Arc<Mutex<Option<String>>>,
    content_type: &str,
    events: &EventBroadcaster,
    strom_flow_id: FlowId,
    block_id: &str,
    segments_registered: &mut u64,
    frag: FragmentReady,
) {
    let tams_flow_id = flow_spec.flow_id.clone();

    // Lazily create the flow once, preferring the codec detected from caps.
    if !*flow_created {
        if let Some(codec) = detected_codec.lock().ok().and_then(|c| c.clone()) {
            flow_spec.codec = Some(codec);
        }
        match retry(MAX_ATTEMPTS, || client.ensure_flow(flow_spec)).await {
            Ok(()) => {
                *flow_created = true;
                info!("TAMS {}: flow {} created", block_id, tams_flow_id);
            }
            Err(e) => {
                let msg = format!("failed to create flow {}: {:#}", tams_flow_id, e);
                error!(
                    "TAMS {}: {}; keeping {}",
                    block_id,
                    msg,
                    frag.path.display()
                );
                emit_error(events, strom_flow_id, block_id, msg);
                return; // keep the file for recovery
            }
        }
    }

    // Read the segment off disk once, up front. retry() only re-runs the
    // network calls below, so a transient HTTP failure no longer re-reads the
    // whole file from disk on every attempt.
    let bytes = match tokio::fs::read(&frag.path).await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("reading segment {}: {}", frag.path.display(), e);
            error!("TAMS {}: {}; keeping file", block_id, msg);
            emit_error(events, strom_flow_id, block_id, msg);
            return; // keep the file for recovery
        }
    };

    debug!(
        "TAMS {}: uploading segment {} ({} bytes) to flow {}",
        block_id,
        frag.path.display(),
        bytes.len(),
        tams_flow_id
    );

    match retry(MAX_ATTEMPTS, || {
        upload_one(client, &tams_flow_id, content_type, &frag, &bytes)
    })
    .await
    {
        Ok((object_id, timerange)) => {
            events.broadcast(StromEvent::TamsSegmentRegistered {
                flow_id: strom_flow_id,
                block_id: block_id.to_string(),
                tams_flow_id: tams_flow_id.clone(),
                object_id,
                timerange,
            });
            *segments_registered += 1;
            // Surface progress at info so a healthy uploader is visible without
            // debug logging: the first segment, then every 30th thereafter.
            if *segments_registered == 1 || segments_registered.is_multiple_of(30) {
                info!(
                    "TAMS {}: {} segments registered on flow {}",
                    block_id, *segments_registered, tams_flow_id
                );
            }
            delete_uploaded(&frag.path);
        }
        Err(e) => {
            let msg = format!(
                "segment upload failed after retries ({} bytes): {:#}",
                bytes.len(),
                e
            );
            error!(
                "TAMS {}: {}; keeping {}",
                block_id,
                msg,
                frag.path.display()
            );
            emit_error(events, strom_flow_id, block_id, msg);
            // Keep the file on disk for manual recovery.
        }
    }
}

/// Run an async operation up to `attempts` times, with exponential backoff (1s, 2s, …).
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
                    let backoff = Duration::from_secs(1u64 << attempt);
                    tokio::time::sleep(backoff).await;
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("operation failed")))
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

fn delete_uploaded(path: &PathBuf) {
    if let Err(e) = std::fs::remove_file(path) {
        warn!(
            "TAMS: could not remove uploaded segment {}: {}",
            path.display(),
            e
        );
    }
}
