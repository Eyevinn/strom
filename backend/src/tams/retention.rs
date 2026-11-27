//! Rolling buffer for a TAMS flow: delete what is older than a window.
//!
//! A writer that records without end (a replay buffer, a monitoring
//! recorder) fills its store unless something deletes. This deletes through
//! the TAMS index, never under it: `DELETE /flows/{id}/segments?timerange=`
//! removes the segments, and the store removes the media objects that no
//! other segment references. A clip that copies segments into a flow of its
//! own keeps their objects alive. Deleting the objects alone (an S3
//! lifecycle rule) would leave an index that points at missing media.
//!
//! The work runs as a task of its own, off any streaming thread: every
//! [`interval`] it asks the store to delete what ends before `now − window`,
//! on the segment grid, so the flows of one writer (video and audio) lose the
//! same segments. Each request covers everything before the cutoff (an open
//! start), so a request that failed, or segments a drainer registered late,
//! are deleted by the next one. A store error is logged and retried on the
//! next round; it never stops the writer.
//!
//! While a task runs, its flows are listed as trimmed ([`runs_for`]): the
//! parked-segment drainer drops only what such a task deletes anyway, and
//! keeps (and uploads) the parked media of a flow that no longer runs.
//!
//! [`ProtectedRanges`] is the hook for ranges that must survive the window
//! (clips with their handles). Nothing provides one yet.

use super::client::{DeleteOutcome, TamsClient};
use super::uploader::ParkTarget;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use strom_types::tams::format_timestamp;
use tracing::{debug, info, warn};

/// The shortest window. Shorter would delete under a player that plays a
/// few seconds behind the live edge.
pub const MIN_WINDOW: Duration = Duration::from_secs(30);
/// Bounds of the wait between two deletion rounds (a tenth of the window).
const MIN_INTERVAL: Duration = Duration::from_secs(5);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// A failing store is warned about on the first failure in a row and then on
/// every this many; the rest go to debug.
const WARN_EVERY: u32 = 10;

/// Ranges of a flow, as TAI ns `[start, end)`, that retention must not
/// delete (clips and their handles). A segment that overlaps one is kept
/// whole: the store deletes only segments that lie wholly within a request.
pub trait ProtectedRanges: Send + Sync {
    fn protected(&self, flow_id: &str) -> Vec<(u64, u64)>;
}

/// What one retention task deletes.
#[derive(Clone)]
pub struct Retention {
    /// The TAMS flows to keep trimmed (those that carry segments).
    pub flow_ids: Vec<String>,
    /// How much media to keep behind now.
    pub window: Duration,
    /// The segment grid (TAI ns): the cutoff is floored to it.
    pub grid_ns: u64,
    pub protected: Option<Arc<dyn ProtectedRanges>>,
}

/// The window for a `retention_s` property value: `None` (off) for 0, at
/// least [`MIN_WINDOW`] otherwise.
pub fn window_from_secs(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs).max(MIN_WINDOW))
}

/// The wait between two rounds for `window`: a tenth of it, 5 to 60 s. The
/// store holds up to the window plus this.
pub fn interval(window: Duration) -> Duration {
    (window / 10).clamp(MIN_INTERVAL, MAX_INTERVAL)
}

/// Where deletion stops at TAI `now_ns`: `now − window`, floored to the
/// segment grid. `None` before there is anything that old.
pub fn cutoff_ns(now_ns: u64, window: Duration, grid_ns: u64) -> Option<u64> {
    let c = now_ns.checked_sub(window.as_nanos() as u64)?;
    let c = if grid_ns > 0 { c - c % grid_ns } else { c };
    (c > 0).then_some(c)
}

/// The TAMS timeranges to delete for everything before `cutoff`, leaving
/// out `protected`: one open-start range (`_<cutoff>)`) without protection,
/// else the stretches between the protected ranges.
pub fn deletion_timeranges(cutoff: u64, protected: &[(u64, u64)]) -> Vec<String> {
    let mut p: Vec<(u64, u64)> = protected
        .iter()
        .copied()
        .filter(|&(s, e)| e > s && s < cutoff)
        .collect();
    p.sort_unstable();
    let mut out = Vec::new();
    // Where the next stretch to delete starts: `None` is the open start.
    let mut from: Option<u64> = None;
    for (s, e) in p {
        if from.is_none_or(|f| s > f) {
            out.push(range(from, s));
        }
        from = Some(from.map_or(e, |f| f.max(e)));
    }
    match from {
        Some(f) if f >= cutoff => {}
        _ => out.push(range(from, cutoff)),
    }
    out
}

fn range(from: Option<u64>, to: u64) -> String {
    match from {
        None => format!("_{})", format_timestamp(to)),
        Some(f) => format!("[{}_{})", format_timestamp(f), format_timestamp(to)),
    }
}

/// The (gateway, TAMS flow) pairs a retention task currently trims, with how
/// many tasks trim each.
static RUNNING: LazyLock<Mutex<HashMap<ParkTarget, usize>>> = LazyLock::new(Default::default);

