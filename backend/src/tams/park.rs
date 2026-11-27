//! Where TAMS Output keeps its segment files, parked segments included.
//!
//! A parked segment (the store was unreachable) must survive a restart of
//! Strom, and of the machine, to be uploaded later, so the files live under
//! Strom's data directory rather than the system temp directory:
//!
//! `<root>/<tams flow id>`
//!
//! - `<root>` is `STROM_TAMS_PARK_DIR` when set, else `<data_dir>/tams-park`,
//!   else (no data directory known, e.g. a bare builder in a test)
//!   `<temp_dir>/strom-tams-park`.
//! - One directory per TAMS flow the block writes to, named by its id. The
//!   TAMS flow id is the recording's identity: it derives from the block's
//!   `tams_key` when set, else from the Strom flow id and the block id (see
//!   `crate::blocks::builtin::tams_output::tams_id_key`). Without a
//!   `tams_key` the backlog is found again only by the same Strom flow (a
//!   restart of the flow or of Strom); a production that recreates its Strom
//!   flow (new flow id, new random block ids) finds its backlog again only
//!   when it sets the same `tams_key`. Otherwise no flow uploads the old
//!   directory's segments; they count against the limits below until they
//!   are the oldest and deleted. Two running blocks never write one TAMS
//!   flow (see `crate::tams::flow_claims`).
//!
//! Directories of the old layout (`<temp_dir>/strom-tams-<block>-<suffix>`) are
//! not migrated; [`init`] warns once at startup when they still hold parked
//! segments.
//!
//! Every segment is parked through [`Parking::park`] (see [`parking`]), which
//! enforces two server-wide limits over all parked segments under the root,
//! whichever flow they belong to:
//!
//! - a cap on their total size ([`ParkLimits::max_total_bytes`],
//!   `STROM_TAMS_PARK_MAX_TOTAL_MB`; default 10 % of the volume, at most
//!   32 GiB);
//! - a floor on the free space of the root's volume
//!   ([`ParkLimits::min_free_bytes`], `STROM_TAMS_PARK_MIN_FREE_MB`; default
//!   10 % of the volume, at least 5 GiB and at most 32 GiB).
//!
//! Both defaults have a ceiling, so a large shared data volume does not keep
//! hundreds of GiB free or parked: on a 4 TB volume parking stops when less
//! than 32 GiB is free, not 400 GiB.
//!
//! Past either, the oldest parked segments of the whole server (lowest start
//! time, whichever flow) are deleted until the new one fits. When deleting
//! every older one would not be enough (the volume is filled by something
//! else, or the segment is larger than the cap), nothing is deleted and the
//! new segment is not parked; the caller drops it. A segment that is parked
//! already (a retry that failed again) takes no new space: neither limit is
//! checked for it. Parking never stops a live flow.
//!
//! The segment, its sidecar and their directory are synced to disk before a
//! park returns, so a parked segment survives a power loss too. The index's
//! lock is never held over a directory scan, a deletion or a sync, and a park
//! from a GStreamer streaming thread runs on a worker thread of its own
//! ([`park_off_thread`]).
//!
//! Deleting the oldest of the whole server gives every flow the same window
//! back in time. A per-flow cap (`STROM_TAMS_PARKED_MAX_MB`,
//! `STROM_TAMS_PARKED_MAX_SEGMENTS`) is off unless set; each uploader's sweep
//! enforces it, see `crate::tams::uploader`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Overrides the root of the segment directories.
pub const PARK_DIR_ENV: &str = "STROM_TAMS_PARK_DIR";

/// The directory under the data directory that holds the segment directories.
const DATA_SUBDIR: &str = "tams-park";

/// The root under the temp directory when no data directory is known.
const TEMP_SUBDIR: &str = "strom-tams-park";

/// Prefix of the old per-block directories under the temp directory.
const LEGACY_PREFIX: &str = "strom-tams-";

/// The sidecar that marks a parked segment (see `crate::tams::uploader`).
const SIDECAR_SUFFIX: &str = ".meta";

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Record Strom's data directory, which the default root lives under, and
/// warn about parked segments left in the old temp-dir layout. Call once at
/// startup, before any flow is built.
pub fn init(data_dir: &Path) {
    let _ = DATA_DIR.set(data_dir.to_path_buf());
    // Build the index of what is parked off the startup path. A park does
    // not wait for it: until it is merged, the limits see only what was
    // parked since.
    std::thread::spawn(|| {
        let p = parking();
        p.warm_up();
    });
    let legacy = legacy_parked_dirs(&std::env::temp_dir());
    if !legacy.is_empty() {
        warn!(
            "TAMS: {} director{} in the old layout still hold parked segments ({}); they are \
             no longer uploaded. TAMS Output now keeps its segments under {}. Move a \
             directory's files to <that dir>/<TAMS flow id> to have them uploaded, or \
             delete them",
            legacy.len(),
            if legacy.len() == 1 { "y" } else { "ies" },
            legacy
                .iter()
                .map(|d| d.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            root().display()
        );
    }
}

/// The root of all TAMS Output segment directories.
pub fn root() -> PathBuf {
    if let Some(dir) = strom_types::env::var_opt(PARK_DIR_ENV) {
        return PathBuf::from(dir);
    }
    match DATA_DIR.get() {
        Some(data_dir) => data_dir.join(DATA_SUBDIR),
        None => std::env::temp_dir().join(TEMP_SUBDIR),
    }
}

/// The segment directory of the TAMS flow `tams_flow_id`.
pub fn flow_dir(tams_flow_id: &str) -> PathBuf {
    root().join(sanitize(tams_flow_id))
}

/// A path component from an id: letters, digits, `-` and `_` are kept, all
/// else becomes `_`, so an id never escapes the root or adds a level.
fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "_".to_string()
    } else {
        s
    }
}

