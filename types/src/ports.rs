//! Port pool: port numbers a Strom administers and hands out.
//!
//! Nothing in Strom can otherwise answer "which ports may I use?". A caller
//! that builds flows over the API has to guess, and two flows that bind the
//! same UDP port fail only at start, with an error that says nothing about the
//! collision. The pool is a general resource allocator: Strom administers a
//! set of port numbers and hands them out, knowing nothing about who is asking
//! or what they do with the numbers.
//!
//! The scope is port *numbers*. There is no protocol distinction, no binding to
//! blocks, and no inspection of what a flow actually opens — a number is handed
//! out once and covers both UDP and TCP use. What a reservation's ports are
//! used for is the owner's business; it may tell the pool through an
//! association, which is bookkeeping and a safety net, not a lifetime.
//!
//! Three levels, each with its own lifetime:
//!
//! - **Pool** — the configured port numbers. Static.
//! - **Reservation** — ports held by an `owner_id` under a renewable TTL.
//! - **Association** — optional: which of a reservation's ports a given flow
//!   uses. A port returns to the pool only when no live reservation *and* no
//!   existing flow holds it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

use crate::flow::FlowId;

/// How long a reservation lives when neither the client nor the operator says.
pub const DEFAULT_PORT_LEASE_TTL_SECS: u64 = 600;
/// Longest lifetime that may be asked for in one request or renewal.
pub const MAX_PORT_LEASE_TTL_SECS: u64 = 86_400;
/// Most ports one reservation may hold.
pub const MAX_RESERVATION_PORTS: u16 = 1000;

/// Ports held by one owner.
///
/// `ports` is an explicit list, ascending. Allocation prefers a contiguous run
/// and growth prefers extending the existing one, but neither is guaranteed —
/// a port the operator left out of the pool, one another owner holds, or one
/// found blocked can put a hole anywhere. Callers must not assume contiguity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortReservation {
    /// Reservation identifier, used to renew, release and assign.
    pub id: Uuid,
    /// The owner's stable name. One reservation per owner; asking again
    /// returns it.
    pub owner_id: String,
    /// The ports held, ascending.
    pub ports: Vec<u16>,
    /// When the reservation was first granted (RFC 3339, UTC).
    pub created_at: String,
    /// When it lapses unless renewed (RFC 3339, UTC).
    pub expires_at: String,
    /// Which of `ports` the owner has told the pool are used by a flow.
    #[serde(default)]
    pub in_use: Vec<PortInUse>,
}

/// One of a reservation's ports, and the flow using it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortInUse {
    /// The port.
    pub port: u16,
    /// The flow the owner says uses it.
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
}

/// Ask for ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct CreateReservationRequest {
    /// Stable name of the owner, such as a production or instance id. A repeat
    /// request under the same name renews and returns the existing
    /// reservation rather than allocating a second one.
    pub owner_id: String,
    /// How many ports the owner wants to hold in total. Larger than it holds
    /// now adds ports; equal or smaller leaves them alone.
    pub count: u16,
    /// Lifetime in seconds. Defaults to the server's configured TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Extend a reservation's lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct RenewReservationRequest {
    /// New lifetime in seconds, counted from now. Defaults to the server's
    /// configured TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// Tell the pool which of a reservation's ports a flow uses.
///
/// Replaces whatever was recorded for that flow, so a caller can correct
/// itself by assigning again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct AssignPortsRequest {
    /// The flow using them.
    #[cfg_attr(feature = "openapi", schema(value_type = String, format = Uuid))]
    pub flow_id: FlowId,
    /// Ports, all of which must belong to this reservation.
    pub ports: Vec<u16>,
}

/// What a port is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum PortState {
    /// Held by a reservation, with no flow declared against it.
    Reserved,
    /// Held by a reservation and declared in use by a flow.
    Assigned,
    /// Something outside Strom holds it: a bind probe failed. Not handed out.
    Blocked,
}

/// A port that is not free, and who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortEntry {
    /// The port.
    pub port: u16,
    /// What it is doing.
    pub state: PortState,
    /// The owner holding it, when a reservation does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<String>,
    /// The reservation holding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reservation_id: Option<Uuid>,
    /// The flow declared against it, when the state is `assigned`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>, format = Uuid))]
    pub flow_id: Option<FlowId>,
}

/// A run of consecutive ports, for reading a pool back compactly.
///
/// A display shape only: the pool is a set, and nothing in the model depends
/// on ports being contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortSpan {
    /// Lowest port in the run.
    pub first: u16,
    /// Highest port in the run, inclusive.
    pub last: u16,
}

