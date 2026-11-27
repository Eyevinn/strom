//! Guard for `macos_dock`: headless Strom stays out of the macOS Dock (#1279).
//!
//! GStreamer's GL library sets the Regular activation policy each time it
//! creates a GL context on macOS, which makes the process a `Foreground` app
//! with a Dock icon. Strom creates one at startup (the GL renderer probe) and
//! flows create more. The test fails if the observer that puts the policy back
//! is removed, or if it is replaced by a one-off reset after the startup probe.
//!
//! Two checks, after the probe and after a GL flow starts:
//! - The observer's reset log line, counted. This runs everywhere, including the
//!   GitHub macOS runner, where no GUI session registers the process with
//!   LaunchServices but GL still sets the policy on `NSApp`.
//! - The LaunchServices application type (`lsappinfo`), which is what decides
//!   the Dock icon. It needs a GUI session, so it is checked where `lsappinfo`
//!   knows the process (a developer's Mac) and noted as unchecked elsewhere.
#![cfg(target_os = "macos")]

use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use strom_types::Flow;

const READY: &str = "Server listening on";
/// Logged by the startup GL probe. Without it the probe made no GL context, and
/// the test would pass without the fix.
const GL_PROBED: &str = "GL renderer:";
/// Logged at debug level by `macos_dock` each time it puts the policy back.
const RESET: &str = "reset activation policy from Regular to Accessory";

struct Server {
    child: std::process::Child,
    log: tempfile::NamedTempFile,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Server {
    /// Start a headless server and wait until it listens. Retries with a new
    /// port when the server exits before that: the port is picked here and
    /// released before the server binds it, so a parallel test can take it.
    fn start_headless() -> Self {
        for _ in 0..3 {
            let mut server = Self::spawn_headless();
            if server.wait_for_log(READY, Duration::from_secs(90)) {
                return server;
            }
            if server.child.try_wait().ok().flatten().is_none() {
                panic!(
                    "the headless server never logged {READY:?}:\n{}",
                    server.log_text()
                );
            }
        }
        panic!("the headless server exited before logging {READY:?} three times");
    }

    fn spawn_headless() -> Self {
        let dir = tempfile::tempdir().expect("create a data directory");
        let log = tempfile::NamedTempFile::new().expect("create a log file");
        // The test needs the port to reach the API, and the server logs the
        // address it was given rather than the one it bound, so pick one here.
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("find a free port")
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_strom"))
            .args(["--headless", "--no-auto-restart", "--port"])
            .arg(port.to_string())
            .arg("--data-dir")
            .arg(dir.path())
            .env("RUST_LOG", "info,strom::macos_dock=debug")
            .stdout(log.reopen().expect("reopen the log for stdout"))
            .stderr(log.reopen().expect("reopen the log for stderr"))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn the server");
        Self {
            child,
            log,
            port,
            _dir: dir,
        }
    }

    fn log_text(&self) -> String {
        let mut text = String::new();
        self.log
            .reopen()
            .expect("reopen the log")
            .read_to_string(&mut text)
            .ok();
        text
    }

    fn wait_for_log(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.log_text().contains(needle) {
                return true;
            }
            if self.child.try_wait().ok().flatten().is_some() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// The LaunchServices application type: `Foreground` has a Dock icon,
    /// `UIElement` (Accessory) does not.
    fn application_type(&self) -> String {
        let out = Command::new("lsappinfo")
            .args(["info", "-only", "applicationtype", "-app"])
            .arg(self.child.id().to_string())
            .output()
            .expect("run lsappinfo");
        let text = String::from_utf8_lossy(&out.stdout);
        text.split("type=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_default()
            .to_string()
    }

    fn resets(&self) -> usize {
        self.log_text().matches(RESET).count()
    }

    /// Wait until the observer has put the policy back `count` times in all.
    async fn wait_for_resets(&self, count: usize, stage: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.resets() < count {
            assert!(
                Instant::now() < deadline,
                "{stage}: the policy was put back {} time(s), expected {count}. GL \
                 set the Regular activation policy and nothing undid it, so \
                 headless Strom has a Dock icon",
                self.resets()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Where LaunchServices knows the process, check the type that decides the
    /// Dock icon. Without a GUI session `lsappinfo` returns nothing.
    async fn check_dock(&self, stage: &str) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let app_type = self.application_type();
        if app_type.is_empty() {
            eprintln!(
                "{stage}: lsappinfo does not know the process (no GUI session), \
                 so only the reset log was checked"
            );
            return;
        }
        assert_eq!(
            app_type, "UIElement",
            "{stage}: headless Strom is {app_type:?}, so it has a Dock icon"
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn gl_flow() -> serde_json::Value {
    let mut flow = serde_json::to_value(Flow::new("dock-guard-gl")).expect("serialize a flow");
    flow["elements"] = serde_json::json!([
        { "id": "src", "element_type": "videotestsrc",
          "properties": { "is-live": true }, "position": [0.0, 0.0] },
        { "id": "up", "element_type": "glupload",
          "properties": {}, "position": [100.0, 0.0] },
        { "id": "sink", "element_type": "fakesink",
          "properties": {}, "position": [200.0, 0.0] }
    ]);
    flow["links"] = serde_json::json!([
        { "from": "src", "to": "up" },
        { "from": "up", "to": "sink" }
    ]);
    flow
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headless_server_stays_out_of_the_dock_after_gl() {
    let server = Server::start_headless();

    assert!(
        server.log_text().contains(GL_PROBED),
        "the server never logged {GL_PROBED:?}, so no GL context was created at \
         startup and this test cannot tell whether GL's Regular policy is undone"
    );

    server
        .wait_for_resets(1, "after the startup GL probe")
        .await;
    server.check_dock("after the startup GL probe").await;

    let base = format!("http://127.0.0.1:{}/api", server.port);
    let client = reqwest::Client::new();
    let flow = gl_flow();
    let id = flow["id"].as_str().expect("flow id").to_string();
    let created = client
        .post(format!("{base}/flows"))
        .json(&flow)
        .send()
        .await
        .expect("create the GL flow");
    assert!(
        created.status().is_success(),
        "creating the GL flow failed: {}",
        created.text().await.unwrap_or_default()
    );
    let started = client
        .post(format!("{base}/flows/{id}/start"))
        .send()
        .await
        .expect("start the GL flow");
    assert!(
        started.status().is_success(),
        "starting the GL flow failed: {}",
        started.text().await.unwrap_or_default()
    );

    // glupload makes its GL context when the flow negotiates, so wait until
    // the flow runs. Otherwise the check below could read the type before the
    // flow's GL context exists and pass without testing anything.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let flow: serde_json::Value = client
            .get(format!("{base}/flows/{id}"))
            .send()
            .await
            .expect("read the GL flow")
            .json()
            .await
            .expect("parse the GL flow");
        let flow = flow.get("flow").unwrap_or(&flow);
        if flow["running"].as_bool() == Some(true) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the GL flow never ran, so it made no GL context to test: {flow}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // One reset for the probe, one for the flow's own GL context. A fix that
    // resets the policy once after startup stops at one.
    server.wait_for_resets(2, "with a GL flow running").await;
    server.check_dock("with a GL flow running").await;
}
