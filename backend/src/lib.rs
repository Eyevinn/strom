//! Strom backend library.
//!
//! This module exposes the application builder for use in tests.

use axum::http::{header, HeaderValue, Method};
use axum::{middleware, routing::get, routing::patch, routing::post, Extension, Router};
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_sessions::{cookie::time::Duration, Expiry, MemoryStore, SessionManagerLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

pub mod api;
pub mod assets;
pub mod auth;
pub mod blocks;
pub mod config;
pub mod events;
pub mod gst;
pub mod gui;
pub mod layout;
pub mod openapi;
pub mod paths;
pub mod state;
pub mod stats;
pub mod storage;
pub mod system_monitor;
pub mod version;

use state::AppState;

/// Create the Axum application router.
///
/// This function is used both by the main server binary and by integration tests.
pub async fn create_app() -> Router {
    create_app_with_state(AppState::default()).await
}

/// Create the Axum application router with a given state.
pub async fn create_app_with_state(state: AppState) -> Router {
    create_app_with_state_and_auth(state, auth::AuthConfig::from_env()).await
}

/// Create the Axum application router with a given state and auth configuration.
pub async fn create_app_with_state_and_auth(
    state: AppState,
    auth_config: auth::AuthConfig,
) -> Router {
    // Initialize GStreamer (idempotent - OK if already initialized)
    if let Err(e) = gstreamer::init() {
        tracing::warn!(
            "GStreamer initialization warning (may already be initialized): {}",
            e
        );
    }

    let auth_config = Arc::new(auth_config);

    if auth_config.enabled {
        tracing::info!("Authentication enabled");
        if auth_config.has_session_auth() {
            tracing::info!("  - Session authentication configured");
        }
        if auth_config.has_api_key_auth() {
            tracing::info!("  - API key authentication configured");
        }
    } else {
        tracing::warn!("Authentication disabled - all endpoints are public!");
    }

    // Create session store (in-memory, sessions lost on restart)
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_expiry(Expiry::OnInactivity(Duration::hours(24)));

    // Build protected API router (requires authentication)
    let protected_api_router = Router::new()
        .route("/flows", get(api::flows::list_flows))
        .route("/flows", post(api::flows::create_flow))
        .route("/flows/{id}", get(api::flows::get_flow))
        .route("/flows/{id}", post(api::flows::update_flow))
        .route(
            "/flows/{id}",
            axum::routing::delete(api::flows::delete_flow),
        )
        .route("/flows/{id}/start", post(api::flows::start_flow))
        .route("/flows/{id}/stop", post(api::flows::stop_flow))
        .route("/flows/{id}/latency", get(api::flows::get_flow_latency))
        .route("/flows/{id}/stats", get(api::flows::get_flow_stats))
        .route("/flows/{id}/debug-graph", get(api::flows::debug_graph))
        .route(
            "/flows/{id}/properties",
            patch(api::flows::update_flow_properties),
        )
        .route(
            "/flows/{id}/webrtc-stats",
            get(api::flows::get_webrtc_stats),
        )
        .route(
            "/flows/{flow_id}/blocks/{block_id}/sdp",
            get(api::flows::get_block_sdp),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/properties",
            get(api::flows::get_element_properties),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/properties",
            patch(api::flows::update_element_property),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
            get(api::flows::get_pad_properties),
        )
        .route(
            "/flows/{flow_id}/elements/{element_id}/pads/{pad_name}/properties",
            patch(api::flows::update_pad_property),
        )
        .route("/elements", get(api::elements::list_elements))
        .route("/elements/{name}", get(api::elements::get_element_info))
        .route(
            "/elements/{name}/pads",
            get(api::elements::get_element_pad_properties),
        )
        .route("/blocks", get(api::blocks::list_blocks))
        .route("/blocks", post(api::blocks::create_block))
        .route("/blocks/categories", get(api::blocks::get_categories))
        .route("/blocks/{id}", get(api::blocks::get_block))
        .route(
            "/blocks/{id}",
            axum::routing::put(api::blocks::update_block),
        )
        .route(
            "/blocks/{id}",
            axum::routing::delete(api::blocks::delete_block),
        )
        .route("/version", get(api::version::get_version))
        .route("/ws", get(api::websocket::websocket_handler))
        // gst-launch-1.0 import/export
        .route("/gst-launch/parse", post(api::gst_launch::parse_gst_launch))
        .route(
            "/gst-launch/export",
            post(api::gst_launch::export_gst_launch),
        )
        // Apply authentication middleware to all protected routes
        .layer(middleware::from_fn(auth::auth_middleware));

    // Build public API router (no authentication required)
    let public_api_router = Router::new()
        .route("/login", post(auth::login_handler))
        .route("/logout", post(auth::logout_handler))
        .route("/auth/status", get(auth::auth_status_handler));

    // Combine routers with auth config extension
    let api_router = Router::new()
        .merge(public_api_router)
        .merge(protected_api_router)
        .layer(Extension(auth_config));

    // Build main router with Swagger UI
    Router::new()
        .route("/health", get(health))
        .merge(
            SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", openapi::ApiDoc::openapi()),
        )
        .nest("/api", api_router)
        .layer(session_layer)
        .layer(
            CorsLayer::new()
                .allow_origin("http://localhost:8080".parse::<HeaderValue>().unwrap())
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::PATCH,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::AUTHORIZATION,
                    header::ACCEPT,
                    header::COOKIE,
                ])
                .allow_credentials(true),
        )
        .with_state(state)
        // Serve embedded frontend for all other routes
        .fallback(assets::serve_static)
}

/// Health check endpoint.
async fn health() -> &'static str {
    "OK"
}
