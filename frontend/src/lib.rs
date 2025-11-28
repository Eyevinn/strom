//! Strom frontend library.
//!
//! This module exposes the frontend application for embedding in native mode.

#![warn(clippy::all, rust_2018_idioms)]
#![allow(dead_code)]

mod api;
mod app;
mod graph;
mod login;
mod meter;
mod palette;
mod properties;
mod state;
mod system_monitor;
mod webrtc_stats;
mod ws;

// Re-export the app for use by the backend
pub use app::StromApp;

// Re-export the native entry point (without tracing init - parent should handle that)
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native_gui(port: u16) -> eframe::Result<()> {
    tracing::info!(
        "Initializing Strom frontend (embedded mode) connecting to port {}",
        port
    );

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("Strom - GStreamer Flow Engine"),
        ..Default::default()
    };

    eframe::run_native(
        "Strom",
        native_options,
        Box::new(move |cc| {
            // Theme is now set by the app based on user preference
            Ok(Box::new(StromApp::new(cc, port)))
        }),
    )
}

// Native entry point with shutdown handler for Ctrl+C
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native_gui_with_shutdown(
    port: u16,
    shutdown_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    auth_token: Option<String>,
) -> eframe::Result<()> {
    tracing::info!(
        "Initializing Strom frontend (embedded mode with shutdown handler) connecting to port {}",
        port
    );

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("Strom - GStreamer Flow Engine"),
        ..Default::default()
    };

    eframe::run_native(
        "Strom",
        native_options,
        Box::new(move |cc| {
            // Theme is now set by the app based on user preference
            Ok(Box::new(StromApp::new_with_shutdown_and_auth(
                cc,
                port,
                shutdown_flag,
                auth_token,
            )))
        }),
    )
}
