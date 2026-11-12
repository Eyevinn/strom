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

use crate::srt_uri::{gst_query_params, gst_unescape};

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

/// The SRT blocks whose `srt_uri` may be `srt+relay://`, by block definition
/// id, with each block's own role on the relay.
pub const SRT_RELAY_BLOCKS: &[(&str, SrtRelayRole)] = &[
    ("builtin.mpegtssrt_input", SrtRelayRole::Request),
    ("builtin.efpsrt_input", SrtRelayRole::Request),
    ("builtin.mpegtssrt_output", SrtRelayRole::Publish),
    ("builtin.efpsrt_output", SrtRelayRole::Publish),
];

/// The relay role of a block definition, or `None` if it is not an SRT block
/// that can use the relay.
pub fn srt_relay_role(block_definition_id: &str) -> Option<SrtRelayRole> {
    SRT_RELAY_BLOCKS
        .iter()
        .find(|(id, _)| *id == block_definition_id)
        .map(|(_, role)| *role)
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
    /// Whether the query carries a parameter with this name. Keys are
    /// matched after decoding and exactly, as GStreamer matches them.
    pub fn has_param(&self, name: &str) -> bool {
        query_keys(&self.query).any(|k| k == name)
    }

    /// The decoded value GStreamer reads for this parameter, matched as in
    /// [`SrtRelayUri::has_param`]. A parameter without `=` has the empty
    /// value; one whose value has a malformed escape has none, as GStreamer
    /// then sets nothing.
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

/// Decode `%XX` escapes; anything malformed is kept as it is.
fn percent_decode(s: &str) -> String {
    String::from_utf8_lossy(&urlencoding::decode_binary(s.as_bytes())).into_owned()
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
                "{} URI used but no SRT relay is configured: set STROM_SRT_RELAY to the relay's public host:port",
                SRT_RELAY_SCHEME
            ),
        }
    }
}

impl std::error::Error for SrtRelayUriError {}

