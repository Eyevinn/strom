//! The `srt_uri` property of the SRT blocks as structured fields.
//!
//! The GUI edits an SRT block's connection as a mode (listener, caller,
//! relay), host, port, stream id, passphrase and key length. The block
//! property stays the single `srt_uri` string: [`SrtUriFields::parse`] reads
//! the fields out of it and [`SrtUriFields::to_uri`] writes them back. Query
//! parameters the fields do not cover (`localaddress`, `rcvlatency`, ...) are
//! kept as they were written.
//!
//! [`validate_srt_uri`] checks an `srt_uri` against what GStreamer's SRT
//! elements actually read from it. The GUI shows its result next to the
//! fields, and the SRT blocks refuse to build on an error, so a URI that
//! GStreamer would read differently from how it is written fails loudly
//! instead of libsrt running with other settings.
//!
//! [`parse_srt_uri`] reads what a GStreamer SRT element opens for an
//! `srt://` URI (its mode and the local port it binds); it is the one
//! GStreamer-exact reader, used for the capability warnings.
//!
//! How GStreamer reads the URI (`gst_srt_object_set_uri`, 1.22 to 1.28): the
//! scheme must be exactly `srt://`; the query is split on `&` and the first
//! `=`, keys and values are percent-decoded, and keys are matched exactly,
//! lower-case. `mode` values are matched exactly before GLib 2.88 and
//! ignoring ASCII case from 2.88 on. An unknown key is ignored without a
//! word, a value with a malformed escape (or `%00`) is not set at all, and a
//! raw `#` starts the fragment, which is dropped. The query is a hash table,
//! so of a key given twice the last one wins. Without a host the element is
//! a listener whatever `mode` says; with a host and no `mode` it is a caller.
//!
//! Pure string logic, unit-tested here; the frontend only draws the fields.

use crate::api::SrtMode;
use crate::srt_relay::{SRT_RELAY_MIN_SAFE_ID_LEN, SRT_RELAY_SCHEME};

/// The plain SRT URI scheme.
pub const SRT_SCHEME: &str = "srt://";

/// Shortest passphrase libsrt accepts, in bytes.
pub const SRT_PASSPHRASE_MIN_LEN: usize = 10;
/// Longest passphrase libsrt accepts, in bytes (`HAICRYPT_SECRET_MAX_SZ`).
pub const SRT_PASSPHRASE_MAX_LEN: usize = 80;

/// The AES key lengths the GUI offers for `pbkeylen`, in bytes.
pub const SRT_PBKEYLEN_VALUES: &[&str] = &["16", "24", "32"];

/// Every `pbkeylen` libsrt accepts: 0 (the default) or an AES key length.
const SRT_PBKEYLEN_ACCEPTED: &[&str] = &["0", "16", "24", "32"];

/// The `mode` values GStreamer reads.
const GST_SRT_MODES: &[&str] = &["caller", "listener", "rendezvous"];

/// The query keys GStreamer's SRT elements read from the URI: the four
/// `gst_srt_object_set_uri` handles itself and its `srt_options` table
/// (identical in 1.22 and 1.28). Used to name the right spelling when a key
/// differs from one of them only in case.
const GST_SRT_URI_KEYS: &[&str] = &[
    "mode",
    "localaddress",
    "localport",
    "poll-timeout",
    "mss",
    "fc",
    "sndbuf",
    "rcvbuf",
    "maxbw",
    "tsbpdmode",
    "latency",
    "inputbw",
    "oheadbw",
    "passphrase",
    "pbkeylen",
    "ipttl",
    "iptos",
    "tlpktdrop",
    "snddropdelay",
    "nakreport",
    "conntimeo",
    "drifttracer",
    "lossmaxttl",
    "rcvlatency",
    "peerlatency",
    "minversion",
    "streamid",
    "congestion",
    "messageapi",
    "payloadsize",
    "transtype",
    "kmrefreshrate",
    "kmpreannounce",
    "enforcedencryption",
    "ipv6only",
    "peeridletimeo",
    "bindtodevice",
    "packetfilter",
    "retransmitalgo",
];

/// Parameters the relay connection drops when an address moves onto the
/// relay: they bind a direct connection to a local address or adapter.
const DIRECT_ONLY_PARAMS: &[&str] = &["localaddress", "localport", "adapter"];

/// How an SRT block connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrtConnectionMode {
    /// Strom waits on a port for the other side to dial in.
    Listener,
    /// Strom dials the other side.
    #[default]
    Caller,
    /// Strom dials a configured SRT relay (`srt+relay://<stream id>[@<relay>]`).
    Relay,
}

impl SrtConnectionMode {
    /// The modes in the order the GUI offers them.
    pub const ALL: [SrtConnectionMode; 3] = [
        SrtConnectionMode::Listener,
        SrtConnectionMode::Caller,
        SrtConnectionMode::Relay,
    ];

    /// Label for the GUI.
    pub fn label(self) -> &'static str {
        match self {
            SrtConnectionMode::Listener => "Listener",
            SrtConnectionMode::Caller => "Caller",
            SrtConnectionMode::Relay => "Relay",
        }
    }
}

/// Why an `srt_uri` does not fit the structured fields. The raw URI is then
/// the only way to edit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrtUriParseError {
    /// Neither `srt://` nor `srt+relay://`.
    UnknownScheme,
    /// A path or user info after the host, or a malformed `[...]` host.
    UnsupportedAddress,
    /// A `mode` other than `listener` or `caller` (`rendezvous`, say).
    UnsupportedMode(String),
    /// A parameter the fields hold appears more than once.
    DuplicateParam(String),
    /// `mode` or `streamid` in an `srt+relay://` URI.
    ReservedRelayParam(String),
}

impl std::fmt::Display for SrtUriParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SrtUriParseError::UnknownScheme => {
                write!(f, "not an {SRT_SCHEME} or {SRT_RELAY_SCHEME} URI")
            }
            SrtUriParseError::UnsupportedAddress => {
                write!(f, "the address is not a plain host:port")
            }
            SrtUriParseError::UnsupportedMode(mode) => match mode_spelling(mode) {
                Some(expected) => {
                    write!(
                        f,
                        "mode '{mode}' is not read by GStreamer: use '{expected}'"
                    )
                }
                None => write!(f, "mode '{mode}' is not supported here"),
            },
            SrtUriParseError::DuplicateParam(name) => write!(f, "'{name}' is given more than once"),
            SrtUriParseError::ReservedRelayParam(name) => {
                write!(f, "'{name}' is not allowed in an {SRT_RELAY_SCHEME} URI")
            }
        }
    }
}

impl std::error::Error for SrtUriParseError {}

/// The fields of an `srt_uri`.
///
/// Besides the fields it remembers the query parameters in the order they
/// were written, so [`SrtUriFields::to_uri`] changes only what was edited.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SrtUriFields {
    pub mode: SrtConnectionMode,
    /// Host without `[ ]` brackets. The address to dial for a caller, the
    /// bind address for a listener (usually empty). Not used by the relay.
    pub host: String,
    /// Port as typed. Not used by the relay.
    pub port: String,
    /// `streamid` for listener and caller; the relay stream id (the part
    /// after `srt+relay://`) for the relay. Empty = none.
    pub stream_id: String,
    /// The relay name after `@` in a relay URI, as written. Empty = the
    /// `default` relay. Kept across mode switches, written only for the relay.
    pub relay: String,
    /// The decoded passphrase. Empty = unencrypted.
    pub passphrase: String,
    /// The decoded `pbkeylen`, if given.
    pub pbkeylen: Option<String>,
    /// The query parameters as written, in order.
    params: Vec<Param>,
}

