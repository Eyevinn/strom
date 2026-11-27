//! Source switching (`cue {source}`, `source`) and the next clip of the
//! queue (`cue_next`): the [`Lane`] behind the player, one per source, and
//! how a switch settles. The player keeps its decoders and output; a switch
//! puts a second lane beside the first, and the tick shows it once it has
//! the picture. For a queued clip the second lane is fetched and decoded at
//! the clip's in point while the current clip plays (with the clip's source
//! for the angle it names, else the source shown), and the tick shows it
//! from the tick the queue starts the clip on.

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
    /// The queued clip the lane is prepared for (its `seq`), 0 for a
    /// switch.
    pub(super) item: u64,
    /// A switch of source the client hears about (`source_changed`,
    /// `source_failed`, `pending_source`): a switch, or a queued clip with
    /// another source for this angle. Not a queued clip's lane on the
    /// source shown.
    pub(super) announce: bool,
}

impl Lane {
    pub(super) fn new(
        fetcher: Fetcher,
        uri: Option<String>,
        ident: SourceIdent,
        client_ref: Option<String>,
        item: u64,
        announce: bool,
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
            item,
            announce,
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
    ///
    /// A lane prepared for a queued clip is shown from the tick on which
    /// the queue plays that clip (`item`), with whatever it has decoded by
    /// then; until then the current lane is shown.
    pub(super) fn settle_switch(&self, want: Option<i64>, item: u64) -> Settle {
        let Some(p) = lock(&self.lanes).pending.clone() else {
            return Settle::None;
        };
        if p.item != 0 {
            if p.item == item {
                self.commit(&p);
                return Settle::Committed;
            }
            if let Err(e) = usable(&p) {
                self.abandon(&p, e);
            }
            return Settle::None;
        }
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
        // From now on it follows the cursor.
        p.fetcher.set_aim(None);
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
        if p.announce {
            self.timeline.source_changed(
                &self.block_id,
                p.ident.source.clone(),
                p.client_ref.clone(),
            );
        } else {
            // The source shown stays; only the stretch of it changed.
            self.stats.set_pending_source(None);
        }
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
        self.lane_failed(p, error);
    }

    /// Tell the client a lane will not be shown, if it is a switch it
    /// heard of.
    fn lane_failed(&self, p: &Lane, error: String) {
        if p.announce {
            self.timeline.source_failed(
                &self.block_id,
                p.ident.source.clone(),
                error,
                p.client_ref.clone(),
            );
        }
    }

    /// Prepare the queue's next clip: a lane at its in point, with the
    /// clip's source for this angle or else the source shown. Once per
    /// clip, while no other switch is under way; a prepared lane whose clip
    /// left the queue (cleared, replaced) is dropped. Called by the
    /// scheduler.
    pub(super) fn prepare_queued(&self) {
        let Some(open) = self.switching.as_ref() else {
            return;
        };
        let (playing, next) = self.timeline.queue_view();
        let (current, pending) = self.lanes();
        if let Some(p) = pending {
            let wanted = p.item == playing || next.as_ref().is_some_and(|i| i.seq == p.item);
            if p.item != 0 && !wanted {
                self.drop_pending(&p, "its clip left the queue");
            }
            return;
        }
        let Some(item) = next else {
            return;
        };
        if self.prepared.swap(item.seq, Ordering::Relaxed) == item.seq {
            return;
        }
        let (opened, announce) = match item.source_for(&self.block_id) {
            Some(s) => (
                Ok(s.clone()),
                current.uri.as_deref() != Some(s.uri.as_str()),
            ),
            None => match &current.uri {
                Some(uri) => (open(uri), false),
                None => return,
            },
        };
        let opened = match opened {
            Ok(o) => o,
            Err(e) => {
                warn!(
                    "Replay {}: the clip queued next cannot be prepared: {}",
                    self.block_id, e
                );
                return;
            }
        };
        let lane = self.new_lane(opened, item.client_ref.clone(), item.seq, announce);
        lane.fetcher.set_aim(Some(item.start));
        debug!(
            "Replay {}: preparing queued clip {} on {}",
            self.block_id,
            item.seq,
            lane.ident.source.as_deref().unwrap_or("?")
        );
        {
            let mut lanes = lock(&self.lanes);
            if lanes.pending.is_some() {
                // A switch came in meanwhile: it goes first.
                drop(lanes);
                lane.halt();
                self.prepared.store(0, Ordering::Relaxed);
                return;
            }
            lanes.pending = Some(Arc::clone(&lane));
        }
        lane.spawn();
        if announce {
            self.stats.set_pending_source(lane.ident.source.clone());
        }
        if !self.alive() {
            lane.halt();
        }
    }

    /// Drop the pending lane `p` without showing it.
    fn drop_pending(&self, p: &Arc<Lane>, why: &str) {
        {
            let mut lanes = lock(&self.lanes);
            if !lanes.pending.as_ref().is_some_and(|q| Arc::ptr_eq(q, p)) {
                return;
            }
            lanes.pending = None;
        }
        p.halt();
        if p.announce {
            self.stats.set_pending_source(None);
        }
        debug!("Replay {}: prepared lane dropped: {}", self.block_id, why);
        self.lane_failed(p, why.to_string());
    }

    /// A lane for `source`, not started, reading into stats of its own (so
    /// the group sees the shown source's extent, grid and index until it is
    /// shown). The trim is the new source's (else the player's own).
    fn new_lane(
        &self,
        source: OpenedSource,
        client_ref: Option<String>,
        item: u64,
        announce: bool,
    ) -> Arc<Lane> {
        let OpenedSource { index, uri, ident } = source;
        let stats = Arc::new(PlayerStats::with_config(
            &self.block_id,
            PlayerConfig {
                source: ident.source.clone(),
                source_id: ident.source_id.clone(),
                store: ident.store.clone(),
                offset_ns: self.stats.config.offset_ns,
                flow_id: self.stats.config.flow_id,
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
        Lane::new(fetcher, Some(uri), ident, client_ref, item, announce)
    }
}

/// Whether lane `p` can be shown at all: an error says why not (see
/// [`switch_ready`]).
fn usable(p: &Lane) -> Result<(), String> {
    if let Some(e) = p.error() {
        return Err(e);
    }
    if p.store().info().is_some_and(|i| !i.absolute_time) {
        return Err("the source is not on real TAI (no common time with the group)".into());
    }
    Ok(())
}

/// Whether lane `p`, being switched to, can be shown at group time `want`:
/// it has the frame for it (or the frame a hole holds), or its media does
/// not reach there (nothing to wait for). An error gives the switch up: the
/// source cannot be played or decoded into the output's layout, it is not
/// on real TAI (it would not line up with the group), or no picture came
/// within [`SWITCH_TIMEOUT`].
fn switch_ready(p: &Lane, want: Option<i64>) -> Result<bool, String> {
    usable(p)?;
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
        let lane = self.new_lane(OpenedSource { index, uri, ident }, client_ref, 0, true);
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
            self.lane_failed(&old, "replaced by a later switch".into());
        }
        // Shut down meanwhile: the shutdown may not have seen the new lane.
        if !self.alive() {
            lane.halt();
        }
    }

    fn pending_stats(&self) -> Option<Arc<PlayerStats>> {
        lock(&self.lanes).pending.as_ref().map(|p| p.stats())
    }
}
