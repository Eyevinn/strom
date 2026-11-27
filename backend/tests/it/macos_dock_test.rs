//! Guard for `macos_dock`: headless Strom stays out of the macOS Dock (#1279).
//!
//! GStreamer's GL library sets the Regular activation policy each time it
//! creates a GL context on macOS, which makes the process a `Foreground` app
//! with a Dock icon. Strom creates one at startup (the GL renderer probe) and
//! flows create more. The test checks the LaunchServices application type after
//! both, so it fails if the observer that puts the policy back is removed, or if
//! it is replaced by a one-off reset after the startup probe.
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

struct Server {
    child: std::process::Child,
    log: tempfile::NamedTempFile,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Server {
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

    /// Give the main run loop time to run the observer, then read the type.
    fn settled_application_type(&self) -> String {
        std::thread::sleep(Duration::from_secs(1));
        self.application_type()
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
    let mut server = Server::spawn_headless();

    assert!(
        server.wait_for_log(READY, Duration::from_secs(90)),
        "the headless server never logged {READY:?}:\n{}",
        server.log_text()
    );
    assert!(
        server.log_text().contains(GL_PROBED),
        "the server never logged {GL_PROBED:?}, so no GL context was created at \
         startup and this test cannot tell whether GL's Regular policy is undone"
    );

    let after_probe = server.settled_application_type();
    assert_eq!(
        after_probe, "UIElement",
        "after the startup GL probe, headless Strom is {after_probe:?}: GL set the \
         Regular activation policy and nothing put it back, so it has a Dock icon"
    );

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

    // glupload makes its GL context when the flow negotiates; give it time.
    std::thread::sleep(Duration::from_secs(2));
    let after_flow = server.settled_application_type();
    assert_eq!(
        after_flow, "UIElement",
        "with a GL flow running, headless Strom is {after_flow:?}: the policy was \
         put back once after startup but not after the flow's GL context"
    );
}
