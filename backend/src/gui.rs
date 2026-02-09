//! Native GUI module - launches the frontend in native mode.
//!
//! GUI is enabled by default. Use --features no-gui to disable.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(not(feature = "no-gui"))]
pub fn launch_gui(port: u16, tls_enabled: bool) -> eframe::Result<()> {
    tracing::info!("Launching native GUI connecting to port {}...", port);
    strom_frontend::run_native_gui(port, tls_enabled)
}

#[cfg(not(feature = "no-gui"))]
pub fn launch_gui_with_shutdown(
    port: u16,
    tls_enabled: bool,
    shutdown_flag: Arc<AtomicBool>,
) -> eframe::Result<()> {
    tracing::info!(
        "Launching native GUI with shutdown handler connecting to port {}...",
        port
    );
    strom_frontend::run_native_gui_with_shutdown(port, tls_enabled, shutdown_flag, None)
}

/// Launch the native GUI with authentication token for auto-login.
#[cfg(not(feature = "no-gui"))]
pub fn launch_gui_with_auth(
    port: u16,
    tls_enabled: bool,
    shutdown_flag: Arc<AtomicBool>,
    auth_token: String,
) -> eframe::Result<()> {
    tracing::info!(
        "Launching native GUI with auth token connecting to port {}...",
        port
    );
    strom_frontend::run_native_gui_with_shutdown(port, tls_enabled, shutdown_flag, Some(auth_token))
}

#[cfg(feature = "no-gui")]
pub fn launch_gui(_port: u16, _tls_enabled: bool) -> Result<(), String> {
    Err("GUI disabled. Rebuild without --features no-gui".to_string())
}

#[cfg(feature = "no-gui")]
pub fn launch_gui_with_shutdown(
    _port: u16,
    _tls_enabled: bool,
    _shutdown_flag: Arc<AtomicBool>,
) -> Result<(), String> {
    Err("GUI disabled. Rebuild without --features no-gui".to_string())
}

#[cfg(feature = "no-gui")]
pub fn launch_gui_with_auth(
    _port: u16,
    _tls_enabled: bool,
    _shutdown_flag: Arc<AtomicBool>,
    _auth_token: String,
) -> Result<(), String> {
    Err("GUI disabled. Rebuild without --features no-gui".to_string())
}
