//! Server subprocess manager
//!
//! Spawns and manages the Strom server as a child process,
//! capturing and displaying its logs in real-time.

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::ExecutableCommand;

/// Manages a Strom server subprocess
pub struct ServerManager {
    child: Child,
    log_task: JoinHandle<()>,
    _shutdown_tx: mpsc::Sender<()>,
}

impl ServerManager {
    /// Start the Strom server with GUI
    pub async fn start(port: u16) -> anyhow::Result<Self> {
        // Use the pre-built binary directly (much faster startup)
        // The binary should be at target/debug/strom or target/release/strom
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_dir = std::path::Path::new(manifest_dir).parent().unwrap();

        // Try debug build first, then release
        let debug_binary = workspace_dir.join("target/debug/strom");
        let release_binary = workspace_dir.join("target/release/strom");

        let binary_path = if debug_binary.exists() {
            debug_binary
        } else if release_binary.exists() {
            release_binary
        } else {
            anyhow::bail!("Strom binary not found. Run 'cargo build --bin strom' first.");
        };

        // Start the server process
        let mut child = Command::new(&binary_path)
            .args(["--port", &port.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Spawn a task to read and display logs
        let log_task = tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    result = stdout_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                print_server_log(&line, false);
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                    result = stderr_reader.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                print_server_log(&line, true);
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(Self {
            child,
            log_task,
            _shutdown_tx: shutdown_tx,
        })
    }

    /// Stop the server gracefully
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        // Try to kill the child process
        self.child.kill().await?;

        // Wait for log task to finish
        self.log_task.abort();

        Ok(())
    }
}

fn print_server_log(line: &str, is_stderr: bool) {
    let mut stdout = std::io::stdout();

    // Use gray color for server output to distinguish from test output
    let color = if is_stderr {
        Color::DarkYellow
    } else {
        Color::DarkGrey
    };

    let _ = stdout.execute(SetForegroundColor(color));
    let _ = stdout.execute(Print(format!("[SERVER] {}\n", line)));
    let _ = stdout.execute(ResetColor);
}
