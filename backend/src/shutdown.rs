//! Graceful shutdown of the process on SIGINT/SIGTERM, the one sequence the
//! GUI and the headless mode both run.

use crate::state::AppState;
use std::net::SocketAddr;
use std::time::Duration;
use tracing::{info, warn};

/// How long the HTTP server is given to finish in-flight requests.
pub const HTTP_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The shutdown signals, installed once so that a second signal is not missed
/// between the first one and waiting for the next.
pub struct ShutdownSignals {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    /// Install the SIGINT and SIGTERM handlers. From here on these signals
    /// no longer end the process on their own.
    pub fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Self {
                sigterm: signal(SignalKind::terminate())
                    .expect("Failed to install SIGTERM handler"),
                sigint: signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler"),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// Wait for the next SIGINT or SIGTERM (Ctrl+C elsewhere).
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.sigterm.recv() => info!("Received SIGTERM"),
                _ = self.sigint.recv() => info!("Received SIGINT (Ctrl+C)"),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
            info!("Received Ctrl+C");
        }
    }
}

/// Wait for a shutdown signal, then run [`shutdown`]. A second signal while
/// it runs exits the process at once, without waiting for the drains or the
/// HTTP server.
pub async fn shutdown_on_signal(handle: axum_server::Handle<SocketAddr>, state: AppState) {
    let mut signals = ShutdownSignals::install();
    signals.recv().await;
    info!("Shutting down gracefully (signal again to exit at once)...");
    tokio::spawn(async move {
        signals.recv().await;
        warn!("Second shutdown signal, exiting without waiting for the shutdown to finish");
        std::process::exit(1);
    });
    shutdown(&handle, &state).await;
}

/// Shut down: stop accepting HTTP connections and let in-flight requests
/// finish for [`HTTP_SHUTDOWN_GRACE`], and meanwhile run the stop drains of
/// every running flow (at most `STOP_DRAIN_TIMEOUT` in all), so blocks finish
/// their output (a recorder's open fragment) before the process exits. From
/// here on no flow starts. Returns once the drains are done; the HTTP server
/// returns from serving on its own.
pub async fn shutdown(handle: &axum_server::Handle<SocketAddr>, state: &AppState) {
    info!("Server shutting down");
    handle.graceful_shutdown(Some(HTTP_SHUTDOWN_GRACE));
    state.run_stop_drains_for_shutdown().await;
}
