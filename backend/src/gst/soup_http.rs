//! Which HTTP version libsoup speaks: under `souphttpsrc`, and the HLS/DASH
//! demuxers' segment downloads.
//!
//! libsoup's HTTP/2 client returns flow-control credit as data arrives rather
//! than as it is read, and queues the body in memory without a limit: a paused
//! Media Player on an http(s) file from an HTTP/2 server (most CDNs) downloads
//! the whole file into RAM. Over HTTP/1.1, TCP flow control holds the server
//! back, so Strom makes libsoup use HTTP/1.1 unless `STROM_SOUP_HTTP2` is on.

/// Strom's switch. On turns libsoup's HTTP/2 back on.
pub const STROM_SOUP_HTTP2: &str = "STROM_SOUP_HTTP2";
/// libsoup's own variable. It checks only that it is set, not its value.
pub const SOUP_FORCE_HTTP1: &str = "SOUP_FORCE_HTTP1";

/// What `configure` did, for the startup log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoupHttp {
    Http1,
    /// HTTP/2 allowed; `true` when an operator-set `SOUP_FORCE_HTTP1` was
    /// removed to get there.
    Http2 {
        removed_force_http1: bool,
    },
}

/// `STROM_SOUP_HTTP2`'s value, read the way the other Strom switches are.
pub fn parse(value: Option<&str>) -> Result<bool, String> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(format!(
            "{STROM_SOUP_HTTP2} must be true or false, got {other}"
        )),
    }
}

/// Set or remove `SOUP_FORCE_HTTP1` from `STROM_SOUP_HTTP2`, which alone
/// decides. libsoup reads `SOUP_FORCE_HTTP1` once, at its first connection, so
/// call this in `main` before any thread exists.
pub fn configure() -> Result<SoupHttp, String> {
    let http2 = parse(strom_types::env::var_opt(STROM_SOUP_HTTP2).as_deref())?;
    if http2 {
        let removed_force_http1 = std::env::var_os(SOUP_FORCE_HTTP1).is_some();
        std::env::remove_var(SOUP_FORCE_HTTP1);
        Ok(SoupHttp::Http2 {
            removed_force_http1,
        })
    } else {
        std::env::set_var(SOUP_FORCE_HTTP1, "1");
        Ok(SoupHttp::Http1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http1_unless_switched_on() {
        assert_eq!(parse(None), Ok(false));
        for off in ["0", "false", "no", "off", " OFF "] {
            assert_eq!(parse(Some(off)), Ok(false), "{off}");
        }
        for on in ["1", "true", "yes", "on", "On"] {
            assert_eq!(parse(Some(on)), Ok(true), "{on}");
        }
    }

    #[test]
    fn an_unknown_value_is_an_error() {
        assert!(parse(Some("maybe")).is_err());
    }
}
