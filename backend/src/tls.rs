//! Optional TLS support for the Strom server.
//!
//! Uses `axum-server` with `RustlsConfig` for TLS termination, including
//! hot reload of certificate files via the `notify` crate.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use axum_server::tls_rustls::RustlsConfig;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tracing::{debug, error, info, warn};

/// Load TLS certificate and key from PEM files, returning a [`RustlsConfig`]
/// that supports hot reload via [`RustlsConfig::reload_from_pem_file`].
pub async fn load_rustls_config(cert_path: &Path, key_path: &Path) -> anyhow::Result<RustlsConfig> {
    // Explicitly select the ring crypto provider. Multiple providers are compiled
    // in (ring via reqwest, aws-lc-rs via gst-plugin-webrtc) so rustls cannot
    // auto-detect which one to use. Ok(()) on first call, Err on subsequent (ignored).
    let _ = rustls::crypto::ring::default_provider().install_default();

    info!("Loading TLS certificate from {}", cert_path.display());
    info!("Loading TLS private key from {}", key_path.display());

    let config = RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load TLS config: {}", e))?;

    info!("TLS configuration loaded successfully");
    Ok(config)
}

/// Spawn a background thread that watches the TLS certificate and key files
/// for changes and reloads the configuration automatically.
///
/// Uses a 2-second debounce to avoid reloading multiple times when both
/// cert and key files are updated in quick succession (e.g. certbot renewal).
pub fn spawn_cert_watcher(
    cert_path: &Path,
    key_path: &Path,
    config: RustlsConfig,
) -> anyhow::Result<()> {
    // Canonicalize so we can reliably compare against absolute paths from notify events
    let cert = std::fs::canonicalize(cert_path)
        .map_err(|e| anyhow::anyhow!("Cannot resolve cert path {}: {}", cert_path.display(), e))?;
    let key = std::fs::canonicalize(key_path)
        .map_err(|e| anyhow::anyhow!("Cannot resolve key path {}: {}", key_path.display(), e))?;

    let cert_dir = cert
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cert path has no parent directory"))?
        .to_path_buf();
    let key_dir = key
        .parent()
        .ok_or_else(|| anyhow::anyhow!("key path has no parent directory"))?
        .to_path_buf();

    info!(
        "Watching TLS certificate files for changes: cert={}, key={}",
        cert.display(),
        key.display()
    );

    // Capture the tokio runtime handle before spawning the OS thread,
    // since std::thread::spawn doesn't carry tokio context.
    let rt = tokio::runtime::Handle::current();

    std::thread::spawn(move || {
        if let Err(e) = run_cert_watcher(&cert, &key, &cert_dir, &key_dir, config, rt) {
            error!("TLS certificate watcher exited with error: {}", e);
        }
    });

    Ok(())
}

fn run_cert_watcher(
    cert: &PathBuf,
    key: &PathBuf,
    cert_dir: &PathBuf,
    key_dir: &PathBuf,
    config: RustlsConfig,
    rt: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(cert_dir, RecursiveMode::NonRecursive)?;
    if key_dir != cert_dir {
        watcher.watch(key_dir, RecursiveMode::NonRecursive)?;
    }
    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                let dominated = matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_));
                let affects_our_files = event.paths.iter().any(|p| p == cert || p == key);

                if dominated && affects_our_files {
                    debug!("TLS file change detected: {:?}", event.paths);

                    // Debounce: wait for both files to be written
                    std::thread::sleep(Duration::from_secs(2));

                    // Drain any queued events during the debounce window
                    while rx.try_recv().is_ok() {}

                    info!("Reloading TLS certificate files...");
                    let cert_clone = cert.clone();
                    let key_clone = key.clone();
                    let config_clone = config.clone();
                    rt.spawn(async move {
                        match config_clone
                            .reload_from_pem_file(&cert_clone, &key_clone)
                            .await
                        {
                            Ok(()) => info!("TLS certificate reloaded successfully"),
                            Err(e) => {
                                warn!(
                                    "Failed to reload TLS certificate (keeping old config): {}",
                                    e
                                )
                            }
                        }
                    });
                }
            }
            Ok(Err(e)) => {
                warn!("File watcher error: {}", e);
            }
            Err(e) => {
                error!("File watcher channel closed: {}", e);
                break;
            }
        }
    }
    Ok(())
}
