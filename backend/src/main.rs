//! Strom backend server.

use clap::Parser;
use gstreamer::glib;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use strom_types::flow::GStreamerClockType;

#[cfg(not(feature = "no-gui"))]
use strom::create_app_with_state_and_auth;
use strom::{auth, config::Config, create_app_with_state, state::AppState};

/// Initialize logging with optional file output and configurable log level
fn init_logging(log_file: Option<&PathBuf>, log_level: Option<&String>) -> anyhow::Result<()> {
    use time::UtcOffset;
    use tracing_subscriber::fmt::time::OffsetTime;

    // Get local UTC offset for timestamp formatting
    // This must be done before any threads are spawned
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let timer = OffsetTime::new(local_offset, time::format_description::well_known::Rfc3339);

    // Priority: RUST_LOG env var > config file log_level > default "info"
    let env_filter = if let Ok(filter) = EnvFilter::try_from_default_env() {
        // RUST_LOG is set, use it (highest priority)
        filter
    } else if let Some(level) = log_level {
        // Use log level from config file
        EnvFilter::new(level)
    } else {
        // Default to info
        EnvFilter::new("info")
    };

    if let Some(log_path) = log_file {
        // Create parent directory if it doesn't exist
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Clear the log file if it exists by truncating it
        if log_path.exists() {
            std::fs::write(log_path, "")?;
        }

        // Set up file appender
        let file_appender = tracing_appender::rolling::never(
            log_path.parent().unwrap_or(std::path::Path::new(".")),
            log_path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("strom.log")),
        );

        // Create layers: stdout + file with local time
        let stdout_layer = fmt::layer()
            .with_target(false)
            .with_timer(timer.clone())
            .compact()
            .with_writer(std::io::stdout);

        let file_layer = fmt::layer()
            .with_target(true)
            .with_timer(timer)
            .with_ansi(false)
            .with_writer(file_appender);

        // Combine layers
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout_layer)
            .with(file_layer)
            .init();

        eprintln!("Logging to file: {}", log_path.display());
    } else {
        // Stdout only with local time
        fmt()
            .with_env_filter(env_filter)
            .with_target(false)
            .with_timer(timer)
            .compact()
            .init();
    }

    Ok(())
}

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
            println!("\nAdd both variables to your environment to enable authentication:");
            println!("export STROM_ADMIN_USER='admin'");
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

    /// Path to media files directory (overrides --data-dir)
    #[arg(long, env = "STROM_MEDIA_PATH")]
    media_path: Option<PathBuf>,

    /// Database URL (e.g., postgresql://user:pass@localhost/strom)
    /// If set, database storage is used instead of JSON files
    /// Supported schemes: postgresql://
    #[arg(long, env = "STROM_DATABASE_URL")]
    database_url: Option<String>,

    /// Run in headless mode (no GUI)
    #[cfg(not(feature = "no-gui"))]
    #[arg(long)]
    headless: bool,

    /// Force X11 display backend (default on WSL2, option on native Linux)
    #[cfg(not(feature = "no-gui"))]
    #[arg(long)]
    x11: bool,

    /// Force Wayland display backend (default on native Linux, option on WSL2)
    #[cfg(not(feature = "no-gui"))]
    #[arg(long)]
    wayland: bool,

    /// Disable automatic restart of flows on startup (useful for development/testing)
    #[arg(long)]
    no_auto_restart: bool,

    /// Print detailed version and build information and exit
    #[arg(long)]
    version_info: bool,
}

