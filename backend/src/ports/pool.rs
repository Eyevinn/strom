//! The port pool: allocation, reservations and flow associations.
//!
//! Pure logic — every operation takes `now` and, where it matters, a probe
//! callback and the set of flows that still exist, so the whole thing can be
//! exercised without a clock, a socket or a GStreamer element. Persistence and
//! locking live in `AppState`.
//!
//! The pool is a set of port numbers. A range is notation the operator writes
//! and the config expands; nothing below knows about ranges, which is what
//! lets a pool have holes in it and lets a blocked port sit anywhere.
//!
//! Four invariants are the point of the whole module:
//!
//! 1. A port number is handed to one owner at a time.
//! 2. An owner's ports never move while its reservation lives. Asking again
//!    returns the same ports renewed; asking for more adds to them.
//! 3. Releasing a flow's association returns its ports to the reservation,
//!    never to the pool.
//! 4. A reservation that lapses while some of its ports are associated with
//!    existing flows releases only the idle ones.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use strom_types::ports::{
    spans, PortEntry, PortInUse, PortPoolStatus, PortReservation, PortState,
    MAX_PORT_LEASE_TTL_SECS, MAX_RESERVATION_PORTS,
};
use strom_types::FlowId;
use uuid::Uuid;

/// Why a reservation could not be granted or changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortPoolError {
    #[error("owner_id must not be empty")]
    EmptyOwnerId,
    #[error("count must be between 1 and {MAX_RESERVATION_PORTS}, got {0}")]
    BadCount(u16),
    #[error("ttl_secs must be between 1 and {MAX_PORT_LEASE_TTL_SECS}, got {0}")]
    BadTtl(u64),
    #[error("only {available} ports free in the pool, {requested} requested")]
    Exhausted { requested: u16, available: u32 },
    #[error("port reservation not found")]
    NotFound,
    #[error("port {0} does not belong to this reservation")]
    NotInReservation(u16),
    #[error(
        "no port pool is configured on this Strom; set ports.ports \
         or STROM_PORTS to the port numbers it may hand out"
    )]
    NotConfigured,
}

/// One reservation, with parsed timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reservation {
    id: Uuid,
    owner_id: String,
    ports: BTreeSet<u16>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    /// Ports the owner has declared in use, and by which flow. Always a subset
    /// of `ports`.
    in_use: BTreeMap<u16, FlowId>,
}

impl Reservation {
    fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }

    fn to_api(&self) -> PortReservation {
        PortReservation {
            id: self.id,
            owner_id: self.owner_id.clone(),
            ports: self.ports.iter().copied().collect(),
            created_at: rfc3339(self.created_at),
            expires_at: rfc3339(self.expires_at),
            in_use: self
                .in_use
                .iter()
                .map(|(port, flow_id)| PortInUse {
                    port: *port,
                    flow_id: *flow_id,
                })
                .collect(),
        }
    }

    /// Rebuild from a persisted record. An unparseable timestamp counts as
    /// already expired, which is the safe direction: idle ports are reclaimed
    /// rather than held for ever, and ports a flow still uses stay held.
    fn from_api(r: &PortReservation) -> Self {
        let parse = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        };
        let ports: BTreeSet<u16> = r.ports.iter().copied().collect();
        Self {
            id: r.id,
            owner_id: r.owner_id.clone(),
            in_use: r
                .in_use
                .iter()
                .filter(|u| ports.contains(&u.port))
                .map(|u| (u.port, u.flow_id))
                .collect(),
            ports,
            created_at: parse(&r.created_at),
            expires_at: parse(&r.expires_at),
        }
    }
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// What `reserve` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    /// A new reservation was granted.
    Created,
    /// The owner already held one; it was renewed, and grown if asked.
    Renewed,
}

/// The outcome of a reservation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationOutcome {
    pub reservation: PortReservation,
    pub how: Reserved,
    /// Ports found held by something outside Strom during this allocation, and
    /// not blocked before. The caller logs these; logging here, on every
    /// probe, would bury the signal the probe exists to produce.
    pub newly_blocked: Vec<u16>,
}