fn running() -> std::sync::MutexGuard<'static, HashMap<ParkTarget, usize>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a retention task trims `target` now, i.e. its writer runs.
pub fn runs_for(target: &ParkTarget) -> bool {
    running().get(target).is_some_and(|&n| n > 0)
}

/// Lists its targets as trimmed (see [`runs_for`]) until dropped.
pub struct Running(Vec<ParkTarget>);

impl Running {
    pub fn new(targets: Vec<ParkTarget>) -> Self {
        let mut r = running();
        for t in &targets {
            *r.entry(t.clone()).or_default() += 1;
        }
        Self(targets)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut r = running();
        for t in &self.0 {
            if let Some(n) = r.get_mut(t) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    r.remove(t);
                }
            }
        }
    }
}

/// Run `r` against `client` every [`interval`] (the first round at once),
/// as long as `alive()` says the writer runs. `now_ns` is TAI now. `label`
/// names the writer in logs.
pub fn spawn(
    client: TamsClient,
    r: Retention,
    label: String,
    alive: impl Fn() -> bool + Send + 'static,
    now_ns: impl Fn() -> u64 + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let every = interval(r.window);
    spawn_every(client, r, label, every, alive, now_ns)
}

/// As [`spawn`], with the wait between rounds given.
pub fn spawn_every(
    client: TamsClient,
    r: Retention,
    label: String,
    every: Duration,
    alive: impl Fn() -> bool + Send + 'static,
    now_ns: impl Fn() -> u64 + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    // Listed from now until the task ends (or is aborted).
    let listed = Running::new(
        r.flow_ids
            .iter()
            .map(|f| ParkTarget::new(client.base_url(), f))
            .collect(),
    );
    tokio::spawn(async move {
        let _listed = listed;
        info!(
            "TAMS {}: retention keeps {} s of {} flow(s), deleting every {} s",
            label,
            r.window.as_secs(),
            r.flow_ids.len(),
            every.as_secs_f32()
        );
        let mut rounds = Rounds::default();
        loop {
            if !alive() {
                break;
            }
            if let Some(cutoff) = cutoff_ns(now_ns(), r.window, r.grid_ns) {
                round(&client, &r, cutoff, &label, &mut rounds).await;
            }
            tokio::time::sleep(every).await;
        }
        debug!("TAMS {}: retention stopped", label);
    })
}

/// What the rounds so far went like, for the logs.
#[derive(Default)]
struct Rounds {
    /// The last cutoff the store took for every flow.
    done: Option<u64>,
    /// Failed requests in a row.
    failures: u32,
}

