//! Whether GStreamer can open `https://` URLs on this host.
//!
//! `souphttpsrc`, and with it HLS and DASH in the Media Player, makes its TLS
//! connections through GIO. GIO gets TLS from a module it loads at runtime
//! (`gioopenssl` from glib-networking), from `lib/gio/modules` next to the
//! GLib it was built with. Without that module every `https://` request fails
//! with "TLS support is not available", which reaches the pipeline only as
//! `Internal data stream error` from `souphttpsrc` (issue #1079).

use gio::prelude::*;
use tracing::warn;

/// What an operator needs to know when GIO has no TLS backend.
pub const NO_TLS_HINT: &str = "GStreamer's GIO has no TLS backend on this host, so https:// \
     URLs cannot be opened. On Windows the GIO TLS module \
     (lib\\gio\\modules\\gioopenssl.dll in the GStreamer folder) is missing; install a Strom \
     release that ships it, or the full GStreamer runtime. On Linux and macOS, install \
     glib-networking.";

/// True when GIO can make TLS connections.
pub fn tls_available() -> bool {
    gio::TlsBackend::default().supports_tls()
}

/// Warn at startup when `https://` sources cannot work. Call after
/// `gst::init()`.
pub fn log_tls_support() {
    if !tls_available() {
        warn!("{}", NO_TLS_HINT);
    }
}

/// True when `uri` needs TLS to open.
pub fn needs_tls(uri: &str) -> bool {
    uri.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_needs_tls() {
        assert!(needs_tls("https://example.com/live/index.m3u8"));
        assert!(needs_tls("HTTPS://example.com/live/index.m3u8"));
        assert!(!needs_tls("http://example.com/live/index.m3u8"));
        assert!(!needs_tls("file:///C:/media/clip.mp4"));
        assert!(!needs_tls("srt://192.0.2.10:9000"));
        assert!(!needs_tls("https:/"));
    }
}
