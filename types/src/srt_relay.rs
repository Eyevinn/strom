//! SRT relay (srt-splice) support.
//!
//! An SRT block can name a stream on an SRT relay instead of an address:
//! `srt+relay://<id>[?<query>]`. When the block is built, Strom turns that into
//! a caller URI that dials the configured relay with the SRT access-control
//! streamid `#!::r=<id>,m=<role>`. The relay pairs it with the outside party,
//! which dials the relay with the plain streamid `<id>`.
//!
//! Everything here is pure string logic, shared by the backend (resolution at
//! build time) and the frontend (dial-in links).

use serde::{Deserialize, Serialize};

#[cfg(feature = "openapi")]
use utoipa::ToSchema;

/// URI scheme prefix that selects the SRT relay.
pub const SRT_RELAY_SCHEME: &str = "srt+relay://";

/// Longest stream id accepted in an `srt+relay://` URI.
pub const SRT_RELAY_MAX_ID_LEN: usize = 64;

/// Query parameters the relay path sets itself and a user may not give.
const RESERVED_PARAMS: &[&str] = &["mode", "streamid"];

/// The block's own role on the relay, as the SRT access-control syntax names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrtRelayRole {
    /// The block sends (SRT output blocks).
    Publish,
    /// The block receives (SRT input blocks).
    Request,
}

impl SrtRelayRole {
    /// The value of `m=` in the streamid.
    pub fn as_str(self) -> &'static str {
        match self {
            SrtRelayRole::Publish => "publish",
            SrtRelayRole::Request => "request",
        }
    }
}

/// A parsed `srt+relay://<id>[?<query>]` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtRelayUri {
    /// The pairing key on the relay.
    pub id: String,
    /// The query string without the leading `?`, passed through unchanged.
    /// Empty when the URI has none.
    pub query: String,
}

impl SrtRelayUri {
    /// Whether the query carries a parameter with this name (case-insensitive).
    pub fn has_param(&self, name: &str) -> bool {
        query_keys(&self.query).any(|k| k.eq_ignore_ascii_case(name))
    }
}

/// Why an `srt+relay://` URI or its resolution was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrtRelayUriError {
    /// The id is empty, too long, or has characters outside `[A-Za-z0-9_-]`.
    InvalidId(String),
    /// The query sets a parameter the relay path sets itself.
    ReservedParam(String),
    /// `srt+relay://` was used but no relay is configured.
    NotConfigured,
}

impl std::fmt::Display for SrtRelayUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SrtRelayUriError::InvalidId(id) => write!(
                f,
                "invalid SRT relay stream id '{}': use 1 to {} characters from A-Z, a-z, 0-9, '_' and '-'",
                id, SRT_RELAY_MAX_ID_LEN
            ),
            SrtRelayUriError::ReservedParam(name) => write!(
                f,
                "'{}' is not allowed in an {} URI: the relay connection sets it",
                name, SRT_RELAY_SCHEME
            ),
            SrtRelayUriError::NotConfigured => write!(
                f,
                "{} URI used but no SRT relay is configured: set STROM_SRT_RELAY to the relay's host:port",
                SRT_RELAY_SCHEME
            ),
        }
    }
}

impl std::error::Error for SrtRelayUriError {}

/// Keys of the parameters in a query string, empty segments skipped.
fn query_keys(query: &str) -> impl Iterator<Item = &str> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').map_or(p, |(k, _)| k).trim())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= SRT_RELAY_MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether the URI uses the `srt+relay://` scheme (case-insensitive).
pub fn is_srt_relay_uri(uri: &str) -> bool {
    let uri = uri.trim();
    uri.len() >= SRT_RELAY_SCHEME.len()
        && uri.as_bytes()[..SRT_RELAY_SCHEME.len()]
            .eq_ignore_ascii_case(SRT_RELAY_SCHEME.as_bytes())
}

