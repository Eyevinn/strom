//! Shared TAMS (Time-Addressable Media Store) protocol constants and helpers.
//!
//! These are pure, dependency-free values describing the TAMS wire format, kept in
//! `strom-types` so both the backend (writing/reading segments) and the frontend
//! (displaying timeranges) use one definition. The HTTP client and GStreamer wiring
//! live in the backend, since they depend on `reqwest`/GStreamer.

/// NMOS format URN for a video flow.
pub const FORMAT_VIDEO: &str = "urn:x-nmos:format:video";
/// NMOS format URN for an audio flow.
pub const FORMAT_AUDIO: &str = "urn:x-nmos:format:audio";
/// NMOS format URN for a data flow.
pub const FORMAT_DATA: &str = "urn:x-nmos:format:data";
/// NMOS format URN for a multi-essence flow (e.g. MPEG-TS carrying video + audio,
/// or a Multi-Flow grouping per-essence flows). Note: TAMS uses `:multi`, not the
/// AMWA NMOS/BCP-006-04 `:mux` — the TAMS content-format enum has no `mux` value.
pub const FORMAT_MULTI: &str = "urn:x-nmos:format:multi";

/// Container MIME for segmented MP4 essence.
pub const CONTENT_TYPE_MP4: &str = "video/mp4";
/// Container MIME for MPEG-TS segments.
pub const CONTENT_TYPE_MPEGTS: &str = "video/mp2t";

const NS_PER_SEC: u64 = 1_000_000_000;

/// Format a nanosecond instant as a TAMS `"<seconds>:<nanoseconds>"` timestamp.
///
/// TAMS timestamps are TAI; this function only formats — the caller is responsible
/// for providing a TAI-based nanosecond value.
pub fn format_timestamp(ns: u64) -> String {
    format!("{}:{}", ns / NS_PER_SEC, ns % NS_PER_SEC)
}

/// Format a half-open `[start, end)` nanosecond range as a TAMS timerange string.
pub fn format_timerange(start_ns: u64, end_ns: u64) -> String {
    format!(
        "[{}_{})",
        format_timestamp(start_ns),
        format_timestamp(end_ns)
    )
}

/// Parse a TAMS `"<seconds>:<nanoseconds>"` timestamp into nanoseconds.
/// A bare `"<seconds>"` (no colon) is accepted as whole seconds.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let (sec, ns) = match s.split_once(':') {
        Some((sec, ns)) => (sec, ns),
        None => (s, "0"),
    };
    let sec: u64 = sec.parse().ok()?;
    let ns: u64 = ns.parse().ok()?;
    sec.checked_mul(NS_PER_SEC)?.checked_add(ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips() {
        assert_eq!(format_timestamp(0), "0:0");
        assert_eq!(format_timestamp(NS_PER_SEC + 500), "1:500");
        assert_eq!(parse_timestamp("1:500"), Some(NS_PER_SEC + 500));
        assert_eq!(parse_timestamp("2"), Some(2 * NS_PER_SEC));
        assert_eq!(parse_timestamp("bad"), None);
    }

    #[test]
    fn timerange_is_half_open() {
        assert_eq!(format_timerange(1_000_000_000, 3_000_000_000), "[1:0_3:0)");
        assert_eq!(format_timerange(0, 1_500_000_000), "[0:0_1:500000000)");
    }
}