/// The pool, its reservations and their flow associations.
#[derive(Debug, Clone, Default)]
pub struct PortPool {
    /// Empty until an operator configures ports; nothing is handed out before
    /// then. Persisted reservations are still loaded and kept, so turning the
    /// pool back on does not hand a port that is already spoken for to a
    /// second owner.
    ports: BTreeSet<u16>,
    reservations: Vec<Reservation>,
    /// Ports a bind probe found held by something outside Strom.
    blocked: BTreeSet<u16>,
}

impl PortPool {
    /// A pool with no ports: every reservation route refuses until
    /// [`Self::set_ports`] runs.
    pub fn new() -> Self {
        Self::default()
    }

    /// A pool over `ports`.
    pub fn with_ports(ports: impl IntoIterator<Item = u16>) -> Self {
        Self {
            ports: ports.into_iter().filter(|p| *p != 0).collect(),
            ..Self::default()
        }
    }

    /// Whether any ports are configured.
    pub fn is_enabled(&self) -> bool {
        !self.ports.is_empty()
    }

    /// The configured ports.
    pub fn ports(&self) -> &BTreeSet<u16> {
        &self.ports
    }

    /// Replace the configured ports. Reservations over ports that are no
    /// longer in the pool keep them until they lapse; nothing new is handed
    /// out from outside it.
    pub fn set_ports(&mut self, ports: impl IntoIterator<Item = u16>) {
        self.ports = ports.into_iter().filter(|p| *p != 0).collect();
        self.blocked.retain(|p| self.ports.contains(p));
    }

    /// Replace the pool's reservations with persisted ones.
    pub fn load(&mut self, reservations: &[PortReservation]) {
        self.reservations = reservations.iter().map(Reservation::from_api).collect();
    }

    /// Every reservation, lapsed ones included, for persistence.
    pub fn snapshot(&self) -> Vec<PortReservation> {
        self.reservations.iter().map(Reservation::to_api).collect()
    }

    /// All live reservations.
    pub fn list(&self, now: DateTime<Utc>) -> Result<Vec<PortReservation>, PortPoolError> {
        self.require_enabled()?;
        Ok(self
            .reservations
            .iter()
            .filter(|r| r.is_live(now))
            .map(Reservation::to_api)
            .collect())
    }

    /// One live reservation.
    pub fn get(&self, id: Uuid, now: DateTime<Utc>) -> Result<PortReservation, PortPoolError> {
        self.require_enabled()?;
        self.reservations
            .iter()
            .find(|r| r.id == id && r.is_live(now))
            .map(Reservation::to_api)
            .ok_or(PortPoolError::NotFound)
    }

    fn require_enabled(&self) -> Result<(), PortPoolError> {
        if self.ports.is_empty() {
            return Err(PortPoolError::NotConfigured);
        }
        Ok(())
    }

    /// Drop associations whose flow is gone, then release what lapsed
    /// reservations no longer need.
    ///
    /// A lapsed reservation keeps exactly the ports a still-existing flow uses
    /// (invariant 4) and is dropped once none are left. Run it against the
    /// current flow list — at startup, and before reading or allocating from
    /// the pool — so no hook in the flow lifecycle is needed.
    pub fn reconcile(&mut self, live_flows: &HashSet<FlowId>, now: DateTime<Utc>) {
        for reservation in &mut self.reservations {
            reservation
                .in_use
                .retain(|_, flow_id| live_flows.contains(flow_id));
            if !reservation.is_live(now) {
                // Invariant 4: only the idle ports go back.
                let held: BTreeSet<u16> = reservation.in_use.keys().copied().collect();
                reservation.ports = held;
            }
        }
        self.reservations
            .retain(|r| r.is_live(now) || !r.ports.is_empty());
    }

