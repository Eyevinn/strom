//! Port lease allocation.
//!
//! The pool is a single configured [`PortRange`]. A lease is the lowest block
//! of consecutive free ports that fits the request, where "free" means not
//! covered by a live lease and not bound by an SRT listener in any flow this
//! Strom knows about. The second condition is the safety net: a lease can
//! lapse while the flows that were built on it keep running, and those ports
//! must not be handed to anyone else until the flows are gone.
//!
//! Leases are keyed by the client's stable `client_id`. Asking again under the
//! same name renews the existing lease rather than allocating another block,
//! so a client that restarts keeps the ports its peers already point at.

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use std::collections::BTreeSet;
use strom_types::port_lease::{
    PortLease, PortRange, DEFAULT_PORT_LEASE_TTL_SECS, MAX_PORT_LEASE_SIZE, MAX_PORT_LEASE_TTL_SECS,
};
use uuid::Uuid;

/// Why a lease could not be granted or changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortLeaseError {
    #[error("client_id must not be empty")]
    EmptyClientId,
    #[error("size must be between 1 and {MAX_PORT_LEASE_SIZE}, got {0}")]
    BadSize(u16),
    #[error("ttl_secs must be between 1 and {MAX_PORT_LEASE_TTL_SECS}, got {0}")]
    BadTtl(u64),
    #[error("no block of {size} free ports left in the lease pool {pool}")]
    Exhausted { size: u16, pool: PortRange },
    #[error("port lease not found")]
    NotFound,
}

/// One granted lease, with parsed timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub id: Uuid,
    pub client_id: String,
    pub range: PortRange,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Lease {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }

    /// The API view of the lease.
    pub fn to_api(&self) -> PortLease {
        PortLease {
            id: self.id,
            client_id: self.client_id.clone(),
            first_port: self.range.first,
            last_port: self.range.last,
            created_at: rfc3339(self.created_at),
            expires_at: rfc3339(self.expires_at),
        }
    }

    /// Rebuild from a persisted record. Unparseable timestamps are treated as
    /// already expired, which is the safe direction: the block is reclaimed
    /// (unless flows still bind it) instead of being reserved forever.
    pub fn from_api(lease: &PortLease) -> Self {
        let parse = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        };
        Self {
            id: lease.id,
            client_id: lease.client_id.clone(),
            range: lease.range(),
            created_at: parse(&lease.created_at),
            expires_at: parse(&lease.expires_at),
        }
    }
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The lease table for one pool. Pure: every operation takes `now` and the
/// set of ports flows currently bind, so it can be exercised without a clock
/// or a pipeline. Persistence and locking live in `AppState`.
#[derive(Debug, Clone)]
pub struct PortLeaseTable {
    pool: PortRange,
    leases: Vec<Lease>,
}

/// What `acquire` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquired {
    /// A new block was allocated.
    Created,
    /// The client already held a lease; it was renewed (and possibly resized).
    Renewed,
}

impl PortLeaseTable {
    pub fn new(pool: PortRange) -> Self {
        Self {
            pool,
            leases: Vec::new(),
        }
    }

    /// Replace the table's contents with persisted leases.
    pub fn load(&mut self, leases: &[PortLease]) {
        self.leases = leases.iter().map(Lease::from_api).collect();
    }

    pub fn pool(&self) -> PortRange {
        self.pool
    }

    /// Change the pool. Existing leases outside the new pool are kept until
    /// they expire; nothing new is cut from outside it.
    pub fn set_pool(&mut self, pool: PortRange) {
        self.pool = pool;
    }

    /// All live leases, in API form.
    pub fn list(&self, now: DateTime<Utc>) -> Vec<PortLease> {
        self.leases
            .iter()
            .filter(|l| !l.is_expired(now))
            .map(Lease::to_api)
            .collect()
    }

    pub fn get(&self, id: Uuid, now: DateTime<Utc>) -> Option<PortLease> {
        self.leases
            .iter()
            .find(|l| l.id == id && !l.is_expired(now))
            .map(Lease::to_api)
    }

    /// Drop lapsed leases. Returns how many went.
    pub fn purge_expired(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.leases.len();
        self.leases.retain(|l| !l.is_expired(now));
        before - self.leases.len()
    }

