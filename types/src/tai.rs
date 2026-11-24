//! TAI and UTC: the fixed offset between them, and UTC text for display and
//! commands.
//!
//! Strom's media timeline (TAMS timeranges, the replay frame grid) is TAI in
//! nanoseconds since 1970-01-01 TAI. People and most other systems use UTC.
//! Convert only at that boundary, with the functions here.
//!
//! Pure and dependency free, so the backend and the frontend share one
//! definition.

/// TAI - UTC, in nanoseconds: 37 s, fixed from now on.
///
/// The 28th CGPM (Resolution 5, 15 October 2026) ended the insertion of leap
/// seconds, and IERS Bulletin C 72 announces none for the end of 2026, so the
/// offset that has held since 1 January 2017 will not change again. Instants
/// before 2017 had a smaller offset; the conversions here do not model that.
pub const TAI_MINUS_UTC_NS: u64 = 37_000_000_000;

const NS: i64 = 1_000_000_000;
const OFFSET: i64 = TAI_MINUS_UTC_NS as i64;

/// UTC ns since the Unix epoch for a TAI ns instant.
pub fn tai_to_utc_ns(tai_ns: i64) -> i64 {
    tai_ns - OFFSET
}

/// TAI ns for a UTC ns instant since the Unix epoch.
pub fn utc_to_tai_ns(utc_ns: i64) -> i64 {
    utc_ns + OFFSET
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Proleptic Gregorian date for days since 1970-01-01 (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DDTHH:MM:SS.mmm` for ns since the epoch, milliseconds truncated
/// (the millisecond the instant lies in). No zone suffix.
pub fn format_calendar_ms(ns: i64) -> String {
    let secs = ns.div_euclid(NS);
    let ms = ns.rem_euclid(NS) / 1_000_000;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        y,
        m,
        d,
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        ms
    )
}

/// ISO 8601 UTC with milliseconds, e.g. `2026-11-24T12:34:56.780Z`.
pub fn format_utc_ms(utc_ns: i64) -> String {
    format!("{}Z", format_calendar_ms(utc_ns))
}

fn digits(s: &str, n: usize) -> Option<(i64, &str)> {
    let (head, rest) = (s.get(..n)?, s.get(n..)?);
    head.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| (head.parse().ok(), rest))
        .and_then(|(v, r)| Some((v?, r)))
}

/// Parse an RFC 3339 / ISO 8601 date-time to UTC ns since the epoch:
/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`. `T` may be a space or
/// `t`, `Z` may be `z`, and the fraction may have up to 9 digits. A zone is
/// required, so a local time is never taken for UTC by mistake.
pub fn parse_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let (y, r) = digits(s, 4)?;
    let r = r.strip_prefix('-')?;
    let (mo, r) = digits(r, 2)?;
    let r = r.strip_prefix('-')?;
    let (d, r) = digits(r, 2)?;
    let r = r
        .strip_prefix('T')
        .or_else(|| r.strip_prefix('t'))
        .or_else(|| r.strip_prefix(' '))?;
    let (h, r) = digits(r, 2)?;
    let r = r.strip_prefix(':')?;
    let (mi, r) = digits(r, 2)?;
    let r = r.strip_prefix(':')?;
    let (sec, mut r) = digits(r, 2)?;
    let mut frac_ns = 0i64;
    if let Some(f) = r.strip_prefix('.') {
        let n = f.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 || n > 9 {
            return None;
        }
        frac_ns = f[..n].parse::<i64>().ok()? * 10i64.pow(9 - n as u32);
        r = &f[n..];
    }
    let offset_s = match r {
        "Z" | "z" => 0,
        _ => {
            let sign = match r.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (oh, rr) = digits(&r[1..], 2)?;
            let rr = rr.strip_prefix(':')?;
            let (om, rr) = digits(rr, 2)?;
            if !rr.is_empty() || oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    // Reject dates such as 02-30 that roll over into the next month.
    if civil_from_days(days) != (y, mo as u32, d as u32) {
        return None;
    }
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_s;
    secs.checked_mul(NS)?.checked_add(frac_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-24T00:00:00Z, from `date -u -d 2026-11-24 +%s`.
    const DAY_S: i64 = 1_795_478_400;

    #[test]
    fn offset_round_trips() {
        let tai = 1_795_526_553_080_598_792;
        assert_eq!(tai_to_utc_ns(tai), tai - 37 * NS);
        assert_eq!(utc_to_tai_ns(tai_to_utc_ns(tai)), tai);
    }

    #[test]
    fn formats_utc_with_milliseconds() {
        assert_eq!(format_utc_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_utc_ms(DAY_S * NS), "2026-11-24T00:00:00.000Z");
        // The TAI start of the spike's live ingest.
        let utc = tai_to_utc_ns(1_795_526_553_080_598_792);
        assert_eq!(format_utc_ms(utc), "2026-11-24T13:21:56.080Z");
        // Truncated, not rounded: 999.9 ms stays in its millisecond.
        assert_eq!(format_utc_ms(999_999_999), "1970-01-01T00:00:00.999Z");
        assert_eq!(format_utc_ms(-1), "1969-12-31T23:59:59.999Z");
        // Leap day and end of year.
        let leap = days_from_civil(2028, 2, 29) * 86_400 * NS;
        assert_eq!(format_utc_ms(leap), "2028-02-29T00:00:00.000Z");
        let eoy = (days_from_civil(2027, 1, 1) * 86_400 - 1) * NS;
        assert_eq!(format_utc_ms(eoy), "2026-12-31T23:59:59.000Z");
    }

    #[test]
    fn parses_rfc3339() {
        let t = DAY_S * NS + 13 * 3600 * NS + 21 * 60 * NS + 56 * NS + 80_000_000;
        assert_eq!(parse_utc("2026-11-24T13:21:56.080Z"), Some(t));
        assert_eq!(parse_utc("2026-11-24 13:21:56.08z"), Some(t));
        assert_eq!(parse_utc("2026-11-24T14:21:56.080+01:00"), Some(t));
        assert_eq!(parse_utc("2026-11-24T12:06:56.080-01:15"), Some(t));
        // The TAI start of the spike's live ingest, to the nanosecond.
        assert_eq!(
            parse_utc("2026-11-24T13:21:56.080598792Z").map(utc_to_tai_ns),
            Some(1_795_526_553_080_598_792)
        );
        assert_eq!(parse_utc("2026-11-24T13:21:56Z"), Some(t - 80_000_000));
        for bad in [
            "2026-11-24T13:15:16",
            "2026-11-24T13:15:16.Z",
            "2026-02-30T00:00:00Z",
            "2026-11-24T24:00:00Z",
            "2026-11-24T13:15:16.0805987921Z",
            "26-11-24T13:15:16Z",
            "",
        ] {
            assert_eq!(parse_utc(bad), None, "{}", bad);
        }
    }

    #[test]
    fn format_and_parse_agree() {
        for ns in [0i64, 1_795_526_516_080_000_000, 4_102_444_800_123_000_000] {
            assert_eq!(parse_utc(&format_utc_ms(ns)), Some(ns));
        }
    }
}
