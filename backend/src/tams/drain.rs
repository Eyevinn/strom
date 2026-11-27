//! The server-wide drainer of parked TAMS segments.
//!
//! A TAMS Output's live uploader (`crate::tams::uploader`) only uploads new
//! segments; one it cannot upload is parked (see `crate::tams::park`). This
//! task uploads what is parked, for every TAMS flow under the park root:
//!
//! - from server start, whether or not the Strom flow that parked a segment
//!   runs, and also when that flow was stopped or deleted;
//! - one upload at a time, round-robin across TAMS flows (one directory each),
//!   oldest first within a flow, so one flow's large backlog does not starve
//!   the others;
//! - live first ([`live`], [`LivePolicy`]): never while some live uploader
//!   has [`LIVE_BEHIND_QUEUED`] segments waiting in its queue (live is falling
//!   behind). One waiting segment per uploader is normal; the drainer then
//!   waits for a moment when none waits, but at most [`DRAIN_MAX_WAIT`], so it
//!   cannot starve. One drain upload may run alongside live uploads in flight;
//! - a segment is claimed (`crate::tams::uploader::Claim`) while it is uploaded,
//!   so the cap on parked segments never deletes it and nothing uploads it
//!   twice; on success its file, sidecar and index entry are deleted;
//! - a gateway that fails backs off, exponentially up to
//!   [`MAX_GATEWAY_BACKOFF`]; a segment that fails waits out its own backoff.
//!
//! Each flow directory holds a `park.json` [`Manifest`], written by the live
//! uploader, with what is needed to register a segment when no block runs:
//! the gateway, the TAMS flow and its spec, and how to authenticate. It holds
//! no secret. Credentials are looked up when a segment is uploaded, in order:
//!
//! 1. OSC: the PAT registered for the flow's credential key
//!    (`PUT /api/osc/pat/{key}`), else the instance's default PAT
//!    (`STROM_OSC_PAT` / `OSC_ACCESS_TOKEN`);
//! 2. a static token set on the block: the client the block last ran with, kept
//!    in memory only ([`remember_client`]), else `STROM_TAMS_API_TOKEN` for an
//!    allow-listed gateway;
//! 3. the instance's static token: `STROM_TAMS_API_TOKEN` for an allow-listed
//!    gateway.
//!
//! When there is none, the flow's segments wait
//! ([`DrainState::WaitingForCredentials`], logged once per gateway); a PAT set
//! through the API wakes the drainer ([`wake`]).

use crate::client_auth::AuthMethod;
use crate::events::EventBroadcaster;
use crate::osc::SatProvider;
use crate::tams::client::{gateway_is_allow_listed, FlowSpec, TamsClient};
use crate::tams::park::Parking;
use crate::tams::uploader::{
    collect_parked_blocking, discard_segment, enforce_parked_limits, parked_backoff,
    upload_segment, Claim, DetectedEssence, FragmentReady, Outcome, ParkTarget, ParkedLimits,
    Retry, SharedDetected, UploadCtx, PARKED_MAX_BACKOFF, PARKED_STUCK_FAILURES, RECOVERY_ATTEMPTS,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use strom_types::FlowId;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// The manifest file in each flow directory.
pub const MANIFEST_FILE: &str = "park.json";

/// How often the flow directories are rescanned, in a task of their own beside
/// the drainer: the per-flow cap (`STROM_TAMS_PARKED_MAX_MB`,
/// `STROM_TAMS_PARKED_MAX_SEGMENTS`), sidecars without a segment, and files
/// deleted or added behind the index's back. A directory whose entries did not
/// change since its last scan (same modification time) is skipped.
const SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// A directory modified this recently before its scan began is scanned again
/// next time: on a file system with coarse timestamps, a change in the same
/// tick as the scan would not change its modification time.
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// How many parked segments of one directory [`Drainer::step`] takes from the
/// index at a time.
const PAGE: usize = 64;

/// Longest wait before a failing gateway is tried again.
pub const MAX_GATEWAY_BACKOFF: Duration = Duration::from_secs(120);

/// Shortest wait when nothing could be uploaded, so the drainer never spins.
const MIN_IDLE_WAIT: Duration = Duration::from_millis(100);

// --- Live first ------------------------------------------------------------------

/// How often the drainer looks again while it holds back for live.
const LIVE_POLL: Duration = Duration::from_millis(50);

/// Segments waiting in one live uploader's queue at which live is behind and
/// the drainer pauses. One waiting segment is the steady state of an uploader
/// whose uploads take a large part of a segment's length; a second one means
/// the oldest has waited about a segment's length. Override with
/// `STROM_TAMS_DRAIN_LIVE_BEHIND`.
pub const LIVE_BEHIND_QUEUED: usize = 2;

/// Longest the drainer waits for a moment when no live segment waits at all,
/// before it uploads anyway (live not behind). Without it, several live
/// uploaders with one segment waiting each leave almost no such moment, and the
/// backlog starved (one upload per 13 s with four cameras of 500 ms segments).
/// Override with `STROM_TAMS_DRAIN_MAX_WAIT_MS`.
pub const DRAIN_MAX_WAIT: Duration = Duration::from_secs(1);

/// When the drainer may upload beside live uploads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivePolicy {
    /// Waiting segments in one queue at which the drainer pauses
    /// ([`LIVE_BEHIND_QUEUED`]).
    pub behind_queued: usize,
    /// Longest wait for a moment with no live segment waiting
    /// ([`DRAIN_MAX_WAIT`]).
    pub max_wait: Duration,
}

impl Default for LivePolicy {
    fn default() -> Self {
        Self {
            behind_queued: LIVE_BEHIND_QUEUED,
            max_wait: DRAIN_MAX_WAIT,
        }
    }
}

impl LivePolicy {
    /// The defaults, with `STROM_TAMS_DRAIN_LIVE_BEHIND` (at least 1) and
    /// `STROM_TAMS_DRAIN_MAX_WAIT_MS` when set.
    pub fn from_env() -> Self {
        fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
            std::env::var(name).ok()?.trim().parse().ok()
        }
        let d = Self::default();
        Self {
            behind_queued: env::<usize>("STROM_TAMS_DRAIN_LIVE_BEHIND")
                .map_or(d.behind_queued, |n| n.max(1)),
            max_wait: env::<u64>("STROM_TAMS_DRAIN_MAX_WAIT_MS")
                .map_or(d.max_wait, Duration::from_millis),
        }
    }
}

/// What the live uploaders' queues hold: segments waiting, not yet started. A
/// segment in flight does not count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveLoad {
    /// Waiting in all queues together.
    pub queued: usize,
    /// Waiting in the fullest queue.
    pub max_queued: usize,
}

