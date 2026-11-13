//! What the GStreamer SRT elements (`srtsrc`, `srtsink`) open for an `srt://`
//! URI: the mode, and the local port a remote peer has to reach.
//!
//! It mirrors `gst_srt_object_set_uri()`, `gst_srt_object_validate_parameters()`
//! and `gst_srt_object_open_internal()` in gst-plugins-bad `ext/srt/gstsrtobject.c`
//! (identical in 1.22 and 1.28), with the URI split as `gst_uri_from_string()`
//! does in `gst/gsturi.c`:
//!
//! - The query is split on `&`, then on the first `=`. Keys and values are
//!   percent-decoded (an invalid escape or `%00` makes them NULL). The query
//!   is a hash table, so the last occurrence of a key wins, even when its value
//!   is not usable.
//! - Keys are compared case-sensitively (`g_strcmp0`). The `mode` value is
//!   looked up with `g_enum_get_value_by_nick()`, which ignores ASCII case
//!   since GLib 2.88 (the Docker image and Homebrew) and is case-sensitive
//!   before (Debian 12, the floor). We follow 2.88. A value that is no nick
//!   is ignored, and the default stays.
//! - The default mode is caller when the URI has a host and listener when it
//!   has none. A wildcard host (`0.0.0.0`, `[::]`) is a host like any other:
//!   caller, unless `mode=` says otherwise.
//! - A URI without a host always opens as a listener, whatever `mode=` says.
//! - Listener and rendezvous bind `localport=` when given, otherwise the URI
//!   port, otherwise 7001 (port 0 in the URI means no port). A query
//!   `localport=` overrides the URI port for a listener too. With an explicit
//!   caller (or `none`) mode and no host, no `localport` is filled in, and the
//!   forced listener binds port 0.
//!
//! Kept self-contained so it can be replaced by a shared `srt://` parser.

use crate::api::SrtMode;

/// What a GStreamer SRT element opens for an `srt://` URI.
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

/// Read an `srt://` URI the way the GStreamer SRT elements do (see the module
/// docs). `None` for another scheme or a URI `gst_uri_from_string()` rejects.
pub fn parse_srt_uri(uri: &str) -> Option<SrtUriEndpoint> {
    // set_uri checks the scheme prefix without trimming whitespace.
    let rest = uri.strip_prefix("srt://")?;
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
    // Last occurrence wins. `Some(None)`: present, but the value is NULL.
    let mut mode_value: Option<Option<Vec<u8>>> = None;
    let mut localport_value: Option<Option<Vec<u8>>> = None;
    for part in query.into_iter().flat_map(|q| q.split('&')) {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (part, None),
        };
        let value = value.and_then(percent_decode);
        match percent_decode(key).as_deref() {
            Some(b"mode") => mode_value = Some(value),
            Some(b"localport") => localport_value = Some(value),
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
        Some(value) => value.and_then(|v| strtoll_unsigned(&v)),
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
fn mode_from_nick(nick: &[u8]) -> Option<GstMode> {
    [
        (&b"none"[..], GstMode::None),
        (b"caller", GstMode::Caller),
        (b"listener", GstMode::Listener),
        (b"rendezvous", GstMode::Rendezvous),
    ]
    .into_iter()
    .find(|(n, _)| n.eq_ignore_ascii_case(nick))
    .map(|(_, m)| m)
}

/// `g_uri_unescape_string()`: `None` on an invalid escape or a decoded NUL.
fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            i += 3;
            u8::from_str_radix(hex, 16).ok()?
        } else {
            i += 1;
            bytes[i - 1]
        };
        if b == 0 {
            return None;
        }
        out.push(b);
    }
    Some(out)
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

#[cfg(test)]
mod tests {
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