/// Parse an `srt+relay://` URI.
///
/// Returns `Ok(None)` for any other URI (plain `srt://` and the rest), so the
/// caller can leave those untouched.
pub fn parse_srt_relay_uri(uri: &str) -> Result<Option<SrtRelayUri>, SrtRelayUriError> {
    let uri = uri.trim();
    if !is_srt_relay_uri(uri) {
        return Ok(None);
    }
    let rest = &uri[SRT_RELAY_SCHEME.len()..];
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    if !valid_id(id) {
        return Err(SrtRelayUriError::InvalidId(id.to_string()));
    }
    if let Some(key) = query_keys(query).find(|k| {
        RESERVED_PARAMS
            .iter()
            .any(|reserved| k.eq_ignore_ascii_case(reserved))
    }) {
        return Err(SrtRelayUriError::ReservedParam(key.to_string()));
    }
    Ok(Some(SrtRelayUri {
        id: id.to_string(),
        query: query.to_string(),
    }))
}

/// Resolve the `srt_uri` property of an SRT block into the URI its
/// `srtsrc`/`srtsink` gets.
///
/// Any URI that is not `srt+relay://` is returned unchanged. An
/// `srt+relay://<id>?<query>` URI becomes
/// `srt://<relay>?mode=caller&streamid=%23!%3A%3Ar%3D<id>%2Cm%3D<role>&<query>`,
/// where `relay` is the relay's SRT `host:port` as Strom reaches it.
pub fn resolve_srt_uri(
    uri: &str,
    role: SrtRelayRole,
    relay: Option<&str>,
) -> Result<String, SrtRelayUriError> {
    let Some(parsed) = parse_srt_relay_uri(uri)? else {
        return Ok(uri.to_string());
    };
    let relay = relay
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or(SrtRelayUriError::NotConfigured)?;
    // `#!::r=<id>,m=<role>`, percent-encoded. The id is limited to unreserved
    // characters, so it needs no encoding itself.
    let mut resolved = format!(
        "srt://{}?mode=caller&streamid=%23!%3A%3Ar%3D{}%2Cm%3D{}",
        relay,
        parsed.id,
        role.as_str()
    );
    if !parsed.query.is_empty() {
        resolved.push('&');
        resolved.push_str(&parsed.query);
    }
    Ok(resolved)
}

/// The URI the outside party dials to reach the block on the relay.
/// It carries no passphrase: that is shared out of band.
pub fn srt_relay_dial_in_uri(public: &str, id: &str) -> String {
    format!("srt://{}?streamid={}", public, id)
}

