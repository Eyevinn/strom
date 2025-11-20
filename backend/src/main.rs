//! Strom backend server.

use clap::Parser;
use gstreamer::glib;
use std::net::SocketAddr;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

use strom_backend::{config::Config, create_app_with_state, state::AppState};

/// Strom - GStreamer Flow Engine Backend
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run in headless mode (no GUI) - only available when gui feature is enabled
    #[cfg(feature = "gui")]
    #[arg(long)]
    headless: bool,
}

fn main() -> anyhow::Result<()> {
    // Parse command line arguments
    #[cfg_attr(not(feature = "gui"), allow(unused_variables))]
    let args = Args::parse();

    // Initialize logging - use RUST_LOG env var or default to info
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    // Determine if GUI should be enabled
    #[cfg(feature = "gui")]
    let gui_enabled = !args.headless;
    #[cfg(not(feature = "gui"))]
    let gui_enabled = false;

    if gui_enabled {
        info!("Starting Strom backend server with GUI...");
    } else {
        info!("Starting Strom backend server (headless mode)...");
    }

    // On Windows, prevent GStreamer registry from forking during plugin scan
    // This helps isolate panics from problematic plugins
    #[cfg(target_os = "windows")]
    {
        std::env::set_var("GST_REGISTRY_FORK", "no");
        info!("Set GST_REGISTRY_FORK=no for Windows compatibility");
    }

    // Initialize GStreamer
    // Some plugins (like hlssink3) may panic during registration if dependencies are missing
    // Our element discovery code wraps registry access in panic handlers to catch this
    gstreamer::init()?;
    info!("GStreamer initialized");

    gstrswebrtc::plugin_register_static().expect("Could not register webrtc plugins");

    // Start GLib main loop in background thread for bus watch callbacks
    start_glib_main_loop();
    info!("GLib main loop started in background thread");

    if gui_enabled {
        // GUI mode: Run HTTP server in background, GUI on main thread
        run_with_gui()
    } else {
        // Headless mode: Run HTTP server on main thread
        run_headless()
    }
}

#[cfg(feature = "gui")]
fn run_with_gui() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Create tokio runtime on main thread
    let runtime = tokio::runtime::Runtime::new()?;

    // Shared shutdown flag for coordination between threads
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_gui = shutdown_flag.clone();

    // Initialize and start server in runtime
    let (server_started_tx, server_started_rx) = std::sync::mpsc::channel();

    runtime.spawn(async move {
        // Load configuration
        let config = Config::from_env();
        info!("Configuration loaded");

        // Create application with persistent storage
        let state = AppState::with_json_storage(&config.flows_path, &config.blocks_path);
        state
            .load_from_storage()
            .await
            .expect("Failed to load storage");

        // GStreamer elements are discovered lazily on first /api/elements request

        // Restart flows that were running before shutdown
        restart_flows(&state).await;

        let app = create_app_with_state(state.clone()).await;

        // Start server - bind to 0.0.0.0 to be accessible from all interfaces
        let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
        info!("Server listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind");

        // Notify main thread that server is ready
        server_started_tx.send(()).ok();

        // Run HTTP server with graceful shutdown
        let shutdown_signal = async move {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");

            info!("Received Ctrl+C, shutting down gracefully...");

            // Note: We don't need to explicitly stop flows here.
            // GStreamer will clean up when the process exits, and
            // we want to preserve the auto_restart flag for flows
            // that were running, so they restart on next backend startup.

            info!("Signaling GUI to close...");
            shutdown_flag.store(true, Ordering::SeqCst);
        };

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await
            .expect("Server error");
    });

    // Wait for server to start
    server_started_rx.recv().ok();
    std::thread::sleep(std::time::Duration::from_millis(100));

    info!("Launching native GUI on main thread...");

    // Enter runtime context so tokio::spawn() works from GUI
    let _guard = runtime.enter();

    // Run GUI on main thread (blocks until window closes)
    if let Err(e) = strom_backend::gui::launch_gui_with_shutdown(shutdown_flag_gui) {
        error!("GUI error: {:?}", e);
    }

    Ok(())
}

#[tokio::main]
async fn run_headless() -> anyhow::Result<()> {
    // Load configuration
    let config = Config::from_env();
    info!("Configuration loaded");

    // Create application with persistent storage
    let state = AppState::with_json_storage(&config.flows_path, &config.blocks_path);
    state.load_from_storage().await?;

    // GStreamer elements are discovered lazily on first /api/elements request

    // Restart flows that were running before shutdown
    restart_flows(&state).await;

    let app = create_app_with_state(state.clone()).await;

    // Start server - bind to 0.0.0.0 to be accessible from all interfaces (Docker, network, etc.)
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("Server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Set up graceful shutdown handler
    let shutdown_signal = async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");

        info!("Received Ctrl+C, shutting down gracefully...");

        // Note: We don't need to explicitly stop flows here.
        // GStreamer will clean up when the process exits, and
        // we want to preserve the auto_restart flag for flows
        // that were running, so they restart on next backend startup.

        info!("Server shutting down");
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal)
        .await?;

    Ok(())
}

async fn restart_flows(state: &AppState) {
    info!("Restarting flows that have auto_restart enabled...");
    let flows = state.get_flows().await;
    for flow in flows {
        if flow.properties.auto_restart {
            info!("Auto-restarting flow: {} ({})", flow.name, flow.id);
            match state.start_flow(&flow.id).await {
                Ok(_) => info!("Successfully restarted flow: {}", flow.name),
                Err(e) => error!("Failed to restart flow {}: {}", flow.name, e),
            }
        }
    }
}

/// Start GLib main loop in a background thread.
/// This is required for GStreamer bus watch callbacks to be dispatched.
fn start_glib_main_loop() {
    std::thread::spawn(|| {
        info!("GLib main loop thread started");
        let main_loop = glib::MainLoop::new(None, false);
        main_loop.run();
        info!("GLib main loop thread exiting");
    });
}