    /// Grant or grow the reservation for `owner_id`.
    ///
    /// `probe` answers whether a candidate port can be handed out; it is
    /// called only for ports the pool believes are free, since a port one of
    /// its own reservations holds is spoken for whatever a bind would say.
    /// Pass `|_| true` where there is nothing to probe.
    ///
    /// An owner that already holds a reservation gets it back renewed, with
    /// its existing ports untouched (invariant 2). A larger `count` adds
    /// ports, preferring ones that continue the block it already has. A
    /// smaller or equal `count` changes nothing but the expiry — ports are
    /// given back by deleting the reservation, not by shrinking it.
    pub fn reserve(
        &mut self,
        owner_id: &str,
        count: u16,
        ttl_secs: Option<u64>,
        default_ttl: u64,
        now: DateTime<Utc>,
        probe: &mut dyn FnMut(u16) -> bool,
    ) -> Result<ReservationOutcome, PortPoolError> {
        self.require_enabled()?;
        let owner_id = owner_id.trim();
        if owner_id.is_empty() {
            return Err(PortPoolError::EmptyOwnerId);
        }
        if count == 0 || count > MAX_RESERVATION_PORTS {
            return Err(PortPoolError::BadCount(count));
        }
        let ttl = Self::ttl(ttl_secs, default_ttl)?;

        let existing = self
            .reservations
            .iter()
            .position(|r| r.owner_id == owner_id);
        let held = existing.map_or(0, |i| self.reservations[i].ports.len());
        let wanted = usize::from(count).saturating_sub(held);

        let mut newly_blocked = Vec::new();
        let grow_from =
            existing.and_then(|i| self.reservations[i].ports.iter().next_back().copied());
        let fresh = if wanted > 0 {
            self.take_free(wanted, grow_from, probe, &mut newly_blocked)?
        } else {
            BTreeSet::new()
        };

        let index = match existing {
            Some(i) => {
                let r = &mut self.reservations[i];
                r.ports.extend(fresh);
                r.expires_at = now + ttl;
                i
            }
            None => {
                self.reservations.push(Reservation {
                    id: Uuid::new_v4(),
                    owner_id: owner_id.to_string(),
                    ports: fresh,
                    created_at: now,
                    expires_at: now + ttl,
                    in_use: BTreeMap::new(),
                });
                self.reservations.len() - 1
            }
        };
        Ok(ReservationOutcome {
            reservation: self.reservations[index].to_api(),
            how: if existing.is_some() {
                Reserved::Renewed
            } else {
                Reserved::Created
            },
            newly_blocked,
        })
    }

    /// Push a reservation's expiry out to `now + ttl`.
    pub fn renew(
        &mut self,
        id: Uuid,
        ttl_secs: Option<u64>,
        default_ttl: u64,
        now: DateTime<Utc>,
    ) -> Result<PortReservation, PortPoolError> {
        self.require_enabled()?;
        let ttl = Self::ttl(ttl_secs, default_ttl)?;
        let reservation = self
            .reservations
            .iter_mut()
            .find(|r| r.id == id && r.is_live(now))
            .ok_or(PortPoolError::NotFound)?;
        reservation.expires_at = now + ttl;
        Ok(reservation.to_api())
    }

    /// Give a reservation back.
    ///
    /// Ports a flow still uses stay held, on the same rule as expiry: a port
    /// returns to the pool only when no live reservation and no existing flow
    /// holds it. Run `reconcile` first so the associations are current.
    pub fn release(&mut self, id: Uuid, now: DateTime<Utc>) -> Result<(), PortPoolError> {
        self.require_enabled()?;
        let index = self
            .reservations
            .iter()
            .position(|r| r.id == id && r.is_live(now))
            .ok_or(PortPoolError::NotFound)?;
        // Expire it now, then let the shared rule decide what it keeps.
        self.reservations[index].expires_at = now;
        let held: BTreeSet<u16> = self.reservations[index].in_use.keys().copied().collect();
        if held.is_empty() {
            self.reservations.remove(index);
        } else {
            self.reservations[index].ports = held;
        }
        Ok(())
    }

    /// Record which of a reservation's ports a flow uses, replacing whatever
    /// was recorded for that flow.
    pub fn assign(
        &mut self,
        id: Uuid,
        flow_id: FlowId,
        ports: &[u16],
        now: DateTime<Utc>,
    ) -> Result<PortReservation, PortPoolError> {
        self.require_enabled()?;
        let reservation = self
            .reservations
            .iter_mut()
            .find(|r| r.id == id && r.is_live(now))
            .ok_or(PortPoolError::NotFound)?;
        if let Some(stray) = ports.iter().find(|p| !reservation.ports.contains(p)) {
            return Err(PortPoolError::NotInReservation(*stray));
        }
        reservation.in_use.retain(|_, f| *f != flow_id);
        for port in ports {
            reservation.in_use.insert(*port, flow_id);
        }
        Ok(reservation.to_api())
    }

