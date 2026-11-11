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

/// Shortest stream id that is not easy to guess. The id is the only thing
/// that names a stream on the relay, so a short one invites guessing.
pub const SRT_RELAY_MIN_SAFE_ID_LEN: usize = 16;

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

    /// The percent-decoded value of the first parameter with this name
    /// (case-insensitive). A parameter without `=` has the empty value.
    pub fn param(&self, name: &str) -> Option<String> {
        self.query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| p.split_once('=').unwrap_or((p, "")))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| percent_decode(v))
    }
}

/// Decode `%XX` escapes; anything malformed is kept as it is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// Random bytes behind a new stream id: 128 bits, 22 characters.
pub const SRT_RELAY_NEW_ID_BYTES: usize = 16;
/// Random bytes behind a new passphrase: 192 bits, 32 characters (SRT
/// accepts 10 to 79).
pub const SRT_RELAY_NEW_PASSPHRASE_BYTES: usize = 24;

/// Unpadded base64url (`[A-Za-z0-9_-]`): valid as a relay id and needs no
/// escaping in a URI query.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Build `srt+relay://<id>?passphrase=<passphrase>[&<other params>]` from the
/// given random bytes. `keep_query` is the query of the URI being replaced:
/// its parameters other than `passphrase`, `mode` and `streamid` are kept, so
/// a new address does not drop settings such as `latency`.
pub fn srt_relay_uri_from_random(
    id_bytes: &[u8; SRT_RELAY_NEW_ID_BYTES],
    passphrase_bytes: &[u8; SRT_RELAY_NEW_PASSPHRASE_BYTES],
    keep_query: &str,
) -> String {
    let mut uri = format!(
        "{}{}?passphrase={}",
        SRT_RELAY_SCHEME,
        base64url(id_bytes),
        base64url(passphrase_bytes)
    );
    for param in keep_query.split('&').filter(|p| !p.is_empty()) {
        let key = param.split_once('=').map_or(param, |(k, _)| k).trim();
        let dropped = key.eq_ignore_ascii_case("passphrase")
            || RESERVED_PARAMS.iter().any(|r| key.eq_ignore_ascii_case(r));
        if !dropped {
            uri.push('&');
            uri.push_str(param);
        }
    }
    uri
}

/// Fill `out` with random bytes. Taken from v4 UUIDs (the OS RNG, also in
/// the browser), skipping the two bytes that carry their fixed version and
/// variant bits.
fn fill_random(out: &mut [u8]) {
    let mut filled = 0;
    while filled < out.len() {
        let uuid = uuid::Uuid::new_v4();
        for (i, b) in uuid.as_bytes().iter().enumerate() {
            if i == 6 || i == 8 || filled == out.len() {
                continue;
            }
            out[filled] = *b;
            filled += 1;
        }
    }
}

/// A new `srt+relay://` URI with a random 22-character id and a random
/// 32-character passphrase. See [`srt_relay_uri_from_random`] for
/// `keep_query`.
pub fn new_srt_relay_uri(keep_query: &str) -> String {
    let mut id = [0u8; SRT_RELAY_NEW_ID_BYTES];
    let mut passphrase = [0u8; SRT_RELAY_NEW_PASSPHRASE_BYTES];
    fill_random(&mut id);
    fill_random(&mut passphrase);
    srt_relay_uri_from_random(&id, &passphrase, keep_query)
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
    fn param_returns_the_decoded_value() {
        let parsed = parse_srt_relay_uri(
            "srt+relay://cam1?latency=200&PassPhrase=s3cret%26more%2&flag&passphrase=second",
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.param("passphrase").as_deref(), Some("s3cret&more%2"));
        assert_eq!(parsed.param("latency").as_deref(), Some("200"));
        assert_eq!(parsed.param("flag").as_deref(), Some(""));
        assert_eq!(parsed.param("pbkeylen"), None);
    }

    #[test]
    fn base64url_matches_the_rfc_vectors() {
        // RFC 4648 section 10, unpadded.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input.as_bytes()), expected);
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    fn assert_generated(uri: &str) -> SrtRelayUri {
        let parsed = parse_srt_relay_uri(uri)
            .unwrap_or_else(|e| panic!("{uri}: {e}"))
            .unwrap_or_else(|| panic!("{uri}: not a relay URI"));
        let url_safe = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        assert_eq!(parsed.id.len(), 22, "{uri}");
        assert!(parsed.id.len() >= SRT_RELAY_MIN_SAFE_ID_LEN);
        assert!(url_safe(&parsed.id), "{uri}");
        let passphrase = parsed.param("passphrase").expect("passphrase");
        assert_eq!(passphrase.len(), 32, "{uri}");
        assert!((10..=79).contains(&passphrase.len()));
        assert!(url_safe(&passphrase), "{uri}");
        // Valid for the relay path, too.
        resolve_srt_uri(uri, SrtRelayRole::Publish, RELAY).unwrap();
        parsed
    }

    #[test]
    fn uri_from_random_has_the_expected_format() {
        let uri = srt_relay_uri_from_random(&[0xff; 16], &[0; 24], "");
        assert_eq!(
            uri,
            "srt+relay://_____________________w?passphrase=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
        assert_generated(&uri);
    }

    #[test]
    fn uri_from_random_keeps_other_params_only() {
        let uri = srt_relay_uri_from_random(
            &[1; 16],
            &[2; 24],
            "passphrase=old&latency=200&MODE=caller&streamid=x&&pbkeylen=32",
        );
        let parsed = assert_generated(&uri);
        assert_eq!(
            parsed.query,
            format!("passphrase={}&latency=200&pbkeylen=32", base64url(&[2; 24]))
        );
    }

    #[test]
    fn new_uris_are_valid_and_differ() {
        let a = new_srt_relay_uri("");
        let b = new_srt_relay_uri("latency=120");
        let pa = assert_generated(&a);
        let pb = assert_generated(&b);
        assert_ne!(pa.id, pb.id);
        assert_ne!(pa.param("passphrase"), pb.param("passphrase"));
        assert!(pb.query.ends_with("&latency=120"));
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
