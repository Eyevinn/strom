//! Strom frontend application.
//!
//! Supports both WASM (for web browsers) and native (embedded in backend) modes.

#![warn(clippy::all, rust_2018_idioms)]
// Allow dead code in frontend - code is used through WASM/eframe traits
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
mod webrtc_stats;
mod ws;

// Make StromApp public so it can be used by the backend
pub use app::StromApp;

// ============================================================================
// WASM Entry Point
// ============================================================================

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    // Initialize panic handler for better error messages in browser console
    console_error_panic_hook::set_once();

    // Initialize tracing for WASM with info level (less verbose)
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::default()
            .set_max_level(tracing::Level::INFO)
            .build(),
    );

    let web_options = eframe::WebOptions::default();

    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window()
            .expect("No window")
            .document()
            .expect("No document");
        let canvas = document
            .get_element_by_id("strom_app_canvas")
            .expect("Failed to find strom_app_canvas")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("strom_app_canvas is not a canvas");

        eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| {
                    // Theme is now set by the app based on user preference
                    Ok(Box::new(StromApp::new(cc)))
                }),
            )
            .await
            .expect("Failed to start eframe");
    });
}

// ============================================================================
// Native Entry Point
// ============================================================================

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

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    // Initialize tracing for native
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!("Starting Strom frontend in native mode");

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
        Box::new(|cc| {
            // Theme is now set by the app based on user preference
            Ok(Box::new(StromApp::new(cc, strom_types::DEFAULT_PORT)))
        }),
    )
}