/// The pool, and everything in it that is not free.
///
/// `entries` lists only ports that are reserved, assigned or blocked, so the
/// response is proportional to what is interesting rather than to pool size —
/// a pool of nine hundred idle ports answers with an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct PortPoolStatus {
    /// Whether any ports are configured. Every reservation route answers 503
    /// while this is `false`.
    pub enabled: bool,
    /// The configured ports, as runs, ascending.
    pub ports: Vec<PortSpan>,
    /// How many ports the pool holds.
    pub total: u32,
    /// How many no reservation holds and no probe has blocked.
    pub free: u32,
    /// Every port that is not free.
    pub entries: Vec<PortEntry>,
}

impl PortPoolStatus {
    /// The answer for a server with no ports configured.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ports: Vec::new(),
            total: 0,
            free: 0,
            entries: Vec::new(),
        }
    }
}

/// Compact an ascending set of ports into runs.
pub fn spans(ports: impl IntoIterator<Item = u16>) -> Vec<PortSpan> {
    let mut out: Vec<PortSpan> = Vec::new();
    for port in ports {
        match out.last_mut() {
            Some(span) if span.last.checked_add(1) == Some(port) => span.last = port,
            _ => out.push(PortSpan {
                first: port,
                last: port,
            }),
        }
    }
    out
}

/// Why a pool specification is not a set of ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortSpecError {
    /// A bound is not a port number.
    NotAPort(String),
    /// Port 0 cannot be listened on.
    Zero,
    /// The first port is above the last.
    Inverted { first: u16, last: u16 },
}

impl std::fmt::Display for PortSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAPort(s) => write!(f, "'{s}' is not a port number (1-65535)"),
            Self::Zero => write!(f, "port 0 cannot be part of a port pool"),
            Self::Inverted { first, last } => write!(f, "port range {first}-{last} runs backwards"),
        }
    }
}

impl std::error::Error for PortSpecError {}

/// Expand one pool entry — `"47100-47199"` or `"47250"` — into its ports.
///
/// A range is notation, not a concept: it is expanded here and the pool is a
/// set from then on, which is what lets an operator punch a hole in one by
/// listing the pieces around it.
pub fn parse_port_spec(spec: &str) -> Result<Vec<u16>, PortSpecError> {
    let spec = spec.trim();
    let port = |p: &str| -> Result<u16, PortSpecError> {
        let p = p.trim();
        let n: u16 = p
            .parse()
            .map_err(|_| PortSpecError::NotAPort(p.to_string()))?;
        if n == 0 {
            return Err(PortSpecError::Zero);
        }
        Ok(n)
    };
    match spec.split_once('-') {
        Some((first, last)) => {
            let (first, last) = (port(first)?, port(last)?);
            if first > last {
                return Err(PortSpecError::Inverted { first, last });
            }
            Ok((first..=last).collect())
        }
        None => Ok(vec![port(spec)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_and_single_ports() {
        assert_eq!(
            parse_port_spec("47100-47102").unwrap(),
            vec![47100, 47101, 47102]
        );
        assert_eq!(parse_port_spec("47250").unwrap(), vec![47250]);
        assert_eq!(parse_port_spec(" 10 - 12 ").unwrap(), vec![10, 11, 12]);
        // A one-port range is the same as the port.
        assert_eq!(parse_port_spec("47150-47150").unwrap(), vec![47150]);
    }

    #[test]
    fn rejects_bad_specs() {
        assert_eq!(
            parse_port_spec("a"),
            Err(PortSpecError::NotAPort("a".into()))
        );
        assert_eq!(parse_port_spec("0"), Err(PortSpecError::Zero));
        assert_eq!(parse_port_spec("0-10"), Err(PortSpecError::Zero));
        assert_eq!(
            parse_port_spec("20-10"),
            Err(PortSpecError::Inverted {
                first: 20,
                last: 10
            })
        );
        assert!(parse_port_spec("1-70000").is_err());
    }

    #[test]
    fn spans_compact_runs_and_keep_holes() {
        assert_eq!(
            spans([47100, 47101, 47102, 47250, 47300, 47301]),
            vec![
                PortSpan {
                    first: 47100,
                    last: 47102
                },
                PortSpan {
                    first: 47250,
                    last: 47250
                },
                PortSpan {
                    first: 47300,
                    last: 47301
                },
            ]
        );
        assert!(spans([]).is_empty());
        assert_eq!(
            spans([65535]),
            vec![PortSpan {
                first: 65535,
                last: 65535
            }]
        );
    }

    #[test]
    fn a_disabled_pool_reports_nothing_available() {
        let p = PortPoolStatus::disabled();
        assert!(!p.enabled);
        assert_eq!((p.total, p.free), (0, 0));
        assert!(p.ports.is_empty() && p.entries.is_empty());
    }

    #[test]
    fn request_ttl_is_optional_in_json() {
        let req: CreateReservationRequest =
            serde_json::from_str(r#"{"owner_id":"a","count":10}"#).unwrap();
        assert_eq!(req.ttl_secs, None);
        assert_eq!(req.count, 10);
    }
}