/// A query parameter of the parsed URI, `key[=value]` as written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Param {
    /// One the fields hold. Written back as it was while its field still has
    /// the value it was read with.
    Field(Known, String),
    /// Any other, written back unchanged.
    Other(String),
}

/// Decode `%XX` escapes; anything malformed is kept as it is.
fn percent_decode(s: &str) -> String {
    String::from_utf8_lossy(&urlencoding::decode_binary(s.as_bytes())).into_owned()
}

/// Strip a case-insensitive scheme prefix.
fn strip_scheme<'a>(uri: &'a str, scheme: &str) -> Option<&'a str> {
    (uri.len() >= scheme.len()
        && uri.as_bytes()[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes()))
    .then(|| &uri[scheme.len()..])
}

/// Split `host[:port]`, `[v6]` or `[v6]:port` into host and port.
fn split_authority(authority: &str) -> Result<(String, String), SrtUriParseError> {
    if authority.contains(['/', '@']) {
        return Err(SrtUriParseError::UnsupportedAddress);
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or(SrtUriParseError::UnsupportedAddress)?;
        let port = match after {
            "" => "",
            _ => after
                .strip_prefix(':')
                .ok_or(SrtUriParseError::UnsupportedAddress)?,
        };
        return Ok((host.to_string(), port.to_string()));
    }
    if authority.contains(['[', ']']) || authority.matches(':').count() > 1 {
        return Err(SrtUriParseError::UnsupportedAddress);
    }
    Ok(match authority.split_once(':') {
        Some((host, port)) => (host.to_string(), port.to_string()),
        None => (authority.to_string(), String::new()),
    })
}

/// The parameters the fields hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Mode,
    StreamId,
    Passphrase,
    Pbkeylen,
}

impl Known {
    /// The field a raw query key names. Matched after decoding and exactly,
    /// as GStreamer matches it: `PassPhrase` is no passphrase.
    fn of(key: &str) -> Option<Known> {
        let key = percent_decode(key);
        [
            ("mode", Known::Mode),
            ("streamid", Known::StreamId),
            ("passphrase", Known::Passphrase),
            ("pbkeylen", Known::Pbkeylen),
        ]
        .into_iter()
        .find(|(name, _)| key == *name)
        .map(|(_, known)| known)
    }

    fn name(self) -> &'static str {
        match self {
            Known::Mode => "mode",
            Known::StreamId => "streamid",
            Known::Passphrase => "passphrase",
            Known::Pbkeylen => "pbkeylen",
        }
    }
}

/// Whether `id` is a valid relay stream id: 1 to 64 characters from
/// `[A-Za-z0-9_-]`.
pub fn is_valid_relay_stream_id(id: &str) -> bool {
    crate::relay::is_valid_relay_id(id)
}

/// Whether a relay stream id is long enough not to be guessed.
pub fn is_short_relay_stream_id(id: &str) -> bool {
    id.len() < SRT_RELAY_MIN_SAFE_ID_LEN
}

/// A problem with a passphrase, or `None` when it is fine. Empty is fine:
/// it means no encryption.
pub fn passphrase_problem(passphrase: &str) -> Option<String> {
    let len = passphrase.len();
    if len == 0 || (SRT_PASSPHRASE_MIN_LEN..=SRT_PASSPHRASE_MAX_LEN).contains(&len) {
        None
    } else {
        Some(format!(
            "Use {SRT_PASSPHRASE_MIN_LEN} to {SRT_PASSPHRASE_MAX_LEN} bytes ({len} now), or leave it empty for no encryption."
        ))
    }
}

impl SrtUriFields {
    /// Read the fields out of an `srt_uri`.
    ///
    /// A plain `srt://` URI without `mode` is a caller when it has a host
    /// and a listener when it has none, as for `srtsrc` and `srtsink`. Keys
    /// and `mode` values are matched exactly, as GStreamer matches them, so
    /// `PassPhrase` is just another parameter and `mode=Listener` is
    /// refused; [`validate_srt_uri`] explains both. The relay stream id is taken as written, valid or not, so
    /// that the GUI can point at a bad one instead of falling back to the raw
    /// field; [`is_valid_relay_stream_id`] checks it.
    pub fn parse(uri: &str) -> Result<SrtUriFields, SrtUriParseError> {
        let uri = uri.trim();
        let (relay, rest) = if let Some(rest) = strip_scheme(uri, SRT_RELAY_SCHEME) {
            (true, rest)
        } else if let Some(rest) = strip_scheme(uri, SRT_SCHEME) {
            (false, rest)
        } else {
            return Err(SrtUriParseError::UnknownScheme);
        };
        let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));

        let mut fields = SrtUriFields {
            mode: if relay {
                SrtConnectionMode::Relay
            } else {
                SrtConnectionMode::Caller
            },
            ..SrtUriFields::default()
        };
        if relay {
            let (id, relay_name, _) = crate::relay::split_relay_uri(authority);
            fields.stream_id = id.to_string();
            fields.relay = relay_name.unwrap_or_default().to_string();
        } else {
            (fields.host, fields.port) = split_authority(authority)?;
            if fields.host.is_empty() {
                fields.mode = SrtConnectionMode::Listener;
            }
        }

        let mut seen: Vec<Known> = Vec::new();
        for param in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = param.split_once('=').unwrap_or((param, ""));
            let Some(known) = Known::of(key) else {
                fields.params.push(Param::Other(param.to_string()));
                continue;
            };
            if seen.contains(&known) {
                return Err(SrtUriParseError::DuplicateParam(known.name().to_string()));
            }
            seen.push(known);
            fields.params.push(Param::Field(known, param.to_string()));
            let value = percent_decode(value);
            match known {
                Known::Mode | Known::StreamId if relay => {
                    return Err(SrtUriParseError::ReservedRelayParam(
                        known.name().to_string(),
                    ));
                }
                Known::Mode => {
                    fields.mode = match value.as_str() {
                        "listener" => SrtConnectionMode::Listener,
                        "caller" => SrtConnectionMode::Caller,
                        _ => return Err(SrtUriParseError::UnsupportedMode(value)),
                    };
                }
                Known::StreamId => fields.stream_id = value,
                Known::Passphrase => fields.passphrase = value,
                Known::Pbkeylen => fields.pbkeylen = Some(value),
            }
        }
        Ok(fields)
    }

    /// The value a field parameter has now, `None` when it is left out:
    /// empty, or not a query parameter in this mode.
    fn field_value(&self, known: Known) -> Option<String> {
        let relay = self.mode == SrtConnectionMode::Relay;
        let value = match known {
            Known::Mode if relay => return None,
            Known::Mode => match self.mode {
                SrtConnectionMode::Listener => "listener",
                _ => "caller",
            }
            .to_string(),
            Known::StreamId if relay => return None,
            Known::StreamId => self.stream_id.clone(),
            Known::Passphrase => self.passphrase.clone(),
            Known::Pbkeylen => self.pbkeylen.clone().unwrap_or_default(),
        };
        (!value.is_empty()).then_some(value)
    }

    /// Write the fields back as an `srt_uri`.
    ///
    /// Parameters keep their order, and a parameter whose value did not
    /// change is written exactly as it was read. A changed value is
    /// percent-encoded where needed; empty stream id, passphrase and key
    /// length are left out; a new parameter goes at the end, except `mode`,
    /// which goes first. An IPv6 host gets its brackets.
    pub fn to_uri(&self) -> String {
        let mut uri = match self.mode {
            SrtConnectionMode::Relay if self.relay.is_empty() => {
                format!("{SRT_RELAY_SCHEME}{}", self.stream_id)
            }
            SrtConnectionMode::Relay => {
                format!("{SRT_RELAY_SCHEME}{}@{}", self.stream_id, self.relay)
            }
            SrtConnectionMode::Listener | SrtConnectionMode::Caller => {
                let host = if self.host.contains(':') {
                    format!("[{}]", self.host)
                } else {
                    self.host.clone()
                };
                let mut uri = format!("{SRT_SCHEME}{host}");
                if !self.port.is_empty() {
                    uri.push(':');
                    uri.push_str(&self.port);
                }
                uri
            }
        };
        let encoded =
            |known: Known, value: &str| format!("{}={}", known.name(), urlencoding::encode(value));
        let mut params: Vec<String> = Vec::new();
        let mut written: Vec<Known> = Vec::new();
        for param in &self.params {
            match param {
                Param::Other(raw) => params.push(raw.clone()),
                Param::Field(known, raw) => {
                    written.push(*known);
                    let Some(value) = self.field_value(*known) else {
                        continue;
                    };
                    let read = percent_decode(raw.split_once('=').map_or("", |(_, v)| v));
                    let unchanged = read == value;
                    params.push(if unchanged {
                        raw.clone()
                    } else {
                        encoded(*known, &value)
                    });
                }
            }
        }
        for known in [
            Known::Mode,
            Known::StreamId,
            Known::Passphrase,
            Known::Pbkeylen,
        ] {
            if written.contains(&known) {
                continue;
            }
            let Some(value) = self.field_value(known) else {
                continue;
            };
            let param = encoded(known, &value);
            if known == Known::Mode {
                params.insert(0, param);
            } else {
                params.push(param);
            }
        }
        if !params.is_empty() {
            uri.push('?');
            uri.push_str(&params.join("&"));
        }
        uri
    }

    /// The query parameters the fields do not hold, as written.
    pub fn other_params(&self) -> impl Iterator<Item = &str> {
        self.params.iter().filter_map(|p| match p {
            Param::Other(raw) => Some(raw.as_str()),
            Param::Field(..) => None,
        })
    }

    /// Switch to another mode, keeping what still applies: the passphrase,
    /// key length and other parameters always, the port between listener and
    /// caller. The stream id is kept unless the relay cannot use it, in which
    /// case `new_relay_id` gives one (as it does when there is none). Moving
    /// onto the relay drops `localaddress`, `localport` and `adapter`, which
    /// bind a direct connection.
    pub fn switch_mode(&mut self, mode: SrtConnectionMode, new_relay_id: impl FnOnce() -> String) {
        if self.mode == mode {
            return;
        }
        if mode == SrtConnectionMode::Relay {
            if !is_valid_relay_stream_id(&self.stream_id) {
                self.stream_id = new_relay_id();
            }
            self.params.retain(|param| {
                let Param::Other(raw) = param else {
                    return true;
                };
                let key = raw.split_once('=').map_or(raw.as_str(), |(k, _)| k);
                !DIRECT_ONLY_PARAMS.contains(&percent_decode(key).as_str())
            });
        }
        if self.mode == SrtConnectionMode::Listener || mode == SrtConnectionMode::Listener {
            // The bind address is no address to dial, and the host a caller
            // (or a relay block, kept from before) dials is no local address
            // to bind: a listener starts on all addresses.
            self.host.clear();
        }
        self.mode = mode;
    }
}