async fn round(client: &TamsClient, r: &Retention, cutoff: u64, label: &str, s: &mut Rounds) {
    if s.done == Some(cutoff) {
        return;
    }
    let mut all_ok = true;
    for flow_id in &r.flow_ids {
        let protected = r
            .protected
            .as_ref()
            .map(|p| p.protected(flow_id))
            .unwrap_or_default();
        for tr in deletion_timeranges(cutoff, &protected) {
            match client.delete_segments(flow_id, &tr).await {
                Ok(DeleteOutcome::Accepted) => {
                    debug!(
                        "TAMS {}: retention deletes {} of flow {}",
                        label, tr, flow_id
                    )
                }
                Ok(DeleteOutcome::NoFlow) => {
                    debug!(
                        "TAMS {}: retention: flow {} not in the store yet",
                        label, flow_id
                    )
                }
                Err(e) => {
                    all_ok = false;
                    s.failures = s.failures.saturating_add(1);
                    if s.failures % WARN_EVERY == 1 {
                        warn!(
                            "TAMS {}: retention could not delete {} of flow {} (failure {} \
                             in a row, retried every round): {:#}",
                            label, tr, flow_id, s.failures, e
                        );
                    } else {
                        debug!(
                            "TAMS {}: retention could not delete {} of flow {}: {:#}",
                            label, tr, flow_id, e
                        );
                    }
                }
            }
        }
    }
    if all_ok {
        if s.failures > 0 {
            info!(
                "TAMS {}: retention deletes again after {} failed request(s)",
                label, s.failures
            );
        } else if s.done.is_none() {
            info!(
                "TAMS {}: retention deleting media before {}",
                label,
                format_timestamp(cutoff)
            );
        }
        s.failures = 0;
        s.done = Some(cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_auth::AuthMethod;
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::Router;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;

    const S: u64 = 1_000_000_000;

    #[test]
    fn window_is_off_at_zero_and_has_a_floor() {
        assert_eq!(window_from_secs(0), None);
        assert_eq!(window_from_secs(5), Some(MIN_WINDOW));
        assert_eq!(window_from_secs(7200), Some(Duration::from_secs(7200)));
        assert_eq!(interval(Duration::from_secs(30)), MIN_INTERVAL);
        assert_eq!(interval(Duration::from_secs(300)), Duration::from_secs(30));
        assert_eq!(interval(Duration::from_secs(7200)), MAX_INTERVAL);
    }

    #[test]
    fn cutoff_is_floored_to_the_grid() {
        let w = Duration::from_secs(60);
        // 1000.7 s − 60 s on a 500 ms grid: 940.5 s.
        assert_eq!(
            cutoff_ns(1000 * S + 700_000_000, w, S / 2),
            Some(940 * S + S / 2)
        );
        assert_eq!(cutoff_ns(1000 * S + 1, w, 0), Some(940 * S + 1));
        // Nothing that old yet.
        assert_eq!(cutoff_ns(59 * S, w, S), None);
        assert_eq!(cutoff_ns(60 * S, w, S), None);
    }

    #[test]
    fn timeranges_leave_out_protected_ranges() {
        assert_eq!(deletion_timeranges(100 * S, &[]), vec!["_100:0)"]);
        // Two clips, one overlapping the cutoff, one after it (ignored), and
        // two that overlap each other.
        let p = [
            (95 * S, 105 * S),
            (10 * S, 20 * S),
            (15 * S, 30 * S),
            (200 * S, 210 * S),
        ];
        assert_eq!(
            deletion_timeranges(100 * S, &p),
            vec!["_10:0)", "[30:0_95:0)"]
        );
        // A clip ending before the cutoff leaves the stretch after it.
        assert_eq!(
            deletion_timeranges(100 * S, &[(40 * S, 50 * S)]),
            vec!["_40:0)", "[50:0_100:0)"]
        );
        // An empty range protects nothing.
        assert_eq!(
            deletion_timeranges(100 * S, &[(5 * S, 5 * S)]),
            vec!["_100:0)"]
        );
    }

    struct Fixed(Vec<(u64, u64)>);
    impl ProtectedRanges for Fixed {
        fn protected(&self, _: &str) -> Vec<(u64, u64)> {
            self.0.clone()
        }
    }

    /// Deletions seen by the stub: (flow, timerange).
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// A gateway stub that takes DELETEs, failing while `down` is set, and
    /// has no flow `gone`.
    async fn gateway(seen: Seen, down: Arc<AtomicBool>) -> String {
        let app = Router::new()
            .route(
                "/flows/{id}/segments",
                axum::routing::delete(
                    move |State(seen): State<Seen>,
                          Path(id): Path<String>,
                          Query(q): Query<HashMap<String, String>>| {
                        let down = down.clone();
                        async move {
                            if down.load(Ordering::Relaxed) {
                                return StatusCode::SERVICE_UNAVAILABLE;
                            }
                            if id == "gone" {
                                return StatusCode::NOT_FOUND;
                            }
                            let tr = q.get("timerange").cloned().unwrap_or_default();
                            seen.lock().unwrap().push((id, tr));
                            StatusCode::ACCEPTED
                        }
                    },
                ),
            )
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    async fn wait_for(seen: &Seen, n: usize) -> Vec<(String, String)> {
        for _ in 0..200 {
            if seen.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn deletes_every_flow_behind_the_window_and_survives_errors() {
        let seen: Seen = Arc::default();
        let down = Arc::new(AtomicBool::new(true));
        let base = gateway(seen.clone(), down.clone()).await;
        let client = TamsClient::new(&base, AuthMethod::Bearer("t".into())).unwrap();
        let now = Arc::new(AtomicU64::new(1000 * S));
        let alive = Arc::new(AtomicBool::new(true));
        let r = Retention {
            flow_ids: vec!["video".into(), "gone".into(), "audio-0".into()],
            window: Duration::from_secs(60),
            grid_ns: S,
            protected: Some(Arc::new(Fixed(vec![(900 * S, 910 * S)]))),
        };
        let task = {
            let (now, alive) = (now.clone(), alive.clone());
            spawn_every(
                client,
                r,
                "test".into(),
                Duration::from_millis(20),
                move || alive.load(Ordering::Relaxed),
                move || now.load(Ordering::Relaxed),
            )
        };
        // The store is down: nothing is deleted, and the task goes on.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(seen.lock().unwrap().is_empty());
        assert!(!task.is_finished());
        down.store(false, Ordering::Relaxed);
        let got = wait_for(&seen, 4).await;
        assert_eq!(
            got[..4],
            [
                ("video".to_string(), "_900:0)".to_string()),
                ("video".to_string(), "[910:0_940:0)".to_string()),
                ("audio-0".to_string(), "_900:0)".to_string()),
                ("audio-0".to_string(), "[910:0_940:0)".to_string()),
            ]
        );
        // The same cutoff is not asked for again.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap().len(), 4);
        // Time goes on: the next cutoff.
        now.store(1010 * S + S / 3, Ordering::Relaxed);
        let got = wait_for(&seen, 8).await;
        assert_eq!(got[5], ("video".to_string(), "[910:0_950:0)".to_string()));
        // The writer stops: so does retention.
        alive.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("retention stops with its writer")
            .unwrap();
    }
}
