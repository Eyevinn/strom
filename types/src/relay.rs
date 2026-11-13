//! Relays, independent of the protocol.
//!
//! A relay lets a flow publish to, or pull from, a remote party without
//! binding a port itself: Strom dials the relay, the remote party dials the
//! relay, and the relay pairs the two by a random stream id. Several relays
//! can be configured per protocol, each under a name (`default` and others,
//! for routing, regions, HA or separate productions).
//!
//! A block names a stream on a relay with `<proto>+relay://<id>[@<relay>][?<query>]`.
//! This module holds what every protocol shares: the protocol key, relay
//! names, the URI grammar, the lookup of a named relay, and redaction of the
//! id. What a protocol makes of a resolved relay URI (SRT's
//! `#!::r=<id>,m=<role>` streamid) lives in its own module
//! (`crate::srt_relay`).
//!
//! A new protocol adds a [`RelayProtocol`] variant. Its config section
//! (`[relays.<key>.<name>]`), environment variables
//! (`STROM_<PROTO>_RELAY_PUBLIC`, `STROM_<PROTO>_RELAY_INTERNAL`,
//! `STROM_<PROTO>_RELAYS`) and URI scheme follow from it.

use crate::srt_uri::{gst_query_params, gst_unescape};

/// A protocol that can use relays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RelayProtocol {
    Srt,
}

impl RelayProtocol {
    /// Every protocol that can use relays.
    pub const ALL: &'static [RelayProtocol] = &[RelayProtocol::Srt];

    /// The key in the config file (`[relays.<key>.<name>]`) and in
    /// `GET /api/capabilities` (`protocols.<key>.relays`).
    pub fn key(self) -> &'static str {
        match self {
            RelayProtocol::Srt => "srt",
        }
    }

    /// The protocol's part of the environment variable names
    /// (`STROM_<PROTO>_RELAY_PUBLIC`, ...).
    pub fn env_name(self) -> &'static str {
        match self {
            RelayProtocol::Srt => "SRT",
        }
    }

    /// The URI scheme prefix that names a stream on a relay.
    pub fn scheme(self) -> &'static str {
        match self {
            RelayProtocol::Srt => "srt+relay://",
        }
    }

    /// The protocol with this config key.
    pub fn from_key(key: &str) -> Option<RelayProtocol> {
        Self::ALL.iter().copied().find(|p| p.key() == key)
    }

    /// `STROM_<PROTO>_RELAY_PUBLIC`: the public address of the `default` relay.
    pub fn public_env_var(self) -> String {
        format!("STROM_{}_RELAY_PUBLIC", self.env_name())
    }

    /// `STROM_<PROTO>_RELAY_INTERNAL`: the internal address of the `default` relay.
    pub fn internal_env_var(self) -> String {
        format!("STROM_{}_RELAY_INTERNAL", self.env_name())
    }

    /// `STROM_<PROTO>_RELAYS`: named relays, `name=public[|internal],...`.
    pub fn relays_env_var(self) -> String {
        format!("STROM_{}_RELAYS", self.env_name())
    }
}

/// The relay a relay URI without `@<relay>` uses.
pub const DEFAULT_RELAY_NAME: &str = "default";

/// Longest relay name.
pub const RELAY_NAME_MAX_LEN: usize = 32;

/// Longest stream id accepted in a relay URI.
pub const RELAY_MAX_ID_LEN: usize = 64;

/// Whether `name` is a valid relay name: 1 to 32 characters from `[a-z0-9-]`.
pub fn is_valid_relay_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= RELAY_NAME_MAX_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `id` is a valid relay stream id: 1 to 64 characters from
/// `[A-Za-z0-9_-]`.
pub fn is_valid_relay_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= RELAY_MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A configured relay, as the server knows it. It holds the internal
/// address, so it is never serialized: the API reports
/// [`crate::capabilities::RelayEndpoint`] (name and public address) instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTarget {
    /// The relay's name (`default` or `[a-z0-9-]{1,32}`).
    pub name: String,
    /// The `host:port` the outside party dials. Strom dials it too unless
    /// `internal` is set.
    pub public: String,
    /// The `host:port` Strom dials when it runs next to the relay.
    pub internal: Option<String>,
}

impl RelayTarget {
    /// The `host:port` Strom dials: the internal address if set, else the
    /// public one.
    pub fn dial_address(&self) -> &str {
        self.internal.as_deref().unwrap_or(&self.public)
    }
}

/// Something that has a relay name: the server's [`RelayTarget`] and the
/// client's [`crate::capabilities::RelayEndpoint`].
pub trait NamedRelay {
    fn relay_name(&self) -> &str;
}