/// The right spelling of a `mode` value that differs from one GStreamer
/// reads only in case (`Listener` for `listener`), else `None`.
fn mode_spelling(mode: &str) -> Option<&'static str> {
    GST_SRT_MODES
        .iter()
        .find(|m| **m != mode && m.eq_ignore_ascii_case(mode))
        .copied()
}

/// Decode `%XX` escapes as `g_uri_unescape_string` does: `None` for a `%`
/// not followed by two hex digits and for `%00`. GStreamer then reads no
/// value at all. `+` stays a `+`.
pub(crate) fn gst_unescape(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            if byte == 0 {
                return None;
            }
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// The parameters of a query string as GStreamer splits them: on `&`, then
/// on the first `=`, empty segments skipped. Yields the raw key, the
/// decoded key (`None` if malformed) and the raw value (`None` without `=`).
pub(crate) fn gst_query_params(
    query: &str,
) -> impl Iterator<Item = (&str, Option<String>, Option<&str>)> {
    query.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (key, value) = match p.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (p, None),
        };
        (key, gst_unescape(key), value)
    })
}

/// What a GStreamer SRT element (`srtsrc`, `srtsink`) opens for an `srt://`
/// URI: the mode, and the local port a remote peer has to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrtUriEndpoint {
    /// The mode the element opens in.
    pub mode: SrtMode,
    /// The local port the element binds for a remote peer to reach, in
    /// listener and rendezvous mode. `None` in caller mode, and when it binds
    /// an ephemeral port or the port cannot be told (out of range, a
    /// `localport` GStreamer cannot read).
    pub local_port: Option<u16>,
}

/// `GST_SRT_DEFAULT_PORT`.
const GST_SRT_DEFAULT_PORT: u64 = 7001;

/// `GstSRTConnectionMode`, including `none`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GstMode {
    None,
    Caller,
    Listener,
    Rendezvous,
}

/// Read an `srt://` URI the way the GStreamer SRT elements open it. `None`
/// for another scheme or a URI `gst_uri_from_string()` rejects.
///
/// It mirrors `gst_srt_object_set_uri()`, `gst_srt_object_validate_parameters()`
/// and `gst_srt_object_open_internal()` in gst-plugins-bad
/// `ext/srt/gstsrtobject.c` (identical in 1.22 and 1.28), with the URI split
/// as `gst_uri_from_string()` does in `gst/gsturi.c`:
///
/// - The query is split as [`gst_query_params`] does. Keys and values are
///   percent-decoded ([`gst_unescape`]); a malformed escape or `%00` makes
///   them NULL. The query is a hash table, so the last occurrence of a key
///   wins, even when its value is not usable.
/// - Keys are compared case-sensitively (`g_strcmp0`). The `mode` value is
///   looked up with `g_enum_get_value_by_nick()`, which ignores ASCII case
///   since GLib 2.88 (the Docker image) and is case-sensitive before
///   (Debian 12, the floor). This follows 2.88; [`validate_srt_uri`] refuses
///   a wrong-case mode because the two disagree. A value that is no nick is
///   ignored, and the default stays.
/// - The default mode is caller when the URI has a host and listener when it
///   has none. A wildcard host (`0.0.0.0`, `[::]`) is a host like any other.
/// - A URI without a host always opens as a listener, whatever `mode=` says.
/// - Listener and rendezvous bind `localport=` when given, otherwise the URI
///   port, otherwise 7001 (port 0 in the URI means no port). With an explicit
///   caller (or `none`) mode and no host, no `localport` is filled in, and the
///   forced listener binds port 0.
pub fn parse_srt_uri(uri: &str) -> Option<SrtUriEndpoint> {
    // set_uri checks the scheme prefix without trimming whitespace.
    let rest = uri.strip_prefix(SRT_SCHEME)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host_port = authority.split_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = match host_port.strip_prefix('[') {
        Some(v6) => v6.split_once(']')?,
        None => host_port.split_at(host_port.find(':').unwrap_or(host_port.len())),
    };
    // The port must be `:` and digits only. 0 or none is GST_URI_NO_PORT.
    let uri_port = match port {
        "" => 0,
        p => {
            let digits = p.strip_prefix(':')?;
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            digits.bytes().fold(0u64, |n, b| {
                n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
            })
        }
    };
    let has_host = !host.is_empty();

    // The query runs from the first `?` to `#`, after the path.
    let query = match tail.find(['?', '#']) {
        Some(i) if tail.as_bytes()[i] == b'?' => tail[i + 1..].split('#').next(),
        _ => None,
    };
    // Last occurrence wins. `Some(None)`: present, but the value is NULL
    // (no `=`, or a malformed escape).
    let mut mode_value: Option<Option<String>> = None;
    let mut localport_value: Option<Option<String>> = None;
    for (_, key, value) in query.into_iter().flat_map(gst_query_params) {
        let value = value.and_then(gst_unescape);
        match key.as_deref() {
            Some("mode") => mode_value = Some(value),
            Some("localport") => localport_value = Some(value),
            _ => {}
        }
    }

    let mut mode = if has_host {
        GstMode::Caller
    } else {
        GstMode::Listener
    };
    if let Some(m) = mode_value.flatten().and_then(|v| mode_from_nick(&v)) {
        mode = m;
    }

    // `None`: the port cannot be told.
    let local_port: Option<u64> = match localport_value {
        Some(value) => value.and_then(|v| strtoll_unsigned(v.as_bytes())),
        None if matches!(mode, GstMode::Listener | GstMode::Rendezvous) => Some(match uri_port {
            0 => GST_SRT_DEFAULT_PORT,
            p => p,
        }),
        None => Some(0),
    };

    let mode = match (has_host, mode) {
        (false, _) | (true, GstMode::Listener) => SrtMode::Listener,
        (true, GstMode::Rendezvous) => SrtMode::Rendezvous,
        (true, GstMode::Caller | GstMode::None) => SrtMode::Caller,
    };
    let local_port = match mode {
        SrtMode::Caller => None,
        // Port 0 binds an ephemeral port (listener) or none (rendezvous).
        _ => local_port
            .filter(|&p| p != 0)
            .and_then(|p| u16::try_from(p).ok()),
    };
    Some(SrtUriEndpoint { mode, local_port })
}

