//! Strom frontend library.
//!
//! This module exposes the frontend application for embedding in native mode.

#![warn(clippy::all, rust_2018_idioms)]
#![deny(clippy::disallowed_types)]
#![allow(dead_code)]

mod api;
mod app;
mod clocks;
mod compositor_editor;
mod discovery;
mod graph;
mod info_page;
mod links;
mod list_navigator;
mod login;
mod media;
mod mediaplayer;
mod meter;
mod palette;
mod properties;
mod ptp_monitor;
mod qos_monitor;
mod state;
mod system_monitor;
mod themes;
mod thread_monitor;
mod webrtc_stats;
mod ws;

// Re-export the app for use by the backend
pub use app::StromApp;

/// Load the app icon for native windows
#[cfg(not(target_arch = "wasm32"))]
fn load_icon() -> Option<egui::IconData> {
    let icon_bytes = include_bytes!("icon.png");
    let image = image::load_from_memory(icon_bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

// Re-export the native entry point (without tracing init - parent should handle that)
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native_gui(port: u16) -> eframe::Result<()> {
    tracing::info!(
        "Initializing Strom frontend (embedded mode) connecting to port {}",
        port
    );

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 720.0])
        .with_title("Strom");

    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
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

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 720.0])
        .with_title("Strom");

    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
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
