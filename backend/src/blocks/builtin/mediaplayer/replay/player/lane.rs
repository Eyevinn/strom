//! Source switching (`cue {source}`, `source`): the [`Lane`] behind the
//! player, one per source, and how a switch settles. The player keeps its
//! decoders and output; a switch puts a second lane beside the first, and
//! the tick shows it once it has the picture.

use super::*;

/// Longest a source switch waits for the new source's first picture before
/// it gives up and stays on the old source.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One source behind the player: its index and fetched bytes, and the
/// frames decoded from it. A player has one, and a second while it switches
/// source: the new one is fetched and decoded at the cursor while the old
/// one stays, so a switch that fails goes back to it at once. Each has its
/// own frame store; while there are two, each gets half the decoded budget
/// (and at least [`MIN_DECODED_SEGMENTS`]).
pub(super) struct Lane {
    pub(super) fetcher: Arc<Fetcher>,
    /// Stops this lane's fetcher (the player's stop stops every lane).
    pub(super) stop: Arc<AtomicBool>,
    pub(super) decoded: Mutex<FrameStore>,
    /// Signalled when frames are added to `decoded` (an offline tick waits
    /// on it).
    pub(super) frame_ready: Condvar,
    pub(super) inflight: Mutex<HashSet<i64>>,
    /// Segments whose decode failed (or gave no frames), retried with backoff.
    pub(super) failed: Mutex<Backoff<i64>>,
    /// The URI as given (may carry credentials: never shown or logged).
    pub(super) uri: Option<String>,
    pub(super) ident: SourceIdent,
    /// Why the source cannot be shown, found while decoding it.
    pub(super) error: Mutex<Option<String>>,
    pub(super) created: Instant,
    /// The `client_ref` of the command that switched to it.
    pub(super) client_ref: Option<String>,
}

impl Lane {
    pub(super) fn new(
        fetcher: Fetcher,
        uri: Option<String>,
        ident: SourceIdent,
        client_ref: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fetcher: Arc::new(fetcher),
            stop: Arc::new(AtomicBool::new(false)),
            decoded: Mutex::new(FrameStore::default()),
            frame_ready: Condvar::new(),
            inflight: Mutex::new(HashSet::new()),
            failed: Mutex::new(Backoff::new(
                Duration::from_millis(250),
                Duration::from_secs(10),
            )),
            uri,
            ident,
            error: Mutex::new(None),
            created: Instant::now(),
            client_ref,
        })
    }

    pub(super) fn store(&self) -> &SegmentStore {
        &self.fetcher.store
    }

    /// The stats the lane reports to: the player's for the lane shown, its
    /// own for a source being switched to.
    pub(super) fn stats(&self) -> Arc<PlayerStats> {
        self.fetcher.stats()
    }

    pub(super) fn spawn(&self) {
        Arc::clone(&self.fetcher).spawn(Arc::clone(&self.stop));
    }

    pub(super) fn halt(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(super) fn error(&self) -> Option<String> {
        lock(&self.error).clone().or_else(|| self.fetcher.error())
    }
}

/// The source shown, and the one being switched to.
pub(super) struct Lanes {
    pub(super) current: Arc<Lane>,
    pub(super) pending: Option<Arc<Lane>>,
}

/// Where a source switch stands at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Settle {
    /// No switch.
    None,
    /// The new source has the picture: it is shown from this tick on.
    Committed,
    /// Waiting for the new source's picture: hold the last one.
    Waiting,
}

impl ReplayPlayer {
    // --- source switch ------------------------------------------------------

    /// Where a switch stands for the tick that wants group time `want`:
    /// shows the new source once it has the picture (or has none to show
    /// there), gives up on an error or after [`SWITCH_TIMEOUT`].
    pub(super) fn settle_switch(&self, want: Option<i64>) -> Settle {
        let Some(p) = lock(&self.lanes).pending.clone() else {
            return Settle::None;
        };
        match switch_ready(&p, want) {
            Ok(true) => {
                self.commit(&p);
                Settle::Committed
            }
            Ok(false) => Settle::Waiting,
            Err(e) => {
                self.abandon(&p, e);
                Settle::None
            }
        }
    }

    /// Show lane `p` from now on, and let the old one go.
    fn commit(&self, p: &Arc<Lane>) {
        let old = {
            let mut lanes = lock(&self.lanes);
            if !lanes.pending.as_ref().is_some_and(|q| Arc::ptr_eq(q, p)) {
                return;
            }
            lanes.pending = None;
            std::mem::replace(&mut lanes.current, Arc::clone(p))
        };
        // The old fetcher reports nowhere from now on; then the new one
        // hands what it knows to the player's stats and reports there.
        old.fetcher
            .retarget(Arc::new(PlayerStats::new(&self.block_id)), |_, _| {});
        old.halt();
        p.fetcher.retarget(Arc::clone(&self.stats), |from, to| {
            to.adopt_source(from, p.ident.clone())
        });
        info!(
            "Replay {}: now showing {} (was {})",
            self.block_id,
            p.ident.source.as_deref().unwrap_or("?"),
            old.ident.source.as_deref().unwrap_or("?")
        );
        self.timeline
            .source_changed(&self.block_id, p.ident.source.clone(), p.client_ref.clone());
        // Freed outside every lock.
        let doomed = std::mem::take(&mut *lock(&old.decoded));
        drop(doomed);
    }