impl NamedRelay for RelayTarget {
    fn relay_name(&self) -> &str {
        &self.name
    }
}

impl NamedRelay for crate::capabilities::RelayEndpoint {
    fn relay_name(&self) -> &str {
        &self.name
    }
}

/// Order relays as they are reported: `default` first, then by name.
pub fn sort_relays<T: NamedRelay>(relays: &mut [T]) {
    relays.sort_by(|a, b| {
        let key = |r: &T| {
            (
                r.relay_name() != DEFAULT_RELAY_NAME,
                r.relay_name().to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
}

/// Why a named relay could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayLookupError {
    /// No relay is configured for the protocol.
    NotConfigured { protocol: RelayProtocol },
    /// No relay of this name is configured; `configured` lists those that are.
    Unknown {
        protocol: RelayProtocol,
        name: String,
        configured: Vec<String>,
    },
}

impl std::fmt::Display for RelayLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayLookupError::NotConfigured { protocol } => write!(
                f,
                "{} URI used but no {} relay is configured: set {} (or [relays.{}.{}])",
                protocol.scheme(),
                protocol.env_name(),
                protocol.public_env_var(),
                protocol.key(),
                DEFAULT_RELAY_NAME
            ),
            RelayLookupError::Unknown {
                protocol,
                name,
                configured,
            } => write!(
                f,
                "no {} relay named '{}' is configured; configured: {}",
                protocol.env_name(),
                name,
                configured.join(", ")
            ),
        }
    }
}

impl std::error::Error for RelayLookupError {}

/// The relay called `name` among `relays`.
pub fn find_relay<'a, T: NamedRelay>(
    protocol: RelayProtocol,
    relays: &'a [T],
    name: &str,
) -> Result<&'a T, RelayLookupError> {
    if relays.is_empty() {
        return Err(RelayLookupError::NotConfigured { protocol });
    }
    relays
        .iter()
        .find(|r| r.relay_name() == name)
        .ok_or_else(|| RelayLookupError::Unknown {
            protocol,
            name: name.to_string(),
            configured: relays.iter().map(|r| r.relay_name().to_string()).collect(),
        })
}

/// A parsed `<proto>+relay://<id>[@<relay>][?<query>]` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayUri {
    /// The pairing key on the relay.
    pub id: String,
    /// The relay named after `@`, `None` for the default relay.
    pub relay: Option<String>,
    /// The query string without the leading `?`, passed through unchanged.
    /// Empty when the URI has none.
    pub query: String,
}

impl RelayUri {
    /// The name of the relay this URI uses (`default` without `@<relay>`).
    pub fn relay_name(&self) -> &str {
        self.relay.as_deref().unwrap_or(DEFAULT_RELAY_NAME)
    }

    /// Whether the query carries a parameter with this name. Keys are
    /// matched after decoding and exactly, as GStreamer matches them.
    pub fn has_param(&self, name: &str) -> bool {
        gst_query_params(&self.query).any(|(_, key, _)| key.as_deref() == Some(name))
    }

    /// The decoded value GStreamer reads for this parameter, matched as in
    /// [`RelayUri::has_param`]. A parameter without `=` has the empty value;
    /// one whose value has a malformed escape has none, as GStreamer then
    /// sets nothing.
    ///
    /// A key given twice is refused by
    /// [`crate::srt_uri::validate_srt_uri`], which the blocks run before they
    /// build. On a URI that was not checked, this returns what GStreamer
    /// uses: the last one.
    pub fn param(&self, name: &str) -> Option<String> {
        gst_query_params(&self.query)
            .filter(|(_, key, _)| key.as_deref() == Some(name))
            .last()
            .and_then(|(_, _, value)| gst_unescape(value.unwrap_or("")))
    }

    /// The decoded passphrase, or `None` when there is none or it is empty
    /// (an empty `passphrase=` encrypts nothing).
    pub fn passphrase(&self) -> Option<String> {
        self.param("passphrase").filter(|p| !p.is_empty())
    }
}

/// Why a relay URI was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayUriError {
    /// The id is empty, too long, or has characters outside `[A-Za-z0-9_-]`.
    InvalidId(String),
    /// The relay name after `@` is empty, too long, or has characters
    /// outside `[a-z0-9-]`.
    InvalidRelayName(String),
}

impl std::fmt::Display for RelayUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayUriError::InvalidId(id) => write!(
                f,
                "invalid relay stream id '{}': use 1 to {} characters from A-Z, a-z, 0-9, '_' and '-'",
                id, RELAY_MAX_ID_LEN
            ),
            RelayUriError::InvalidRelayName(name) => write!(
                f,
                "invalid relay name '{}': use 1 to {} characters from a-z, 0-9 and '-'",
                name, RELAY_NAME_MAX_LEN
            ),
        }
    }
}