/// Old-layout directories under `tmp` (`strom-tams-<block>-<suffix>`) that
/// still hold a parked segment's sidecar.
fn legacy_parked_dirs(tmp: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(LEGACY_PREFIX)
                && name != TEMP_SUBDIR
                // The uploader's unit tests use these.
                && !name.starts_with("strom-tams-uploader-")
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .map(|e| e.path())
        .filter(|dir| has_sidecar(dir))
        .collect();
    dirs.sort();
    dirs
}

// --- Server-wide limits -------------------------------------------------------

/// Overrides the cap on the total size of all parked segments, in MiB.
pub const MAX_TOTAL_MB_ENV: &str = "STROM_TAMS_PARK_MAX_TOTAL_MB";

/// Overrides the free space parking keeps on the root's volume, in MiB.
pub const MIN_FREE_MB_ENV: &str = "STROM_TAMS_PARK_MIN_FREE_MB";

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Default cap on all parked segments: this share of the volume ...
const DEFAULT_MAX_TOTAL_PERCENT: u64 = 10;
/// ... but never more than this, which is also the cap when the volume's
/// size is unknown.
const DEFAULT_MAX_TOTAL_CEILING: u64 = 32 * GIB;
/// Default free space kept: this share of the volume ...
const DEFAULT_MIN_FREE_PERCENT: u64 = 10;
/// ... but never less than this ...
const DEFAULT_MIN_FREE_FLOOR: u64 = 5 * GIB;
/// ... nor more than this.
const DEFAULT_MIN_FREE_CEILING: u64 = 32 * GIB;

/// How long a free-space reading is reused.
const SPACE_CACHE: Duration = Duration::from_secs(1);

/// How often a usage warning repeats while usage stays at its level.
const USAGE_WARN_INTERVAL: Duration = Duration::from_secs(300);

/// Usage levels (percent of the cap) that are warned about.
const USAGE_WARN_LEVELS: [u64; 2] = [80, 50];

/// How often deletions for space are reported while they go on.
const EVICTION_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Server-wide limits on parked segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkLimits {
    /// Cap on the total size of all parked segments under the root.
    pub max_total_bytes: u64,
    /// Free space on the root's volume below which nothing new is parked.
    pub min_free_bytes: u64,
}

impl ParkLimits {
    /// No limits: nothing is ever deleted for space or refused.
    pub const UNLIMITED: Self = Self {
        max_total_bytes: u64::MAX,
        min_free_bytes: 0,
    };

    /// The defaults for a volume of `volume_bytes`: the cap is 10 % of the
    /// volume, at most 32 GiB; the free space kept is 10 % of the volume, at
    /// least 5 GiB and at most 32 GiB. When the volume's size is unknown: a
    /// 32 GiB cap and 5 GiB free.
    pub fn defaults(volume_bytes: Option<u64>) -> Self {
        match volume_bytes {
            Some(v) => Self {
                max_total_bytes: percent_of(v, DEFAULT_MAX_TOTAL_PERCENT)
                    .min(DEFAULT_MAX_TOTAL_CEILING),
                min_free_bytes: percent_of(v, DEFAULT_MIN_FREE_PERCENT)
                    .clamp(DEFAULT_MIN_FREE_FLOOR, DEFAULT_MIN_FREE_CEILING),
            },
            None => Self {
                max_total_bytes: DEFAULT_MAX_TOTAL_CEILING,
                min_free_bytes: DEFAULT_MIN_FREE_FLOOR,
            },
        }
    }

    /// The defaults, overridden by `STROM_TAMS_PARK_MAX_TOTAL_MB` and
    /// `STROM_TAMS_PARK_MIN_FREE_MB`. A value that is not a whole number of
    /// MiB is warned about (once per value) and the default used.
    pub fn from_env(volume_bytes: Option<u64>) -> Self {
        fn mb(name: &str) -> Option<u64> {
            let value = strom_types::env::var_opt(name)?;
            match mb_value(name, &value) {
                Ok(bytes) => Some(bytes),
                Err(why) => {
                    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
                    let fresh = WARNED
                        .get_or_init(Default::default)
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(format!("{name}={value}"));
                    if fresh {
                        warn!("TAMS: {}", why);
                    }
                    None
                }
            }
        }
        let d = Self::defaults(volume_bytes);
        Self {
            max_total_bytes: mb(MAX_TOTAL_MB_ENV).unwrap_or(d.max_total_bytes),
            min_free_bytes: mb(MIN_FREE_MB_ENV).unwrap_or(d.min_free_bytes),
        }
    }
}

/// The bytes of `value`, a whole number of MiB set in variable `name`, or
/// why it is not one.
fn mb_value(name: &str, value: &str) -> Result<u64, String> {
    value
        .trim()
        .parse::<u64>()
        .map(|v| v.saturating_mul(MIB))
        .map_err(|_| {
            format!(
                "{}={:?} is not a whole number of MiB (e.g. 10240 for 10 GiB); using the \
                 default",
                name, value
            )
        })
}

fn percent_of(v: u64, percent: u64) -> u64 {
    (u128::from(v) * u128::from(percent) / 100) as u64
}

/// Free and total bytes of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// Bytes an unprivileged process may still write.
    pub free: u64,
    pub total: u64,
}