    /// Give up the switch to lane `p`: the old source stays.
    fn abandon(&self, p: &Arc<Lane>, error: String) {
        {
            let mut lanes = lock(&self.lanes);
            if !lanes.pending.as_ref().is_some_and(|q| Arc::ptr_eq(q, p)) {
                return;
            }
            lanes.pending = None;
        }
        p.halt();
        self.stats.set_pending_source(None);
        warn!(
            "Replay {}: switch to {} given up, staying on {}: {}",
            self.block_id,
            p.ident.source.as_deref().unwrap_or("?"),
            self.stats.source_ident().source.as_deref().unwrap_or("?"),
            error
        );
        self.timeline.source_failed(
            &self.block_id,
            p.ident.source.clone(),
            error,
            p.client_ref.clone(),
        );
    }
}

/// Whether lane `p`, being switched to, can be shown at group time `want`:
/// it has the frame for it (or the frame a hole holds), or its media does
/// not reach there (nothing to wait for). An error gives the switch up: the
/// source cannot be played or decoded into the output's layout, it is not
/// on real TAI (it would not line up with the group), or no picture came
/// within [`SWITCH_TIMEOUT`].
fn switch_ready(p: &Lane, want: Option<i64>) -> Result<bool, String> {
    if let Some(e) = p.error() {
        return Err(e);
    }
    if p.store().info().is_some_and(|i| !i.absolute_time) {
        return Err("the source is not on real TAI (no common time with the group)".into());
    }
    if p.created.elapsed() > SWITCH_TIMEOUT {
        return Err(format!(
            "no picture from the source within {} s",
            SWITCH_TIMEOUT.as_secs()
        ));
    }
    let stats = p.stats();
    let (Some(want), Some(grid), Some((start, end))) = (want, stats.grid(), stats.source_extent())
    else {
        return Ok(false);
    };
    let t = want.saturating_sub(stats.offset_ns());
    if t < start || t >= end {
        return Ok(true);
    }
    let decoded = lock(&p.decoded);
    if frame_holding(&decoded.frames, t, grid.frame_ns().ceil() as i64).is_some() {
        return Ok(true);
    }
    Ok(p.store()
        .hold()
        .filter(|h| h.holds(t))
        .is_some_and(|h| held_in(&decoded, &h.seg).is_some()))
}

impl SourceControl for ReplayPlayer {
    fn open_source(&self, uri: &str) -> Result<OpenedSource, String> {
        let open = self
            .switching
            .as_ref()
            .ok_or("this player cannot switch source")?;
        open(uri)
    }

    fn begin_switch(&self, source: OpenedSource, client_ref: Option<String>) {
        let OpenedSource { index, uri, ident } = source;
        if !self.alive() || !self.started.load(Ordering::SeqCst) {
            self.timeline.source_failed(
                &self.block_id,
                ident.source,
                "the player is not running".into(),
                client_ref,
            );
            return;
        }
        let (current, pending) = self.lanes();
        if pending.is_none() && current.uri.as_deref() == Some(uri.as_str()) {
            // Already shown: nothing to switch.
            self.timeline
                .source_changed(&self.block_id, ident.source, client_ref);
            return;
        }
        // The new source is read into stats of its own, so the group sees
        // the old source's extent, grid and index until the switch. The trim
        // is the player's (one day, the source's own).
        let stats = Arc::new(PlayerStats::with_config(
            &self.block_id,
            PlayerConfig {
                offset_ns: self.stats.offset_ns(),
                ..Default::default()
            },
        ));
        let fetcher = Fetcher::new(
            &self.block_id,
            index,
            Arc::clone(&self.timeline),
            stats,
            self.prefetch_s,
        );
        let lane = Lane::new(fetcher, Some(uri), ident, client_ref);
        info!(
            "Replay {}: switching from {} to {}",
            self.block_id,
            current.ident.source.as_deref().unwrap_or("?"),
            lane.ident.source.as_deref().unwrap_or("?")
        );
        lane.spawn();
        let replaced = lock(&self.lanes).pending.replace(Arc::clone(&lane));
        self.stats.set_pending_source(lane.ident.source.clone());
        if let Some(old) = replaced {
            old.halt();
            self.timeline.source_failed(
                &self.block_id,
                old.ident.source.clone(),
                "replaced by a later switch".into(),
                old.client_ref.clone(),
            );
        }
        // Shut down meanwhile: the shutdown may not have seen the new lane.
        if !self.alive() {
            lane.halt();
        }
    }
}