    /// Drop a flow's association. Its ports go back to the reservation, never
    /// to the pool (invariant 3).
    pub fn unassign(
        &mut self,
        id: Uuid,
        flow_id: FlowId,
        now: DateTime<Utc>,
    ) -> Result<(), PortPoolError> {
        self.require_enabled()?;
        let reservation = self
            .reservations
            .iter_mut()
            .find(|r| r.id == id && r.is_live(now))
            .ok_or(PortPoolError::NotFound)?;
        let before = reservation.in_use.len();
        reservation.in_use.retain(|_, f| *f != flow_id);
        if reservation.in_use.len() == before {
            return Err(PortPoolError::NotFound);
        }
        Ok(())
    }

    /// The pool and everything in it that is not free. Answers on an
    /// unconfigured server too — saying so is the point of it.
    pub fn status(&self, now: DateTime<Utc>) -> PortPoolStatus {
        if self.ports.is_empty() {
            return PortPoolStatus::disabled();
        }
        let mut entries: BTreeMap<u16, PortEntry> = BTreeMap::new();
        for port in &self.blocked {
            entries.insert(
                *port,
                PortEntry {
                    port: *port,
                    state: PortState::Blocked,
                    owner_id: None,
                    reservation_id: None,
                    flow_id: None,
                },
            );
        }
        for reservation in self.reservations.iter().filter(|r| r.is_live(now)) {
            for port in &reservation.ports {
                let flow_id = reservation.in_use.get(port).copied();
                entries.insert(
                    *port,
                    PortEntry {
                        port: *port,
                        state: if flow_id.is_some() {
                            PortState::Assigned
                        } else {
                            PortState::Reserved
                        },
                        owner_id: Some(reservation.owner_id.clone()),
                        reservation_id: Some(reservation.id),
                        flow_id,
                    },
                );
            }
        }
        // A lapsed reservation still holding ports for a live flow keeps them
        // out of the pool, so they are not free and have to show up as such.
        for reservation in self.reservations.iter().filter(|r| !r.is_live(now)) {
            for (port, flow_id) in &reservation.in_use {
                entries.entry(*port).or_insert_with(|| PortEntry {
                    port: *port,
                    state: PortState::Assigned,
                    owner_id: Some(reservation.owner_id.clone()),
                    reservation_id: Some(reservation.id),
                    flow_id: Some(*flow_id),
                });
            }
        }
        let taken = entries.keys().filter(|p| self.ports.contains(p)).count() as u32;
        PortPoolStatus {
            enabled: true,
            ports: spans(self.ports.iter().copied()),
            total: self.ports.len() as u32,
            free: self.ports.len() as u32 - taken,
            entries: entries.into_values().collect(),
        }
    }

    fn ttl(ttl_secs: Option<u64>, default_ttl: u64) -> Result<Duration, PortPoolError> {
        let secs = ttl_secs.unwrap_or(default_ttl);
        if secs == 0 || secs > MAX_PORT_LEASE_TTL_SECS {
            return Err(PortPoolError::BadTtl(secs));
        }
        Ok(Duration::seconds(secs as i64))
    }

    /// Ports no reservation holds.
    fn free_ports(&self) -> BTreeSet<u16> {
        let taken: BTreeSet<u16> = self
            .reservations
            .iter()
            .flat_map(|r| r.ports.iter().copied())
            .collect();
        self.ports.difference(&taken).copied().collect()
    }

    /// Take `wanted` free ports, probing each candidate.
    ///
    /// Candidate order is what "prefers contiguous" means: the lowest run long
    /// enough, then — when growing — the ports that continue the owner's
    /// existing block, then anything free. Nothing is guaranteed, because a
    /// probe can knock a hole in any of it.
    fn take_free(
        &mut self,
        wanted: usize,
        grow_from: Option<u16>,
        probe: &mut dyn FnMut(u16) -> bool,
        newly_blocked: &mut Vec<u16>,
    ) -> Result<BTreeSet<u16>, PortPoolError> {
        // Blocked ports stay candidates: `blocked` records what the last probe
        // found, not a decision. Whatever held the number may be gone, and
        // re-probing is how it comes back — which is also why the caller logs
        // the transition rather than every probe.
        let free: Vec<u16> = self.free_ports().into_iter().collect();
        let candidates = order_candidates(&free, wanted, grow_from);

        let mut taken = BTreeSet::new();
        for port in candidates {
            if taken.len() == wanted {
                break;
            }
            if probe(port) {
                if self.blocked.remove(&port) {
                    // It came back; it is a normal free port again.
                }
                taken.insert(port);
            } else if self.blocked.insert(port) {
                newly_blocked.push(port);
            }
        }
        if taken.len() < wanted {
            // Nothing is committed: a failed request leaves the owner's
            // existing ports exactly as they were.
            return Err(PortPoolError::Exhausted {
                requested: wanted as u16,
                available: taken.len() as u32,
            });
        }
        Ok(taken)
    }
}