    /// Grant or renew a lease for `client_id`.
    ///
    /// `bound_ports` are ports some flow binds as an SRT listener; they are
    /// never allocated even when no lease covers them. A client that already
    /// holds a lease gets it back renewed. If it now asks for a different
    /// size, the block is grown in place when the ports after it are free,
    /// shrunk in place when smaller, and otherwise moved to a fresh block; if
    /// none fits, the old lease is kept and `Exhausted` is returned.
    pub fn acquire(
        &mut self,
        client_id: &str,
        size: u16,
        ttl_secs: Option<u64>,
        bound_ports: &BTreeSet<u16>,
        now: DateTime<Utc>,
    ) -> Result<(PortLease, Acquired), PortLeaseError> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(PortLeaseError::EmptyClientId);
        }
        if size == 0 || size > MAX_PORT_LEASE_SIZE {
            return Err(PortLeaseError::BadSize(size));
        }
        let ttl = Self::ttl(ttl_secs)?;
        self.purge_expired(now);

        if let Some(index) = self.leases.iter().position(|l| l.client_id == client_id) {
            let current = self.leases[index].range;
            let range = if u32::from(size) == current.len() {
                current
            } else {
                // Try to keep the first port: peers already point at it.
                let wanted_last = u32::from(current.first) + u32::from(size) - 1;
                let in_place = u16::try_from(wanted_last)
                    .ok()
                    .and_then(|last| PortRange::new(current.first, last).ok())
                    .filter(|r| {
                        self.pool.contains(r.last)
                            && r.iter()
                                .filter(|p| !current.contains(*p))
                                .all(|p| self.is_free(p, bound_ports, Some(index)))
                    });
                match in_place {
                    Some(r) => r,
                    None => self.find_block(size, bound_ports, Some(index)).ok_or(
                        PortLeaseError::Exhausted {
                            size,
                            pool: self.pool,
                        },
                    )?,
                }
            };
            let lease = &mut self.leases[index];
            lease.range = range;
            lease.expires_at = now + ttl;
            return Ok((lease.to_api(), Acquired::Renewed));
        }

        let range = self
            .find_block(size, bound_ports, None)
            .ok_or(PortLeaseError::Exhausted {
                size,
                pool: self.pool,
            })?;
        let lease = Lease {
            id: Uuid::new_v4(),
            client_id: client_id.to_string(),
            range,
            created_at: now,
            expires_at: now + ttl,
        };
        let api = lease.to_api();
        self.leases.push(lease);
        Ok((api, Acquired::Created))
    }

    /// Push a lease's expiry out to `now + ttl`.
    pub fn renew(
        &mut self,
        id: Uuid,
        ttl_secs: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<PortLease, PortLeaseError> {
        let ttl = Self::ttl(ttl_secs)?;
        self.purge_expired(now);
        let lease = self
            .leases
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(PortLeaseError::NotFound)?;
        lease.expires_at = now + ttl;
        Ok(lease.to_api())
    }

    /// Give a lease back. Expired leases count as gone.
    pub fn release(&mut self, id: Uuid, now: DateTime<Utc>) -> Result<(), PortLeaseError> {
        self.purge_expired(now);
        let before = self.leases.len();
        self.leases.retain(|l| l.id != id);
        if self.leases.len() == before {
            return Err(PortLeaseError::NotFound);
        }
        Ok(())
    }

    /// The API form of every lease, expired or not, for persistence.
    pub fn snapshot(&self) -> Vec<PortLease> {
        self.leases.iter().map(Lease::to_api).collect()
    }

    fn ttl(ttl_secs: Option<u64>) -> Result<Duration, PortLeaseError> {
        let secs = ttl_secs.unwrap_or(DEFAULT_PORT_LEASE_TTL_SECS);
        if secs == 0 || secs > MAX_PORT_LEASE_TTL_SECS {
            return Err(PortLeaseError::BadTtl(secs));
        }
        Ok(Duration::seconds(secs as i64))
    }

    /// Whether `port` is neither bound by a flow nor covered by a lease other
    /// than the one at `except`.
    fn is_free(&self, port: u16, bound_ports: &BTreeSet<u16>, except: Option<usize>) -> bool {
        if bound_ports.contains(&port) {
            return false;
        }
        !self
            .leases
            .iter()
            .enumerate()
            .any(|(i, l)| Some(i) != except && l.range.contains(port))
    }

    /// Lowest block of `size` consecutive free ports inside the pool.
    fn find_block(
        &self,
        size: u16,
        bound_ports: &BTreeSet<u16>,
        except: Option<usize>,
    ) -> Option<PortRange> {
        let size = u32::from(size);
        if size > self.pool.len() {
            return None;
        }
        let mut run_start: Option<u16> = None;
        let mut run_len: u32 = 0;
        for port in self.pool.iter() {
            if self.is_free(port, bound_ports, except) {
                if run_start.is_none() {
                    run_start = Some(port);
                    run_len = 0;
                }
                run_len += 1;
                if run_len == size {
                    let first = run_start?;
                    return PortRange::new(first, port).ok();
                }
            } else {
                run_start = None;
                run_len = 0;
            }
        }
        None
    }
}