/// Reads the free space of the volume a path is on. A trait so tests can
/// give any value without filling a disk.
pub trait FreeSpace: Send + Sync {
    /// `None` when it cannot be read; then only the cap applies.
    fn space(&self, path: &Path) -> Option<Space>;
}

/// The real volume, through `statvfs` (Unix) or `sysinfo` (elsewhere).
pub struct VolumeSpace;

impl FreeSpace for VolumeSpace {
    fn space(&self, path: &Path) -> Option<Space> {
        // The root may not exist yet: ask about its nearest existing ancestor.
        let existing = path.ancestors().find(|p| p.exists())?;
        volume_space(existing)
    }
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)] // The field types differ per platform.
fn volume_space(path: &Path) -> Option<Space> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs
    // that the call fills in.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = u64::from(st.f_frsize);
    Some(Space {
        free: u64::from(st.f_bavail).saturating_mul(frsize),
        total: u64::from(st.f_blocks).saturating_mul(frsize),
    })
}

#[cfg(not(unix))]
fn volume_space(path: &Path) -> Option<Space> {
    // `std::fs::canonicalize` gives a verbatim path on Windows (`\\?\C:\...`),
    // whose prefix never matches a mount point (`C:\`); `dunce` gives the
    // plain form when one exists.
    let path = dunce::canonicalize(path).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| Space {
            free: d.available_space(),
            total: d.total_space(),
        })
}

/// Why a segment was not parked. The caller drops it.
#[derive(Debug)]
pub enum ParkError {
    /// Over the cap or short of free space, even after deleting the oldest.
    Refused(String),
    /// The segment or its sidecar could not be read or written.
    Io(std::io::Error),
}

impl std::fmt::Display for ParkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParkError::Refused(why) => write!(f, "{}", why),
            ParkError::Io(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for ParkError {}

/// One parked segment the index knows.
#[derive(Debug, Clone, Copy)]
struct Entry {
    start_ns: u64,
    bytes: u64,
    /// When the index learnt of it, so a directory snapshot taken before
    /// does not forget it (see [`Parking::sync_dir`]).
    seen: Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<PathBuf, Entry>,
    /// Segments being parked: counted in `entries` (so a concurrent park
    /// sees their bytes) but not parked yet, so never deleted for space.
    reserved: HashSet<PathBuf>,
    /// The startup scan runs (without this lock); what is removed meanwhile
    /// is recorded, so the scan's merge does not bring it back.
    scanning: bool,
    removed_while_scanning: HashSet<PathBuf>,
    total: u64,
    space: Option<(Instant, Option<Space>)>,
    /// Last usage warning: its level and when.
    warned: Option<(u64, Instant)>,
    /// Deletions for space not yet reported: count, bytes.
    evicted: (usize, u64),
    evicted_reported_at: Option<Instant>,
}

impl State {
    fn remove(&mut self, path: &Path) {
        if self.scanning {
            self.removed_while_scanning.insert(path.to_path_buf());
        }
        self.reserved.remove(path);
        if let Some(e) = self.entries.remove(path) {
            self.total = self.total.saturating_sub(e.bytes);
        }
    }

    fn insert(&mut self, path: PathBuf, entry: Entry) {
        self.total = self.total.saturating_add(entry.bytes);
        if let Some(old) = self.entries.insert(path, entry) {
            self.total = self.total.saturating_sub(old.bytes);
        }
    }

    /// Adjust the cached free space by bytes deleted (`+`) or not deleted
    /// after all (`-`), which the cached reading does not know of yet.
    fn adjust_free(&mut self, freed: u64, unfreed: u64) {
        if let Some((_, Some(s))) = self.space.as_mut() {
            s.free = s.free.saturating_add(freed).saturating_sub(unfreed);
        }
    }
}

/// A parked segment chosen to be deleted for space. Holds its claim, so no
/// uploader takes it up meanwhile.
struct Victim {
    path: PathBuf,
    entry: Entry,
    _claim: crate::tams::uploader::Claim,
}

/// The parked segments of the whole server, and the limits on them. One per
/// process ([`parking`]); tests make their own.
pub struct Parking {
    root: PathBuf,
    limits: Mutex<Option<ParkLimits>>,
    fs: Box<dyn FreeSpace>,
    state: Mutex<State>,
    /// Held while the root is scanned; `true` once it was.
    scanned: Mutex<bool>,
}

/// The process-wide [`Parking`] over [`root`].
pub fn parking() -> &'static Parking {
    static PARKING: OnceLock<Parking> = OnceLock::new();
    PARKING.get_or_init(|| {
        // Unit tests park in the temp dir, whose volume is the developer's
        // disk; the limits are tested on their own `Parking`.
        let limits = cfg!(test).then_some(ParkLimits::UNLIMITED);
        Parking::new(root(), limits, Box::new(VolumeSpace))
    })
}

/// Run `job` (a park) on the parking worker thread, not the caller's. For a
/// GStreamer streaming thread: a park can delete older segments and syncs to
/// disk, which must not hold up the stream. Jobs run one at a time, in order.
pub fn park_off_thread(job: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static WORKER: OnceLock<Option<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("tams-park".to_string())
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .map_err(|e| error!("TAMS: cannot start the parking thread: {}", e))
            .ok()
            .map(|_| tx)
    });
    let job: Job = Box::new(job);
    let job = match worker {
        Some(tx) => match tx.send(job) {
            Ok(()) => return,
            Err(e) => e.0,
        },
        None => job,
    };
    // No worker: park here rather than lose the segment.
    job();
}