/// Order free ports so that a contiguous result is preferred.
///
/// Pure and separate so the preference is testable on its own: the lowest run
/// of `wanted` consecutive ports first, then ports continuing `grow_from`,
/// then the rest ascending. Every free port appears exactly once, so the
/// caller always gets an answer if enough of them survive probing.
fn order_candidates(free: &[u16], wanted: usize, grow_from: Option<u16>) -> Vec<u16> {
    let mut preferred: Vec<u16> = Vec::new();
    if let Some(last) = grow_from {
        // Growth: the ports immediately after what the owner already holds.
        let mut next = last;
        for port in free {
            if *port == next.saturating_add(1) {
                preferred.push(*port);
                next = *port;
            } else if *port > next {
                break;
            }
        }
    }
    if preferred.len() < wanted {
        // A fresh block: the lowest run long enough, else the lowest ports.
        if let Some(start) = lowest_run(free, wanted) {
            for port in free.iter().skip(start).take(wanted) {
                if !preferred.contains(port) {
                    preferred.push(*port);
                }
            }
        }
    }
    for port in free {
        if !preferred.contains(port) {
            preferred.push(*port);
        }
    }
    preferred
}

/// Index into `free` where the lowest run of `wanted` consecutive ports starts.
fn lowest_run(free: &[u16], wanted: usize) -> Option<usize> {
    if wanted == 0 || free.len() < wanted {
        return None;
    }
    let mut start = 0usize;
    for i in 1..=free.len() {
        if i < free.len() && free[i] == free[i - 1] + 1 {
            continue;
        }
        if i - start >= wanted {
            return Some(start);
        }
        start = i;
    }
    None
}