impl std::error::Error for RelayUriError {}

/// The URI after a case-insensitive `scheme`, or `None` if it has another
/// scheme.
fn strip_scheme<'a>(uri: &'a str, scheme: &str) -> Option<&'a str> {
    uri.get(..scheme.len())
        .filter(|s| s.eq_ignore_ascii_case(scheme))
        .map(|_| &uri[scheme.len()..])
}

/// Whether the URI uses the protocol's relay scheme (case-insensitive,
/// surrounding whitespace ignored).
pub fn is_relay_uri(protocol: RelayProtocol, uri: &str) -> bool {
    strip_scheme(uri.trim(), protocol.scheme()).is_some()
}

/// Split the part after the scheme into id, relay name and query, as
/// written and unchecked.
pub fn split_relay_uri(rest: &str) -> (&str, Option<&str>, &str) {
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    match authority.split_once('@') {
        Some((id, relay)) => (id, Some(relay), query),
        None => (authority, None, query),
    }
}

/// Parse a `<proto>+relay://<id>[@<relay>][?<query>]` URI.
///
/// Returns `Ok(None)` for a URI with another scheme, so the caller can leave
/// those untouched.
pub fn parse_relay_uri(
    protocol: RelayProtocol,
    uri: &str,
) -> Result<Option<RelayUri>, RelayUriError> {
    let Some(rest) = strip_scheme(uri.trim(), protocol.scheme()) else {
        return Ok(None);
    };
    let (id, relay, query) = split_relay_uri(rest);
    if !is_valid_relay_id(id) {
        return Err(RelayUriError::InvalidId(id.to_string()));
    }
    if let Some(relay) = relay {
        if !is_valid_relay_name(relay) {
            return Err(RelayUriError::InvalidRelayName(relay.to_string()));
        }
    }
    Ok(Some(RelayUri {
        id: id.to_string(),
        relay: relay.map(str::to_string),
        query: query.to_string(),
    }))
}

/// Write `<scheme><id>[@<relay>]`, leaving out `@default`.
pub fn relay_uri_authority(protocol: RelayProtocol, id: &str, relay: Option<&str>) -> String {
    match relay.filter(|r| !r.is_empty() && *r != DEFAULT_RELAY_NAME) {
        Some(relay) => format!("{}{id}@{relay}", protocol.scheme()),
        None => format!("{}{id}", protocol.scheme()),
    }
}