/// `g_enum_get_value_by_nick()` on `GstSRTConnectionMode`, GLib 2.88 and later.
fn mode_from_nick(nick: &str) -> Option<GstMode> {
    [
        ("none", GstMode::None),
        ("caller", GstMode::Caller),
        ("listener", GstMode::Listener),
        ("rendezvous", GstMode::Rendezvous),
    ]
    .into_iter()
    .find(|(n, _)| n.eq_ignore_ascii_case(nick))
    .map(|(_, m)| m)
}

/// `g_ascii_strtoll()` as GStreamer reads `localport`: leading whitespace, a
/// sign, the leading digits, 0 when there are none. `None` for a negative
/// value, which wraps to a port nobody meant.
fn strtoll_unsigned(s: &[u8]) -> Option<u64> {
    let s = s.trim_ascii_start();
    let (negative, s) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let n = s
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0u64, |n, b| {
            n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
        });
    (!negative || n == 0).then_some(n)
}

/// Why GStreamer would not use an `srt_uri` as written. The SRT blocks
/// refuse to build on one; the GUI shows it.
///
/// No variant carries a parameter value that may be secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrtUriError {
    /// Neither `srt://` nor `srt+relay://`.
    UnknownScheme,
    /// `SRT://` and the like: GStreamer's SRT elements accept only `srt://`
    /// and refuse any other spelling.
    SchemeCase,
    /// A raw `#`: GStreamer reads the rest of the URI as its fragment and
    /// drops it, so a `streamid=#!::r=...` or a passphrase with a `#` is
    /// cut short.
    Fragment,
    /// A key or value with a malformed escape (`%zz`, `%2`) or `%00`, which
    /// GStreamer does not set. Holds the key as written.
    BadEscape(String),
    /// A key that GStreamer ignores because it differs only in case from
    /// one it reads.
    KeyCase {
        written: String,
        expected: &'static str,
    },
    /// A `mode` value that differs only in case from one GStreamer reads.
    ModeCase {
        written: String,
        expected: &'static str,
    },
    /// A `mode` that is none of `caller`, `listener` and `rendezvous`.
    UnknownMode(String),
    /// A key given more than once (after decoding). GStreamer would take
    /// the last one; Strom refuses the URI instead, because a second
    /// `passphrase` or `mode` is a mistake, and its fields and helpers would
    /// otherwise have to guess which one was meant.
    DuplicateParam(String),
    /// A passphrase outside 10 to 80 bytes (its length here). Empty is no
    /// encryption and fine.
    PassphraseLength(usize),
    /// A `pbkeylen` other than 0, 16, 24 or 32.
    Pbkeylen(String),
}

impl std::fmt::Display for SrtUriError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SrtUriError::UnknownScheme => {
                write!(f, "not an {SRT_SCHEME} or {SRT_RELAY_SCHEME} URI")
            }
            SrtUriError::SchemeCase => write!(
                f,
                "GStreamer accepts the scheme only as '{SRT_SCHEME}', in lower case"
            ),
            SrtUriError::Fragment => write!(
                f,
                "'#' ends the URI for GStreamer, which drops everything after it: write it as %23"
            ),
            SrtUriError::BadEscape(key) => write!(
                f,
                "'{key}' has a malformed %-escape or %00, so GStreamer does not set it: \
                 write '%' as %25"
            ),
            SrtUriError::KeyCase { written, expected } => {
                write!(f, "'{written}' is not read by GStreamer: use '{expected}'")
            }
            SrtUriError::ModeCase { written, expected } => {
                write!(
                    f,
                    "mode '{written}' is not read by GStreamer: use '{expected}'"
                )
            }
            SrtUriError::UnknownMode(mode) => write!(
                f,
                "mode '{mode}' is not one of {}",
                GST_SRT_MODES.join(", ")
            ),
            SrtUriError::DuplicateParam(key) => write!(f, "'{key}' is given more than once"),
            SrtUriError::PassphraseLength(len) => write!(
                f,
                "the passphrase must be {SRT_PASSPHRASE_MIN_LEN} to {SRT_PASSPHRASE_MAX_LEN} \
                 bytes ({len} now), or empty for no encryption"
            ),
            SrtUriError::Pbkeylen(value) => write!(
                f,
                "pbkeylen '{value}' is not one of {}",
                SRT_PBKEYLEN_ACCEPTED.join(", ")
            ),
        }
    }
}

impl std::error::Error for SrtUriError {}

/// Something in an `srt_uri` that has no effect. Logged when the block
/// builds and shown in the GUI; it does not stop the build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrtUriWarning {
    /// `latency=`: the SRT blocks set the element's `latency` property after
    /// its `uri`, and setting `uri` resets the latency, so the block's own
    /// Latency property always wins. (`rcvlatency` and `peerlatency` do
    /// apply: they are set after `latency`.)
    IgnoredLatency,
}

impl std::fmt::Display for SrtUriWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SrtUriWarning::IgnoredLatency => write!(
                f,
                "'latency' in the URI is ignored: the block's Latency property is used"
            ),
        }
    }
}

