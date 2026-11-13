//! Refusing an SRT URI that would crash GStreamer's SRT elements.
//!
//! `srtsrc` and `srtsink` (gst-plugins-bad `ext/srt`, measured on 1.22.0 and
//! 1.28.6) segfault the whole process on a query key without `=`, such as
//! `srt://:5000?latency`: the query table holds a NULL value for that key, and
//! a numeric option hands it to `g_ascii_strtoll`. See `docs/UPSTREAM_NOTES.md`.
//! Every path that gives a user URI to an SRT element checks it here first.

use crate::blocks::BlockBuildError;

/// The query keys `gst_srt_object_set_uri` handles: its own (`mode`,
/// `localaddress`, `localport`, `poll-timeout`) and the socket options in its
/// `srt_options` table (identical in 1.22.0 and 1.28.6). Only these are named
/// in an error; any other key could be a secret written in the wrong place.
const KNOWN_SRT_QUERY_KEYS: &[&str] = &[
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
    "peeridletimeo",
    "bindtodevice",
    "packetfilter",
    "retransmitalgo",
];

/// Whether `uri` has the `srt` scheme, the one GStreamer hands to `srtsrc`.
pub(crate) fn is_srt_uri(uri: &str) -> bool {
    uri.split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("srt"))
}

/// Refuse an SRT URI whose query has a key without `=`.
///
/// Empty segments (`&&`, a trailing `&`) and an empty value (`latency=`) do
/// not crash GStreamer, so they pass. The error never contains the URI or a
/// value, and names the key only if it is a known SRT option.
pub(crate) fn check_srt_uri_query(property: &str, uri: &str) -> Result<(), BlockBuildError> {
    let Some((_, query)) = uri.split_once('?') else {
        return Ok(());
    };
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    let Some(key) = query
        .split('&')
        .find(|segment| !segment.is_empty() && !segment.contains('='))
    else {
        return Ok(());
    };
    let message = if KNOWN_SRT_QUERY_KEYS.contains(&key) {
        format!(
            "{property}: query parameter '{key}' has no value (write {key}=<value>); \
             GStreamer's SRT elements crash on a key without a value"
        )
    } else {
        format!(
            "{property}: a query parameter has no value (write key=<value>); \
             GStreamer's SRT elements crash on a key without a value"
        )
    };
    Err(BlockBuildError::InvalidProperty(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(uri: &str) -> String {
        match check_srt_uri_query("srt_uri", uri) {
            Err(BlockBuildError::InvalidProperty(m)) => m,
            other => panic!("expected InvalidProperty for {uri}, got {other:?}"),
        }
    }

    #[test]
    fn a_known_key_without_value_is_refused_and_named() {
        let m = message("srt://:5000?latency");
        assert!(m.starts_with("srt_uri: query parameter 'latency' has no value"));
        assert!(m.contains("latency=<value>"));
        assert!(!m.contains("srt://"));
    }

    #[test]
    fn a_bare_key_after_valid_pairs_is_refused_without_leaking_values() {
        let m = message("srt://192.0.2.1:5000?mode=caller&passphrase=topsecret123&pbkeylen");
        assert!(m.contains("'pbkeylen'"));
        assert!(!m.contains("topsecret123"));
        assert!(!m.contains("192.0.2.1"));

        let m = message("srt://192.0.2.1:5000?mode=caller&passphrase");
        assert!(m.contains("'passphrase'"));
    }

    #[test]
    fn an_unknown_key_without_value_is_refused_but_not_named() {
        let m = message("srt://:5000?latency=100&hunter2secret");
        assert!(!m.contains("hunter2secret"));
        assert!(m.contains("a query parameter has no value"));
    }

    #[test]
    fn a_percent_encoded_key_without_raw_equals_is_refused() {
        // GStreamer splits on a raw `=` before it unescapes, so `lat%65ncy`
        // still reaches srtsrc as `latency` with no value.
        let m = message("srt://:5000?lat%65ncy");
        assert!(m.contains("a query parameter has no value"));
        assert!(check_srt_uri_query("srt_uri", "srt://:5000?latency%3D100").is_err());
    }

    #[test]
    fn well_formed_and_harmless_queries_pass() {
        for uri in [
            "srt://:5000",
            "srt://:5000?",
            "srt://192.0.2.1:5000?mode=caller",
            "srt://:5000?latency=100&mode=listener",
            "srt://:5000?latency=",
            "srt://:5000?latency=100&&mode=listener",
            "srt://:5000?latency=100&",
            "srt://:5000?&latency=100",
            "srt://:5000?latency=100#frag",
            "srt://:5000?streamid=#!::r=live,m=publish",
        ] {
            assert!(check_srt_uri_query("srt_uri", uri).is_ok(), "{uri}");
        }
    }

    #[test]
    fn a_bare_key_in_the_fragment_is_not_in_the_query() {
        assert!(check_srt_uri_query("srt_uri", "srt://:5000?latency=100#latency").is_ok());
    }

    #[test]
    fn srt_scheme_detection() {
        assert!(is_srt_uri("srt://:5000"));
        assert!(is_srt_uri("SRT://host:5000?latency"));
        assert!(!is_srt_uri("https://example.com/a?flag"));
        assert!(!is_srt_uri("/media/srt://file"));
        assert!(!is_srt_uri("srtp://host"));
    }
}