/// SRT relay status, as `GET /api/srt-relay` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtRelayInfo {
    /// Whether an SRT relay is configured (`STROM_SRT_RELAY`).
    pub configured: bool,
    /// The relay's SRT `host:port` as Strom reaches it.
    pub relay: Option<String>,
    /// The relay's public `host:port`, which the outside world dials.
    /// `null` when it is not known (see `error`).
    pub public: Option<String>,
    /// Why `public` is not known, if it is not.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: Option<&str> = Some("srt-splice:9000");

    #[test]
    fn input_resolves_to_request() {
        assert_eq!(
            resolve_srt_uri("srt+relay://cam1", SrtRelayRole::Request, RELAY).unwrap(),
            "srt://srt-splice:9000?mode=caller&streamid=%23!%3A%3Ar%3Dcam1%2Cm%3Drequest"
        );
    }

    #[test]
    fn output_resolves_to_publish() {
        assert_eq!(
            resolve_srt_uri("srt+relay://pgm-1", SrtRelayRole::Publish, RELAY).unwrap(),
            "srt://srt-splice:9000?mode=caller&streamid=%23!%3A%3Ar%3Dpgm-1%2Cm%3Dpublish"
        );
    }

    #[test]
    fn query_is_passed_through_unchanged() {
        assert_eq!(
            resolve_srt_uri(
                "srt+relay://cam_1?passphrase=s3cret%26more&pbkeylen=32&latency=200",
                SrtRelayRole::Request,
                Some("127.0.0.1:9000"),
            )
            .unwrap(),
            "srt://127.0.0.1:9000?mode=caller&streamid=%23!%3A%3Ar%3Dcam_1%2Cm%3Drequest\
             &passphrase=s3cret%26more&pbkeylen=32&latency=200"
        );
    }

    #[test]
    fn empty_query_adds_nothing() {
        assert_eq!(
            resolve_srt_uri("srt+relay://cam1?", SrtRelayRole::Request, RELAY).unwrap(),
            "srt://srt-splice:9000?mode=caller&streamid=%23!%3A%3Ar%3Dcam1%2Cm%3Drequest"
        );
    }

    #[test]
    fn scheme_is_case_insensitive_and_whitespace_trimmed() {
        assert_eq!(
            resolve_srt_uri("  SRT+Relay://cam1 ", SrtRelayRole::Publish, RELAY).unwrap(),
            "srt://srt-splice:9000?mode=caller&streamid=%23!%3A%3Ar%3Dcam1%2Cm%3Dpublish"
        );
    }

    #[test]
    fn mode_and_streamid_are_rejected() {
        for uri in [
            "srt+relay://cam1?mode=listener",
            "srt+relay://cam1?passphrase=abcdefghij&MODE=caller",
            "srt+relay://cam1?streamid=other",
            "srt+relay://cam1?latency=100&streamid",
        ] {
            let err = resolve_srt_uri(uri, SrtRelayRole::Request, RELAY).unwrap_err();
            assert!(
                matches!(err, SrtRelayUriError::ReservedParam(_)),
                "{uri}: {err:?}"
            );
        }
    }

    #[test]
    fn bad_ids_are_rejected() {
        let long = format!("srt+relay://{}", "a".repeat(SRT_RELAY_MAX_ID_LEN + 1));
        for uri in [
            "srt+relay://",
            "srt+relay://?passphrase=x",
            "srt+relay://cam 1",
            "srt+relay://cam.1",
            "srt+relay://cam1/",
            "srt+relay://host:9000",
            "srt+relay://%23cam",
            long.as_str(),
        ] {
            let err = resolve_srt_uri(uri, SrtRelayRole::Request, RELAY).unwrap_err();
            assert!(
                matches!(err, SrtRelayUriError::InvalidId(_)),
                "{uri}: {err:?}"
            );
        }
        let max = format!("srt+relay://{}", "a".repeat(SRT_RELAY_MAX_ID_LEN));
        assert!(resolve_srt_uri(&max, SrtRelayRole::Request, RELAY).is_ok());
    }

    #[test]
    fn plain_uris_are_untouched_with_or_without_a_relay() {
        for uri in [
            "srt://:5000?mode=listener",
            "srt://192.0.2.1:4000?mode=caller&streamid=x",
            "",
            "srt+relayx",
        ] {
            for relay in [RELAY, None] {
                assert_eq!(
                    resolve_srt_uri(uri, SrtRelayRole::Publish, relay).unwrap(),
                    uri
                );
            }
        }
    }

    #[test]
    fn relay_uri_without_relay_names_the_env_var() {
        for relay in [None, Some(""), Some("  ")] {
            let err =
                resolve_srt_uri("srt+relay://cam1", SrtRelayRole::Request, relay).unwrap_err();
            assert_eq!(err, SrtRelayUriError::NotConfigured);
            assert!(err.to_string().contains("STROM_SRT_RELAY"));
        }
    }

    #[test]
    fn parse_exposes_id_query_and_params() {
        let parsed = parse_srt_relay_uri("srt+relay://cam1?passphrase=abcdefghij&latency=200")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.id, "cam1");
        assert_eq!(parsed.query, "passphrase=abcdefghij&latency=200");
        assert!(parsed.has_param("passphrase"));
        assert!(!parsed.has_param("pbkeylen"));
        assert_eq!(parse_srt_relay_uri("srt://:5000").unwrap(), None);
    }

    #[test]
    fn dial_in_uri_uses_the_plain_id() {
        assert_eq!(
            srt_relay_dial_in_uri("srt.example.com:9000", "cam1"),
            "srt://srt.example.com:9000?streamid=cam1"
        );
    }

    #[test]
    fn info_serializes_nulls() {
        let json = serde_json::to_value(SrtRelayInfo::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"configured": false, "relay": null, "public": null, "error": null})
        );
    }
}