/// Check an `srt_uri` (`srt://` or `srt+relay://`) against how GStreamer
/// reads it (see the module docs): an error for what GStreamer would read
/// differently from how it is written, or what libsrt would refuse, and
/// warnings for what has no effect.
///
/// Surrounding whitespace is ignored: the blocks trim the URI. The relay
/// specifics (stream id, `mode` and `streamid` reserved) are checked by
/// [`crate::srt_relay::parse_srt_relay_uri`].
pub fn validate_srt_uri(uri: &str) -> Result<Vec<SrtUriWarning>, SrtUriError> {
    let uri = uri.trim();
    let rest = if let Some(rest) = strip_scheme(uri, SRT_RELAY_SCHEME) {
        rest
    } else if let Some(rest) = uri.strip_prefix(SRT_SCHEME) {
        rest
    } else if strip_scheme(uri, SRT_SCHEME).is_some() {
        return Err(SrtUriError::SchemeCase);
    } else {
        return Err(SrtUriError::UnknownScheme);
    };
    if rest.contains('#') {
        return Err(SrtUriError::Fragment);
    }
    let query = rest.split_once('?').map_or("", |(_, q)| q);

    let mut warnings = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (raw_key, key, value) in gst_query_params(query) {
        let Some(key) = key else {
            return Err(SrtUriError::BadEscape(raw_key.to_string()));
        };
        let value = match value {
            Some(raw) => {
                Some(gst_unescape(raw).ok_or_else(|| SrtUriError::BadEscape(key.clone()))?)
            }
            None => None,
        };
        if !GST_SRT_URI_KEYS.contains(&key.as_str()) {
            if let Some(expected) = GST_SRT_URI_KEYS
                .iter()
                .find(|k| k.eq_ignore_ascii_case(&key))
            {
                return Err(SrtUriError::KeyCase {
                    written: key,
                    expected,
                });
            }
        }
        if seen.contains(&key) {
            return Err(SrtUriError::DuplicateParam(key));
        }
        let value = value.unwrap_or_default();
        match key.as_str() {
            "mode" if !GST_SRT_MODES.contains(&value.as_str()) => {
                return Err(match mode_spelling(&value) {
                    Some(expected) => SrtUriError::ModeCase {
                        written: value,
                        expected,
                    },
                    None => SrtUriError::UnknownMode(value),
                });
            }
            "passphrase" => {
                let len = value.len();
                if len != 0 && !(SRT_PASSPHRASE_MIN_LEN..=SRT_PASSPHRASE_MAX_LEN).contains(&len) {
                    return Err(SrtUriError::PassphraseLength(len));
                }
            }
            "pbkeylen" if !value.is_empty() && !SRT_PBKEYLEN_ACCEPTED.contains(&value.as_str()) => {
                return Err(SrtUriError::Pbkeylen(value));
            }
            "latency" => warnings.push(SrtUriWarning::IgnoredLatency),
            _ => {}
        }
        seen.push(key);
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(uri: &str) -> SrtUriFields {
        SrtUriFields::parse(uri).unwrap_or_else(|e| panic!("{uri}: {e}"))
    }

    fn round_trip(uri: &str) {
        assert_eq!(parse(uri).to_uri(), uri);
    }

    #[test]
    fn listener_round_trip() {
        let f = parse("srt://:5000?mode=listener");
        assert_eq!(f.mode, SrtConnectionMode::Listener);
        assert_eq!(f.host, "");
        assert_eq!(f.port, "5000");
        round_trip("srt://:5000?mode=listener");
        round_trip("srt://0.0.0.0:5000?mode=listener&streamid=cam1&passphrase=abcdefghijk");
    }

    #[test]
    fn caller_round_trip() {
        let f = parse("srt://192.0.2.1:4000?mode=caller&streamid=cam1&rcvlatency=200");
        assert_eq!(f.mode, SrtConnectionMode::Caller);
        assert_eq!(f.host, "192.0.2.1");
        assert_eq!(f.port, "4000");
        assert_eq!(f.stream_id, "cam1");
        assert_eq!(f.other_params().collect::<Vec<_>>(), ["rcvlatency=200"]);
        round_trip("srt://192.0.2.1:4000?mode=caller&streamid=cam1&rcvlatency=200");
    }

    #[test]
    fn without_mode_a_host_makes_a_caller_and_no_host_a_listener() {
        // As GStreamer: caller when the URI has a host, listener when not.
        let f = parse("srt://example.com:4000");
        assert_eq!(f.mode, SrtConnectionMode::Caller);
        assert_eq!(f.to_uri(), "srt://example.com:4000?mode=caller");
        let f = parse("srt://:5000?latency=200");
        assert_eq!(f.mode, SrtConnectionMode::Listener);
        assert_eq!(f.to_uri(), "srt://:5000?mode=listener&latency=200");
    }

    #[test]
    fn relay_round_trip() {
        let f = parse(
            "srt+relay://Ab3kQ9xR2Ab3kQ9xR2Ab?passphrase=abcdefghijkl&pbkeylen=32&rcvlatency=200",
        );
        assert_eq!(f.mode, SrtConnectionMode::Relay);
        assert_eq!(f.stream_id, "Ab3kQ9xR2Ab3kQ9xR2Ab");
        assert_eq!(f.passphrase, "abcdefghijkl");
        assert_eq!(f.pbkeylen.as_deref(), Some("32"));
        round_trip(
            "srt+relay://Ab3kQ9xR2Ab3kQ9xR2Ab?passphrase=abcdefghijkl&pbkeylen=32&rcvlatency=200",
        );
        round_trip("srt+relay://cam1");
        let f = parse("srt+relay://cam1@eu-north?passphrase=abcdefghijkl");
        assert_eq!(f.stream_id, "cam1");
        assert_eq!(f.relay, "eu-north");
        round_trip("srt+relay://cam1@eu-north?passphrase=abcdefghijkl");
        round_trip("srt+relay://cam1@default");
        let mut f = parse("srt+relay://cam1@eu-north");
        f.relay.clear();
        assert_eq!(f.to_uri(), "srt+relay://cam1");
        f.relay = "us".to_string();
        f.stream_id = "cam2".to_string();
        assert_eq!(f.to_uri(), "srt+relay://cam2@us");
    }

    #[test]
    fn unknown_params_are_kept_as_written() {
        let uri = "srt://h:1?mode=caller&passphrase=abcdefghijk&Latency=200&x=a%26b&flag&localaddress=0.0.0.0";
        let f = parse(uri);
        assert_eq!(
            f.other_params().collect::<Vec<_>>(),
            ["Latency=200", "x=a%26b", "flag", "localaddress=0.0.0.0"]
        );
        assert_eq!(f.to_uri(), uri);
    }

    #[test]
    fn order_and_spelling_survive_an_edit() {
        // Only the edited parameter is rewritten; the rest stays as pasted.
        let mut f = parse("srt://h:1?rcvlatency=200&pass%70hrase=abc%2Ddefghij&mode=caller&x=1");
        assert_eq!(f.passphrase, "abc-defghij");
        f.port = "2".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?rcvlatency=200&pass%70hrase=abc%2Ddefghij&mode=caller&x=1"
        );
        f.passphrase = "new passphrase".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?rcvlatency=200&passphrase=new%20passphrase&mode=caller&x=1"
        );
        // A cleared field drops out; a new one goes at the end.
        f.passphrase.clear();
        f.stream_id = "cam1".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?rcvlatency=200&mode=caller&x=1&streamid=cam1"
        );
        // A new mode goes first.
        let mut f = parse("srt://h:1?rcvlatency=200");
        f.switch_mode(SrtConnectionMode::Listener, String::new);
        assert_eq!(f.to_uri(), "srt://:1?mode=listener&rcvlatency=200");
    }

    #[test]
    fn passphrase_with_special_characters() {
        let mut f = parse("srt://h:1?mode=caller");
        f.passphrase = "a&b c#d=e%f+g/h?".to_string();
        let uri = f.to_uri();
        assert_eq!(
            uri,
            "srt://h:1?mode=caller&passphrase=a%26b%20c%23d%3De%25f%2Bg%2Fh%3F"
        );
        assert_eq!(parse(&uri).passphrase, "a&b c#d=e%f+g/h?");
        // Decoded on the way in.
        assert_eq!(
            parse("srt+relay://cam1?passphrase=s3cret%26more").passphrase,
            "s3cret&more"
        );
    }

    #[test]
    fn stream_id_is_encoded() {
        let mut f = parse("srt://h:1?mode=caller");
        f.stream_id = "#!::r=cam1,m=publish".to_string();
        let uri = f.to_uri();
        assert_eq!(
            uri,
            "srt://h:1?mode=caller&streamid=%23%21%3A%3Ar%3Dcam1%2Cm%3Dpublish"
        );
        assert_eq!(parse(&uri).stream_id, "#!::r=cam1,m=publish");
    }

    #[test]
    fn empty_fields_are_left_out() {
        let f = parse("srt://h:1?mode=caller&streamid=&passphrase=&pbkeylen=");
        assert_eq!(f.to_uri(), "srt://h:1?mode=caller");
        let relay = SrtUriFields {
            mode: SrtConnectionMode::Relay,
            host: "ignored".to_string(),
            port: "1".to_string(),
            stream_id: "cam1".to_string(),
            ..SrtUriFields::default()
        };
        assert_eq!(relay.to_uri(), "srt+relay://cam1");
        let no_port = SrtUriFields {
            mode: SrtConnectionMode::Caller,
            host: "h".to_string(),
            port: String::new(),
            stream_id: String::new(),
            ..relay
        };
        assert_eq!(no_port.to_uri(), "srt://h?mode=caller");
    }

    #[test]
    fn ipv6_hosts_keep_their_brackets() {
        let f = parse("srt://[2001:db8::1]:4000?mode=caller");
        assert_eq!(f.host, "2001:db8::1");
        assert_eq!(f.port, "4000");
        round_trip("srt://[2001:db8::1]:4000?mode=caller");
        round_trip("srt://[::]:5000?mode=listener");
        let mut f = parse("srt://h:1?mode=caller");
        f.host = "::1".to_string();
        assert_eq!(f.to_uri(), "srt://[::1]:1?mode=caller");
    }

    #[test]
    fn keys_and_mode_are_matched_exactly_as_gstreamer_does() {
        // The scheme is read loosely (the fields write it back in lower
        // case; validate_srt_uri points at it), keys and mode are not.
        let f = parse("  SRT://h:5000?mode=listener ");
        assert_eq!(f.mode, SrtConnectionMode::Listener);
        assert_eq!(parse("SRT+Relay://cam1").mode, SrtConnectionMode::Relay);
        // GStreamer ignores `MODE` and `PassPhrase`, and so do the fields.
        let f = parse("srt://h:1?MODE=listener&PassPhrase=abcdefghijk");
        assert_eq!(f.mode, SrtConnectionMode::Caller);
        assert_eq!(f.passphrase, "");
        assert_eq!(
            f.other_params().collect::<Vec<_>>(),
            ["MODE=listener", "PassPhrase=abcdefghijk"]
        );
        // A mode value in another case is refused, naming the spelling.
        let err = SrtUriFields::parse("srt://h:1?mode=Listener").unwrap_err();
        assert_eq!(
            err,
            SrtUriParseError::UnsupportedMode("Listener".to_string())
        );
        assert_eq!(
            err.to_string(),
            "mode 'Listener' is not read by GStreamer: use 'listener'"
        );
        assert!(SrtUriFields::parse("srt://h:1?mode=%20caller").is_err());
    }

    #[test]
    fn what_the_fields_cannot_hold_is_refused() {
        for (uri, err) in [
            ("udp://:5000", SrtUriParseError::UnknownScheme),
            ("", SrtUriParseError::UnknownScheme),
            ("srt://h:1/path", SrtUriParseError::UnsupportedAddress),
            ("srt://user@h:1", SrtUriParseError::UnsupportedAddress),
            (
                "srt://2001:db8::1:4000",
                SrtUriParseError::UnsupportedAddress,
            ),
            ("srt://[::1:4000", SrtUriParseError::UnsupportedAddress),
            (
                "srt://h:1?mode=rendezvous",
                SrtUriParseError::UnsupportedMode("rendezvous".to_string()),
            ),
            (
                "srt://h:1?passphrase=abcdefghijk&pass%70hrase=x",
                SrtUriParseError::DuplicateParam("passphrase".to_string()),
            ),
            (
                "srt+relay://cam1?mode=caller",
                SrtUriParseError::ReservedRelayParam("mode".to_string()),
            ),
            (
                "srt+relay://cam1?streamid=x",
                SrtUriParseError::ReservedRelayParam("streamid".to_string()),
            ),
        ] {
            assert_eq!(SrtUriFields::parse(uri), Err(err), "{uri}");
        }
    }

    #[test]
    fn a_bad_relay_id_still_parses() {
        let f = parse("srt+relay://bad id?latency=1");
        assert_eq!(f.stream_id, "bad id");
        assert!(!is_valid_relay_stream_id(&f.stream_id));
        assert!(!is_valid_relay_stream_id(""));
        assert!(is_valid_relay_stream_id("Ab3_-x"));
        assert!(is_short_relay_stream_id("Ab3_-x"));
        assert!(!is_short_relay_stream_id("Ab3kQ9xR2Ab3kQ9x"));
    }

    #[test]
    fn switching_modes_keeps_what_applies() {
        let mut f = parse(
            "srt://0.0.0.0:5000?mode=listener&streamid=cam1&passphrase=abcdefghijk&latency=200",
        );
        f.switch_mode(SrtConnectionMode::Caller, || unreachable!());
        assert_eq!(
            f.to_uri(),
            "srt://:5000?mode=caller&streamid=cam1&passphrase=abcdefghijk&latency=200"
        );
        f.host = "example.com".to_string();
        f.switch_mode(SrtConnectionMode::Relay, || unreachable!());
        assert_eq!(
            f.to_uri(),
            "srt+relay://cam1?passphrase=abcdefghijk&latency=200"
        );
        f.switch_mode(SrtConnectionMode::Listener, || unreachable!());
        assert_eq!(
            f.to_uri(),
            "srt://:5000?mode=listener&streamid=cam1&passphrase=abcdefghijk&latency=200"
        );
    }

    #[test]
    fn caller_to_listener_does_not_bind_the_remote_host() {
        let mut f = parse("srt://192.0.2.1:4000?mode=caller");
        f.switch_mode(SrtConnectionMode::Listener, || unreachable!());
        assert_eq!(f.to_uri(), "srt://:4000?mode=listener");
    }

    #[test]
    fn switching_to_the_relay_makes_a_stream_id_and_drops_local_binds() {
        let mut f = parse(
            "srt://h:1?mode=caller&localaddress=0.0.0.0&localport=5000&adapter=1&rcvlatency=200",
        );
        f.switch_mode(SrtConnectionMode::Relay, || "newid".to_string());
        assert_eq!(f.to_uri(), "srt+relay://newid?rcvlatency=200");

        let mut f = parse("srt://h:1?mode=caller&streamid=%23!%3A%3Ar%3Dx");
        f.switch_mode(SrtConnectionMode::Relay, || "newid".to_string());
        assert_eq!(f.stream_id, "newid");
    }

    #[test]
    fn passphrase_length_is_checked() {
        assert_eq!(passphrase_problem(""), None);
        assert_eq!(passphrase_problem(&"a".repeat(10)), None);
        assert_eq!(passphrase_problem(&"a".repeat(80)), None);
        assert!(passphrase_problem("short").is_some());
        assert!(passphrase_problem(&"a".repeat(81)).is_some());
        // Bytes, as libsrt counts them: 41 two-byte characters are 82.
        assert!(passphrase_problem(&"\u{e9}".repeat(41)).is_some());
    }

    fn invalid(uri: &str) -> SrtUriError {
        validate_srt_uri(uri).expect_err(uri)
    }

    fn valid(uri: &str) -> Vec<SrtUriWarning> {
        validate_srt_uri(uri).unwrap_or_else(|e| panic!("{uri}: {e}"))
    }

    #[test]
    fn validate_accepts_only_srt_and_relay_schemes() {
        assert_eq!(valid("srt://:5000?mode=listener"), []);
        assert_eq!(valid(" srt://h:1 "), []);
        assert_eq!(valid("SRT+Relay://cam1?passphrase=abcdefghij"), []);
        assert_eq!(invalid("SRT://h:1"), SrtUriError::SchemeCase);
        assert_eq!(invalid("udp://h:1"), SrtUriError::UnknownScheme);
        assert_eq!(invalid(""), SrtUriError::UnknownScheme);
    }

    #[test]
    fn validate_refuses_a_raw_hash() {
        for uri in [
            "srt://h:1?streamid=#!::r=cam1,m=publish",
            "srt://h:1?passphrase=abcdef#ghijk",
            "srt+relay://cam1#x",
        ] {
            assert_eq!(invalid(uri), SrtUriError::Fragment, "{uri}");
        }
        assert!(SrtUriError::Fragment.to_string().contains("%23"));
        assert_eq!(valid("srt://h:1?streamid=%23!%3A%3Ar%3Dcam1"), []);
    }

    #[test]
    fn validate_refuses_malformed_escapes() {
        for (uri, key) in [
            ("srt://h:1?passphrase=abcdefghij%zz", "passphrase"),
            ("srt://h:1?passphrase=abcdefghij%2", "passphrase"),
            ("srt://h:1?passphrase=abcdefghij%", "passphrase"),
            ("srt://h:1?streamid=a%00b", "streamid"),
            ("srt://h:1?streamid=a%+1b", "streamid"),
            ("srt://h:1?pass%zzphrase=abcdefghij", "pass%zzphrase"),
        ] {
            assert_eq!(
                invalid(uri),
                SrtUriError::BadEscape(key.to_string()),
                "{uri}"
            );
        }
        // The message names the key, never the secret value.
        let msg = invalid("srt://h:1?passphrase=s3cretvalue%zz").to_string();
        assert!(!msg.contains("s3cret"), "{msg}");
        assert_eq!(valid("srt://h:1?passphrase=abc%2Bdef+ghij%25"), []);
    }

    #[test]
    fn validate_refuses_keys_in_another_case() {
        for (uri, written, expected) in [
            (
                "srt://h:1?PassPhrase=abcdefghij",
                "PassPhrase",
                "passphrase",
            ),
            ("srt://h:1?Mode=listener", "Mode", "mode"),
            ("srt://h:1?LocalPort=5000", "LocalPort", "localport"),
            ("srt://h:1?StreamID=cam1", "StreamID", "streamid"),
            ("srt://h:1?Latency=200", "Latency", "latency"),
        ] {
            assert_eq!(
                invalid(uri),
                SrtUriError::KeyCase {
                    written: written.to_string(),
                    expected
                },
                "{uri}"
            );
        }
        assert_eq!(
            invalid("srt://h:1?PassPhrase=x").to_string(),
            "'PassPhrase' is not read by GStreamer: use 'passphrase'"
        );
        // A key GStreamer does not know at all is left alone.
        assert_eq!(valid("srt://h:1?foo=1&Bar"), []);
    }

    #[test]
    fn validate_checks_the_mode() {
        for mode in ["caller", "listener", "rendezvous"] {
            assert_eq!(valid(&format!("srt://h:1?mode={mode}")), []);
        }
        assert_eq!(
            invalid("srt://h:1?mode=Listener"),
            SrtUriError::ModeCase {
                written: "Listener".to_string(),
                expected: "listener"
            }
        );
        assert_eq!(
            invalid("srt://h:1?mode=push"),
            SrtUriError::UnknownMode("push".to_string())
        );
        assert_eq!(
            invalid("srt://h:1?mode=%20caller"),
            SrtUriError::UnknownMode(" caller".to_string())
        );
    }

    #[test]
    fn validate_refuses_a_key_given_twice() {
        for uri in [
            "srt://h:1?passphrase=abcdefghij&passphrase=0123456789",
            "srt://h:1?passphrase=abcdefghij&pass%70hrase=0123456789",
            "srt://h:1?mode=caller&x=1&mode=listener",
            "srt://h:1?foo&foo=1",
        ] {
            assert!(
                matches!(invalid(uri), SrtUriError::DuplicateParam(_)),
                "{uri}"
            );
        }
    }

    #[test]
    fn validate_checks_the_passphrase_length_in_bytes() {
        for len in [10, 80] {
            assert_eq!(
                valid(&format!("srt://h:1?passphrase={}", "a".repeat(len))),
                []
            );
        }
        assert_eq!(valid("srt://h:1?passphrase="), []);
        assert_eq!(valid("srt://h:1?passphrase"), []);
        for len in [9, 81] {
            assert_eq!(
                invalid(&format!("srt://h:1?passphrase={}", "a".repeat(len))),
                SrtUriError::PassphraseLength(len)
            );
        }
        // Decoded first: ten `%41` are ten bytes, not thirty.
        assert_eq!(
            valid(&format!("srt://h:1?passphrase={}", "%41".repeat(10))),
            []
        );
        assert_eq!(
            invalid(&format!("srt://h:1?passphrase={}", "%41".repeat(9))),
            SrtUriError::PassphraseLength(9)
        );
        // Bytes, not characters.
        let e_acute = "%C3%A9";
        assert_eq!(
            valid(&format!("srt://h:1?passphrase={}", e_acute.repeat(40))),
            []
        );
        assert_eq!(
            invalid(&format!("srt://h:1?passphrase={}", e_acute.repeat(41))),
            SrtUriError::PassphraseLength(82)
        );
    }

    #[test]
    fn validate_checks_pbkeylen() {
        for v in ["0", "16", "24", "32", ""] {
            assert_eq!(valid(&format!("srt://h:1?pbkeylen={v}")), [], "{v}");
        }
        for v in ["20", "128", "aes-128"] {
            assert_eq!(
                invalid(&format!("srt://h:1?pbkeylen={v}")),
                SrtUriError::Pbkeylen(v.to_string())
            );
        }
    }

    #[test]
    fn validate_warns_that_latency_in_the_uri_is_ignored() {
        assert_eq!(
            valid("srt://:5000?mode=listener&latency=200"),
            [SrtUriWarning::IgnoredLatency]
        );
        assert_eq!(
            valid("srt+relay://cam1?latency=200"),
            [SrtUriWarning::IgnoredLatency]
        );
        assert!(SrtUriWarning::IgnoredLatency
            .to_string()
            .contains("Latency property"));
        // These do apply.
        assert_eq!(valid("srt://h:1?rcvlatency=200&peerlatency=200"), []);
    }

    #[test]
    fn what_strom_writes_validates() {
        let mut f = parse("srt://h:1?mode=caller");
        f.passphrase = "a&b c#d=e%f+g/h?".to_string();
        f.stream_id = "#!::r=cam1,m=publish".to_string();
        f.pbkeylen = Some("32".to_string());
        assert_eq!(valid(&f.to_uri()), []);
        let mut f = parse("srt+relay://cam1");
        f.passphrase = crate::srt_relay::new_srt_passphrase();
        assert_eq!(valid(&f.to_uri()), []);
        assert_eq!(valid(&crate::srt_relay::new_srt_relay_uri(None, "")), []);
        assert_eq!(
            valid(&crate::srt_relay::new_srt_relay_uri(Some("eu-north"), "")),
            []
        );
        let resolved = crate::srt_relay::resolve_srt_uri(
            "srt+relay://cam1?passphrase=abcdefghij",
            crate::srt_relay::SrtRelayRole::Publish,
            &[crate::relay::RelayTarget {
                name: "default".into(),
                public: "relay.example.com:9000".into(),
                internal: None,
            }],
        )
        .unwrap();
        assert_eq!(valid(&resolved), []);
    }

    #[test]
    fn gst_unescape_matches_glib() {
        assert_eq!(gst_unescape("a%26b+c").as_deref(), Some("a&b+c"));
        assert_eq!(gst_unescape("%e9").as_deref(), Some("\u{fffd}"));
        assert_eq!(gst_unescape("%C3%A9").as_deref(), Some("\u{e9}"));
        for bad in ["%", "%2", "%zz", "%00", "%+1", "a%-1"] {
            assert_eq!(gst_unescape(bad), None, "{bad}");
        }
    }
}