/// The live uploaders' queues, for the drainer to put live first: it pauses
/// while some queue holds [`LivePolicy::behind_queued`] segments (live is
/// falling behind), prefers a moment when no segment waits at all, and waits
/// for one at most [`LivePolicy::max_wait`].
pub struct LiveBusy {
    /// The live uploaders' queues (see [`crate::tams::uploader::channel`]),
    /// weak, so a queue closes when its producer goes.
    queues: Mutex<Vec<tokio::sync::mpsc::WeakSender<FragmentReady>>>,
}

impl LiveBusy {
    pub const fn new() -> Self {
        Self {
            queues: Mutex::new(Vec::new()),
        }
    }

    /// Count the segments waiting in `tx`'s queue.
    pub fn watch(&self, tx: &tokio::sync::mpsc::Sender<FragmentReady>) {
        self.lock().push(tx.downgrade());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<tokio::sync::mpsc::WeakSender<FragmentReady>>> {
        self.queues.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Live segments waiting in the queues of all live uploaders, not yet
    /// started. Queues whose producer is gone are dropped.
    pub fn load(&self) -> LiveLoad {
        let mut queues = self.lock();
        let mut load = LiveLoad::default();
        queues.retain(|weak| match weak.upgrade() {
            Some(tx) => {
                let n = tx.max_capacity() - tx.capacity();
                load.queued += n;
                load.max_queued = load.max_queued.max(n);
                true
            }
            None => false,
        });
        load
    }

    /// Live segments waiting in all queues together.
    pub fn queued(&self) -> usize {
        self.load().queued
    }
}

impl Default for LiveBusy {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide live-busy signal.
pub fn live() -> &'static LiveBusy {
    static LIVE: LiveBusy = LiveBusy::new();
    &LIVE
}

static WAKE: Notify = Notify::const_new();

/// Wake the drainer: something was parked, or credentials arrived.
pub fn wake() {
    WAKE.notify_one();
}

// --- Credentials held in memory ----------------------------------------------------

/// Clients live uploaders ran with, by (gateway, TAMS flow id). In memory only:
/// a static token set on a block is never written to disk for the drainer.
static CLIENTS: LazyLock<Mutex<HashMap<(String, String), TamsClient>>> =
    LazyLock::new(Default::default);

/// Keep the client a live uploader of `tams_flow_id` runs with, for the drainer
/// to use for the flow's parked segments, also after the flow stops.
pub fn remember_client(tams_flow_id: &str, client: &TamsClient) {
    let target = ParkTarget::new(client.base_url(), tams_flow_id);
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((target.gateway, target.flow_id), client.clone());
    wake();
}

fn remembered_client(target: &ParkTarget) -> Option<TamsClient> {
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(target.gateway.clone(), target.flow_id.clone()))
        .cloned()
}

/// `STROM_TAMS_API_TOKEN` when it may go to `gateway`. Quiet, unlike
/// `crate::tams::client::env_api_token`: the drainer asks often.
fn env_token(gateway: &str) -> Option<String> {
    strom_types::env::var_opt("STROM_TAMS_API_TOKEN").filter(|_| gateway_is_allow_listed(gateway))
}

// --- Manifest ----------------------------------------------------------------------

/// How a flow authenticates, without the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthRef {
    /// No credentials (an open gateway, or one behind an access gate).
    None,
    /// A static token set on the block.
    StaticToken,
    /// The instance's `STROM_TAMS_API_TOKEN`.
    StaticEnv,
    /// OSC SATs for `service_id`, minted from the PAT of `credential_key`.
    Osc {
        service_id: String,
        credential_key: String,
    },
}

impl AuthRef {
    fn of(auth: &AuthMethod) -> Self {
        match auth {
            AuthMethod::None => AuthRef::None,
            AuthMethod::Bearer(t) => {
                if strom_types::env::var_opt("STROM_TAMS_API_TOKEN").as_deref() == Some(t) {
                    AuthRef::StaticEnv
                } else {
                    AuthRef::StaticToken
                }
            }
            AuthMethod::Osc {
                service_id,
                credential_key,
                ..
            } => AuthRef::Osc {
                service_id: service_id.clone(),
                credential_key: credential_key.clone(),
            },
        }
    }
}

/// What the drainer needs to upload a flow directory's parked segments when no
/// block of the flow runs. Written by the live uploader as `park.json`; holds
/// no secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Gateway URL without credentials, query or fragment.
    pub gateway: String,
    pub tams_flow_id: String,
    /// The Strom flow and block that wrote it, for events and logs.
    pub strom_flow_id: FlowId,
    pub block_id: String,
    /// MIME of the segment files.
    pub content_type: String,
    pub auth: AuthRef,
    /// The flow as registered, to create it again if the store lost it.
    pub flow_spec: FlowSpec,
    /// What caps said about the essence.
    #[serde(default)]
    pub detected: DetectedEssence,
}

impl Manifest {
    pub fn new(
        client: &TamsClient,
        flow_spec: &FlowSpec,
        detected: DetectedEssence,
        content_type: &str,
        strom_flow_id: FlowId,
        block_id: &str,
    ) -> Self {
        let target = ParkTarget::new(client.base_url(), &flow_spec.flow_id);
        Self {
            gateway: target.gateway,
            tams_flow_id: target.flow_id,
            strom_flow_id,
            block_id: block_id.to_string(),
            content_type: content_type.to_string(),
            auth: AuthRef::of(client.auth()),
            flow_spec: flow_spec.clone(),
            detected,
        }
    }

    fn target(&self) -> ParkTarget {
        ParkTarget {
            gateway: self.gateway.clone(),
            flow_id: self.tams_flow_id.clone(),
        }
    }
}

/// Write `contents` as the manifest of `dir`, through a temporary file renamed
/// into place. Blocking.
pub fn write_manifest_file(dir: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.tmp", MANIFEST_FILE));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, dir.join(MANIFEST_FILE))
}

fn read_manifest(dir: &Path) -> Option<Manifest> {
    let s = std::fs::read_to_string(dir.join(MANIFEST_FILE)).ok()?;
    serde_json::from_str(&s).ok()
}

/// Rescan the flow directories under `parking`'s root every
/// [`SYNC_INTERVAL`], and wake the drainer after each pass.
async fn sync_loop(parking: &'static Parking, events: EventBroadcaster, wake: &'static Notify) {
    let mut unchanged = HashMap::new();
    loop {
        sync(parking, &events, &mut unchanged).await;
        wake.notify_one();
        tokio::time::sleep(SYNC_INTERVAL).await;
    }
}

