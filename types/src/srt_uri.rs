//! The `srt_uri` property of the SRT blocks as structured fields.
//!
//! The GUI edits an SRT block's connection as a mode (listener, caller,
//! relay), host, port, stream id, passphrase and key length. The block
//! property stays the single `srt_uri` string: [`SrtUriFields::parse`] reads
//! the fields out of it and [`SrtUriFields::to_uri`] writes them back. Query
//! parameters the fields do not cover (`latency`, `localaddress`, ...) are
//! kept as they were written.
//!
//! Pure string logic, unit-tested here; the frontend only draws the fields.

use crate::srt_relay::{SRT_RELAY_MAX_ID_LEN, SRT_RELAY_MIN_SAFE_ID_LEN, SRT_RELAY_SCHEME};

/// The plain SRT URI scheme.
pub const SRT_SCHEME: &str = "srt://";

/// Shortest passphrase SRT accepts.
pub const SRT_PASSPHRASE_MIN_LEN: usize = 10;
/// Longest passphrase SRT accepts.
pub const SRT_PASSPHRASE_MAX_LEN: usize = 79;

/// The AES key lengths SRT accepts for `pbkeylen`, in bytes.
pub const SRT_PBKEYLEN_VALUES: &[&str] = &["16", "24", "32"];

/// Parameters the relay connection drops when an address moves onto the
/// relay: they bind a direct connection to a local address or adapter.
const DIRECT_ONLY_PARAMS: &[&str] = &["localaddress", "localport", "adapter"];

/// How an SRT block connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrtConnectionMode {
    /// Strom waits on a port for the other side to dial in.
    Listener,
    /// Strom dials the other side. A plain `srt://` URI without `mode`.
    #[default]
    Caller,
    /// Strom dials the configured SRT relay (`srt+relay://<stream id>`).
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
            SrtUriParseError::UnsupportedMode(mode) => {
                write!(f, "mode '{mode}' is not supported here")
            }
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
    fn of(key: &str) -> Option<Known> {
        let key = percent_decode(key);
        let key = key.trim();
        [
            ("mode", Known::Mode),
            ("streamid", Known::StreamId),
            ("passphrase", Known::Passphrase),
            ("pbkeylen", Known::Pbkeylen),
        ]
        .into_iter()
        .find(|(name, _)| key.eq_ignore_ascii_case(name))
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
    !id.is_empty()
        && id.len() <= SRT_RELAY_MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
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
            "Use {SRT_PASSPHRASE_MIN_LEN} to {SRT_PASSPHRASE_MAX_LEN} characters ({len} now), or leave it empty for no encryption."
        ))
    }
}

