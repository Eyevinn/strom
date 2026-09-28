//! Judging whether the system clock is fit for media timestamping.
//!
//! The Clocks panel and the backend's startup check both use this, so an
//! operator sees the same verdict in the UI and in the log.

use crate::api::SystemClockInfo;

/// Max error above which the kernel is too unsure of the time to trust it.
const MAX_ERROR_LIMIT_US: i64 = 500_000;

/// Offset above which a large correction is in flight.
const OFFSET_LIMIT_NS: i64 = 1_000_000;

/// TAI - UTC as of 2026.
const EXPECTED_TAI_OFFSET_SEC: i32 = 37;

/// Where to point an operator whose system clock is poorly disciplined.
const CHRONY_SETUP_HINT: &str = "The Strom repository has setup scripts that configure NTP for \
     broadcast use (chrony with multiple sources and the TAI/UTC leap table). \
     Run them on the host.";

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClockHealthLevel {
    Healthy,
    Degraded,
    Bad,
}

#[derive(Debug, Clone)]
pub struct ClockHealth {
    pub level: ClockHealthLevel,
    /// What was found, in words for an operator. Ends with a pointer to the
    /// NTP setup scripts when the level is not `Healthy`.
    pub findings: Vec<String>,
}

pub fn assess_clock_health(info: &SystemClockInfo) -> ClockHealth {
    let mut level = ClockHealthLevel::Healthy;
    let mut findings = Vec::new();
    let bump = |to: ClockHealthLevel, level: &mut ClockHealthLevel| {
        if to > *level {
            *level = to;
        }
    };

    if !info.synchronized || info.state == "error" {
        bump(ClockHealthLevel::Bad, &mut level);
        findings.push(
            "Kernel reports the clock is NOT synchronized — discipline source missing or failing."
                .into(),
        );
    }

    if info.tai_offset_sec == 0 {
        bump(ClockHealthLevel::Degraded, &mut level);
        findings.push(format!(
            "TAI − UTC offset is 0. The discipline daemon has not configured leap seconds, \
             so CLOCK_TAI cannot be trusted as a global time source. \
             Expected value as of 2026 is {EXPECTED_TAI_OFFSET_SEC} s."
        ));
    } else if info.tai_offset_sec != EXPECTED_TAI_OFFSET_SEC {
        findings.push(format!(
            "TAI − UTC offset is {} s (expected {EXPECTED_TAI_OFFSET_SEC} as of 2026). \
             OK if your discipline source is authoritative on leap seconds.",
            info.tai_offset_sec
        ));
    }

    if info.max_error_us > MAX_ERROR_LIMIT_US {
        bump(ClockHealthLevel::Degraded, &mut level);
        findings.push(format!(
            "Max error estimate is {:.0} ms — kernel is uncertain about sync quality. \
             A healthy disciplined clock stays under 100 ms.",
            info.max_error_us as f64 / 1000.0
        ));
    }

    if info.offset_ns.abs() > OFFSET_LIMIT_NS {
        bump(ClockHealthLevel::Degraded, &mut level);
        findings.push(format!(
            "Current offset is {:.0} µs (>1 ms) — large correction in flight, sync is drifting.",
            info.offset_ns.abs() as f64 / 1000.0
        ));
    }

    if findings.is_empty() {
        findings.push("No issues detected. Clock looks well disciplined.".into());
    } else if level > ClockHealthLevel::Healthy {
        findings.push(CHRONY_SETUP_HINT.into());
    }

    ClockHealth { level, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrony_with_leap_table() -> SystemClockInfo {
        SystemClockInfo {
            tai_offset_sec: 37,
            state: "ok".into(),
            synchronized: true,
            max_error_us: 4_785,
            ..Default::default()
        }
    }

    #[test]
    fn well_disciplined_clock_is_healthy_without_hint() {
        let health = assess_clock_health(&chrony_with_leap_table());
        assert_eq!(health.level, ClockHealthLevel::Healthy);
        assert!(!health.findings.iter().any(|f| f == CHRONY_SETUP_HINT));
    }

    #[test]
    fn timesyncd_clock_is_degraded_on_leap_table_and_max_error() {
        let info = SystemClockInfo {
            tai_offset_sec: 0,
            max_error_us: 940_000,
            ..chrony_with_leap_table()
        };
        let health = assess_clock_health(&info);
        assert_eq!(health.level, ClockHealthLevel::Degraded);
        assert!(health.findings[0].contains("TAI − UTC offset is 0"));
        assert!(health.findings[1].contains("940 ms"));
        assert_eq!(health.findings.last().unwrap(), CHRONY_SETUP_HINT);
    }

    #[test]
    fn unsynchronized_clock_is_bad() {
        for info in [
            SystemClockInfo {
                synchronized: false,
                ..chrony_with_leap_table()
            },
            SystemClockInfo {
                state: "error".into(),
                ..chrony_with_leap_table()
            },
        ] {
            assert_eq!(assess_clock_health(&info).level, ClockHealthLevel::Bad);
        }
    }

    #[test]
    fn large_offset_is_degraded() {
        let info = SystemClockInfo {
            offset_ns: -2_500_000,
            ..chrony_with_leap_table()
        };
        assert_eq!(assess_clock_health(&info).level, ClockHealthLevel::Degraded);
    }

    #[test]
    fn unexpected_tai_offset_is_noted_but_healthy() {
        let info = SystemClockInfo {
            tai_offset_sec: 36,
            ..chrony_with_leap_table()
        };
        let health = assess_clock_health(&info);
        assert_eq!(health.level, ClockHealthLevel::Healthy);
        assert!(!health.findings.iter().any(|f| f == CHRONY_SETUP_HINT));
    }
}
