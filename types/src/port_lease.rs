//! Port leases: contiguous UDP port ranges handed out to API clients.
//!
//! A Strom shared by several orchestrators (for example, several Open Live
//! instances) is the only place that knows which SRT listener ports are
//! already spoken for. Each client asks for a block of ports under a stable
//! `client_id`, renews it while it is alive, and lets it expire when it is
//! gone. The server never binds the ports itself; a lease is a promise not to
//! hand the same block to anyone else.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// First port of the pool leases are cut from when nothing else is configured.
pub const DEFAULT_PORT_LEASE_RANGE_FIRST: u16 = 47100;
/// Last port of the default pool.
pub const DEFAULT_PORT_LEASE_RANGE_LAST: u16 = 47999;
/// How long a lease lives when the client does not say.
pub const DEFAULT_PORT_LEASE_TTL_SECS: u64 = 600;
/// Longest lifetime a client may ask for in one request or renewal.
pub const MAX_PORT_LEASE_TTL_SECS: u64 = 86_400;
/// Largest block one lease may cover.
pub const MAX_PORT_LEASE_SIZE: u16 = 1000;

/// An inclusive range of ports, written `first-last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortRange {
    /// Lowest port in the range.
    pub first: u16,
    /// Highest port in the range, inclusive.
    pub last: u16,
}

impl PortRange {
    /// Build a range, refusing port 0 and an inverted pair.
    pub fn new(first: u16, last: u16) -> Result<Self, PortRangeError> {
        if first == 0 {
            return Err(PortRangeError::Zero);
        }
        if first > last {
            return Err(PortRangeError::Inverted { first, last });
        }
        Ok(Self { first, last })
    }

    /// Number of ports in the range.
    pub fn len(&self) -> u32 {
        u32::from(self.last) - u32::from(self.first) + 1
    }

    /// A range always holds at least one port; this exists for clippy's sake.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `port` lies inside the range.
    pub fn contains(&self, port: u16) -> bool {
        self.first <= port && port <= self.last
    }

    /// Whether the two ranges share at least one port.
    pub fn overlaps(&self, other: &PortRange) -> bool {
        self.first <= other.last && other.first <= self.last
    }

    /// Every port in the range, ascending.
    pub fn iter(&self) -> impl Iterator<Item = u16> {
        self.first..=self.last
    }
}

impl Default for PortRange {
    fn default() -> Self {
        Self {
            first: DEFAULT_PORT_LEASE_RANGE_FIRST,
            last: DEFAULT_PORT_LEASE_RANGE_LAST,
        }
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// Why a `first-last` string is not a port range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortRangeError {
    /// The text is not two numbers joined by a dash.
    Syntax(String),
    /// A bound is not a port number.
    NotAPort(String),
    /// Port 0 cannot be listened on.
    Zero,
    /// The first port is above the last.
    Inverted { first: u16, last: u16 },
}

impl fmt::Display for PortRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(s) => write!(f, "expected a port range like 47100-47999, got '{s}'"),
            Self::NotAPort(s) => write!(f, "'{s}' is not a port number (1-65535)"),
            Self::Zero => write!(f, "port 0 cannot be part of a port range"),
            Self::Inverted { first, last } => {
                write!(f, "port range {first}-{last} runs backwards")
            }
        }
    }
}

impl std::error::Error for PortRangeError {}

impl FromStr for PortRange {
    type Err = PortRangeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (first, last) = s
            .split_once('-')
            .ok_or_else(|| PortRangeError::Syntax(s.to_string()))?;
        let parse = |p: &str| {
            p.trim()
                .parse::<u16>()
                .map_err(|_| PortRangeError::NotAPort(p.trim().to_string()))
        };
        Self::new(parse(first)?, parse(last)?)
    }
}

/// A block of ports reserved for one client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortLease {
    /// Lease identifier, used to renew and release it.
    pub id: Uuid,
    /// The client's stable name. One lease per client; asking again returns it.
    pub client_id: String,
    /// Lowest leased port.
    pub first_port: u16,
    /// Highest leased port, inclusive.
    pub last_port: u16,
    /// When the lease was first granted (RFC 3339, UTC).
    pub created_at: String,
    /// When the lease lapses unless renewed (RFC 3339, UTC).
    pub expires_at: String,
}

impl PortLease {
    /// The leased ports as a range.
    pub fn range(&self) -> PortRange {
        PortRange {
            first: self.first_port,
            last: self.last_port,
        }
    }
}

/// Ask for a block of ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortLeaseRequest {
    /// Stable name of the requesting client, such as an instance hostname.
    /// A repeat request under the same name renews and returns the existing
    /// lease instead of allocating a second one.
    pub client_id: String,
    /// Number of consecutive ports wanted.
    pub size: u16,
    /// Lifetime in seconds. Defaults to `DEFAULT_PORT_LEASE_TTL_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Extend a lease's lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RenewPortLeaseRequest {
    /// New lifetime in seconds, counted from now. Defaults to
    /// `DEFAULT_PORT_LEASE_TTL_SECS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_range() {
        let r: PortRange = "47100-47999".parse().unwrap();
        assert_eq!(r, PortRange::new(47100, 47999).unwrap());
        assert_eq!(r.len(), 900);
        assert_eq!(r.to_string(), "47100-47999");
        assert_eq!(" 10 - 12 ".parse::<PortRange>().unwrap().len(), 3);
    }

    #[test]
    fn rejects_bad_ranges() {
        assert_eq!(
            "47100".parse::<PortRange>(),
            Err(PortRangeError::Syntax("47100".into()))
        );
        assert_eq!(
            "a-b".parse::<PortRange>(),
            Err(PortRangeError::NotAPort("a".into()))
        );
        assert_eq!("0-10".parse::<PortRange>(), Err(PortRangeError::Zero));
        assert_eq!(
            "20-10".parse::<PortRange>(),
            Err(PortRangeError::Inverted {
                first: 20,
                last: 10
            })
        );
        assert!("1-70000".parse::<PortRange>().is_err());
    }

    #[test]
    fn overlap_and_contains() {
        let a = PortRange::new(10, 20).unwrap();
        let b = PortRange::new(20, 30).unwrap();
        let c = PortRange::new(21, 30).unwrap();
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
        assert!(a.contains(10) && a.contains(20) && !a.contains(21));
    }

    #[test]
    fn request_ttl_is_optional_in_json() {
        let req: PortLeaseRequest = serde_json::from_str(r#"{"client_id":"a","size":20}"#).unwrap();
        assert_eq!(req.ttl_secs, None);
        assert_eq!(req.size, 20);
    }
}