/// Rescan every flow directory that changed since its last scan: enforce the
/// per-flow cap, clean sidecars without a segment, and correct the index.
/// `unchanged` holds the modification time of each directory as last
/// scanned. A directory is read on the blocking pool, one at a time.
async fn sync(
    parking: &Parking,
    events: &EventBroadcaster,
    unchanged: &mut HashMap<PathBuf, SystemTime>,
) {
    let root = parking.root().to_path_buf();
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return;
    };
    let limits = ParkedLimits::from_env();
    let mut present = HashSet::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry.file_type().await.is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        present.insert(dir.clone());
        let modified = tokio::fs::metadata(&dir)
            .await
            .and_then(|m| m.modified())
            .ok();
        if modified.is_some() && unchanged.get(&dir) == modified.as_ref() {
            continue;
        }
        let started = SystemTime::now();
        let scanned_at = Instant::now();
        let d = dir.clone();
        let Ok((mut all, manifest)) =
            tokio::task::spawn_blocking(move || (collect_parked_blocking(&d), read_manifest(&d)))
                .await
        else {
            continue;
        };
        let (flow_id, block_id) = match manifest {
            Some(m) => (m.strom_flow_id, m.block_id),
            None => (
                FlowId::nil(),
                entry.file_name().to_string_lossy().to_string(),
            ),
        };
        enforce_parked_limits(events, flow_id, &block_id, &limits, &mut all).await;
        parking.sync_dir(&dir, scanned_at, all);
        match modified {
            Some(m)
                if started
                    .duration_since(m)
                    .is_ok_and(|age| age >= MTIME_SLACK) =>
            {
                unchanged.insert(dir, m);
            }
            _ => {
                unchanged.remove(&dir);
            }
        }
    }
    unchanged.retain(|d, _| present.contains(d));
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- Status --------------------------------------------------------------------------

/// What the drainer does with one flow directory's parked segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainState {
    /// Uploading.
    Draining,
    /// No credentials for the gateway yet.
    WaitingForCredentials,
    /// The gateway failed; waiting out its backoff.
    GatewayFailing,
    /// No readable `park.json`: written by a version without manifests, or
    /// never run since. Kept, not uploaded.
    NoManifest,
}

static STATUS: LazyLock<Mutex<HashMap<String, DrainState>>> = LazyLock::new(Default::default);

/// The drain state per flow directory (named by its TAMS flow id) that has
/// parked segments.
pub fn status() -> HashMap<String, DrainState> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// --- Drainer -------------------------------------------------------------------------

/// Start the process-wide drainer over [`crate::tams::park::parking`]. Call once
/// at startup, inside the Tokio runtime; later calls do nothing.
pub fn start(events: EventBroadcaster) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let drainer = Drainer::new(
        crate::tams::park::parking(),
        live(),
        &WAKE,
        crate::osc::sat_provider(),
        events,
    );
    tokio::spawn(drainer.run());
}

/// A cached client and the credentials it was built with.
#[derive(Clone, PartialEq, Eq)]
enum AuthKey {
    None,
    Bearer(u64),
    Osc(String, String),
}

impl AuthKey {
    fn of(auth: &AuthMethod) -> Self {
        match auth {
            AuthMethod::None => AuthKey::None,
            AuthMethod::Bearer(t) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                t.hash(&mut h);
                AuthKey::Bearer(h.finish())
            }
            AuthMethod::Osc {
                service_id,
                credential_key,
                ..
            } => AuthKey::Osc(service_id.clone(), credential_key.clone()),
        }
    }
}

/// One flow directory the drainer works on.
struct DrainFlow {
    manifest: Manifest,
    modified: SystemTime,
    client: Option<(AuthKey, TamsClient)>,
    spec: FlowSpec,
    detected: SharedDetected,
    registered: Option<DetectedEssence>,
    segments: u64,
    auth_reported_at: Option<Instant>,
}

impl DrainFlow {
    fn new(manifest: Manifest, modified: SystemTime) -> Self {
        Self {
            spec: manifest.flow_spec.clone(),
            detected: Arc::new(Mutex::new(manifest.detected.clone())),
            // Taken as registered: the flow is created again only when the
            // store says it is gone.
            registered: Some(manifest.detected.clone()),
            manifest,
            modified,
            client: None,
            segments: 0,
            auth_reported_at: None,
        }
    }

    /// A client with `auth`, reused while the credentials stay the same.
    fn client(&mut self, auth: AuthMethod) -> Result<TamsClient, String> {
        let key = AuthKey::of(&auth);
        if let Some((k, c)) = &self.client {
            if *k == key {
                return Ok(c.clone());
            }
        }
        let client =
            TamsClient::new(self.manifest.gateway.clone(), auth).map_err(|e| format!("{:#}", e))?;
        self.client = Some((key, client.clone()));
        Ok(client)
    }
}

#[derive(Default)]
struct GatewayState {
    failures: u32,
    until: Option<Instant>,
    /// Why it waits for credentials, as last logged.
    waiting: Option<String>,
}

/// Wait before a gateway that failed `failures` times in a row is tried again:
/// 2 s, doubling, up to [`MAX_GATEWAY_BACKOFF`].
fn gateway_backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.clamp(1, 7)).min(MAX_GATEWAY_BACKOFF)
}

/// What one [`Drainer::step`] did.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Tried one segment.
    Tried,
    /// Held back for live (see [`Drainer::may_upload`]).
    LiveBusy,
    /// Nothing to try now; try again after this long, or when woken.
    Idle(Duration),
}

/// Uploads the parked segments of one [`Parking`]; see the module docs.
pub struct Drainer {
    parking: &'static Parking,
    live: &'static LiveBusy,
    policy: LivePolicy,
    /// Since when the drainer has held back for live, with a segment to
    /// upload; reset by an upload and when there is nothing to upload.
    held_since: Option<Instant>,
    wake: &'static Notify,
    sat: Arc<SatProvider>,
    events: EventBroadcaster,
    flows: HashMap<PathBuf, DrainFlow>,
    gateways: HashMap<String, GatewayState>,
    retries: HashMap<PathBuf, Retry>,
    /// The directory last served, for the round-robin.
    cursor: Option<PathBuf>,
    states: HashMap<PathBuf, DrainState>,
    /// Directories without a manifest, last reported.
    no_manifest_reported: HashSet<PathBuf>,
}

impl Drainer {
    pub fn new(
        parking: &'static Parking,
        live: &'static LiveBusy,
        wake: &'static Notify,
        sat: Arc<SatProvider>,
        events: EventBroadcaster,
    ) -> Self {
        Self {
            parking,
            live,
            policy: LivePolicy::from_env(),
            held_since: None,
            wake,
            sat,
            events,
            flows: HashMap::new(),
            gateways: HashMap::new(),
            retries: HashMap::new(),
            cursor: None,
            states: HashMap::new(),
            no_manifest_reported: HashSet::new(),
        }
    }