/// Detect if running under WSL (Windows Subsystem for Linux).
#[cfg(not(feature = "no-gui"))]
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
    // Initialize process startup time before anything else
    strom::version::init_process_startup_time();

    // Parse command line arguments
    #[cfg_attr(feature = "no-gui", allow(unused_variables))]
    let args = Args::parse();

    // Handle subcommands before starting server
    if let Some(command) = &args.command {
        match command {
            Commands::HashPassword { password } => {
                return handle_hash_password(password.as_deref());
            }
        }
    }

    // Handle --version-info flag
    if args.version_info {
        let info = strom::version::VersionInfo::get();
        println!("Strom - GStreamer Flow Engine");
        println!("==============================");
        println!("Version:     v{}", info.version);
        if !info.git_tag.is_empty() {
            println!("Tag:         {}", info.git_tag);
        }
        println!("Git Hash:    {}", info.git_hash);
        println!("Branch:      {}", info.git_branch);
        if info.git_dirty {
            println!("Status:      Modified (dirty)");
        }
        println!("Build Time:  {}", info.build_timestamp);
        println!("Build ID:    {}", info.build_id);
        println!("GStreamer:   {}", info.gstreamer_version);
        println!("OS:          {}", info.os_info);
        if info.in_docker {
            println!("Container:   Docker");
        }
        return Ok(());
    }

    // Select display backend based on platform and CLI flags
    // WSL2 has clipboard issues with Wayland (smithay-clipboard), so default to X11 there
    // Native Linux works better with Wayland by default
    // This must happen before any GUI initialization
    #[cfg(not(feature = "no-gui"))]
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

    // Load configuration early to get log_file setting
    let config = Config::from_figment(
        args.port,
        args.data_dir.clone(),
        args.flows_path.clone(),
        args.blocks_path.clone(),
        args.media_path.clone(),
        args.database_url.clone(),
    )
    .unwrap_or_else(|e| {
        eprintln!("Failed to load configuration: {}", e);
        std::process::exit(1);
    });

    // Initialize logging with optional file output and log level
    if let Err(e) = init_logging(config.log_file.as_ref(), config.log_level.as_ref()) {
        eprintln!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }

    // Determine if GUI should be enabled
    #[cfg(not(feature = "no-gui"))]
    let gui_enabled = !args.headless;
    #[cfg(feature = "no-gui")]
    let gui_enabled = false;

    // Log version and build info at startup
    let version_info = strom::version::VersionInfo::get();
    info!(
        "Strom v{} ({}) build_id={} gstreamer={}",
        version_info.version,
        if version_info.git_tag.is_empty() {
            &version_info.git_hash
        } else {
            &version_info.git_tag
        },
        &version_info.build_id[..8.min(version_info.build_id.len())],
        version_info.gstreamer_version
    );

    if gui_enabled {
        info!("Starting Strom backend server with GUI...");
    } else {
        info!("Starting Strom backend server (headless mode)...");
    }

    #[cfg(not(feature = "no-gui"))]
    {
        if gui_enabled {
            // GUI mode: Run HTTP server in background, GUI on main thread
            run_with_gui(config, args.no_auto_restart)
        } else {
            // Headless mode: Run HTTP server on main thread
            run_headless(config, args.no_auto_restart)
        }
    }

    #[cfg(feature = "no-gui")]
    {
        // Always headless when no-gui feature is enabled
        run_headless(config, args.no_auto_restart)
    }
}