impl SrtUriFields {
    /// Read the fields out of an `srt_uri`.
    ///
    /// A plain `srt://` URI without `mode` is a caller, as for `srtsrc` and
    /// `srtsink`. The relay stream id is taken as written, valid or not, so
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
            fields.stream_id = authority.to_string();
        } else {
            (fields.host, fields.port) = split_authority(authority)?;
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
                    fields.mode = match value.trim().to_ascii_lowercase().as_str() {
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
            SrtConnectionMode::Relay => format!("{SRT_RELAY_SCHEME}{}", self.stream_id),
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
                    let unchanged = match known {
                        Known::Mode => read.trim().eq_ignore_ascii_case(&value),
                        _ => read == value,
                    };
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
                let key = percent_decode(key);
                !DIRECT_ONLY_PARAMS
                    .iter()
                    .any(|d| key.trim().eq_ignore_ascii_case(d))
            });
        }
        if self.mode == SrtConnectionMode::Listener && mode == SrtConnectionMode::Caller {
            // The bind address is no address to dial.
            self.host.clear();
        }
        self.mode = mode;
    }
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
        let f = parse("srt://192.0.2.1:4000?mode=caller&streamid=cam1&latency=200");
        assert_eq!(f.mode, SrtConnectionMode::Caller);
        assert_eq!(f.host, "192.0.2.1");
        assert_eq!(f.port, "4000");
        assert_eq!(f.stream_id, "cam1");
        assert_eq!(f.other_params().collect::<Vec<_>>(), ["latency=200"]);
        round_trip("srt://192.0.2.1:4000?mode=caller&streamid=cam1&latency=200");
    }

    #[test]
    fn caller_is_the_default_mode() {
        let f = parse("srt://example.com:4000");
        assert_eq!(f.mode, SrtConnectionMode::Caller);
        assert_eq!(f.to_uri(), "srt://example.com:4000?mode=caller");
    }

    #[test]
    fn relay_round_trip() {
        let f = parse(
            "srt+relay://Ab3kQ9xR2Ab3kQ9xR2Ab?passphrase=abcdefghijkl&pbkeylen=32&latency=200",
        );
        assert_eq!(f.mode, SrtConnectionMode::Relay);
        assert_eq!(f.stream_id, "Ab3kQ9xR2Ab3kQ9xR2Ab");
        assert_eq!(f.passphrase, "abcdefghijkl");
        assert_eq!(f.pbkeylen.as_deref(), Some("32"));
        round_trip(
            "srt+relay://Ab3kQ9xR2Ab3kQ9xR2Ab?passphrase=abcdefghijkl&pbkeylen=32&latency=200",
        );
        round_trip("srt+relay://cam1");
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
        let mut f = parse("srt://h:1?latency=200&PassPhrase=abc%2Ddefghij&MODE=Caller&x=1");
        assert_eq!(f.passphrase, "abc-defghij");
        f.port = "2".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?latency=200&PassPhrase=abc%2Ddefghij&MODE=Caller&x=1"
        );
        f.passphrase = "new passphrase".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?latency=200&passphrase=new%20passphrase&MODE=Caller&x=1"
        );
        // A cleared field drops out; a new one goes at the end.
        f.passphrase.clear();
        f.stream_id = "cam1".to_string();
        assert_eq!(
            f.to_uri(),
            "srt://h:2?latency=200&MODE=Caller&x=1&streamid=cam1"
        );
        // A new mode goes first.
        let mut f = parse("srt://h:1?latency=200");
        f.switch_mode(SrtConnectionMode::Listener, String::new);
        assert_eq!(f.to_uri(), "srt://h:1?mode=listener&latency=200");
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
    fn scheme_and_mode_are_case_insensitive() {
        let f = parse("  SRT://:5000?MODE=Listener ");
        assert_eq!(f.mode, SrtConnectionMode::Listener);
        assert_eq!(f.to_uri(), "srt://:5000?MODE=Listener");
        assert_eq!(parse("SRT+Relay://cam1").mode, SrtConnectionMode::Relay);
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
                "srt://h:1?passphrase=abcdefghijk&PASSPHRASE=x",
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
            "srt://example.com:5000?mode=listener&streamid=cam1&passphrase=abcdefghijk&latency=200"
        );
    }

    #[test]
    fn switching_to_the_relay_makes_a_stream_id_and_drops_local_binds() {
        let mut f = parse(
            "srt://h:1?mode=caller&localaddress=0.0.0.0&LocalPort=5000&adapter=1&latency=200",
        );
        f.switch_mode(SrtConnectionMode::Relay, || "newid".to_string());
        assert_eq!(f.to_uri(), "srt+relay://newid?latency=200");

        let mut f = parse("srt://h:1?mode=caller&streamid=%23!%3A%3Ar%3Dx");
        f.switch_mode(SrtConnectionMode::Relay, || "newid".to_string());
        assert_eq!(f.stream_id, "newid");
    }

    #[test]
    fn passphrase_length_is_checked() {
        assert_eq!(passphrase_problem(""), None);
        assert_eq!(passphrase_problem(&"a".repeat(10)), None);
        assert_eq!(passphrase_problem(&"a".repeat(79)), None);
        assert!(passphrase_problem("short").is_some());
        assert!(passphrase_problem(&"a".repeat(80)).is_some());
    }
}
