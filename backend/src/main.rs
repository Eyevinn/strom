//! Strom backend server.

use clap::Parser;
use gstreamer::glib;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

#[cfg(feature = "gui")]
use strom::create_app_with_state_and_auth;
use strom::{auth, config::Config, create_app_with_state, state::AppState};

/// Handle the hash-password subcommand
fn handle_hash_password(password: Option<&str>) -> anyhow::Result<()> {
    use std::io::{self, Write};

    let password = if let Some(pwd) = password {
        pwd.to_string()
    } else {
        // Read from stdin
        print!("Enter password to hash: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        input.trim().to_string()
    };

    if password.is_empty() {
        eprintln!("Error: Password cannot be empty");
        std::process::exit(1);
    }

    match auth::hash_password(&password) {
        Ok(hash) => {
            println!("\nPassword hash:");
            println!("{}", hash);
            println!("\nAdd this to your environment:");
            println!("export STROM_ADMIN_PASSWORD_HASH='{}'", hash);
        }
        Err(e) => {
            eprintln!("Error hashing password: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Strom - GStreamer Flow Engine Backend
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Port to listen on
    #[arg(short, long, env = "STROM_PORT")]
    port: Option<u16>,

    /// Data directory (contains flows.json and blocks.json)
    #[arg(long, env = "STROM_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// Path to flows storage file (overrides --data-dir)
    #[arg(long, env = "STROM_FLOWS_PATH")]
    flows_path: Option<PathBuf>,

    /// Path to blocks storage file (overrides --data-dir)
    #[arg(long, env = "STROM_BLOCKS_PATH")]
    blocks_path: Option<PathBuf>,

    /// Database URL (e.g., postgresql://user:pass@localhost/strom)
    /// If set, database storage is used instead of JSON files
    /// Supported schemes: postgresql://
    #[arg(long, env = "STROM_DATABASE_URL")]
    database_url: Option<String>,

    /// Run in headless mode (no GUI) - only available when gui feature is enabled
    #[cfg(feature = "gui")]
    #[arg(long)]
    headless: bool,

    /// Force X11 display backend (default on WSL2, option on native Linux)
    #[cfg(feature = "gui")]
    #[arg(long)]
    x11: bool,

    /// Force Wayland display backend (default on native Linux, option on WSL2)
    #[cfg(feature = "gui")]
    #[arg(long)]
    wayland: bool,
}

/// Detect if running under WSL (Windows Subsystem for Linux).
#[cfg(feature = "gui")]
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| {
            let lower = v.to_lowercase();
            lower.contains("microsoft") || lower.contains("wsl")
        })
        .unwrap_or(false)
}

#[derive(Parser, Debug)]
enum Commands {
    /// Hash a password for use with STROM_ADMIN_PASSWORD_HASH
    HashPassword {
        /// Password to hash (if not provided, will read from stdin)
        password: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    // Parse command line arguments
    #[cfg_attr(not(feature = "gui"), allow(unused_variables))]
    let args = Args::parse();

    // Handle subcommands before starting server
    if let Some(command) = &args.command {
        match command {
            Commands::HashPassword { password } => {
                return handle_hash_password(password.as_deref());
            }
        }
    }

    // Select display backend based on platform and CLI flags
    // WSL2 has clipboard issues with Wayland (smithay-clipboard), so default to X11 there
    // Native Linux works better with Wayland by default
    // This must happen before any GUI initialization
    #[cfg(feature = "gui")]
    if !args.headless {
        let force_x11 = if args.x11 {
            true // Explicit --x11 flag
        } else if args.wayland {
            false // Explicit --wayland flag
        } else {
            // Default: X11 on WSL (clipboard compatibility), Wayland on native Linux
            is_wsl()
        };

        if force_x11 {
            std::env::set_var("WAYLAND_DISPLAY", "");
        }
    }

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

    // Initialize GStreamer
    gstreamer::init()?;
    info!("GStreamer initialized");

    // Register WebRTC plugins
    gstrswebrtc::plugin_register_static().expect("Could not register webrtc plugins");

    // Start GLib main loop in background thread for bus watch callbacks
    start_glib_main_loop();
    info!("GLib main loop started in background thread");

    #[cfg(feature = "gui")]
    {
        if gui_enabled {
            // GUI mode: Run HTTP server in background, GUI on main thread
            run_with_gui(
                args.port,
                args.data_dir,
                args.flows_path,
                args.blocks_path,
                args.database_url,
            )
        } else {
            // Headless mode: Run HTTP server on main thread
            run_headless(
                args.port,
                args.data_dir,
                args.flows_path,
                args.blocks_path,
                args.database_url,
            )
        }
    }

    #[cfg(not(feature = "gui"))]
    {
        // Always headless when gui feature is disabled
        run_headless(
            args.port,
            args.data_dir,
            args.flows_path,
            args.blocks_path,
            args.database_url,
        )
    }
}

#[cfg(feature = "gui")]
fn run_with_gui(
    port: Option<u16>,
    data_dir: Option<PathBuf>,
    flows_path: Option<PathBuf>,
    blocks_path: Option<PathBuf>,
    database_url: Option<String>,
) -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Create tokio runtime on main thread
    let runtime = tokio::runtime::Runtime::new()?;

    // Shared shutdown flag for coordination between threads
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_gui = shutdown_flag.clone();

    // Create auth config and generate native GUI token if auth is enabled
    let mut auth_config = auth::AuthConfig::from_env();
    let native_gui_token = if auth_config.enabled {
        let token = auth_config.generate_native_gui_token();
        info!("Generated native GUI token for auto-authentication");
        Some(token)
    } else {
        None
    };

    // Initialize and start server in runtime
    let (server_started_tx, server_started_rx) = std::sync::mpsc::channel::<u16>();

    runtime.spawn(async move {
        // Load configuration from CLI args, env vars, and config files
        let config = Config::from_figment(port, data_dir, flows_path, blocks_path, database_url)
            .expect("Failed to resolve configuration");
        info!("Configuration loaded");

        let actual_port = config.port;

        // Create application with persistent storage
        let state = if let Some(ref db_url) = config.database_url {
            info!("Using PostgreSQL storage");
            AppState::with_postgres_storage(db_url, &config.blocks_path)
                .await
                .expect("Failed to initialize PostgreSQL storage")
        } else {
            info!("Using JSON file storage");
            AppState::with_json_storage(&config.flows_path, &config.blocks_path)
        };
        state
            .load_from_storage()
            .await
            .expect("Failed to load storage");

        // GStreamer elements are discovered lazily on first /api/elements request

        // Restart flows that were running before shutdown
        restart_flows(&state).await;

        let app = create_app_with_state_and_auth(state.clone(), auth_config).await;

        // Start server - bind to 0.0.0.0 to be accessible from all interfaces
        let addr = SocketAddr::from(([0, 0, 0, 0], config.port));

        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => {
                info!("Server listening on {}", addr);
                l
            }
            Err(e) => {
                eprintln!("Error: Failed to bind to port {}: {}", config.port, e);
                eprintln!("Port {} is already in use or unavailable.", config.port);
                eprintln!("Please either:");
                eprintln!("  - Stop the other process using this port");
                eprintln!("  - Use a different port with --port <PORT> or STROM_PORT=<PORT>");
                std::process::exit(1);
            }
        };

        // Notify main thread that server is ready and send the actual port
        server_started_tx.send(actual_port).ok();

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

    // Wait for server to start and get the actual port
    let actual_port = server_started_rx
        .recv()
        .expect("Failed to receive port from server");
    std::thread::sleep(std::time::Duration::from_millis(100));

    info!("Launching native GUI on main thread...");

    // Enter runtime context so tokio::spawn() works from GUI
    let _guard = runtime.enter();

    // Run GUI on main thread (blocks until window closes)
    // If auth is enabled, pass the native GUI token for auto-authentication
    let gui_result = if let Some(token) = native_gui_token {
        strom::gui::launch_gui_with_auth(actual_port, shutdown_flag_gui, token)
    } else {
        strom::gui::launch_gui_with_shutdown(actual_port, shutdown_flag_gui)
    };

    if let Err(e) = gui_result {
        error!("GUI error: {:?}", e);
    }

    Ok(())
}

#[tokio::main]
async fn run_headless(
    port: Option<u16>,
    data_dir: Option<PathBuf>,
    flows_path: Option<PathBuf>,
    blocks_path: Option<PathBuf>,
    database_url: Option<String>,
) -> anyhow::Result<()> {
    // Load configuration from CLI args, env vars, and config files
    let config = Config::from_figment(port, data_dir, flows_path, blocks_path, database_url)?;
    info!("Configuration loaded");

    // Create application with persistent storage
    let state = if let Some(ref db_url) = config.database_url {
        info!("Using PostgreSQL storage");
        AppState::with_postgres_storage(db_url, &config.blocks_path).await?
    } else {
        info!("Using JSON file storage");
        AppState::with_json_storage(&config.flows_path, &config.blocks_path)
    };
    state.load_from_storage().await?;

    // GStreamer elements are discovered lazily on first /api/elements request

    // Restart flows that were running before shutdown
    restart_flows(&state).await;

    let app = create_app_with_state(state.clone()).await;

    // Start server - bind to 0.0.0.0 to be accessible from all interfaces (Docker, network, etc.)
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => {
            info!("Server listening on {}", addr);
            l
        }
        Err(e) => {
            eprintln!("Error: Failed to bind to port {}: {}", config.port, e);
            eprintln!("Port {} is already in use or unavailable.", config.port);
            eprintln!("Please either:");
            eprintln!("  - Stop the other process using this port");
            eprintln!("  - Use a different port with --port <PORT> or STROM_PORT=<PORT>");
            std::process::exit(1);
        }
    };

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