#[cfg(not(feature = "no-gui"))]
fn run_with_gui(config: Config, no_auto_restart: bool) -> anyhow::Result<()> {
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
        // Initialize GStreamer INSIDE tokio runtime
        gstreamer::init().expect("Failed to initialize GStreamer");
        info!("GStreamer initialized");

        // Register GStreamer plugins statically
        gstwebrtchttp::plugin_register_static().expect("Could not register webrtchttp plugins");
        gstrsinter::plugin_register_static().expect("Could not register inter plugins");

        // Start GLib main loop in background thread for bus watch callbacks
        start_glib_main_loop();
        info!("GLib main loop started in background thread");

        info!("Configuration loaded");

        let actual_port = config.port;

        // Create application with persistent storage
        let state = if let Some(ref db_url) = config.database_url {
            info!("Using PostgreSQL storage");
            AppState::with_postgres_storage(db_url, &config.blocks_path, &config.media_path)
                .await
                .expect("Failed to initialize PostgreSQL storage")
        } else {
            info!("Using JSON file storage");
            AppState::with_json_storage(&config.flows_path, &config.blocks_path, &config.media_path)
        };
        state
            .load_from_storage()
            .await
            .expect("Failed to load storage");

        // Start background services (SAP discovery listener and announcer)
        state.start_services().await;

        // GStreamer elements are discovered lazily on first /api/elements request

        // Create the HTTP app BEFORE auto-restart
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

        // Restart flows AFTER server binds, on a separate tokio task
        // This ensures auto-restart runs on a worker thread, not the main thread,
        // which prevents GStreamer/SRT threading issues during pipeline initialization
        if !no_auto_restart {
            let state_for_restart = state.clone();
            tokio::spawn(async move {
                restart_flows(&state_for_restart).await;
            });
        } else {
            info!("Auto-restart disabled by --no-auto-restart flag");
        }

        // Run HTTP server with graceful shutdown for both SIGINT (Ctrl+C) and SIGTERM (Docker stop)
        let shutdown_signal = async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};

                let mut sigterm =
                    signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");

                tokio::select! {
                    _ = sigterm.recv() => {
                        info!("Received SIGTERM, shutting down gracefully...");
                    }
                    _ = sigint.recv() => {
                        info!("Received SIGINT (Ctrl+C), shutting down gracefully...");
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c()
                    .await
                    .expect("Failed to install Ctrl+C handler");
                info!("Received Ctrl+C, shutting down gracefully...");
            }

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
async fn run_headless(config: Config, no_auto_restart: bool) -> anyhow::Result<()> {
    // Initialize GStreamer INSIDE tokio runtime
    gstreamer::init()?;
    info!("GStreamer initialized");

    // Register GStreamer plugins statically
    gstwebrtchttp::plugin_register_static().expect("Could not register webrtchttp plugins");
    gstrsinter::plugin_register_static().expect("Could not register inter plugins");

    // Start GLib main loop in background thread for bus watch callbacks
    start_glib_main_loop();
    info!("GLib main loop started in background thread");

    info!("Configuration loaded");

    // Create application with persistent storage
    let state = if let Some(ref db_url) = config.database_url {
        info!("Using PostgreSQL storage");
        AppState::with_postgres_storage(db_url, &config.blocks_path, &config.media_path).await?
    } else {
        info!("Using JSON file storage");
        AppState::with_json_storage(&config.flows_path, &config.blocks_path, &config.media_path)
    };
    state.load_from_storage().await?;

    // Start background services (SAP discovery listener and announcer)
    state.start_services().await;

    // GStreamer elements are discovered lazily on first /api/elements request

    // Create the HTTP app BEFORE auto-restart, then bind AFTER
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

    // Restart flows AFTER server binds, on a separate tokio task
    // This ensures auto-restart runs on a worker thread, not the main thread,
    // which prevents GStreamer/SRT threading issues during pipeline initialization
    if !no_auto_restart {
        let state_for_restart = state.clone();
        tokio::spawn(async move {
            restart_flows(&state_for_restart).await;
        });
    } else {
        info!("Auto-restart disabled by --no-auto-restart flag");
    }

    // Set up graceful shutdown handler for both SIGINT (Ctrl+C) and SIGTERM (Docker stop)
    let shutdown_signal = async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let mut sigterm =
                signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
            let mut sigint =
                signal(SignalKind::interrupt()).expect("Failed to install SIGINT handler");

            tokio::select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, shutting down gracefully...");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT (Ctrl+C), shutting down gracefully...");
                }
            }
        }

        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
            info!("Received Ctrl+C, shutting down gracefully...");
        }

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
    // Ensure SRT plugin is loaded before auto-restart
    // Manual flow starts work because by that time, plugins have been loaded naturally
    // We just need to trigger SRT plugin load without full discovery/introspection
    info!("Pre-loading SRT plugin before auto-restart...");
    tokio::task::spawn_blocking(|| {
        use gstreamer as gst;

        // Create and immediately drop a srtsink element to ensure SRT plugin is loaded
        // This initializes the SRT library without doing full element discovery
        match gst::ElementFactory::make("srtsink").build() {
            Ok(element) => {
                info!("SRT plugin loaded successfully");
                drop(element);
            }
            Err(e) => {
                error!("Failed to load SRT plugin: {}", e);
            }
        }
    })
    .await
    .expect("SRT plugin loading failed");

    info!("Restarting flows that have auto_restart enabled...");
    let flows = state.get_flows().await;

    // Separate flows into PTP and non-PTP
    let mut non_ptp_flows = Vec::new();
    let mut ptp_flows = Vec::new();

    for flow in flows {
        if flow.properties.auto_restart {
            if flow.properties.clock_type == GStreamerClockType::Ptp {
                ptp_flows.push(flow);
            } else {
                non_ptp_flows.push(flow);
            }
        }
    }

    let mut count = 0;

    // Start non-PTP flows immediately
    for flow in non_ptp_flows {
        count += 1;
        info!(
            "Auto-restarting flow {}: {} ({}) [non-PTP]",
            count, flow.name, flow.id
        );
        match state.start_flow(&flow.id).await {
            Ok(_) => {
                info!("Successfully restarted flow: {}", flow.name);
                // Small delay between flow starts to avoid overwhelming GStreamer
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            Err(e) => {
                error!("Failed to restart flow {}: {}", flow.name, e);
            }
        }
    }

    // For PTP flows, wait for PTP clock to sync before starting
    if !ptp_flows.is_empty() {
        // Collect unique PTP domains that need to be synced
        let mut domains_to_wait: std::collections::HashSet<u8> = std::collections::HashSet::new();
        for flow in &ptp_flows {
            let domain = flow.properties.ptp_domain.unwrap_or(0);
            domains_to_wait.insert(domain);
            // Register the flow with PTP monitor so it starts tracking the domain
            if let Err(e) = state.ptp_monitor().register_flow(flow.id, domain) {
                warn!(
                    "Failed to register flow {} for PTP domain {}: {}",
                    flow.name, domain, e
                );
            }
        }

        info!(
            "Waiting for PTP sync on {} domain(s) before starting {} PTP flow(s)...",
            domains_to_wait.len(),
            ptp_flows.len()
        );

        // Wait for all PTP domains to sync (with timeout)
        const PTP_SYNC_TIMEOUT_SECS: u64 = 30;
        const PTP_POLL_INTERVAL_MS: u64 = 500;
        let start_time = std::time::Instant::now();

        loop {
            // Check if all domains are synced
            let all_synced = domains_to_wait
                .iter()
                .all(|domain| state.ptp_monitor().is_domain_synced(*domain));

            if all_synced {
                info!("All PTP domains synchronized, starting PTP flows...");
                break;
            }

            // Check timeout
            if start_time.elapsed().as_secs() >= PTP_SYNC_TIMEOUT_SECS {
                warn!(
                    "PTP sync timeout after {}s, starting PTP flows anyway (may have clock issues)",
                    PTP_SYNC_TIMEOUT_SECS
                );
                break;
            }

            // Log which domains are still waiting
            let unsynced: Vec<u8> = domains_to_wait
                .iter()
                .filter(|d| !state.ptp_monitor().is_domain_synced(**d))
                .copied()
                .collect();
            info!(
                "Waiting for PTP sync on domain(s) {:?} ({:.1}s elapsed)",
                unsynced,
                start_time.elapsed().as_secs_f32()
            );

            tokio::time::sleep(tokio::time::Duration::from_millis(PTP_POLL_INTERVAL_MS)).await;
        }

        // Now start PTP flows
        for flow in ptp_flows {
            count += 1;
            let domain = flow.properties.ptp_domain.unwrap_or(0);
            let synced = state.ptp_monitor().is_domain_synced(domain);
            info!(
                "Auto-restarting flow {}: {} ({}) [PTP domain {}, synced={}]",
                count, flow.name, flow.id, domain, synced
            );
            match state.start_flow(&flow.id).await {
                Ok(_) => {
                    info!("Successfully restarted flow: {}", flow.name);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    error!("Failed to restart flow {}: {}", flow.name, e);
                }
            }
        }
    }

    if count > 0 {
        info!("Auto-restart complete: {} flow(s) restarted", count);
    }
}

/// Start GLib main loop in a background thread.
/// This is required for GStreamer bus watch callbacks to be dispatched.
fn start_glib_main_loop() {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        info!("GLib main loop thread started");
        let main_loop = glib::MainLoop::new(None, false);

        // Signal that the main loop is ready
        tx.send(()).ok();

        main_loop.run();
        info!("GLib main loop thread exiting");
    });

    // Wait for confirmation that the GLib main loop thread has started
    rx.recv().expect("Failed to start GLib main loop");
    info!("GLib main loop is now running");
}