/// Whether nothing outside Strom holds `port` on this host, right now.
///
/// Handing out a number another process already holds produces exactly the
/// failure the pool exists to prevent, in its worst form: the flow is built,
/// looks correct, and then fails to start some of the time with nothing in the
/// record pointing at the cause. Binding it first turns that into a log line
/// and a visible state.
///
/// Both UDP and TCP, because the pool makes no protocol distinction and a
/// number handed out covers either use. `SO_REUSEPORT` is deliberately not
/// set: it would let the bind succeed against an occupied port and defeat the
/// whole check. `std`'s listeners do not set it, so plain binds are correct
/// here — the sockets are dropped immediately.
///
/// This is a diagnostic, not a guarantee. Something can take the number
/// between this check and the flow's own bind, and nothing in Strom can close
/// that window. It also only sees its own network namespace: under
/// `network_mode: host` that is the host's real port space and the answer
/// means something, but behind Docker bridge networking with published ports
/// it is not, and a conflict on the host stays invisible.
pub fn port_is_free_on_host(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, UdpSocket};
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    UdpSocket::bind(addr).is_ok() && TcpListener::bind(addr).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 600;

    fn pool() -> PortPool {
        PortPool::with_ports(100..=119)
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn no_flows() -> HashSet<FlowId> {
        HashSet::new()
    }

    /// Everything is free on the host unless a test says otherwise.
    fn open(_: u16) -> bool {
        true
    }

    fn reserve(p: &mut PortPool, owner: &str, count: u16, now: DateTime<Utc>) -> PortReservation {
        p.reserve(owner, count, None, TTL, now, &mut open)
            .unwrap()
            .reservation
    }

    #[test]
    fn allocates_the_lowest_contiguous_run_and_is_idempotent_per_owner() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 5, t0());
        assert_eq!(a.ports, vec![100, 101, 102, 103, 104]);
        let b = reserve(&mut p, "b", 5, t0());
        assert_eq!(b.ports, vec![105, 106, 107, 108, 109]);

        // Invariant 2: asking again returns the same ports, renewed.
        let again = p
            .reserve("a", 5, None, TTL, t0() + Duration::seconds(30), &mut open)
            .unwrap();
        assert_eq!(again.how, Reserved::Renewed);
        assert_eq!(again.reservation.id, a.id);
        assert_eq!(again.reservation.ports, a.ports);
        assert!(again.reservation.expires_at > a.expires_at);
        assert_eq!(p.list(t0()).unwrap().len(), 2);
    }

    #[test]
    fn growth_adds_ports_and_never_moves_the_ones_already_held() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 5, t0());
        // A neighbour takes the ports immediately after a's block.
        reserve(&mut p, "b", 5, t0());
        // Growing a can no longer extend in place — but a's ports must not move.
        let grown = reserve(&mut p, "a", 8, t0());
        assert_eq!(grown.id, a.id);
        assert_eq!(&grown.ports[..5], &a.ports[..]);
        assert_eq!(grown.ports, vec![100, 101, 102, 103, 104, 110, 111, 112]);

        // Growth that fits nowhere leaves the reservation exactly as it was.
        let err = p.reserve("a", 100, None, TTL, t0(), &mut open).unwrap_err();
        assert!(matches!(err, PortPoolError::Exhausted { .. }));
        assert_eq!(p.get(a.id, t0()).unwrap().ports, grown.ports);

        // A smaller count is a no-op: ports are given back by deleting.
        let same = reserve(&mut p, "a", 2, t0());
        assert_eq!(same.ports, grown.ports);
    }

    #[test]
    fn a_full_pool_is_reported_rather_than_overcommitted() {
        let mut p = pool();
        reserve(&mut p, "a", 20, t0());
        let err = p.reserve("b", 1, None, TTL, t0(), &mut open).unwrap_err();
        assert_eq!(
            err,
            PortPoolError::Exhausted {
                requested: 1,
                available: 0
            }
        );
    }

    #[test]
    fn releasing_an_association_returns_ports_to_the_reservation_not_the_pool() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 5, t0());
        let flow = FlowId::new_v4();
        let assigned = p.assign(a.id, flow, &[100, 101], t0()).unwrap();
        assert_eq!(assigned.in_use.len(), 2);

        // Invariant 3: unassigning gives them back to the owner, not the pool.
        p.unassign(a.id, flow, t0()).unwrap();
        let after = p.get(a.id, t0()).unwrap();
        assert_eq!(after.ports, a.ports);
        assert!(after.in_use.is_empty());
        // Nobody else can have them.
        let b = reserve(&mut p, "b", 5, t0());
        assert_eq!(b.ports, vec![105, 106, 107, 108, 109]);
    }

    #[test]
    fn assigning_a_port_the_reservation_does_not_hold_is_refused() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 2, t0());
        assert_eq!(
            p.assign(a.id, FlowId::new_v4(), &[100, 117], t0()),
            Err(PortPoolError::NotInReservation(117))
        );
        // Nothing was recorded: the call is all or nothing.
        assert!(p.get(a.id, t0()).unwrap().in_use.is_empty());
    }

    #[test]
    fn assigning_again_replaces_what_that_flow_had() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 4, t0());
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[100, 101], t0()).unwrap();
        let fixed = p.assign(a.id, flow, &[102], t0()).unwrap();
        assert_eq!(
            fixed.in_use,
            vec![PortInUse {
                port: 102,
                flow_id: flow
            }]
        );
    }

    #[test]
    fn expiry_frees_idle_ports_but_keeps_those_a_live_flow_uses() {
        let mut p = pool();
        let a = p
            .reserve("a", 5, Some(60), TTL, t0(), &mut open)
            .unwrap()
            .reservation;
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[100, 101], t0()).unwrap();

        let later = t0() + Duration::seconds(61);
        let live: HashSet<FlowId> = [flow].into_iter().collect();
        p.reconcile(&live, later);

        // Invariant 4: the reservation is gone, but its in-use ports are not free.
        assert!(p.list(later).unwrap().is_empty());
        let b = reserve(&mut p, "b", 5, later);
        assert_eq!(b.ports, vec![102, 103, 104, 105, 106]);
        let status = p.status(later);
        assert_eq!(status.free, 20 - 5 - 2);

        // Once the flow is gone the ports come back.
        p.reconcile(&no_flows(), later);
        let c = reserve(&mut p, "c", 2, later);
        assert_eq!(c.ports, vec![100, 101]);
    }

    #[test]
    fn deleting_a_reservation_also_keeps_ports_a_live_flow_uses() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 5, t0());
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[100], t0()).unwrap();
        p.release(a.id, t0()).unwrap();

        // A port returns to the pool only when no live reservation AND no
        // existing flow holds it.
        assert!(p.list(t0()).unwrap().is_empty());
        let b = reserve(&mut p, "b", 4, t0());
        assert_eq!(b.ports, vec![101, 102, 103, 104]);

        p.reconcile(&no_flows(), t0());
        let c = reserve(&mut p, "c", 1, t0());
        assert_eq!(c.ports, vec![100]);
    }

    #[test]
    fn reconcile_drops_associations_whose_flow_is_gone() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 3, t0());
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[100], t0()).unwrap();
        p.reconcile(&no_flows(), t0());
        let after = p.get(a.id, t0()).unwrap();
        // The association went; the ports stayed with their owner.
        assert!(after.in_use.is_empty());
        assert_eq!(after.ports, vec![100, 101, 102]);
    }

    #[test]
    fn renew_and_release() {
        let mut p = pool();
        let a = p
            .reserve("a", 5, Some(60), TTL, t0(), &mut open)
            .unwrap()
            .reservation;
        let renewed = p
            .renew(a.id, Some(120), TTL, t0() + Duration::seconds(30))
            .unwrap();
        assert_eq!(renewed.expires_at, "2026-09-23T12:02:30Z");
        assert!(p.get(a.id, t0() + Duration::seconds(149)).is_ok());
        assert_eq!(
            p.get(a.id, t0() + Duration::seconds(150)),
            Err(PortPoolError::NotFound)
        );
        assert_eq!(
            p.renew(a.id, None, TTL, t0() + Duration::seconds(150)),
            Err(PortPoolError::NotFound)
        );
    }

    #[test]
    fn a_blocked_port_is_skipped_and_reported_once() {
        let mut p = pool();
        // Something outside Strom holds 100 and 101.
        let mut probe = |port: u16| !matches!(port, 100 | 101);
        let out = p.reserve("a", 3, None, TTL, t0(), &mut probe).unwrap();
        assert_eq!(out.reservation.ports, vec![102, 103, 104]);
        assert_eq!(out.newly_blocked, vec![100, 101]);

        // The log line is for the transition: a second allocation re-probes but
        // reports nothing new.
        let out = p.reserve("b", 2, None, TTL, t0(), &mut probe).unwrap();
        assert_eq!(out.reservation.ports, vec![105, 106]);
        assert!(out.newly_blocked.is_empty());

        // And the pool shows why those ports are not free.
        let status = p.status(t0());
        let blocked: Vec<u16> = status
            .entries
            .iter()
            .filter(|e| e.state == PortState::Blocked)
            .map(|e| e.port)
            .collect();
        assert_eq!(blocked, vec![100, 101]);
        assert_eq!(status.free, 20 - 5 - 2);

        // A port that comes back is handed out again.
        let mut open_again = |_: u16| true;
        let out = p.reserve("c", 20, None, TTL, t0(), &mut open_again);
        assert!(out.is_err(), "pool is too small for 20 after a and b");
        let out = p.reserve("c", 2, None, TTL, t0(), &mut open_again).unwrap();
        assert_eq!(out.reservation.ports, vec![100, 101]);
    }

    #[test]
    fn every_route_refuses_until_ports_are_configured() {
        let mut p = PortPool::new();
        assert!(!p.is_enabled());
        let id = Uuid::new_v4();
        assert_eq!(
            p.reserve("a", 5, None, TTL, t0(), &mut open).unwrap_err(),
            PortPoolError::NotConfigured
        );
        assert_eq!(p.list(t0()), Err(PortPoolError::NotConfigured));
        assert_eq!(p.get(id, t0()), Err(PortPoolError::NotConfigured));
        assert_eq!(
            p.renew(id, None, TTL, t0()),
            Err(PortPoolError::NotConfigured)
        );
        assert_eq!(p.release(id, t0()), Err(PortPoolError::NotConfigured));
        assert_eq!(
            p.assign(id, FlowId::new_v4(), &[100], t0()),
            Err(PortPoolError::NotConfigured)
        );
        assert_eq!(
            p.unassign(id, FlowId::new_v4(), t0()),
            Err(PortPoolError::NotConfigured)
        );
        // The pool endpoint still answers, and says why.
        assert_eq!(p.status(t0()), PortPoolStatus::disabled());

        p.set_ports(100..=119);
        assert_eq!(reserve(&mut p, "a", 2, t0()).ports, vec![100, 101]);
        assert!(p.status(t0()).enabled);
    }

    #[test]
    fn validates_input() {
        let mut p = pool();
        assert_eq!(
            p.reserve("  ", 1, None, TTL, t0(), &mut open).unwrap_err(),
            PortPoolError::EmptyOwnerId
        );
        assert_eq!(
            p.reserve("a", 0, None, TTL, t0(), &mut open).unwrap_err(),
            PortPoolError::BadCount(0)
        );
        assert_eq!(
            p.reserve("a", 1, Some(0), TTL, t0(), &mut open)
                .unwrap_err(),
            PortPoolError::BadTtl(0)
        );
        assert_eq!(
            p.reserve(
                "a",
                1,
                Some(MAX_PORT_LEASE_TTL_SECS + 1),
                TTL,
                t0(),
                &mut open
            )
            .unwrap_err(),
            PortPoolError::BadTtl(MAX_PORT_LEASE_TTL_SECS + 1)
        );
    }

    #[test]
    fn a_pool_with_holes_hands_out_what_it_has() {
        // 100-104 and 110-114: the operator left the middle to something else.
        let mut p = PortPool::with_ports((100..=104).chain(110..=114));
        let a = reserve(&mut p, "a", 7, t0());
        // No run of 7 exists, so it falls back to the lowest free ports —
        // and the result is not contiguous, which callers must tolerate.
        assert_eq!(a.ports, vec![100, 101, 102, 103, 104, 110, 111]);
        assert_eq!(p.status(t0()).total, 10);
        assert_eq!(
            p.status(t0()).ports,
            vec![
                strom_types::ports::PortSpan {
                    first: 100,
                    last: 104
                },
                strom_types::ports::PortSpan {
                    first: 110,
                    last: 114
                },
            ]
        );
    }

    #[test]
    fn survives_a_persistence_round_trip() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 5, t0());
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[100], t0()).unwrap();
        let saved = p.snapshot();

        let mut restored = PortPool::with_ports(100..=119);
        restored.load(&saved);
        assert_eq!(
            restored.get(a.id, t0()).unwrap(),
            p.get(a.id, t0()).unwrap()
        );
        // The same owner after a restart gets the same ports back.
        let again = restored
            .reserve("a", 5, None, TTL, t0(), &mut open)
            .unwrap();
        assert_eq!(again.how, Reserved::Renewed);
        assert_eq!(again.reservation.ports, a.ports);
    }

    #[test]
    fn the_status_view_lists_only_ports_that_are_not_free() {
        let mut p = pool();
        let a = reserve(&mut p, "a", 3, t0());
        let flow = FlowId::new_v4();
        p.assign(a.id, flow, &[101], t0()).unwrap();
        let status = p.status(t0());
        assert_eq!(status.total, 20);
        assert_eq!(status.free, 17);
        assert_eq!(status.entries.len(), 3, "17 idle ports must not be listed");
        assert_eq!(status.entries[0].state, PortState::Reserved);
        assert_eq!(status.entries[1].state, PortState::Assigned);
        assert_eq!(status.entries[1].flow_id, Some(flow));
        assert_eq!(status.entries[1].owner_id.as_deref(), Some("a"));
    }

    #[test]
    fn lowest_run_finds_the_first_long_enough_stretch() {
        //            0    1    2      3    4    5    6
        let free = [10, 11, 20, 21, 22, 23, 30];
        assert_eq!(lowest_run(&free, 2), Some(0));
        assert_eq!(lowest_run(&free, 3), Some(2));
        assert_eq!(lowest_run(&free, 4), Some(2));
        assert_eq!(lowest_run(&free, 5), None);
        assert_eq!(lowest_run(&free, 0), None);
        assert_eq!(lowest_run(&[], 1), None);
    }
}