/// The port an SRT URI listens on, if it is a listener.
///
/// Strom's SRT blocks describe their socket with a single `srt_uri` property,
/// such as `srt://:47110?mode=listener` or `srt://0.0.0.0:47110?mode=listener`.
/// Caller URIs (`srt://host:port?mode=caller`, or no mode at all, which
/// libsrt treats as caller) bind an ephemeral local port and return `None`.
pub fn srt_listener_port(uri: &str) -> Option<u16> {
    let rest = uri.trim().strip_prefix("srt://")?;
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    let is_listener = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .any(|(k, v)| k.eq_ignore_ascii_case("mode") && v.eq_ignore_ascii_case("listener"));
    if !is_listener {
        return None;
    }
    // IPv6 literals carry their own colons: take the port after the closing
    // bracket, otherwise after the last colon.
    let port_str = match authority.rfind(']') {
        Some(end) => authority[end + 1..].strip_prefix(':')?,
        None => authority.rsplit_once(':')?.1,
    };
    port_str.parse().ok().filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> PortRange {
        PortRange::new(100, 119).unwrap()
    }

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-10T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn range(l: &PortLease) -> (u16, u16) {
        (l.first_port, l.last_port)
    }

    #[test]
    fn allocates_lowest_blocks_first_and_is_idempotent_per_client() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        let (a, how) = t.acquire("a", 5, None, &none, t0()).unwrap();
        assert_eq!((range(&a), how), ((100, 104), Acquired::Created));
        let (b, _) = t.acquire("b", 5, None, &none, t0()).unwrap();
        assert_eq!(range(&b), (105, 109));
        let (a2, how) = t
            .acquire("a", 5, None, &none, t0() + Duration::seconds(30))
            .unwrap();
        assert_eq!((range(&a2), how), ((100, 104), Acquired::Renewed));
        assert_eq!(a2.id, a.id);
        assert!(a2.expires_at > a.expires_at);
        assert_eq!(t.list(t0()).len(), 2);
    }

    #[test]
    fn pool_exhaustion_is_reported() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        t.acquire("a", 15, None, &none, t0()).unwrap();
        let err = t.acquire("b", 6, None, &none, t0()).unwrap_err();
        assert_eq!(
            err,
            PortLeaseError::Exhausted {
                size: 6,
                pool: pool()
            }
        );
        // A smaller block still fits at the end.
        let (b, _) = t.acquire("b", 5, None, &none, t0()).unwrap();
        assert_eq!(range(&b), (115, 119));
    }

    #[test]
    fn ports_bound_by_flows_are_skipped_even_without_a_lease() {
        let mut t = PortLeaseTable::new(pool());
        let bound: BTreeSet<u16> = [102, 110].into_iter().collect();
        let (a, _) = t.acquire("a", 5, None, &bound, t0()).unwrap();
        assert_eq!(range(&a), (103, 107));
        let (b, _) = t.acquire("b", 5, None, &bound, t0()).unwrap();
        assert_eq!(range(&b), (111, 115));
    }

    #[test]
    fn expired_leases_are_reclaimed_but_bound_ports_are_not() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        t.acquire("a", 5, Some(60), &none, t0()).unwrap();
        let later = t0() + Duration::seconds(61);
        assert!(t.list(later).is_empty());
        // The old client's flows still listen on 100: the block moves past it.
        let bound: BTreeSet<u16> = [100].into_iter().collect();
        let (b, _) = t.acquire("b", 5, None, &bound, later).unwrap();
        assert_eq!(range(&b), (101, 105));
        // Without bound ports the whole block is reused.
        let mut t = PortLeaseTable::new(pool());
        t.acquire("a", 5, Some(60), &none, t0()).unwrap();
        let (b, _) = t.acquire("b", 5, None, &none, later).unwrap();
        assert_eq!(range(&b), (100, 104));
    }

    #[test]
    fn resize_grows_in_place_when_possible_else_moves() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        let (a, _) = t.acquire("a", 5, None, &none, t0()).unwrap();
        let (a2, how) = t.acquire("a", 8, None, &none, t0()).unwrap();
        assert_eq!((range(&a2), how), ((100, 107), Acquired::Renewed));
        assert_eq!(a2.id, a.id);
        // Shrinking keeps the first port too.
        let (a3, _) = t.acquire("a", 3, None, &none, t0()).unwrap();
        assert_eq!(range(&a3), (100, 102));
        // A neighbour blocks growth in place, so the block moves.
        t.acquire("b", 2, None, &none, t0()).unwrap(); // 103-104
        let (a4, _) = t.acquire("a", 6, None, &none, t0()).unwrap();
        assert_eq!(range(&a4), (105, 110));
        // Growth that fits nowhere keeps the old lease.
        let err = t.acquire("a", 19, None, &none, t0()).unwrap_err();
        assert!(matches!(err, PortLeaseError::Exhausted { size: 19, .. }));
        assert_eq!(range(&t.list(t0())[0]), (105, 110));
    }

    #[test]
    fn renew_and_release() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        let (a, _) = t.acquire("a", 5, Some(60), &none, t0()).unwrap();
        let renewed = t
            .renew(a.id, Some(120), t0() + Duration::seconds(30))
            .unwrap();
        assert_eq!(renewed.expires_at, "2026-09-10T12:02:30Z");
        assert!(t.get(a.id, t0() + Duration::seconds(149)).is_some());
        assert!(t.get(a.id, t0() + Duration::seconds(150)).is_none());
        assert_eq!(
            t.renew(a.id, None, t0() + Duration::seconds(150)),
            Err(PortLeaseError::NotFound)
        );
        let (b, _) = t.acquire("b", 1, None, &none, t0()).unwrap();
        t.release(b.id, t0()).unwrap();
        assert_eq!(t.release(b.id, t0()), Err(PortLeaseError::NotFound));
    }

    #[test]
    fn validates_input() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        assert_eq!(
            t.acquire("  ", 1, None, &none, t0()).unwrap_err(),
            PortLeaseError::EmptyClientId
        );
        assert_eq!(
            t.acquire("a", 0, None, &none, t0()).unwrap_err(),
            PortLeaseError::BadSize(0)
        );
        assert_eq!(
            t.acquire("a", 1, Some(0), &none, t0()).unwrap_err(),
            PortLeaseError::BadTtl(0)
        );
        assert_eq!(
            t.acquire("a", 1, Some(MAX_PORT_LEASE_TTL_SECS + 1), &none, t0())
                .unwrap_err(),
            PortLeaseError::BadTtl(MAX_PORT_LEASE_TTL_SECS + 1)
        );
    }

    #[test]
    fn survives_a_persistence_round_trip() {
        let mut t = PortLeaseTable::new(pool());
        let none = BTreeSet::new();
        let (a, _) = t.acquire("a", 5, None, &none, t0()).unwrap();
        let saved = t.snapshot();
        let mut restored = PortLeaseTable::new(pool());
        restored.load(&saved);
        assert_eq!(restored.get(a.id, t0()), Some(a.clone()));
        // Same client after a restart gets the same block back.
        let (again, how) = restored.acquire("a", 5, None, &none, t0()).unwrap();
        assert_eq!((again.id, how), (a.id, Acquired::Renewed));
    }

    #[test]
    fn listener_port_is_read_from_srt_uris() {
        assert_eq!(srt_listener_port("srt://:47110?mode=listener"), Some(47110));
        assert_eq!(
            srt_listener_port("srt://0.0.0.0:5000?mode=listener&latency=200"),
            Some(5000)
        );
        assert_eq!(
            srt_listener_port("srt://[::]:5001?latency=200&mode=Listener"),
            Some(5001)
        );
        assert_eq!(srt_listener_port("srt://192.0.2.1:5000?mode=caller"), None);
        assert_eq!(srt_listener_port("srt://192.0.2.1:5000"), None);
        assert_eq!(srt_listener_port("udp://:5000"), None);
        assert_eq!(srt_listener_port("srt://:0?mode=listener"), None);
        assert_eq!(srt_listener_port("srt://?mode=listener"), None);
    }
}