#[cfg(test)]
mod endpoint_tests {
    //! Expected values were checked against GStreamer 1.28.6 (GLib 2.90) with
    //! `GST_DEBUG=srtobject:5 gst-launch-1.0 srtsrc uri=<uri> ! fakesink`,
    //! reading the "Opening SRT socket" and "Binding to" lines.

    use super::*;

    fn p(uri: &str) -> Option<(SrtMode, Option<u16>)> {
        parse_srt_uri(uri).map(|e| (e.mode, e.local_port))
    }

    const L: SrtMode = SrtMode::Listener;
    const C: SrtMode = SrtMode::Caller;
    const R: SrtMode = SrtMode::Rendezvous;

    #[test]
    fn default_mode_follows_the_host() {
        assert_eq!(p("srt://:5000"), Some((L, Some(5000))));
        assert_eq!(p("srt://192.0.2.1:5000"), Some((C, None)));
        assert_eq!(p("srt://example.com:5000?latency=200"), Some((C, None)));
        // A wildcard host is a host: caller (connects to 0.0.0.0).
        assert_eq!(p("srt://0.0.0.0:5000"), Some((C, None)));
        assert_eq!(p("srt://[::]:7000"), Some((C, None)));
        // Userinfo is not the host.
        assert_eq!(p("srt://user@:5000"), Some((L, Some(5000))));
    }