impl Parking {
    /// `limits` `None`: [`ParkLimits::from_env`] for the root's volume.
    pub fn new(root: PathBuf, limits: Option<ParkLimits>, fs: Box<dyn FreeSpace>) -> Self {
        Self {
            root,
            limits: Mutex::new(limits),
            fs,
            state: Mutex::new(State::default()),
            scanned: Mutex::new(false),
        }
    }

    /// Set the limits instead of [`ParkLimits::from_env`] (`None`: from the
    /// environment again). For an embedding or a test harness that must not
    /// depend on the host disk's free space.
    pub fn set_limits(&self, limits: Option<ParkLimits>) {
        *self.limits.lock().unwrap_or_else(|e| e.into_inner()) = limits;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Index what is parked under the root and log the limits.
    fn warm_up(&self) {
        self.ensure_scanned(true);
        let mut st = self.lock();
        let space = self.space(&mut st);
        let limits = self.limits(space);
        info!(
            "TAMS: {} parked segment(s), {} MiB, under {}; cap {} MiB ({}), free space kept {} \
             MiB ({}){}",
            st.entries.len(),
            st.total / MIB,
            self.root.display(),
            limits.max_total_bytes / MIB,
            MAX_TOTAL_MB_ENV,
            limits.min_free_bytes / MIB,
            MIN_FREE_MB_ENV,
            match space {
                Some(s) => format!(", {} MiB free now", s.free / MIB),
                None => ", free space unknown".to_string(),
            }
        );
    }

    fn limits(&self, space: Option<Space>) -> ParkLimits {
        let set = *self.limits.lock().unwrap_or_else(|e| e.into_inner());
        set.unwrap_or_else(|| ParkLimits::from_env(space.map(|s| s.total)))
    }

    /// The root volume's space, read at most once per [`SPACE_CACHE`].
    fn space(&self, st: &mut State) -> Option<Space> {
        match st.space {
            Some((at, space)) if at.elapsed() < SPACE_CACHE => space,
            _ => {
                let space = self.fs.space(&self.root);
                st.space = Some((Instant::now(), space));
                space
            }
        }
    }

    /// Index what is parked under the root, once. The scan runs without the
    /// index's lock and is merged when done. With `wait`, a scan that runs
    /// elsewhere is waited for; without, the caller goes on with what the
    /// index holds, so a park never waits for the startup scan.
    fn ensure_scanned(&self, wait: bool) {
        let mut done = if wait {
            self.scanned.lock().unwrap_or_else(|e| e.into_inner())
        } else {
            match self.scanned.try_lock() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(e)) => e.into_inner(),
                Err(TryLockError::WouldBlock) => return,
            }
        };
        if *done {
            return;
        }
        self.lock().scanning = true;
        let found = scan_root(&self.root);
        let mut st = self.lock();
        st.scanning = false;
        let removed = std::mem::take(&mut st.removed_while_scanning);
        for (path, entry) in found {
            // Parked again since (the newer entry wins), or gone since.
            if !st.entries.contains_key(&path) && !removed.contains(&path) {
                st.insert(path, entry);
            }
        }
        *done = true;
    }

    /// Park `segment` (already written, starting at `start_ns`): write its
    /// sidecar, `sidecar_contents`, to a temporary file and rename it into
    /// place, and sync the segment, the sidecar and their directory to disk.
    /// The only way a segment is parked, so nothing bypasses the limits: past
    /// the cap, or short of free space, the oldest parked segments of the
    /// whole server are deleted first. When deleting all of them would not be
    /// enough, nothing is deleted or written and [`ParkError::Refused`] is
    /// returned; the caller then drops the segment.
    ///
    /// A segment that is parked already (its sidecar exists: a retry that
    /// failed again) takes no new space: neither limit is checked and nothing
    /// is deleted for it, so a retry never loses a segment for space. If its
    /// sidecar cannot be rewritten, the one it has is kept.
    ///
    /// Blocking; the index's lock is not held over file I/O.
    pub fn park(
        &self,
        segment: &Path,
        start_ns: u64,
        sidecar_contents: &str,
    ) -> Result<(), ParkError> {
        let bytes = std::fs::metadata(segment).map_err(ParkError::Io)?.len();
        let parked_already = crate::tams::uploader::sidecar_path(segment).exists();
        self.ensure_scanned(false);
        let mut limits = None;
        if !parked_already {
            let mut st = self.lock();
            let space = self.space(&mut st);
            let l = self.limits(space);
            let victims = self.reserve(&mut st, segment, start_ns, bytes, space, &l)?;
            drop(st);
            self.evict(victims, &l);
            limits = Some(l);
        }

        if let Err(e) = write_sidecar(segment, sidecar_contents) {
            if parked_already {
                warn!(
                    "TAMS: could not rewrite the sidecar of parked segment {} ({}); it keeps \
                     the one it has",
                    segment.display(),
                    e
                );
            } else {
                self.lock().remove(segment);
                return Err(ParkError::Io(e));
            }
        }
        let mut st = self.lock();
        st.reserved.remove(segment);
        st.insert(
            segment.to_path_buf(),
            Entry {
                start_ns,
                bytes,
                seen: Instant::now(),
            },
        );
        if let Some(l) = limits {
            self.warn_usage(&mut st, &l);
        }
        Ok(())
    }

    /// Make room for a new segment of `bytes`, under the lock: choose the
    /// oldest parked segments (never `segment`, one being parked, or one an
    /// uploader is busy with) whose deletion brings `bytes` more under the
    /// cap and keeps the free space, take them out of the index and reserve
    /// `segment`'s bytes. Deletes nothing: the caller deletes the victims
    /// ([`Self::evict`]) without the lock. When even all of them would not
    /// be enough, nothing is chosen and the park is refused.
    fn reserve(
        &self,
        st: &mut State,
        segment: &Path,
        start_ns: u64,
        bytes: u64,
        space: Option<Space>,
        limits: &ParkLimits,
    ) -> Result<Vec<Victim>, ParkError> {
        st.remove(segment);
        let need_cap = st
            .total
            .saturating_add(bytes)
            .saturating_sub(limits.max_total_bytes);
        let need_free = space.map_or(0, |s| limits.min_free_bytes.saturating_sub(s.free));
        let need = need_cap.max(need_free);
        let mut victims = Vec::new();
        if need > 0 {
            let mut oldest: Vec<(u64, PathBuf)> = st
                .entries
                .iter()
                .filter(|(p, _)| p.as_path() != segment && !st.reserved.contains(*p))
                .map(|(p, e)| (e.start_ns, p.clone()))
                .collect();
            oldest.sort();
            let mut planned = 0u64;
            for (_, path) in oldest {
                if planned >= need {
                    break;
                }
                let Some(claim) = crate::tams::uploader::Claim::take(&path) else {
                    continue;
                };
                let Some(entry) = st.entries.get(&path).copied() else {
                    continue;
                };
                planned = planned.saturating_add(entry.bytes);
                victims.push(Victim {
                    path,
                    entry,
                    _claim: claim,
                });
            }
            if planned < need {
                // Deleting them all would not make room: keep them (dropping
                // the victims releases their claims) and refuse this one.
                return Err(ParkError::Refused(if need_free > planned {
                    let free = space.map_or(0, |s| s.free);
                    format!(
                        "only {} MiB free on the volume of {}, under the {} MiB parking keeps \
                         free ({}), and deleting every older parked segment ({} MiB) would \
                         not make up for it; none was deleted",
                        free / MIB,
                        self.root.display(),
                        limits.min_free_bytes / MIB,
                        MIN_FREE_MB_ENV,
                        planned / MIB
                    )
                } else {
                    format!(
                        "parking it would pass the cap on parked segments ({} MiB, {}) even \
                         after deleting every older parked segment; none was deleted",
                        limits.max_total_bytes / MIB,
                        MAX_TOTAL_MB_ENV
                    )
                }));
            }
            for v in &victims {
                st.remove(&v.path);
            }
            st.adjust_free(planned, 0);
        }
        st.reserved.insert(segment.to_path_buf());
        st.insert(
            segment.to_path_buf(),
            Entry {
                start_ns,
                bytes,
                seen: Instant::now(),
            },
        );
        Ok(victims)
    }

    /// Delete the segments [`Self::reserve`] chose, without the lock, and
    /// report it. One that cannot be deleted goes back into the index.
    fn evict(&self, victims: Vec<Victim>, limits: &ParkLimits) {
        if victims.is_empty() {
            return;
        }
        let (mut count, mut count_bytes) = (0usize, 0u64);
        let mut kept = Vec::new();
        for v in victims {
            match std::fs::remove_file(&v.path) {
                Ok(()) => {
                    count += 1;
                    count_bytes += v.entry.bytes;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    warn!(
                        "TAMS: could not delete parked segment {}: {}",
                        v.path.display(),
                        e
                    );
                    kept.push(v);
                    continue;
                }
            }
            let _ = std::fs::remove_file(crate::tams::uploader::sidecar_path(&v.path));
        }
        let mut st = self.lock();
        for v in kept {
            st.adjust_free(0, v.entry.bytes);
            st.insert(v.path.clone(), v.entry);
        }
        if count > 0 {
            st.evicted.0 += count;
            st.evicted.1 += count_bytes;
            let due = st
                .evicted_reported_at
                .is_none_or(|at| at.elapsed() >= EVICTION_REPORT_INTERVAL);
            if due {
                error!(
                    "TAMS: deleted the {} oldest parked segment(s) of the server ({} MiB) to \
                     stay under the cap on parked segments ({} MiB, {}) and keep {} MiB free \
                     ({}); they are lost from the store",
                    st.evicted.0,
                    st.evicted.1 / MIB,
                    limits.max_total_bytes / MIB,
                    MAX_TOTAL_MB_ENV,
                    limits.min_free_bytes / MIB,
                    MIN_FREE_MB_ENV
                );
                st.evicted = (0, 0);
                st.evicted_reported_at = Some(Instant::now());
            }
        }
    }

    /// Warn when what is parked reaches 50 % or 80 % of the cap: on reaching a
    /// level, and again every [`USAGE_WARN_INTERVAL`] while it stays there.
    fn warn_usage(&self, st: &mut State, limits: &ParkLimits) {
        let cap = limits.max_total_bytes.max(1);
        let percent = (u128::from(st.total) * 100 / u128::from(cap)) as u64;
        let Some(level) = USAGE_WARN_LEVELS.into_iter().find(|l| percent >= *l) else {
            st.warned = None;
            return;
        };
        let due = st
            .warned
            .is_none_or(|(l, at)| level > l || at.elapsed() >= USAGE_WARN_INTERVAL);
        if due {
            warn!(
                "TAMS: parked segments use {} % of their cap ({} of {} MiB, {}); past it the \
                 oldest are deleted",
                percent,
                st.total / MIB,
                limits.max_total_bytes / MIB,
                MAX_TOTAL_MB_ENV
            );
            st.warned = Some((level, Instant::now()));
        }
    }

    /// Forget a parked segment that was deleted (uploaded, or given up).
    pub fn forget(&self, segment: &Path) {
        self.lock().remove(segment);
    }

    /// Replace what the index knows of `dir` with a fresh scan of it, taken
    /// at `scanned_at`: corrects segments deleted or added behind its back.
    /// A segment parked after the scan began is kept.
    pub fn sync_dir(
        &self,
        dir: &Path,
        scanned_at: Instant,
        parked: impl IntoIterator<Item = (PathBuf, u64, u64)>,
    ) {
        let mut st = self.lock();
        let stale: Vec<PathBuf> = st
            .entries
            .iter()
            .filter(|(p, e)| {
                p.parent() == Some(dir) && e.seen < scanned_at && !st.reserved.contains(*p)
            })
            .map(|(p, _)| p.clone())
            .collect();
        for p in stale {
            st.remove(&p);
        }
        for (path, start_ns, bytes) in parked {
            if !st.entries.contains_key(&path) {
                st.insert(
                    path,
                    Entry {
                        start_ns,
                        bytes,
                        seen: scanned_at,
                    },
                );
            }
        }
    }

    /// Total size of what is parked, as the index knows it.
    pub fn total_bytes(&self) -> u64 {
        self.lock().total
    }
}

