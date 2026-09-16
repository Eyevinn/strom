//! Guards for the App Nap opt-out in `macos_app_nap`.
//!
//! ~32 s after launch macOS moves headless Strom into the background QoS band and
//! every thread drops to scheduling priority 4 — on Apple Silicon, efficiency
//! cores at a throttled clock, which costs a 1080p x264 flow ~7x its wall clock.
//! The server holds an `NSProcessInfo` activity assertion to opt out.
//!
//! Two failure modes, tested separately because they cost three orders of
//! magnitude apart: the call going missing from `run_headless_entry` (the
//! realistic regression, caught in well under a second), and macOS ceasing to
//! honour the assertion (only observable by waiting out the demotion window, so
//! opt-in and run by hand).
#![cfg(target_os = "macos")]

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Logged by `hold_activity_for_process_lifetime`. Asserted below, so treat it as
/// part of the contract rather than a message to reword freely.
const MARKER: &str = "App Nap opt-out";

/// Logged well after the assertion is taken. Once this appears, the marker either
/// arrived or never will, so the test can fail immediately instead of waiting out
/// the deadline.
const READY: &str = "Server listening on";

/// Darwin's QoS classes map onto scheduling priorities: 46-47 user-interactive,
/// 37 user-initiated, 31 default, 20 utility, 4 background.
const BACKGROUND_PRIORITY: u32 = 4;

struct Server {
    child: std::process::Child,
    log: tempfile::NamedTempFile,
    _dir: tempfile::TempDir,
}

impl Server {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("create a data directory");
        let log = tempfile::NamedTempFile::new().expect("create a log file");
        // Port 0 lets the OS pick, so concurrent tests cannot collide.
        let child = Command::new(env!("CARGO_BIN_EXE_strom"))
            .args(["--headless", "--port", "0", "--data-dir"])
            .arg(dir.path())
            .stdout(log.reopen().expect("reopen the log for stdout"))
            .stderr(log.reopen().expect("reopen the log for stderr"))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn the headless server");
        Self {
            child,
            log,
            _dir: dir,
        }
    }

    fn log_contains(&self, needle: &str) -> bool {
        let mut text = String::new();
        self.log
            .reopen()
            .expect("reopen the log")
            .read_to_string(&mut text)
            .ok();
        text.contains(needle)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The wiring test. Fails if the call in `run_headless_entry` is removed.
#[test]
fn headless_entry_takes_the_activity_assertion() {
    let server = Server::spawn();

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut started = false;
    while Instant::now() < deadline {
        if server.log_contains(MARKER) {
            return;
        }
        if server.log_contains(READY) {
            started = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        started,
        "the headless server never got as far as logging {READY:?}, so this says \
         nothing about the activity assertion"
    );
    panic!(
        "the headless server reached {READY:?} without logging {MARKER:?}: nothing \
         took an NSProcessInfo activity assertion, so macOS will App-Nap the process \
         into the background QoS band about thirty seconds in and every pipeline \
         after that will run on efficiency cores"
    );
}

/// `ps -M` prints one row per thread. Priority is the only column that is digits
/// followed by the scheduling-policy letter (`31T`, `4T`, `50R`) — %CPU has a
/// dot, TIME has colons, STAT has no digits.
fn priority_of_row(row: &str) -> Option<u32> {
    row.split_whitespace().find_map(|token| {
        if !token.is_ascii() || token.len() < 2 {
            return None;
        }
        let (digits, policy) = token.split_at(token.len() - 1);
        if !policy.chars().all(|c| c.is_ascii_uppercase()) {
            return None;
        }
        digits.parse().ok()
    })
}

fn max_thread_priority(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .arg("-M")
        .arg(pid.to_string())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(priority_of_row)
        .max()
}

/// The behavioural check: does macOS still honour the assertion?
///
/// Opt-in, because the demotion lands at ~32 s and cannot be observed without
/// waiting past it. Run by hand on a real Mac after touching this module or
/// upgrading macOS or `objc2-foundation`:
///
/// ```text
/// STROM_TEST_APP_NAP=1 cargo test --test macos_app_nap_test -- --ignored
/// ```
///
/// Not wired into CI: the GitHub macOS runner is a headless VM and it is
/// unverified whether it App-Naps at all.
#[test]
#[ignore = "waits ~50 s for the App Nap window; set STROM_TEST_APP_NAP=1"]
fn server_is_not_demoted_to_background_qos() {
    if std::env::var_os("STROM_TEST_APP_NAP").is_none() {
        eprintln!("skipped: set STROM_TEST_APP_NAP=1 to run");
        return;
    }
    let settle = Duration::from_secs(50);
    let mut server = Server::spawn();

    std::thread::sleep(settle);

    let exited = server.child.try_wait().expect("poll the server");
    let observed = max_thread_priority(server.child.id());

    assert!(
        exited.is_none(),
        "the server exited before the priority could be read ({exited:?})"
    );
    let highest = observed.expect("read thread priorities from `ps -M`");
    assert!(
        highest > BACKGROUND_PRIORITY,
        "every thread sits at scheduling priority {highest} after {}s: the process \
         has been App-Napped into the background QoS band and its pipelines will \
         run on efficiency cores. The NSProcessInfo activity assertion is no longer \
         effective.",
        settle.as_secs()
    );
}