    #[test]
    fn explicit_mode() {
        assert_eq!(
            p("srt://:5000?mode=listener&latency=200"),
            Some((L, Some(5000)))
        );
        assert_eq!(p("srt://0.0.0.0:5000?mode=listener"), Some((L, Some(5000))));
        assert_eq!(p("srt://[::]:5000?mode=listener"), Some((L, Some(5000))));
        assert_eq!(p("srt://example.com:5000?mode=caller"), Some((C, None)));
        assert_eq!(
            p("srt://192.0.2.1:6000?latency=200&mode=rendezvous"),
            Some((R, Some(6000)))
        );
        // `none` with a host connects out like a caller.
        assert_eq!(p("srt://192.0.2.1:5000?mode=none"), Some((C, None)));
    }

    #[test]
    fn no_host_is_always_a_listener() {
        // Rendezvous fills in localport from the URI before the override.
        assert_eq!(p("srt://:5000?mode=rendezvous"), Some((L, Some(5000))));
        // Caller and none do not: the listener binds port 0.
        assert_eq!(p("srt://:5000?mode=caller"), Some((L, None)));
        assert_eq!(p("srt://:5000?mode=none"), Some((L, None)));
        assert_eq!(
            p("srt://:5000?mode=caller&localport=6000"),
            Some((L, Some(6000)))
        );
    }