/// Write the sidecar of `segment` through a temporary file renamed into
/// place. The segment's data, the sidecar and their directory are synced
/// first, so a sidecar that survives a power loss never names a segment
/// whose data did not.
fn write_sidecar(segment: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Opened for writing: syncing needs it on Windows. Nothing is written.
    std::fs::OpenOptions::new()
        .write(true)
        .open(segment)?
        .sync_all()?;
    let sidecar = crate::tams::uploader::sidecar_path(segment);
    let tmp = crate::tams::uploader::sidecar_tmp_path(segment);
    let written = std::fs::File::create(&tmp)
        .and_then(|mut f| {
            f.write_all(contents.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &sidecar));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(dir) = segment.parent() {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Sync a directory, so the names created or renamed in it survive a power
/// loss. Unix only; elsewhere a directory cannot be opened for it.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Every parked segment under `root`: each sidecar that parses, with its
/// segment's size. Blocking, and without the index's lock.
fn scan_root(root: &Path) -> Vec<(PathBuf, Entry)> {
    let mut found = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else {
        return found;
    };
    let now = Instant::now();
    for dir in dirs
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
    {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for sidecar in files.flatten().map(|e| e.path()) {
            let Some(segment) = sidecar
                .to_str()
                .and_then(|s| s.strip_suffix(SIDECAR_SUFFIX))
                .map(PathBuf::from)
            else {
                continue;
            };
            let start = std::fs::read_to_string(&sidecar)
                .ok()
                .and_then(|s| crate::tams::uploader::parse_sidecar(&s))
                .map(|(start_ns, _, _)| start_ns);
            let bytes = std::fs::metadata(&segment).ok().map(|m| m.len());
            if let (Some(start_ns), Some(bytes)) = (start, bytes) {
                found.push((
                    segment,
                    Entry {
                        start_ns,
                        bytes,
                        seen: now,
                    },
                ));
            }
        }
    }
    found
}

fn has_sidecar(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|rd| {
        rd.filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(SIDECAR_SUFFIX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tams_park_dir_is_one_level_per_tams_flow() {
        let id = "6f1c2a4e-0b7d-5c3e-9a1f-2d4b6c8e0a1b";
        assert_eq!(flow_dir(id), root().join(id));
        // An id never escapes the root or adds a level.
        let odd = flow_dir("../x/y");
        assert_eq!(odd.strip_prefix(root()).unwrap().iter().count(), 1);
    }

    #[test]
    fn tams_legacy_dirs_with_parked_segments_are_found() {
        let tmp = tempfile::tempdir().unwrap();
        let parked = tmp.path().join("strom-tams-cam1-mux");
        let empty = tmp.path().join("strom-tams-cam2-mux");
        let tests = tmp.path().join("strom-tams-uploader-x");
        for d in [&parked, &empty, &tests] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(parked.join("seg_r1_00001.ts.meta"), "{}").unwrap();
        std::fs::write(empty.join("seg_r1_00001.ts"), "").unwrap();
        std::fs::write(tests.join("seg_r1_00001.ts.meta"), "{}").unwrap();
        assert_eq!(legacy_parked_dirs(tmp.path()), vec![parked]);
    }

    /// The real volume is found for the temp dir and for a directory under
    /// it that does not exist yet (the park root before the first park). On
    /// Windows a canonicalised (verbatim) path matched no mount point, so the
    /// free-space floor was never applied.
    #[test]
    fn tams_park_volume_space_finds_the_real_volume() {
        let tmp = tempfile::tempdir().unwrap();
        for p in [tmp.path().to_path_buf(), tmp.path().join("a").join("b")] {
            let space = VolumeSpace
                .space(&p)
                .unwrap_or_else(|| panic!("no volume found for {}", p.display()));
            assert!(space.total > 0, "{}: {:?}", p.display(), space);
            assert!(space.free <= space.total, "{}: {:?}", p.display(), space);
        }
    }

    /// A [`FreeSpace`] whose value the test sets.
    #[derive(Clone)]
    struct FakeSpace(std::sync::Arc<Mutex<Space>>);

    impl FreeSpace for FakeSpace {
        fn space(&self, _: &Path) -> Option<Space> {
            Some(*self.0.lock().unwrap())
        }
    }

    fn fake(free: u64) -> FakeSpace {
        FakeSpace(std::sync::Arc::new(Mutex::new(Space {
            free,
            total: 100 * GIB,
        })))
    }

    /// Write a 100-byte segment in `<root>/<flow>` and park it.
    fn park_one(
        p: &Parking,
        root: &Path,
        flow: &str,
        start_s: u64,
    ) -> (PathBuf, Result<(), ParkError>) {
        let dir = root.join(flow);
        std::fs::create_dir_all(&dir).unwrap();
        let seg = dir.join(format!("seg_r1_{:05}.ts", start_s));
        std::fs::write(&seg, [0u8; 100]).unwrap();
        let start_ns = start_s * 1_000_000_000;
        let contents = format!("{} {}", start_ns, start_ns + 1_000_000_000);
        let r = p.park(&seg, start_ns, &contents);
        (seg, r)
    }

    fn parked(seg: &Path) -> bool {
        seg.exists() && crate::tams::uploader::sidecar_path(seg).exists()
    }

    /// Past the server-wide cap, the oldest parked segments of the whole
    /// server go first, whichever flow they belong to.
    #[test]
    fn tams_park_cap_deletes_the_oldest_across_flows() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let limits = ParkLimits {
            max_total_bytes: 300,
            min_free_bytes: 0,
        };
        let p = Parking::new(root.into(), Some(limits), Box::new(fake(u64::MAX)));
        let (a10, _) = park_one(&p, root, "a", 10);
        let (b20, _) = park_one(&p, root, "b", 20);
        let (a30, _) = park_one(&p, root, "a", 30);
        assert_eq!(p.total_bytes(), 300);

        // Flow b parks: flow a's oldest segment goes.
        let (b40, r) = park_one(&p, root, "b", 40);
        assert!(r.is_ok());
        assert!(!a10.exists() && !crate::tams::uploader::sidecar_path(&a10).exists());
        assert!(parked(&b20) && parked(&a30) && parked(&b40));

        // Flow a parks: flow b's oldest goes, not a's own.
        let (a50, r) = park_one(&p, root, "a", 50);
        assert!(r.is_ok());
        assert!(!b20.exists());
        assert!(parked(&a30) && parked(&b40) && parked(&a50));
        assert_eq!(p.total_bytes(), 300);

        // A new process finds what is parked.
        let again = Parking::new(root.into(), Some(limits), Box::new(fake(u64::MAX)));
        again.warm_up();
        assert_eq!(again.total_bytes(), 300);

        // An uploaded segment is forgotten, so it frees its share.
        std::fs::remove_file(&a30).unwrap();
        p.forget(&a30);
        let (_, r) = park_one(&p, root, "b", 60);
        assert!(r.is_ok() && parked(&b40) && parked(&a50));
    }

    /// Short of free space, the oldest parked segments go first; when even
    /// deleting all of them is not enough (the volume is filled by something
    /// else), the new segment is refused and nothing is deleted for it.
    #[test]
    fn tams_park_keeps_free_space_or_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let limits = ParkLimits {
            max_total_bytes: u64::MAX,
            min_free_bytes: 2_000,
        };
        let space = fake(10_000);
        let p = Parking::new(root.into(), Some(limits), Box::new(space.clone()));
        let (a10, _) = park_one(&p, root, "a", 10);
        let (b20, _) = park_one(&p, root, "b", 20);

        // 1 950 free: deleting the oldest (100 bytes) is enough.
        space.0.lock().unwrap().free = 1_950;
        std::thread::sleep(SPACE_CACHE);
        let (a30, r) = park_one(&p, root, "a", 30);
        assert!(r.is_ok());
        assert!(!a10.exists() && parked(&b20) && parked(&a30));

        // 500 free: deleting all older segments (200 bytes) is not enough.
        // The backlog stays; only the new segment is refused.
        space.0.lock().unwrap().free = 500;
        std::thread::sleep(SPACE_CACHE);
        let (b40, r) = park_one(&p, root, "b", 40);
        assert!(matches!(r, Err(ParkError::Refused(_))), "{:?}", r);
        assert!(
            parked(&b20) && parked(&a30),
            "the backlog was deleted for nothing"
        );
        assert!(!crate::tams::uploader::sidecar_path(&b40).exists());
        assert_eq!(p.total_bytes(), 200);
    }

    /// A segment larger than the cap could never fit: it is refused without
    /// deleting what is parked.
    #[test]
    fn tams_park_over_the_cap_refuses_without_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let limits = ParkLimits {
            max_total_bytes: 250,
            min_free_bytes: 0,
        };
        let p = Parking::new(root.into(), Some(limits), Box::new(fake(u64::MAX)));
        let (a10, _) = park_one(&p, root, "a", 10);
        let (b20, _) = park_one(&p, root, "b", 20);
        let big = root.join("a").join("seg_r1_00030.ts");
        std::fs::write(&big, [0u8; 300]).unwrap();
        let r = p.park(&big, 30_000_000_000, "30000000000 31000000000");
        assert!(matches!(r, Err(ParkError::Refused(_))), "{:?}", r);
        assert!(parked(&a10) && parked(&b20));
        assert_eq!(p.total_bytes(), 200);
    }

    /// A segment parked again (a retry that failed again) is on disk and
    /// counted already: short of free space, it deletes no older segment and
    /// is not refused, so the caller never deletes it for space.
    #[test]
    fn tams_park_again_needs_no_new_space() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let limits = ParkLimits {
            max_total_bytes: 200,
            min_free_bytes: 2_000,
        };
        let space = fake(10_000);
        let p = Parking::new(root.into(), Some(limits), Box::new(space.clone()));
        let (a10, _) = park_one(&p, root, "a", 10);
        let (b20, _) = park_one(&p, root, "b", 20);

        // The volume fills with something else.
        space.0.lock().unwrap().free = 500;
        std::thread::sleep(SPACE_CACHE);
        let start_ns = 20 * 1_000_000_000;
        let contents = format!("{} {}", start_ns, start_ns + 1_000_000_000);
        let r = p.park(&b20, start_ns, &contents);
        assert!(r.is_ok(), "{:?}", r);
        assert!(parked(&a10) && parked(&b20));
        assert_eq!(p.total_bytes(), 200);
        // Also when the index has not seen it (a new process before its scan).
        let fresh = Parking::new(root.into(), Some(limits), Box::new(space.clone()));
        *fresh.scanned.lock().unwrap() = true;
        assert!(fresh.park(&b20, start_ns, &contents).is_ok());
        assert!(parked(&a10) && parked(&b20));
    }

    /// A park does not wait for the startup scan, which runs without the
    /// index's lock; the scan's merge then counts what was parked before,
    /// and does not bring back what went meanwhile.
    #[test]
    fn tams_park_does_not_wait_for_the_startup_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let limits = ParkLimits::UNLIMITED;
        let before = Parking::new(root.clone(), Some(limits), Box::new(fake(u64::MAX)));
        let (a10, _) = park_one(&before, &root, "a", 10);
        let (a11, _) = park_one(&before, &root, "a", 11);

        let p = std::sync::Arc::new(Parking::new(
            root.clone(),
            Some(limits),
            Box::new(fake(u64::MAX)),
        ));
        // A scan runs elsewhere.
        let gate = p.scanned.lock().unwrap();
        p.lock().scanning = true;
        let (tx, rx) = std::sync::mpsc::channel();
        let (p2, root2) = (p.clone(), root.clone());
        std::thread::spawn(move || {
            let _ = tx.send(park_one(&p2, &root2, "b", 20));
            // The index can be read meanwhile.
            let _ = p2.total_bytes();
        });
        let (b20, r) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the park waited for the scan");
        assert!(r.is_ok() && parked(&b20));
        // Uploaded while the scan runs.
        std::fs::remove_file(&a11).unwrap();
        p.forget(&a11);
        drop(gate);

        p.warm_up();
        let has = |seg: &Path| p.lock().entries.contains_key(seg);
        assert!(has(&a10) && has(&b20) && !has(&a11));
        assert_eq!(p.total_bytes(), 200);
    }

    /// Parks from many threads at once, with deletions for space, keep the
    /// index and the disk in step and the total under the cap.
    #[test]
    fn tams_concurrent_parks_stay_under_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let limits = ParkLimits {
            max_total_bytes: 1_000,
            min_free_bytes: 0,
        };
        let p = std::sync::Arc::new(Parking::new(
            root.clone(),
            Some(limits),
            Box::new(fake(u64::MAX)),
        ));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let (p, root) = (p.clone(), root.clone());
                std::thread::spawn(move || {
                    for i in 0..20 {
                        let (_, r) = park_one(&p, &root, &format!("f{t}"), 1 + i * 8 + t);
                        assert!(r.is_ok(), "{:?}", r);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(p.total_bytes(), 1_000);
        let again = Parking::new(root, Some(limits), Box::new(fake(u64::MAX)));
        again.warm_up();
        assert_eq!(again.total_bytes(), 1_000);
    }

    /// A park handed to the parking thread does not hold up the caller (a
    /// GStreamer streaming thread), however long it takes.
    #[test]
    fn tams_park_off_thread_does_not_block_the_caller() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let started = Instant::now();
        park_off_thread(move || {
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            let _ = done_tx.send(());
        });
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(done_rx.try_recv().is_err(), "the job ran on the caller");
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn tams_park_default_limits_follow_the_volume() {
        let d = |v| ParkLimits::defaults(v);
        // Large shared volume: both stop at 32 GiB, not 10 % (400 GiB).
        assert_eq!(
            d(Some(4000 * GIB)),
            ParkLimits {
                max_total_bytes: 32 * GIB,
                min_free_bytes: 32 * GIB,
            }
        );
        // 100 GiB: 10 % each.
        assert_eq!(
            d(Some(100 * GIB)),
            ParkLimits {
                max_total_bytes: 10 * GIB,
                min_free_bytes: 10 * GIB,
            }
        );
        // Small volume: at least 5 GiB stays free.
        assert_eq!(
            d(Some(20 * GIB)),
            ParkLimits {
                max_total_bytes: 2 * GIB,
                min_free_bytes: 5 * GIB,
            }
        );
        // Unknown volume: a fixed cap and the floor.
        assert_eq!(
            d(None),
            ParkLimits {
                max_total_bytes: 32 * GIB,
                min_free_bytes: 5 * GIB,
            }
        );
    }

    /// A value that is not a whole number of MiB is reported, naming the
    /// variable, and the default is used.
    #[test]
    fn tams_park_invalid_mb_value_is_reported() {
        assert_eq!(mb_value(MAX_TOTAL_MB_ENV, " 12 "), Ok(12 * MIB));
        let why = mb_value(MAX_TOTAL_MB_ENV, "10G").unwrap_err();
        assert!(
            why.contains(MAX_TOTAL_MB_ENV) && why.contains("10G"),
            "{why}"
        );
    }
}
