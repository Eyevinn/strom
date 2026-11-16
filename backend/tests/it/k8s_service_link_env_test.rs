//! A Kubernetes Service named `strom` injects `STROM_PORT=tcp://<ip>:<port>`
//! into the pod, which used to stop Strom at startup (#1266). These run the
//! real binary, so they fail if `main` stops dropping the value before clap
//! parses the arguments.
//!
//! `--version-info` exits right after argument parsing, so no server starts.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Runs `strom --version-info` in `dir` (so no `.env` from the checkout is
/// read) with `STROM_PORT` set to `port`.
fn version_info(dir: &Path, port: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_strom"))
        .arg("--version-info")
        .current_dir(dir)
        .env("STROM_PORT", port)
        .stdin(Stdio::null())
        .output()
        .expect("run strom")
}

#[test]
fn service_link_value_in_strom_port_is_ignored() {
    let dir = tempfile::tempdir().expect("create a directory");
    let out = version_info(dir.path(), "tcp://192.0.2.10:8080");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn invalid_strom_port_still_fails() {
    let dir = tempfile::tempdir().expect("create a directory");
    let out = version_info(dir.path(), "abc");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("invalid value 'abc' for '--port <PORT>'"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// dotenvy does not set a key that is already set. If the injected value were
/// only dropped after `.env` is loaded, it would shadow the `.env` value and
/// then be deleted, losing both. An invalid `.env` value makes the loss
/// visible: it must still reach clap and fail.
#[test]
fn service_link_value_does_not_shadow_dotenv() {
    let dir = tempfile::tempdir().expect("create a directory");
    std::fs::write(dir.path().join(".env"), "STROM_PORT=abc\n").expect("write .env");
    let out = version_info(dir.path(), "tcp://192.0.2.10:8080");
    assert_eq!(
        out.status.code(),
        Some(2),
        "the .env value was lost; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