/// Decoded keys of the parameters in a query string, empty segments and
/// malformed keys skipped.
fn query_keys(query: &str) -> impl Iterator<Item = String> + '_ {
    gst_query_params(query).filter_map(|(_, key, _)| key)
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
    if let Some(key) = query_keys(query).find(|k| RESERVED_PARAMS.contains(&k.as_str())) {
        return Err(SrtRelayUriError::ReservedParam(key));
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

/// Query parameters of the relay URI that a player needs to decrypt the stream.
const PLAYER_PARAMS: &[&str] = &["passphrase", "pbkeylen"];

/// The URI a player such as VLC opens to pull an SRT output block from the
/// relay: the dial-in URI plus the block's `passphrase` and `pbkeylen`, if it
/// has them. Their values are percent-encoded where they are not URI
/// unreserved characters (generated passphrases never need it).
pub fn srt_relay_player_uri(public: &str, relay: &SrtRelayUri) -> String {
    let mut uri = srt_relay_dial_in_uri(public, &relay.id);
    for name in PLAYER_PARAMS {
        if let Some(value) = relay.param(name).filter(|v| !v.is_empty()) {
            uri.push('&');
            uri.push_str(name);
            uri.push('=');
            // Everything but the URI unreserved characters is encoded.
            uri.push_str(&urlencoding::encode(&value));
        }
    }
    uri
}

/// Query parameters of an SRT URI whose values are secrets. Their values are
/// never logged or shown unasked.
const SECRET_PARAMS: &[&str] = &["passphrase"];

/// Rewrite the secret query parameters of an SRT URI (`srt://` or
/// `srt+relay://`): `Some(mask)` replaces a non-empty value with `mask`,
/// `None` drops the parameter. Keys are matched after percent-decoding, and
/// ignoring case and surrounding whitespace: GStreamer reads only the exact
/// key, but a misspelled one is still the user's secret, so it is hidden
/// too. An empty value is left as is.
fn rewrite_secret_params(uri: &str, mask: Option<&str>) -> String {
    let Some((head, query)) = uri.split_once('?') else {
        return uri.to_string();
    };
    let params: Vec<String> = query
        .split('&')
        .filter_map(|param| {
            let (key, value) = param.split_once('=').unwrap_or((param, ""));
            let decoded = percent_decode(key);
            let secret = SECRET_PARAMS
                .iter()
                .any(|s| decoded.trim().eq_ignore_ascii_case(s));
            if !secret || value.is_empty() {
                return Some(param.to_string());
            }
            mask.map(|mask| format!("{key}={mask}"))
        })
        .collect();
    if params.is_empty() {
        head.to_string()
    } else {
        format!("{head}?{}", params.join("&"))
    }
}

/// An SRT URI fit for a log line: the passphrase (and any other secret query
/// value) replaced with `***`. Use it wherever an SRT URI is logged.
pub fn redact_srt_uri(uri: &str) -> String {
    rewrite_secret_params(uri, Some("***"))
}

/// An SRT URI with the secret query values replaced with `mask`, for showing
/// the URI on screen without its passphrase.
pub fn mask_srt_uri(uri: &str, mask: &str) -> String {
    rewrite_secret_params(uri, Some(mask))
}

/// An SRT URI without its secret query parameters, for a label next to the
/// full URI (a playlist title, say).
pub fn strip_srt_uri_secrets(uri: &str) -> String {
    rewrite_secret_params(uri, None)
}

/// Random bytes behind a new stream id: 128 bits, 22 characters.
pub const SRT_RELAY_NEW_ID_BYTES: usize = 16;
/// Random bytes behind a new passphrase: 192 bits, 32 characters (libsrt
/// accepts 10 to 80 bytes).
pub const SRT_RELAY_NEW_PASSPHRASE_BYTES: usize = 24;

/// Unpadded base64url (`[A-Za-z0-9_-]`): valid as a relay id and needs no
/// escaping in a URI query.
fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Parameters of the URI being replaced that a new relay address does not
/// keep: the relay connection sets `mode` and `streamid`, the new address has
/// its own passphrase, the local bind settings belong to a direct
/// connection, not to one through the relay, and `latency` has no effect in
/// the URI (the block's Latency property wins, see
/// [`crate::srt_uri::SrtUriWarning::IgnoredLatency`]).
const NOT_KEPT_PARAMS: &[&str] = &[
    "mode",
    "streamid",
    "passphrase",
    "localaddress",
    "localport",
    "adapter",
    "latency",
];

/// The query string of an SRT URI (after the first `?`), empty if it has none.
pub fn srt_uri_query(uri: &str) -> &str {
    uri.trim().split_once('?').map_or("", |(_, q)| q)
}

/// Build `srt+relay://<id>?passphrase=<passphrase>[&<other params>]` from the
/// given random bytes. `keep_query` is the query of the URI being replaced
/// (a plain `srt://` URI or an `srt+relay://` one): its parameters other than
/// those in [`NOT_KEPT_PARAMS`] are kept, so a new address does not drop
/// settings such as `pbkeylen` or `rcvlatency`. The dropped ones are matched
/// after decoding and ignoring case: GStreamer reads only the exact key, but
/// a new, generated address is better off without a misspelled `MODE` that
/// validation would refuse.
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
        let key = param.split_once('=').map_or(param, |(k, _)| k);
        let key = percent_decode(key);
        let dropped = NOT_KEPT_PARAMS
            .iter()
            .any(|r| key.trim().eq_ignore_ascii_case(r));
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

/// A new random relay stream id: 22 characters from `[A-Za-z0-9_-]`.
pub fn new_srt_relay_id() -> String {
    let mut id = [0u8; SRT_RELAY_NEW_ID_BYTES];
    fill_random(&mut id);
    base64url(&id)
}

/// A new random SRT passphrase: 32 characters from `[A-Za-z0-9_-]`, so it
/// needs no escaping in a URI.
pub fn new_srt_passphrase() -> String {
    let mut passphrase = [0u8; SRT_RELAY_NEW_PASSPHRASE_BYTES];
    fill_random(&mut passphrase);
    base64url(&passphrase)
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

/// A fresh address for a copy of a block whose `srt_uri` is `uri`: a new id
/// and passphrase with the other parameters kept, or `None` when `uri` is not
/// a valid `srt+relay://` URI. A copy that kept the id would share the
/// stream on the relay with the original, as a copied listener would collide
/// on its port.
pub fn renew_srt_relay_uri(uri: &str) -> Option<String> {
    parse_srt_relay_uri(uri)
        .ok()
        .flatten()
        .map(|parsed| new_srt_relay_uri(&parsed.query))
}

/// SRT relay status, as `GET /api/srt-relay` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
pub struct SrtRelayInfo {
    /// Whether an SRT relay is configured (`STROM_SRT_RELAY`).
    pub configured: bool,
    /// The relay's SRT `host:port` that Strom dials: `STROM_SRT_RELAY_INTERNAL`
    /// if set, else `STROM_SRT_RELAY`.
    pub relay: Option<String>,
    /// The relay's public `host:port` (`STROM_SRT_RELAY`), which the outside
    /// world dials. Set whenever `configured` is.
    pub public: Option<String>,
    /// Reserved, always `null`. Kept so clients that read it keep working.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: Option<&str> = Some("srt-splice:9000");

    fn player(uri: &str) -> String {
        let parsed = parse_srt_relay_uri(uri).unwrap().unwrap();
        srt_relay_player_uri("relay.example.com:9000", &parsed)
    }

    #[test]
    fn player_uri_without_passphrase() {
        assert_eq!(
            player("srt+relay://cam1?latency=200"),
            "srt://relay.example.com:9000?streamid=cam1"
        );
    }

    #[test]
    fn player_uri_with_passphrase_and_pbkeylen() {
        assert_eq!(
            player("srt+relay://cam1?latency=200&passphrase=abcdefghij_-12&pbkeylen=32"),
            "srt://relay.example.com:9000?streamid=cam1&passphrase=abcdefghij_-12&pbkeylen=32"
        );
    }

    #[test]
    fn player_uri_encodes_passphrase() {
        // `a%26b c` decodes to `a&b c`, which is encoded again for the player.
        assert_eq!(
            player("srt+relay://cam1?passphrase=a%26b c#d=e"),
            "srt://relay.example.com:9000?streamid=cam1&passphrase=a%26b%20c%23d%3De"
        );
    }

    #[test]
    fn player_uri_skips_empty_passphrase() {
        assert_eq!(
            player("srt+relay://cam1?passphrase="),
            "srt://relay.example.com:9000?streamid=cam1"
        );
    }

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
    fn reserved_params_are_matched_exactly() {
        // GStreamer ignores `MODE`; validate_srt_uri refuses it with the
        // right spelling, so it is not reserved here.
        let uri = "srt+relay://cam1?passphrase=abcdefghij&MODE=caller";
        assert!(resolve_srt_uri(uri, SrtRelayRole::Request, RELAY).is_ok());
        // A percent-encoded key is the key GStreamer reads.
        let err = resolve_srt_uri(
            "srt+relay://cam1?stream%69d=x",
            SrtRelayRole::Request,
            RELAY,
        )
        .unwrap_err();
        assert_eq!(err, SrtRelayUriError::ReservedParam("streamid".into()));
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
    fn param_returns_what_gstreamer_reads() {
        let parsed = parse_srt_relay_uri(
            "srt+relay://cam1?rcvlatency=200&PassPhrase=other&flag&passphrase=first\
             &pass%70hrase=s3cret%26more&pbkeylen=%zz",
        )
        .unwrap()
        .unwrap();
        // Exact key after decoding; of two, the last.
        assert_eq!(parsed.param("passphrase").as_deref(), Some("s3cret&more"));
        assert!(parsed.has_param("passphrase"));
        assert!(!parsed.has_param("Passphrase"));
        assert_eq!(parsed.param("rcvlatency").as_deref(), Some("200"));
        assert_eq!(parsed.param("flag").as_deref(), Some(""));
        // A malformed value is not set at all.
        assert!(parsed.has_param("pbkeylen"));
        assert_eq!(parsed.param("pbkeylen"), None);
        assert_eq!(parsed.param("latency"), None);
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
        assert!(
            (crate::srt_uri::SRT_PASSPHRASE_MIN_LEN..=crate::srt_uri::SRT_PASSPHRASE_MAX_LEN)
                .contains(&passphrase.len())
        );
        assert!(url_safe(&passphrase), "{uri}");
        // Valid for the relay path, too, with nothing to warn about.
        resolve_srt_uri(uri, SrtRelayRole::Publish, RELAY).unwrap();
        assert_eq!(crate::srt_uri::validate_srt_uri(uri), Ok(vec![]), "{uri}");
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
            "passphrase=old&latency=200&MODE=caller&streamid=x&&pbkeylen=32&rcvlatency=300",
        );
        let parsed = assert_generated(&uri);
        // `latency` goes too: it has no effect in the URI.
        assert_eq!(
            parsed.query,
            format!(
                "passphrase={}&pbkeylen=32&rcvlatency=300",
                base64url(&[2; 24])
            )
        );
    }

    #[test]
    fn new_uris_are_valid_and_differ() {
        let a = new_srt_relay_uri("");
        let b = new_srt_relay_uri("rcvlatency=120");
        let pa = assert_generated(&a);
        let pb = assert_generated(&b);
        assert_ne!(pa.id, pb.id);
        assert_ne!(pa.param("passphrase"), pb.param("passphrase"));
        assert!(pb.query.ends_with("&rcvlatency=120"));
    }

    #[test]
    fn new_ids_and_passphrases_are_url_safe_and_differ() {
        let (a, b) = (new_srt_relay_id(), new_srt_relay_id());
        assert_eq!(a.len(), 22);
        assert_ne!(a, b);
        assert!(parse_srt_relay_uri(&format!("srt+relay://{a}")).is_ok());
        let p = new_srt_passphrase();
        assert_eq!(p.len(), 32);
        assert_ne!(p, new_srt_passphrase());
        assert_eq!(urlencoding::encode(&p), p);
    }

    #[test]
    fn passphrase_ignores_an_empty_value() {
        let parsed = |uri| parse_srt_relay_uri(uri).unwrap().unwrap();
        assert_eq!(parsed("srt+relay://cam1?passphrase=").passphrase(), None);
        assert_eq!(parsed("srt+relay://cam1?latency=200").passphrase(), None);
        assert_eq!(
            parsed("srt+relay://cam1?passphrase=abc%26def").passphrase(),
            Some("abc&def".to_string())
        );
    }

    #[test]
    fn the_four_srt_blocks_have_a_role() {
        use SrtRelayRole::*;
        assert_eq!(srt_relay_role("builtin.mpegtssrt_input"), Some(Request));
        assert_eq!(srt_relay_role("builtin.efpsrt_input"), Some(Request));
        assert_eq!(srt_relay_role("builtin.mpegtssrt_output"), Some(Publish));
        assert_eq!(srt_relay_role("builtin.efpsrt_output"), Some(Publish));
        assert_eq!(srt_relay_role("builtin.whep_output"), None);
    }

    #[test]
    fn redaction_hides_the_passphrase_and_keeps_the_rest() {
        let resolved = resolve_srt_uri(
            "srt+relay://cam1?passphrase=s3cretpassphrase&latency=200&pbkeylen=32",
            SrtRelayRole::Publish,
            RELAY,
        )
        .unwrap();
        let redacted = redact_srt_uri(&resolved);
        assert!(!redacted.contains("s3cret"), "{redacted}");
        assert_eq!(
            redacted,
            "srt://srt-splice:9000?mode=caller&streamid=%23!%3A%3Ar%3Dcam1%2Cm%3Dpublish\
             &passphrase=***&latency=200&pbkeylen=32"
        );
        for (uri, expected) in [
            // Case and percent-encoding of the key do not matter.
            (
                "srt://:5000?mode=listener&PassPhrase=abc",
                "srt://:5000?mode=listener&PassPhrase=***",
            ),
            (
                "srt://:5000?pass%70hrase=abc",
                "srt://:5000?pass%70hrase=***",
            ),
            // A raw '#' inside the value does not leak the rest of it.
            ("srt://h:1?passphrase=a#b", "srt://h:1?passphrase=***"),
            // Nothing to hide.
            ("srt://:5000?mode=listener", "srt://:5000?mode=listener"),
            ("srt://:5000?passphrase=", "srt://:5000?passphrase="),
            ("srt://:5000", "srt://:5000"),
        ] {
            assert_eq!(redact_srt_uri(uri), expected);
        }
        assert_eq!(
            mask_srt_uri("srt+relay://cam1?passphrase=abc&latency=1", "..."),
            "srt+relay://cam1?passphrase=...&latency=1"
        );
    }

    #[test]
    fn stripping_drops_the_passphrase_param() {
        assert_eq!(
            strip_srt_uri_secrets(
                "srt://relay.example.com:9000?streamid=cam1&passphrase=abc&pbkeylen=32"
            ),
            "srt://relay.example.com:9000?streamid=cam1&pbkeylen=32"
        );
        assert_eq!(
            strip_srt_uri_secrets("srt://relay.example.com:9000?passphrase=abc"),
            "srt://relay.example.com:9000"
        );
    }

    #[test]
    fn a_plain_uri_keeps_its_settings_on_the_relay() {
        let uri = srt_relay_uri_from_random(
            &[1; 16],
            &[2; 24],
            srt_uri_query("srt://:5000?mode=listener&rcvlatency=200&localaddress=0.0.0.0&localport=5000&pbkeylen=32&passphrase=old&streamid=x"),
        );
        let parsed = assert_generated(&uri);
        assert_eq!(
            parsed.query,
            format!(
                "passphrase={}&rcvlatency=200&pbkeylen=32",
                base64url(&[2; 24])
            )
        );
        assert_eq!(srt_uri_query("srt://:5000"), "");
    }

    #[test]
    fn renewing_gives_a_new_id_and_passphrase_and_keeps_settings() {
        let old = "srt+relay://cam1camcamcamcamcam?passphrase=oldoldoldold&latency=200&pbkeylen=32";
        let new = renew_srt_relay_uri(old).unwrap();
        let parsed = assert_generated(&new);
        assert_ne!(parsed.id, "cam1camcamcamcamcam");
        assert_ne!(parsed.passphrase().as_deref(), Some("oldoldoldold"));
        assert!(parsed.query.ends_with("&pbkeylen=32"), "{new}");
        assert!(!parsed.has_param("latency"), "{new}");
        assert_eq!(renew_srt_relay_uri("srt://:5000?mode=listener"), None);
        assert_eq!(renew_srt_relay_uri("srt+relay://bad id"), None);
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