    /// Upload parked segments, forever.
    pub async fn run(mut self) {
        info!(
            "TAMS: draining parked segments under {} in the background",
            self.parking.root().display()
        );
        // The rescan runs beside the uploads, not between them.
        let _sync = AbortOnDrop(tokio::spawn(sync_loop(
            self.parking,
            self.events.clone(),
            self.wake,
        )));
        let mut next_prune = Instant::now() + SYNC_INTERVAL;
        loop {
            if Instant::now() >= next_prune {
                // Retries of segments that are gone.
                let parking = self.parking;
                self.retries.retain(|p, _| parking.contains(p));
                next_prune = Instant::now() + SYNC_INTERVAL;
            }
            match self.step().await {
                Step::Tried => tokio::task::yield_now().await,
                Step::LiveBusy => tokio::time::sleep(LIVE_POLL).await,
                Step::Idle(wait) => {
                    let wait = wait.max(MIN_IDLE_WAIT);
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
    }

    /// The flow of `dir`, its manifest read again when it changed. `None`
    /// without a readable manifest.
    async fn load_flow(&mut self, dir: &Path) -> Option<&mut DrainFlow> {
        let modified = tokio::fs::metadata(dir.join(MANIFEST_FILE))
            .await
            .and_then(|m| m.modified())
            .ok();
        let Some(modified) = modified else {
            self.flows.remove(dir);
            return None;
        };
        let fresh = self.flows.get(dir).is_some_and(|f| f.modified == modified);
        if !fresh {
            let d = dir.to_path_buf();
            let manifest = tokio::task::spawn_blocking(move || read_manifest(&d))
                .await
                .ok()
                .flatten()?;
            let mut flow = DrainFlow::new(manifest, modified);
            if let Some(old) = self.flows.remove(dir) {
                flow.segments = old.segments;
                flow.auth_reported_at = old.auth_reported_at;
            }
            self.flows.insert(dir.to_path_buf(), flow);
        }
        self.flows.get_mut(dir)
    }

    /// Credentials for `m`'s gateway, in the order of the module docs; the
    /// reason when there are none.
    fn credentials(&self, m: &Manifest) -> Result<AuthMethod, String> {
        match &m.auth {
            AuthRef::None => Ok(AuthMethod::None),
            AuthRef::Osc {
                service_id,
                credential_key,
            } => {
                if self.sat.has_pat_for(credential_key) {
                    Ok(AuthMethod::Osc {
                        provider: self.sat.clone(),
                        service_id: service_id.clone(),
                        credential_key: credential_key.clone(),
                    })
                } else {
                    Err(format!(
                        "no OSC PAT for credential {} (PUT /api/osc/pat/{}, or set \
                         STROM_OSC_PAT)",
                        credential_key, credential_key
                    ))
                }
            }
            AuthRef::StaticToken => remembered_client(&m.target())
                .map(|c| c.auth().clone())
                .or_else(|| env_token(&m.gateway).map(AuthMethod::Bearer))
                .ok_or_else(|| {
                    "the block's API token is only held while it runs; start the flow, \
                     or set STROM_TAMS_API_TOKEN and allow-list the gateway"
                        .to_string()
                }),
            AuthRef::StaticEnv => env_token(&m.gateway)
                .map(AuthMethod::Bearer)
                .ok_or_else(|| {
                    "STROM_TAMS_API_TOKEN is not set, or the gateway is not allow-listed"
                        .to_string()
                }),
        }
    }

    fn set_state(&mut self, dir: &Path, state: DrainState) {
        if self.states.get(dir) == Some(&state) {
            return;
        }
        self.states.insert(dir.to_path_buf(), state);
        if let Some(name) = dir.file_name() {
            STATUS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string_lossy().to_string(), state);
        }
    }

    fn clear_state(&mut self, dir: &Path) {
        if self.states.remove(dir).is_some() {
            if let Some(name) = dir.file_name() {
                STATUS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(name.to_string_lossy().as_ref());
            }
        }
    }

    /// Upload the next segment: the oldest due one of the next flow
    /// directory, round-robin, that has credentials and whose gateway is not
    /// backing off.
    async fn step(&mut self) -> Step {
        let now = Instant::now();
        let mut dirs = self.parking.dirs();
        let gone: Vec<PathBuf> = self
            .states
            .keys()
            .filter(|d| dirs.binary_search(d).is_err())
            .cloned()
            .collect();
        for dir in gone {
            self.clear_state(&dir);
        }
        if dirs.is_empty() {
            return Step::Idle(SYNC_INTERVAL);
        }
        // Round-robin: the directories after the one last served first.
        if let Some(cursor) = &self.cursor {
            let next = dirs.partition_point(|d| d <= cursor);
            dirs.rotate_left(next);
        }
        let mut next_due: Option<Instant> = None;
        let mut due = |at: Instant| next_due = Some(next_due.map_or(at, |n| n.min(at)));

        for dir in dirs {
            let Some(flow) = self.load_flow(&dir).await else {
                if self.no_manifest_reported.insert(dir.clone()) {
                    warn!(
                        "TAMS: parked segments in {} have no readable {}; they are kept \
                         but not uploaded until a TAMS Output writes that flow again",
                        dir.display(),
                        MANIFEST_FILE
                    );
                }
                self.set_state(&dir, DrainState::NoManifest);
                continue;
            };
            let manifest = flow.manifest.clone();
            self.no_manifest_reported.remove(&dir);
            let gateway = manifest.gateway.clone();
            if let Some(until) = self.gateways.get(&gateway).and_then(|g| g.until) {
                if until > now {
                    due(until);
                    self.set_state(&dir, DrainState::GatewayFailing);
                    continue;
                }
            }
            let auth = match self.credentials(&manifest) {
                Ok(auth) => auth,
                Err(why) => {
                    let g = self.gateways.entry(gateway.clone()).or_default();
                    if g.waiting.as_deref() != Some(why.as_str()) {
                        warn!(
                            "TAMS: parked segments for {} wait for credentials: {}",
                            gateway, why
                        );
                        g.waiting = Some(why);
                    }
                    self.set_state(&dir, DrainState::WaitingForCredentials);
                    continue;
                }
            };
            if let Some(g) = self.gateways.get_mut(&gateway) {
                if g.waiting.take().is_some() {
                    info!("TAMS: credentials for {} found; uploading", gateway);
                }
            }
            let target = manifest.target();
            // Oldest first, a page at a time from the index: what the
            // sidecars say is in it, so none is read here.
            let mut after: Option<(u64, PathBuf)> = None;
            loop {
                let page = self.parking.page(&dir, after.as_ref(), PAGE);
                let full = page.len() == PAGE;
                after = page.last().map(|p| (p.start_ns, p.path.clone()));
                for seg in page {
                    if let Some(r) = self.retries.get(&seg.path) {
                        if r.next > now {
                            due(r.next);
                            continue;
                        }
                    }
                    if seg.target.as_ref().is_some_and(|t| *t != target) {
                        // Parked for another gateway or flow; kept (see the
                        // uploader's docs).
                        continue;
                    }
                    if !self.may_upload(Instant::now()) {
                        return Step::LiveBusy;
                    }
                    let Some(claim) = Claim::take(&seg.path) else {
                        continue;
                    };
                    if !tokio::fs::try_exists(&seg.path).await.unwrap_or(false) {
                        // Uploaded or deleted behind the index's back.
                        self.parking.forget(&seg.path);
                        continue;
                    }
                    self.cursor = Some(dir.clone());
                    self.set_state(&dir, DrainState::Draining);
                    let frag = FragmentReady {
                        path: seg.path.clone(),
                        start_ns: seg.start_ns,
                        end_ns: seg.end_ns,
                    };
                    self.held_since = None;
                    let outcome = self.upload(&dir, auth, &frag).await;
                    drop(claim);
                    self.after_upload(&dir, &gateway, &seg.path, outcome);
                    return Step::Tried;
                }
                if !full {
                    break;
                }
            }
        }
        // Nothing to upload: a wait for live starts again with the next one.
        self.held_since = None;
        Step::Idle(next_due.map_or(SYNC_INTERVAL, |at| at.saturating_duration_since(now)))
    }

    /// Whether a drain upload may start now, beside the live uploads:
    ///
    /// - not while some live queue holds [`LivePolicy::behind_queued`]
    ///   segments: live is falling behind;
    /// - at once when no live segment waits;
    /// - else (one segment waits here and there, the steady state with many
    ///   live uploaders) once the drainer has held back for
    ///   [`LivePolicy::max_wait`], so it cannot starve.
    fn may_upload(&mut self, now: Instant) -> bool {
        let load = self.live.load();
        let since = *self.held_since.get_or_insert(now);
        if load.max_queued >= self.policy.behind_queued {
            return false;
        }
        load.queued == 0 || now.saturating_duration_since(since) >= self.policy.max_wait
    }

    /// Upload one claimed segment of `dir`'s flow.
    async fn upload(&mut self, dir: &Path, auth: AuthMethod, frag: &FragmentReady) -> Outcome {
        let failing = self
            .flows
            .get(dir)
            .and_then(|f| self.gateways.get(&f.manifest.gateway))
            .is_some_and(|g| g.failures > 0);
        let events = &self.events;
        let Some(flow) = self.flows.get_mut(dir) else {
            return Outcome::Parked;
        };
        let client = match flow.client(auth) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "TAMS: cannot upload parked segments to {}: {}",
                    flow.manifest.gateway, e
                );
                return Outcome::Parked;
            }
        };
        let mut ctx = UploadCtx {
            client: &client,
            registered: &mut flow.registered,
            flow_spec: &mut flow.spec,
            detected_codec: &flow.detected,
            content_type: &flow.manifest.content_type,
            events,
            strom_flow_id: flow.manifest.strom_flow_id,
            block_id: &flow.manifest.block_id,
            segments_registered: &mut flow.segments,
            failing,
            next_probe: None,
            auth_reported_at: flow.auth_reported_at,
            manifest: None,
        };
        let outcome = upload_segment(&mut ctx, frag, RECOVERY_ATTEMPTS, true).await;
        flow.auth_reported_at = ctx.auth_reported_at;
        if outcome == Outcome::Unreadable {
            // A sidecar whose segment is gone is useless. A segment that
            // could not be read for another reason is kept and tried again.
            discard_segment(&frag.path).await;
        }
        outcome
    }

    fn after_upload(&mut self, dir: &Path, gateway: &str, path: &Path, outcome: Outcome) {
        let now = Instant::now();
        match outcome {
            Outcome::ReadFailed => {
                // A local problem: the segment waits out its own backoff; the
                // gateway is not to blame.
                let retry = self.retries.entry(path.to_path_buf()).or_insert(Retry {
                    failures: 0,
                    next: now,
                });
                retry.failures += 1;
                retry.next = now + parked_backoff(retry.failures);
            }
            Outcome::Parked => {
                let retry = self.retries.entry(path.to_path_buf()).or_insert(Retry {
                    failures: 0,
                    next: now,
                });
                retry.failures += 1;
                retry.next = now + parked_backoff(retry.failures);
                if retry.failures == PARKED_STUCK_FAILURES {
                    warn!(
                        "TAMS: parked segment {} failed {} times; trying the others first, \
                         and it again every {} s at most",
                        path.display(),
                        retry.failures,
                        PARKED_MAX_BACKOFF.as_secs()
                    );
                }
                let g = self.gateways.entry(gateway.to_string()).or_default();
                g.failures += 1;
                let wait = gateway_backoff(g.failures);
                g.until = Some(now + wait);
                debug!(
                    "TAMS: parked segment upload to {} failed ({} in a row); next try in {} s",
                    gateway,
                    g.failures,
                    wait.as_secs()
                );
                self.set_state(dir, DrainState::GatewayFailing);
            }
            Outcome::Registered | Outcome::Dropped | Outcome::Unreadable => {
                self.retries.remove(path);
                // The process-wide index is told by the uploader; this one may
                // be another (tests).
                self.parking.forget(path);
                if outcome == Outcome::Registered {
                    if let Some(g) = self.gateways.get_mut(gateway) {
                        g.failures = 0;
                        g.until = None;
                    }
                    debug!("TAMS: uploaded parked segment {}", path.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tams::park::{FreeSpace, ParkLimits, Space};
    use crate::tams::uploader::{sidecar_contents, sidecar_path};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Unlimited;
    impl FreeSpace for Unlimited {
        fn space(&self, _: &Path) -> Option<Space> {
            None
        }
    }

    fn leak<T>(v: T) -> &'static T {
        Box::leak(Box::new(v))
    }

    fn parking(root: &Path, max_total_bytes: u64) -> &'static Parking {
        leak(Parking::new(
            root.to_path_buf(),
            Some(ParkLimits {
                max_total_bytes,
                min_free_bytes: 0,
            }),
            Box::new(Unlimited),
        ))
    }

    /// A store that records `(flow id, timerange)` per registration, and
    /// signals each upload it gets; uploads take `upload_delay`.
    #[derive(Clone, Default)]
    struct Store {
        base: Arc<Mutex<String>>,
        registered: Arc<Mutex<Vec<(String, String)>>>,
        uploads: Arc<AtomicUsize>,
        upload_delay: Duration,
        /// Storage allocations still to fail with a 500.
        fail: Arc<AtomicUsize>,
    }

    async fn start_store(store: Store) -> String {
        use axum::extract::{Path as P, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post, put};
        use axum::{Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        *store.base.lock().unwrap() = base.clone();
        let app = Router::new()
            .route("/flows/{id}", put(|| async { StatusCode::CREATED }))
            .route(
                "/flows/{id}/storage",
                post(|State(s): State<Store>| async move {
                    use axum::response::IntoResponse;
                    if s.fail
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    let base = s.base.lock().unwrap().clone();
                    Json(serde_json::json!({ "media_objects": [{
                        "object_id": "obj",
                        "put_url": { "url": format!("{}/upload", base), "content-type": "video/mp2t" }
                    }]}))
                    .into_response()
                }),
            )
            .route(
                "/upload",
                put(|State(s): State<Store>| async move {
                    s.uploads.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(s.upload_delay).await;
                    StatusCode::OK
                }),
            )
            .route(
                "/flows/{id}/segments",
                get(|| async { Json(serde_json::json!([])) }).post(
                    |State(s): State<Store>,
                     P(id): P<String>,
                     Json(body): Json<serde_json::Value>| async move {
                        let tr = body["timerange"].as_str().unwrap_or_default().to_string();
                        s.registered.lock().unwrap().push((id, tr));
                        StatusCode::CREATED
                    },
                ),
            )
            .with_state(store);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        base
    }

    fn spec(flow_id: &str) -> FlowSpec {
        FlowSpec {
            flow_id: flow_id.into(),
            source_id: "src".into(),
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

    /// Write the manifest of flow `flow` under `root`, for `gateway`.
    fn manifest(root: &Path, gateway: &str, flow: &str, auth: AuthRef) -> Manifest {
        let m = Manifest {
            gateway: ParkTarget::new(gateway, flow).gateway,
            tams_flow_id: flow.into(),
            strom_flow_id: FlowId::new_v4(),
            block_id: "out".into(),
            content_type: "video/mp2t".into(),
            auth,
            flow_spec: spec(flow),
            detected: DetectedEssence::default(),
        };
        write_manifest_file(&root.join(flow), &serde_json::to_string(&m).unwrap()).unwrap();
        m
    }

    /// Park a 100-byte segment of `flow` starting at `start_s`.
    fn park(p: &Parking, root: &Path, gateway: &str, flow: &str, start_s: u64) -> PathBuf {
        let dir = root.join(flow);
        std::fs::create_dir_all(&dir).unwrap();
        let seg = dir.join(format!("seg_r1_{:05}.ts", start_s));
        std::fs::write(&seg, [0u8; 100]).unwrap();
        let start = start_s * 1_000_000_000;
        let target = ParkTarget::new(gateway, flow);
        p.park(
            &seg,
            start,
            &sidecar_contents(start, start + 1_000_000_000, &target),
        )
        .unwrap();
        seg
    }

    fn drainer(p: &'static Parking, live: &'static LiveBusy) -> Drainer {
        Drainer::new(
            p,
            live,
            leak(Notify::new()),
            Arc::new(SatProvider::new(None, None).unwrap()),
            EventBroadcaster::default(),
        )
    }

    fn start_s(timerange: &str) -> u64 {
        timerange
            .trim_start_matches('[')
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Flows take turns, and each flow goes oldest first.
    #[tokio::test]
    async fn tams_drain_is_round_robin_and_oldest_first() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = parking(root, u64::MAX);
        manifest(root, &gw, "fa", AuthRef::None);
        manifest(root, &gw, "fb", AuthRef::None);
        for s in [12, 10, 11] {
            park(p, root, &gw, "fa", s);
        }
        for s in [21, 20] {
            park(p, root, &gw, "fb", s);
        }
        let mut d = drainer(p, leak(LiveBusy::new()));
        for _ in 0..5 {
            assert_eq!(d.step().await, Step::Tried);
        }
        assert!(matches!(d.step().await, Step::Idle(_)));
        let order: Vec<(String, u64)> = store
            .registered
            .lock()
            .unwrap()
            .iter()
            .map(|(f, tr)| (f.clone(), start_s(tr)))
            .collect();
        let want: Vec<(String, u64)> = [("fa", 10), ("fb", 20), ("fa", 11), ("fb", 21), ("fa", 12)]
            .iter()
            .map(|(f, s)| (f.to_string(), *s))
            .collect();
        assert_eq!(order, want);
        assert!(p.dirs().is_empty());
        assert_eq!(std::fs::read_dir(root.join("fa")).unwrap().count(), 1); // park.json
    }

    fn live_fragment(n: u64) -> FragmentReady {
        FragmentReady {
            path: PathBuf::from(format!("live-{}", n)),
            start_ns: n,
            end_ns: n + 1,
        }
    }

    fn drainer_with(p: &'static Parking, live: &'static LiveBusy, policy: LivePolicy) -> Drainer {
        let mut d = drainer(p, live);
        d.policy = policy;
        d
    }

    /// `n` live queues watched by `live`, each with one segment in flight
    /// (taken off the queue) and `waiting` segments waiting behind it.
    async fn live_queues(
        live: &LiveBusy,
        n: usize,
        waiting: usize,
    ) -> Vec<(
        tokio::sync::mpsc::Sender<FragmentReady>,
        tokio::sync::mpsc::Receiver<FragmentReady>,
    )> {
        let mut queues = Vec::new();
        for i in 0..n {
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            live.watch(&tx);
            tx.try_send(live_fragment(i as u64)).unwrap();
            let _in_flight = rx.recv().await.unwrap();
            for k in 0..waiting {
                tx.try_send(live_fragment((i * 100 + k) as u64)).unwrap();
            }
            queues.push((tx, rx));
        }
        queues
    }

    async fn wait_registered(store: &Store, n: usize, within: Duration) -> usize {
        let deadline = Instant::now() + within;
        while store.registered.lock().unwrap().len() < n && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        store.registered.lock().unwrap().len()
    }

    /// Several live uploaders with one segment waiting each is the steady
    /// state; it left the old drainer (paused while any segment waited) almost
    /// no moment to upload. Now it keeps uploading, one per
    /// [`LivePolicy::max_wait`] at most.
    #[tokio::test]
    async fn tams_drain_progresses_while_each_live_queue_has_one_waiting() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        manifest(&root, &gw, "fa", AuthRef::None);
        for s in 10..14 {
            park(p, &root, &gw, "fa", s);
        }
        let live: &'static LiveBusy = leak(LiveBusy::new());
        let _queues = live_queues(live, 4, 1).await;
        assert_eq!(
            live.load(),
            LiveLoad {
                queued: 4,
                max_queued: 1
            }
        );
        let policy = LivePolicy {
            behind_queued: 2,
            max_wait: Duration::from_millis(100),
        };
        let task = tokio::spawn(drainer_with(p, live, policy).run());
        let done = wait_registered(&store, 4, Duration::from_secs(5)).await;
        task.abort();
        assert_eq!(done, 4, "the drainer starved beside healthy live uploads");
    }

    /// A live queue with [`LivePolicy::behind_queued`] segments waiting means
    /// live is falling behind: the drainer pauses, however long it has waited,
    /// and resumes when the queue is back to one. A queue whose producer is gone
    /// no longer counts.
    #[tokio::test]
    async fn tams_drain_pauses_while_live_is_behind() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        manifest(&root, &gw, "fa", AuthRef::None);
        park(p, &root, &gw, "fa", 10);
        park(p, &root, &gw, "fa", 11);
        let live: &'static LiveBusy = leak(LiveBusy::new());
        let _healthy = live_queues(live, 2, 1).await;
        let mut behind = live_queues(live, 1, 2).await;
        assert_eq!(live.load().max_queued, 2);
        let policy = LivePolicy {
            behind_queued: 2,
            max_wait: Duration::from_millis(50),
        };
        let mut d = drainer_with(p, live, policy);
        assert_eq!(d.step().await, Step::LiveBusy);
        let task = tokio::spawn(d.run());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(store.registered.lock().unwrap().len(), 0);

        // The behind uploader takes one: one waits, live keeps up again.
        let (tx, rx) = &mut behind[0];
        let _in_flight = rx.recv().await.unwrap();
        assert_eq!(wait_registered(&store, 2, Duration::from_secs(5)).await, 2);
        task.abort();

        // A queue whose producer is gone no longer counts.
        tx.try_send(live_fragment(9)).unwrap();
        assert_eq!(live.load().max_queued, 2);
        behind.clear();
        assert_eq!(live.load().max_queued, 1);
    }

    /// With no live segment waiting the drainer uploads at once; with one
    /// waiting it first holds back, and uploads once it has held back for
    /// [`LivePolicy::max_wait`] (the fairness floor). The old drainer held back
    /// as long as any segment waited.
    #[tokio::test]
    async fn tams_drain_fairness_floor() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        manifest(&root, &gw, "fa", AuthRef::None);
        park(p, &root, &gw, "fa", 10);
        park(p, &root, &gw, "fa", 11);
        park(p, &root, &gw, "fa", 12);
        let live: &'static LiveBusy = leak(LiveBusy::new());
        let policy = LivePolicy {
            behind_queued: 2,
            max_wait: Duration::from_millis(300),
        };
        let mut d = drainer_with(p, live, policy);

        // Nothing waits: at once.
        let queues = live_queues(live, 1, 0).await;
        assert_eq!(d.step().await, Step::Tried);

        // One waits: held back, then the floor lets one through.
        queues[0].0.try_send(live_fragment(1)).unwrap();
        assert_eq!(d.step().await, Step::LiveBusy);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.step().await, Step::LiveBusy);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(d.step().await, Step::Tried);
        // The wait starts again after an upload.
        assert_eq!(d.step().await, Step::LiveBusy);
        assert_eq!(store.registered.lock().unwrap().len(), 2);
    }

    /// The segment being uploaded is never deleted to make room, and a claimed
    /// segment is not uploaded by the drainer: nothing goes twice.
    #[tokio::test]
    async fn tams_drain_claim_prevents_eviction_and_double_upload() {
        let store = Store {
            upload_delay: Duration::from_millis(500),
            ..Default::default()
        };
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, 300);
        manifest(&root, &gw, "fa", AuthRef::None);
        let a = park(p, &root, &gw, "fa", 10);
        let b = park(p, &root, &gw, "fa", 11);
        let c = park(p, &root, &gw, "fa", 12);
        let mut d = drainer(p, leak(LiveBusy::new()));

        // `a` is in flight when a new segment needs room: `b` goes instead.
        let step = tokio::spawn(async move {
            let s = d.step().await;
            (d, s)
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while store.uploads.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let new = park(p, &root, &gw, "fa", 13);
        assert!(a.exists(), "the segment in flight was deleted");
        assert!(!b.exists());
        let (mut d, s) = step.await.unwrap();
        assert_eq!(s, Step::Tried);
        assert!(!a.exists());

        // `c` is claimed elsewhere: the drainer takes the next one.
        let claim = Claim::take(&c).unwrap();
        assert_eq!(d.step().await, Step::Tried);
        assert!(c.exists() && !new.exists());
        assert!(matches!(d.step().await, Step::Idle(_)));
        drop(claim);
        assert_eq!(d.step().await, Step::Tried);
        let starts: Vec<u64> = store
            .registered
            .lock()
            .unwrap()
            .iter()
            .map(|(_, tr)| start_s(tr))
            .collect();
        assert_eq!(starts, vec![10, 13, 12]);
    }

    /// Without credentials, segments wait and nothing is sent; they are found
    /// when a PAT arrives or the block's client is known.
    #[tokio::test]
    async fn tams_drain_waits_for_credentials() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        let key = format!("cred-{}", uuid::Uuid::new_v4());
        let osc = manifest(
            &root,
            &gw,
            "fosc",
            AuthRef::Osc {
                service_id: "eyevinn-tams-gateway".into(),
                credential_key: key.clone(),
            },
        );
        let flow_static = format!("fstatic-{}", uuid::Uuid::new_v4());
        let stat = manifest(&root, &gw, &flow_static, AuthRef::StaticToken);
        park(p, &root, &gw, "fosc", 10);
        park(p, &root, &gw, &flow_static, 11);
        let mut d = drainer(p, leak(LiveBusy::new()));

        assert!(matches!(d.step().await, Step::Idle(_)));
        assert!(store.registered.lock().unwrap().is_empty());
        assert_eq!(
            d.states.get(&root.join("fosc")),
            Some(&DrainState::WaitingForCredentials)
        );
        assert_eq!(
            d.states.get(&root.join(&flow_static)),
            Some(&DrainState::WaitingForCredentials)
        );
        assert!(d.gateways[&osc.gateway].waiting.is_some());

        // The block's own token, held in memory while it ran.
        let client = TamsClient::new(gw.clone(), AuthMethod::Bearer("tok".into())).unwrap();
        remember_client(&flow_static, &client);
        assert!(matches!(d.credentials(&stat), Ok(AuthMethod::Bearer(t)) if t == "tok"));
        assert_eq!(d.step().await, Step::Tried);
        let registered = store.registered.lock().unwrap().clone();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].0, flow_static);
        assert_eq!(
            d.states.get(&root.join("fosc")),
            Some(&DrainState::WaitingForCredentials)
        );

        // A PAT for the OSC flow's credential key (not used here: it would
        // mint a SAT).
        d.sat.set_pat(key, "pat".into()).await;
        assert!(matches!(d.credentials(&osc), Ok(AuthMethod::Osc { .. })));
    }

    /// A segment that fails waits out its backoff; once the gateway's backoff
    /// is over, the next one is tried, so one bad segment does not block the
    /// others.
    #[tokio::test]
    async fn tams_drain_failing_segment_does_not_block_the_others() {
        let store = Store::default();
        store.fail.store(1, Ordering::SeqCst);
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        let m = manifest(&root, &gw, "fa", AuthRef::None);
        let old = park(p, &root, &gw, "fa", 10);
        let new = park(p, &root, &gw, "fa", 11);
        let mut d = drainer(p, leak(LiveBusy::new()));

        assert_eq!(d.step().await, Step::Tried);
        assert!(old.exists());
        assert_eq!(d.states[&root.join("fa")], DrainState::GatewayFailing);
        // The gateway backs off.
        assert!(matches!(d.step().await, Step::Idle(w) if w > Duration::ZERO));
        d.gateways.get_mut(&m.gateway).unwrap().until = None;
        assert_eq!(d.step().await, Step::Tried);
        assert!(!new.exists() && old.exists());
        assert_eq!(d.gateways[&m.gateway].failures, 0);
        // The old one waits out its own backoff.
        assert!(matches!(d.step().await, Step::Idle(w) if w > Duration::from_secs(1)));
    }

    /// A segment parked for another gateway or flow stays, and is not sent to
    /// the manifest's.
    #[tokio::test]
    async fn tams_drain_keeps_segments_parked_for_another_target() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        manifest(&root, &gw, "fa", AuthRef::None);
        let foreign = park(p, &root, "http://elsewhere.example", "fa", 10);
        park(p, &root, &gw, "fa", 11);
        let mut d = drainer(p, leak(LiveBusy::new()));
        assert_eq!(d.step().await, Step::Tried);
        assert!(matches!(d.step().await, Step::Idle(_)));
        assert!(foreign.exists());
        assert_eq!(store.registered.lock().unwrap().len(), 1);
    }

    /// A parked segment that cannot be read for a reason other than being
    /// gone (here: a directory in its place, as an I/O error would) is kept
    /// with its sidecar, and tried again after its backoff.
    #[tokio::test]
    async fn tams_drain_keeps_a_segment_it_could_not_read() {
        let store = Store::default();
        let gw = start_store(store.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        let m = manifest(&root, &gw, "fa", AuthRef::None);
        let seg = park(p, &root, &gw, "fa", 10);
        std::fs::remove_file(&seg).unwrap();
        std::fs::create_dir(&seg).unwrap();
        let mut d = drainer(p, leak(LiveBusy::new()));

        assert_eq!(d.step().await, Step::Tried);
        assert!(
            sidecar_path(&seg).exists(),
            "the parked segment was deleted"
        );
        assert!(d.retries.contains_key(&seg));
        assert_eq!(d.gateways.get(&m.gateway).map_or(0, |g| g.failures), 0);
        assert!(matches!(d.step().await, Step::Idle(w) if w > Duration::from_secs(1)));
    }

    /// The rescan corrects the index from disk: a segment deleted behind its
    /// back is forgotten, one added is found; a directory that did not change
    /// since is not read again.
    #[tokio::test]
    async fn tams_sync_corrects_the_index_and_skips_unchanged_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = parking(&root, u64::MAX);
        let gw = "http://gw.example";
        manifest(&root, gw, "fa", AuthRef::None);
        let a = park(p, &root, gw, "fa", 10);
        let b = park(p, &root, gw, "fa", 11);
        // Behind the index's back: `a` goes, `c` comes.
        std::fs::remove_file(&a).unwrap();
        let c = root.join("fa").join("seg_r1_00012.ts");
        std::fs::write(&c, [0u8; 100]).unwrap();
        let start = 12_000_000_000;
        std::fs::write(
            sidecar_path(&c),
            sidecar_contents(start, start + 1_000_000_000, &ParkTarget::new(gw, "fa")),
        )
        .unwrap();
        // Old enough to be trusted as unchanged next time.
        let old = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::open(root.join("fa"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let mut unchanged = HashMap::new();
        let events = EventBroadcaster::default();
        sync(p, &events, &mut unchanged).await;
        assert!(!p.contains(&a) && p.contains(&b) && p.contains(&c));
        assert!(!sidecar_path(&a).exists());
        assert_eq!(unchanged.get(&root.join("fa")), Some(&old));

        // Unchanged: not read, so a deletion that kept the time is not seen.
        std::fs::remove_file(&b).unwrap();
        std::fs::File::open(root.join("fa"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        sync(p, &events, &mut unchanged).await;
        assert!(p.contains(&b));
        // Changed: read again.
        std::fs::File::open(root.join("fa"))
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        sync(p, &events, &mut unchanged).await;
        assert!(!p.contains(&b));
    }

    #[test]
    fn tams_gateway_backoff_is_capped() {
        assert_eq!(gateway_backoff(1), Duration::from_secs(2));
        assert_eq!(gateway_backoff(2), Duration::from_secs(4));
        assert_eq!(gateway_backoff(40), MAX_GATEWAY_BACKOFF);
    }

    /// The manifest holds no secret: a static token is named, not written.
    #[test]
    fn tams_manifest_holds_no_secret() {
        let client = TamsClient::new(
            "https://user:pw@gw.example/tams/?token=q",
            AuthMethod::Bearer("s3cret".into()),
        )
        .unwrap();
        let m = Manifest::new(
            &client,
            &spec("f1"),
            DetectedEssence::default(),
            "video/mp2t",
            FlowId::new_v4(),
            "out",
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("s3cret") && !json.contains("pw") && !json.contains("token=q"));
        assert_eq!(m.auth, AuthRef::StaticToken);
        assert_eq!(m.gateway, "https://gw.example/tams");
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.flow_spec.flow_id, "f1");
    }
}
