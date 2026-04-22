//! System clock synchronization status.
//!
//! On Linux this reads the kernel's `ntp_adjtime(2)` state (the same information
//! used by chrony, ntpd, and systemd-timesyncd) and reports how `CLOCK_REALTIME`
//! / `CLOCK_TAI` are being disciplined. On other platforms the kernel APIs are
//! different or unavailable, so `read_system_clock_info` returns
//! [`SystemClockError::Unsupported`].

use strom_types::api::SystemClockInfo;

#[derive(Debug)]
pub enum SystemClockError {
    /// The platform does not expose kernel clock discipline state in a
    /// compatible form (macOS, Windows, WASM, etc.).
    Unsupported,
    /// The syscall returned an error.
    Io(std::io::Error),
}

impl std::fmt::Display for SystemClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "system clock discipline info is not available on this platform"
            ),
            Self::Io(e) => write!(f, "ntp_adjtime failed: {}", e),
        }
    }
}

impl std::error::Error for SystemClockError {}

#[cfg(target_os = "linux")]
pub fn read_system_clock_info() -> Result<SystemClockInfo, SystemClockError> {
    // SAFETY: `libc::timex` is a plain C struct; zero-initializing is fine.
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    tx.modes = 0; // read-only mode

    // SAFETY: We pass a valid mutable pointer to a zero-initialized timex.
    let state = unsafe { libc::ntp_adjtime(&mut tx) };
    if state < 0 {
        return Err(SystemClockError::Io(std::io::Error::last_os_error()));
    }

    let state_str = match state {
        libc::TIME_OK => "ok",
        libc::TIME_INS => "ins",
        libc::TIME_DEL => "del",
        libc::TIME_OOP => "oop",
        libc::TIME_WAIT => "wait",
        libc::TIME_ERROR => "error",
        _ => "unknown",
    }
    .to_string();

    let synchronized = (tx.status & libc::STA_UNSYNC) == 0;
    let pll_active = (tx.status & libc::STA_PLL) != 0;

    // `offset` is in nanoseconds when STA_NANO is set, otherwise microseconds.
    let offset_ns = if (tx.status & libc::STA_NANO) != 0 {
        tx.offset as i64
    } else {
        (tx.offset as i64).saturating_mul(1_000)
    };

    // `freq` is in units of scaled ppm (ppm << 16).
    let frequency_ppm = tx.freq as f64 / 65_536.0;

    let last_update = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());

    Ok(SystemClockInfo {
        tai_offset_sec: tx.tai,
        state: state_str,
        synchronized,
        pll_active,
        offset_ns,
        frequency_ppm,
        max_error_us: tx.maxerror as i64,
        est_error_us: tx.esterror as i64,
        last_update,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn read_system_clock_info() -> Result<SystemClockInfo, SystemClockError> {
    // macOS has `ntp_gettime(2)` but the struct layout and semantics differ from
    // Linux's `struct timex`; Windows uses `GetSystemTimeAdjustmentPrecise`. Until
    // we need strom to report clock discipline on those platforms, we report
    // `Unsupported` and let the API layer surface a 501.
    Err(SystemClockError::Unsupported)
}