    #[test]
    fn mode_value_ignores_ascii_case_keys_do_not() {
        // GLib >= 2.88 ignores case in enum nicks.
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=Listener"),
            Some((L, Some(5000)))
        );
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=LISTENER"),
            Some((L, Some(5000)))
        );
        // The key is compared with g_strcmp0.
        assert_eq!(p("srt://192.0.2.1:5000?MODE=listener"), Some((C, None)));
    }

    #[test]
    fn unknown_mode_keeps_the_default() {
        assert_eq!(p("srt://:5000?mode=bogus"), Some((L, Some(5000))));
        assert_eq!(p("srt://192.0.2.1:5000?mode=listenerx"), Some((C, None)));
        assert_eq!(p("srt://192.0.2.1:5000?mode="), Some((C, None)));
    }

    #[test]
    fn last_occurrence_wins_even_when_unusable() {
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=caller&mode=listener"),
            Some((L, Some(5000)))
        );
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=listener&mode=bogus"),
            Some((C, None))
        );
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=listener&mode"),
            Some((C, None))
        );
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=listener&mode=%zz"),
            Some((C, None))
        );
    }

    #[test]
    fn keys_and_values_are_percent_decoded() {
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=listen%65r"),
            Some((L, Some(5000)))
        );
        assert_eq!(
            p("srt://:5000?m%6Fde=rendezvous&localport=60%30%30"),
            Some((L, Some(6000)))
        );
        // `+` is not a space.
        assert_eq!(p("srt://192.0.2.1:5000?mode=+listener"), Some((C, None)));
    }

    #[test]
    fn local_port() {
        // localport overrides the URI port, for a listener too.
        assert_eq!(p("srt://:5000?localport=6000"), Some((L, Some(6000))));
        assert_eq!(
            p("srt://192.0.2.1:5000?mode=rendezvous&localport=6000"),
            Some((R, Some(6000)))
        );
        // No port, or port 0, is 7001.
        assert_eq!(p("srt://"), Some((L, Some(7001))));
        assert_eq!(p("srt://:0"), Some((L, Some(7001))));
        assert_eq!(p("srt://192.0.2.1?mode=rendezvous"), Some((R, Some(7001))));
        // g_ascii_strtoll: sign and trailing junk.
        assert_eq!(p("srt://:5000?localport=+6001x"), Some((L, Some(6001))));
        // Ephemeral, out of range, or unreadable: unknown.
        assert_eq!(p("srt://:5000?localport=0"), Some((L, None)));
        assert_eq!(p("srt://:5000?localport=abc"), Some((L, None)));
        assert_eq!(p("srt://:5000?localport=70000"), Some((L, None)));
        assert_eq!(p("srt://:5000?localport=-1"), Some((L, None)));
        assert_eq!(p("srt://:5000?localport"), Some((L, None)));
        assert_eq!(p("srt://:70000"), Some((L, None)));
        // The query ends at `#` and follows the path.
        assert_eq!(
            p("srt://:5000/path?localport=6002#frag"),
            Some((L, Some(6002)))
        );
        assert_eq!(p("srt://:5000#?localport=6002"), Some((L, Some(5000))));
    }

    #[test]
    fn rejected_uris() {
        assert_eq!(p("udp://:5000"), None);
        assert_eq!(p("SRT://:5000"), None);
        assert_eq!(p(" srt://:5000"), None);
        assert_eq!(p("srt://host:abc"), None);
        assert_eq!(p("srt://a:b:5000"), None);
        assert_eq!(p("srt://[::1:5000"), None);
        assert_eq!(p("srt+relay://edge/stream"), None);
    }
}
