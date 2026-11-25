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
///
/// Both parts must be plain decimal digits (no sign, no spaces) and the
/// nanoseconds below one second, so `"1:2000000000"` and `"+1:0"` are rejected
/// rather than read as 3 s and 1 s.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let (sec, ns) = match s.split_once(':') {
        Some((sec, ns)) => (sec, ns),
        None => (s, "0"),
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(sec) || !digits(ns) {
        return None;
    }
    let sec: u64 = sec.parse().ok()?;
    let ns: u64 = ns.parse().ok()?;
    if ns >= NS_PER_SEC {
        return None;
    }
    sec.checked_mul(NS_PER_SEC)?.checked_add(ns)
}

/// A parsed TAMS timerange. Bounds are signed nanoseconds (TAI); `None` is
/// eternity on that side. `empty` is the `()` range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start: Option<i64>,
    pub start_inclusive: bool,
    pub end: Option<i64>,
    pub end_inclusive: bool,
    pub empty: bool,
}

impl TimeRange {
    /// Half-open `[start, end)` view, mapping an inclusive end to `end + 1` ns
    /// and an exclusive start to `start + 1` ns. `None` for an empty or
    /// unbounded range, or one whose bound does not fit after that step (the
    /// timerange comes from a gateway, so `i64::MAX` must not overflow).
    pub fn half_open(&self) -> Option<(i64, i64)> {
        if self.empty {
            return None;
        }
        let s = self.start?.checked_add(i64::from(!self.start_inclusive))?;
        let e = self.end?.checked_add(i64::from(self.end_inclusive))?;
        (e > s).then_some((s, e))
    }
}

/// Parse a signed TAMS timestamp `"[-]<seconds>:<nanoseconds>"` into
/// nanoseconds. The sign applies to the whole value (`"-1:500"` is
/// -1.0000005 s).
pub fn parse_timestamp_signed(s: &str) -> Option<i64> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let ns = i64::try_from(parse_timestamp(rest)?).ok()?;
    Some(if neg { -ns } else { ns })
}

/// Parse a TAMS timerange string: `"[1:0_2:0)"`, `"(1:0_2:0]"`, `"[1:0_"`,
/// `"_2:0)"`, `"_"`, a single instant `"[1:0]"`, or the empty range `"()"`.
/// A missing bracket means inclusive start / exclusive end.
pub fn parse_timerange(s: &str) -> Option<TimeRange> {
    let s = s.trim();
    if s == "()" {
        return Some(TimeRange {
            start: None,
            start_inclusive: false,
            end: None,
            end_inclusive: false,
            empty: true,
        });
    }
    let (start_inclusive, rest) = match s.as_bytes().first()? {
        b'[' => (true, &s[1..]),
        b'(' => (false, &s[1..]),
        _ => (true, s),
    };
    let (end_inclusive, body) = match rest.as_bytes().last() {
        Some(b']') => (true, &rest[..rest.len() - 1]),
        Some(b')') => (false, &rest[..rest.len() - 1]),
        _ => (false, rest),
    };
    // A leading '-' belongs to a timestamp, so split on the first '_' only.
    let (start, end) = match body.split_once('_') {
        Some((a, b)) => {
            let start = if a.is_empty() {
                None
            } else {
                Some(parse_timestamp_signed(a)?)
            };
            let end = if b.is_empty() {
                None
            } else {
                Some(parse_timestamp_signed(b)?)
            };
            (start, end)
        }
        None => {
            // A single instant: "[t]".
            let t = parse_timestamp_signed(body)?;
            (Some(t), Some(t))
        }
    };
    Some(TimeRange {
        start,
        start_inclusive: start.is_some() && start_inclusive,
        end,
        end_inclusive: end.is_some() && end_inclusive,
        empty: false,
    })
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
    fn timestamp_parsing_is_strict() {
        // Nanoseconds must stay below one second.
        assert_eq!(parse_timestamp("1:2000000000"), None);
        assert_eq!(parse_timestamp("1:999999999"), Some(2 * NS_PER_SEC - 1));
        // No signs or blanks inside the parts.
        assert_eq!(parse_timestamp("+1:0"), None);
        assert_eq!(parse_timestamp("1:+5"), None);
        assert_eq!(parse_timestamp(" 1:0"), None);
        assert_eq!(parse_timestamp("1:"), None);
        assert_eq!(parse_timestamp(":5"), None);
        assert_eq!(parse_timestamp_signed("-+1:0"), None);
        assert_eq!(parse_timestamp_signed("--1:0"), None);
        assert_eq!(
            parse_timestamp_signed("-1:5"),
            Some(-(NS_PER_SEC as i64) - 5)
        );
        // Overflow of u64 seconds.
        assert_eq!(parse_timestamp("18446744073709551615:0"), None);
    }

    #[test]
    fn half_open_does_not_overflow() {
        let r = TimeRange {
            start: Some(0),
            start_inclusive: true,
            end: Some(i64::MAX),
            end_inclusive: true,
            empty: false,
        };
        assert_eq!(r.half_open(), None);
        let r = TimeRange {
            start: Some(i64::MAX),
            start_inclusive: false,
            end: Some(i64::MAX),
            end_inclusive: false,
            empty: false,
        };
        assert_eq!(r.half_open(), None);
    }

    #[test]
    fn timerange_is_half_open() {
        assert_eq!(format_timerange(1_000_000_000, 3_000_000_000), "[1:0_3:0)");
        assert_eq!(format_timerange(0, 1_500_000_000), "[0:0_1:500000000)");
    }

    #[test]
    fn timerange_parses_all_forms() {
        let r = parse_timerange("[1800000001:0_1800000002:0)").unwrap();
        assert_eq!(
            r.half_open(),
            Some((1_800_000_001_000_000_000, 1_800_000_002_000_000_000))
        );
        let r = parse_timerange("(1:0_2:0]").unwrap();
        assert_eq!(r.half_open(), Some((1_000_000_001, 2_000_000_001)));
        let r = parse_timerange("[1:500_").unwrap();
        assert_eq!(r.start, Some(1_000_000_500));
        assert_eq!(r.end, None);
        assert_eq!(r.half_open(), None);
        let r = parse_timerange("_").unwrap();
        assert_eq!((r.start, r.end, r.empty), (None, None, false));
        assert!(parse_timerange("()").unwrap().empty);
        let r = parse_timerange("[5:0]").unwrap();
        assert_eq!(r.half_open(), Some((5_000_000_000, 5_000_000_001)));
        let r = parse_timerange("[-1:500_0:0)").unwrap();
        assert_eq!(r.start, Some(-1_000_000_500));
        assert!(parse_timerange("[x_1:0)").is_none());
    }
}