/// A relay URI with its id replaced by `***`, the relay name kept:
/// `<scheme>***@eu-north?...`. Any other URI is returned unchanged. The id
/// grants access to the stream without a passphrase, so it stays out of
/// logs. The query is left as it is: redact its secrets separately.
pub fn redact_relay_uri_id(protocol: RelayProtocol, uri: &str) -> String {
    let scheme = protocol.scheme();
    let Some(rest) = strip_scheme(uri, scheme) else {
        return uri.to_string();
    };
    let id_end = rest.find(['@', '?', '#']).unwrap_or(rest.len());
    if id_end == 0 {
        return uri.to_string();
    }
    format!("{scheme}***{}", &rest[id_end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::RelayEndpoint;

    const SRT: RelayProtocol = RelayProtocol::Srt;

    fn target(name: &str) -> RelayTarget {
        RelayTarget {
            name: name.to_string(),
            public: format!("{name}.example.com:9000"),
            internal: None,
        }
    }

    #[test]
    fn protocol_names_follow_the_key() {
        assert_eq!(SRT.key(), "srt");
        assert_eq!(SRT.scheme(), "srt+relay://");
        assert_eq!(SRT.public_env_var(), "STROM_SRT_RELAY_PUBLIC");
        assert_eq!(SRT.internal_env_var(), "STROM_SRT_RELAY_INTERNAL");
        assert_eq!(SRT.relays_env_var(), "STROM_SRT_RELAYS");
        assert_eq!(RelayProtocol::from_key("srt"), Some(SRT));
        assert_eq!(RelayProtocol::from_key("rist"), None);
    }

    #[test]
    fn relay_names() {
        for good in ["default", "eu-north", "a", "0", &"a".repeat(32)] {
            assert!(is_valid_relay_name(good), "{good}");
        }
        for bad in [
            "",
            "EU",
            "eu_north",
            "eu north",
            "eu.north",
            &"a".repeat(33),
        ] {
            assert!(!is_valid_relay_name(bad), "{bad}");
        }
    }

    #[test]
    fn parse_without_relay_uses_default() {
        let uri = parse_relay_uri(SRT, "srt+relay://cam1?latency=1")
            .unwrap()
            .unwrap();
        assert_eq!(uri.id, "cam1");
        assert_eq!(uri.relay, None);
        assert_eq!(uri.relay_name(), "default");
        assert_eq!(uri.query, "latency=1");
    }

    #[test]
    fn parse_with_relay() {
        let uri = parse_relay_uri(SRT, " SRT+Relay://cam1@eu-north?latency=1 ")
            .unwrap()
            .unwrap();
        assert_eq!(uri.id, "cam1");
        assert_eq!(uri.relay.as_deref(), Some("eu-north"));
        assert_eq!(uri.relay_name(), "eu-north");
        assert_eq!(uri.query, "latency=1");
        let uri = parse_relay_uri(SRT, "srt+relay://cam1@us")
            .unwrap()
            .unwrap();
        assert_eq!(uri.relay_name(), "us");
        assert_eq!(uri.query, "");
    }

    #[test]
    fn parse_refuses_bad_ids_and_relay_names() {
        assert_eq!(
            parse_relay_uri(SRT, "srt+relay://@eu"),
            Err(RelayUriError::InvalidId(String::new()))
        );
        for (uri, name) in [
            ("srt+relay://cam1@", ""),
            ("srt+relay://cam1@EU", "EU"),
            ("srt+relay://cam1@eu@us", "eu@us"),
            ("srt+relay://cam1@eu:9000", "eu:9000"),
        ] {
            assert_eq!(
                parse_relay_uri(SRT, uri),
                Err(RelayUriError::InvalidRelayName(name.to_string())),
                "{uri}"
            );
        }
        assert_eq!(parse_relay_uri(SRT, "srt://host:9000"), Ok(None));
    }

    #[test]
    fn authority_leaves_out_the_default_relay() {
        assert_eq!(relay_uri_authority(SRT, "cam1", None), "srt+relay://cam1");
        assert_eq!(
            relay_uri_authority(SRT, "cam1", Some("default")),
            "srt+relay://cam1"
        );
        assert_eq!(
            relay_uri_authority(SRT, "cam1", Some("")),
            "srt+relay://cam1"
        );
        assert_eq!(
            relay_uri_authority(SRT, "cam1", Some("eu-north")),
            "srt+relay://cam1@eu-north"
        );
    }

    #[test]
    fn redaction_hides_the_id_and_keeps_the_relay() {
        assert_eq!(
            redact_relay_uri_id(SRT, "srt+relay://secretid@eu-north?latency=1"),
            "srt+relay://***@eu-north?latency=1"
        );
        assert_eq!(
            redact_relay_uri_id(SRT, "srt+relay://secretid?latency=1"),
            "srt+relay://***?latency=1"
        );
        assert_eq!(
            redact_relay_uri_id(SRT, "srt+relay://secretid"),
            "srt+relay://***"
        );
        assert_eq!(redact_relay_uri_id(SRT, "srt://h:1"), "srt://h:1");
    }

    #[test]
    fn find_relay_by_name() {
        let relays = vec![target("default"), target("eu-north")];
        assert_eq!(
            find_relay(SRT, &relays, "eu-north").unwrap().public,
            "eu-north.example.com:9000"
        );
        let err = find_relay(SRT, &relays, "us").unwrap_err();
        assert_eq!(
            err,
            RelayLookupError::Unknown {
                protocol: SRT,
                name: "us".into(),
                configured: vec!["default".into(), "eu-north".into()],
            }
        );
        assert!(err.to_string().contains("'us'"), "{err}");
        assert!(err.to_string().contains("default, eu-north"), "{err}");

        let none: Vec<RelayTarget> = Vec::new();
        let err = find_relay(SRT, &none, "default").unwrap_err();
        assert_eq!(err, RelayLookupError::NotConfigured { protocol: SRT });
        assert!(err.to_string().contains("STROM_SRT_RELAY_PUBLIC"), "{err}");

        // The client looks up the endpoints the server reports the same way.
        let endpoints = vec![RelayEndpoint {
            name: "default".into(),
            public: "srt.example.com:9000".into(),
        }];
        assert!(find_relay(SRT, &endpoints, "default").is_ok());
    }

    #[test]
    fn sort_puts_default_first() {
        let mut relays = vec![target("us"), target("default"), target("eu-north")];
        sort_relays(&mut relays);
        let names: Vec<_> = relays.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["default", "eu-north", "us"]);
    }

    #[test]
    fn internal_is_dialed_when_set() {
        let mut relay = target("default");
        assert_eq!(relay.dial_address(), "default.example.com:9000");
        relay.internal = Some("srt-splice:9000".into());
        assert_eq!(relay.dial_address(), "srt-splice:9000");
    }
}
